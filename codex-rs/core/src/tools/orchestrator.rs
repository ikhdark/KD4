/*
Module: orchestrator

Central place for approvals + sandbox selection + retry semantics. Drives a
simple sequence for any ToolRuntime: approval → select sandbox → attempt →
retry with an escalated sandbox strategy only when a denial is proven safe to
replay (no re-approval thanks to caching).
*/
use crate::hook_runtime::run_permission_request_hooks;
use crate::tools::flat_tool_name;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::SandboxOverride;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::default_exec_approval_requirement;
use crate::tools::sandboxing::sandbox_override_for_first_attempt;
use crate::tools::sandboxing::unsandboxed_execution_allowed;
use codex_hooks::PermissionRequestDecision;
use codex_otel::ToolDecisionSource;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxType;
use codex_sandboxing::select_initial;
use codex_sandboxing::should_sandbox;
use std::time::Instant;

pub(crate) struct ToolOrchestrator;

pub(crate) struct OrchestratorRunResult<Out> {
    pub output: Out,
}

impl ToolOrchestrator {
    pub fn new() -> Self {
        Self
    }

    async fn run_attempt<Rq, Out, T>(
        tool: &mut T,
        req: &Rq,
        tool_ctx: &ToolCtx,
        attempt: &SandboxAttempt<'_>,
    ) -> Result<Out, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        let timing = tool_ctx.turn.turn_timing_state.begin_tool_execution();
        let result = tool.run(req, attempt, tool_ctx).await;
        drop(timing);
        result
    }

    pub async fn run<Rq, Out, T>(
        &mut self,
        tool: &mut T,
        req: &Rq,
        tool_ctx: &ToolCtx,
        turn_ctx: &crate::session::turn_context::TurnContext,
        approval_policy: AskForApproval,
    ) -> Result<OrchestratorRunResult<Out>, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        let otel = turn_ctx.session_telemetry.clone();
        let otel_tn = flat_tool_name(&tool_ctx.tool_name).into_owned();
        let otel_ci = &tool_ctx.call_id;

        // 1) Approval
        let mut already_approved = false;

        let file_system_sandbox_policy = turn_ctx.file_system_sandbox_policy();
        let network_sandbox_policy = turn_ctx.network_sandbox_policy();
        let requirement = tool.exec_approval_requirement(req).unwrap_or_else(|| {
            default_exec_approval_requirement(approval_policy, &file_system_sandbox_policy)
        });
        match &requirement {
            ExecApprovalRequirement::Skip { .. } => {
                otel.tool_decision(
                    &otel_tn,
                    otel_ci,
                    &ReviewDecision::Approved,
                    ToolDecisionSource::Config,
                );
            }
            ExecApprovalRequirement::Forbidden { reason } => {
                return Err(ToolError::Denied(reason.clone()));
            }
            ExecApprovalRequirement::NeedsApproval { reason, .. } => {
                let approval_ctx = ApprovalCtx {
                    session: &tool_ctx.session,
                    turn: &tool_ctx.turn,
                    call_id: &tool_ctx.call_id,
                    retry_reason: reason.clone(),
                    network_approval_context: None,
                };
                let decision = Self::request_approval(
                    tool,
                    req,
                    tool_ctx.call_id.as_str(),
                    approval_ctx,
                    tool_ctx,
                    &otel,
                )
                .await?;

                Self::reject_if_not_approved(decision).await?;
                already_approved = true;
            }
        }

        // 2) First attempt under the selected sandbox.
        let sandbox_override = sandbox_override_for_first_attempt(
            tool.sandbox_permissions(req),
            &requirement,
            &file_system_sandbox_policy,
        );
        let sandbox_preference = tool.sandbox_preference();
        let sandbox_requested = match sandbox_override {
            SandboxOverride::BypassSandboxFirstAttempt => false,
            SandboxOverride::NoOverride => should_sandbox(
                &file_system_sandbox_policy,
                network_sandbox_policy,
                sandbox_preference,
                false,
            ),
        };
        let initial_sandbox = if sandbox_requested {
            select_initial(
                &file_system_sandbox_policy,
                network_sandbox_policy,
                sandbox_preference,
                turn_ctx.windows_sandbox_level,
                false,
            )
        } else {
            SandboxType::None
        };

        let sandbox_policy_cwd = tool
            .sandbox_cwd(req)
            .cloned()
            .unwrap_or_else(|| turn_ctx.cwd_uri());
        let workspace_roots = turn_ctx.effective_workspace_roots();
        let initial_attempt = SandboxAttempt {
            codex_home: &turn_ctx.config.codex_home,
            sandbox: initial_sandbox,
            sandbox_requested,
            permissions: &turn_ctx.permission_profile,
            exec_server_permissions: turn_ctx.config.permissions.permission_profile(),
            sandbox_cwd: &sandbox_policy_cwd,
            workspace_roots,
            windows_sandbox_level: turn_ctx.windows_sandbox_level,
            windows_sandbox_private_desktop: turn_ctx
                .config
                .permissions
                .windows_sandbox_private_desktop,
        };

        let initial_attempt_start = Instant::now();
        let first_result = Self::run_attempt(tool, req, tool_ctx, &initial_attempt).await;
        let initial_duration = initial_attempt_start.elapsed();
        match first_result {
            Ok(out) => {
                // We have a successful initial result
                Ok(OrchestratorRunResult {
                    output: out,
                })
            }
            Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output,
                network_policy_decision,
            }))) => {
                if network_policy_decision.is_some() {
                    otel.sandbox_outcome(
                        &otel_tn,
                        otel_ci,
                        "denied",
                        initial_duration,
                        /*escalated_duration*/ None,
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output,
                        network_policy_decision,
                    })));
                }
                if !tool.escalate_on_failure() || !tool.sandbox_denial_replay_is_safe() {
                    otel.sandbox_outcome(
                        &otel_tn,
                        otel_ci,
                        "denied",
                        initial_duration,
                        /*escalated_duration*/ None,
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output,
                        network_policy_decision,
                    })));
                }
                let unsandboxed_allowed =
                    unsandboxed_execution_allowed(&file_system_sandbox_policy);
                // Under `Never` or `OnRequest`, do not retry without sandbox;
                // surface a concise sandbox denial that preserves the
                // original output.
                if !tool.wants_no_sandbox_approval(approval_policy) {
                    otel.sandbox_outcome(
                        &otel_tn, otel_ci, "denied", initial_duration, None,
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output, network_policy_decision,
                    })));
                }
                if !unsandboxed_allowed {
                    otel.sandbox_outcome(
                        &otel_tn,
                        otel_ci,
                        "denied",
                        initial_duration,
                        /*escalated_duration*/ None,
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output,
                        network_policy_decision,
                    })));
                }
                let bypass_retry_approval = tool
                    .should_bypass_approval(approval_policy, already_approved);
                if !bypass_retry_approval {
                    let retry_reason = build_denial_reason_from_output(output.as_ref());
                    let approval_ctx = ApprovalCtx {
                        session: &tool_ctx.session,
                        turn: &tool_ctx.turn,
                        call_id: &tool_ctx.call_id,
                        retry_reason: Some(retry_reason),
                        network_approval_context: None,
                    };

                    let permission_request_run_id = format!("{}:retry", tool_ctx.call_id);
                    let decision = Self::request_approval(
                        tool,
                        req,
                        &permission_request_run_id,
                        approval_ctx,
                        tool_ctx,
                        &otel,
                    )
                    .await;
                    let approved = match decision {
                        Ok(decision) => Self::reject_if_not_approved(decision).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = approved {
                        return Err(ToolError::Denied(format!(
                            "retry refused; no second attempt: {error:?}\nInitial command exit code: {}\nCommand output:\n{}",
                            output.exit_code,
                            bounded_denial_diagnostic(output.as_ref())
                                .unwrap_or_else(|| "[no diagnostic output]".into()),
                        )));
                    }
                }

                let retry_sandbox_requested = !unsandboxed_allowed
                    && should_sandbox(
                        &file_system_sandbox_policy,
                        network_sandbox_policy,
                        sandbox_preference,
                        false,
                    );
                let retry_sandbox = if retry_sandbox_requested {
                    select_initial(
                        &file_system_sandbox_policy,
                        network_sandbox_policy,
                        sandbox_preference,
                        turn_ctx.windows_sandbox_level,
                        false,
                    )
                } else {
                    SandboxType::None
                };
                let retry_attempt = SandboxAttempt {
                    codex_home: &turn_ctx.config.codex_home,
                    sandbox: retry_sandbox,
                    sandbox_requested: retry_sandbox_requested,
                    permissions: &turn_ctx.permission_profile,
                    exec_server_permissions: turn_ctx.config.permissions.permission_profile(),
                    sandbox_cwd: &sandbox_policy_cwd,
                    workspace_roots,
                    windows_sandbox_level: turn_ctx.windows_sandbox_level,
                    windows_sandbox_private_desktop: turn_ctx
                        .config
                        .permissions
                        .windows_sandbox_private_desktop,
                                };

                // Second attempt.
                if let Some(timing) =
                    crate::tools::tool_dispatch_trace::active_tool_dispatch_timing()
                {
                    timing.increment_retry_count();
                }
                let escalated_attempt_start = Instant::now();
                let retry_result = Self::run_attempt(tool, req, tool_ctx, &retry_attempt).await;
                let escalated_duration = escalated_attempt_start.elapsed();
                match retry_result {
                    Ok(output) => {
                        otel.sandbox_outcome(
                            &otel_tn,
                            otel_ci,
                            "escalated",
                            initial_duration,
                            Some(escalated_duration),
                        );
                        Ok(OrchestratorRunResult {
                            output,
                        })
                    }
                    Err(err) => {
                        if let Some(outcome) = sandbox_outcome_from_tool_error(&err) {
                            otel.sandbox_outcome(
                                &otel_tn,
                                otel_ci,
                                outcome,
                                initial_duration,
                                Some(escalated_duration),
                            );
                        }
                        Err(err)
                    }
                }
            }
            Err(err) => {
                if let Some(outcome) = sandbox_outcome_from_tool_error(&err) {
                    otel.sandbox_outcome(
                        &otel_tn,
                        otel_ci,
                        outcome,
                        initial_duration,
                        /*escalated_duration*/ None,
                    );
                }
                Err(err)
            }
        }
    }

    // PermissionRequest hooks take top precedence for answering approval
    // prompts. If no matching hook returns a decision, fall back to the
    // normal user approval path.
    async fn request_approval<Rq, Out, T>(
        tool: &mut T,
        req: &Rq,
        permission_request_run_id: &str,
        approval_ctx: ApprovalCtx<'_>,
        tool_ctx: &ToolCtx,
        otel: &codex_otel::SessionTelemetry,
    ) -> Result<ReviewDecision, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        if let Some(permission_request) = tool.permission_request_payload(req) {
            let tool_name = flat_tool_name(&tool_ctx.tool_name);
            match run_permission_request_hooks(
                approval_ctx.session,
                approval_ctx.turn,
                permission_request_run_id,
                permission_request,
            )
            .await
            {
                Some(PermissionRequestDecision::Allow) => {
                    let decision = ReviewDecision::Approved;
                    otel.tool_decision(
                        tool_name.as_ref(),
                        &tool_ctx.call_id,
                        &decision,
                        ToolDecisionSource::Config,
                    );
                    return Ok(decision);
                }
                Some(PermissionRequestDecision::Deny { message }) => {
                    let decision = ReviewDecision::Denied;
                    otel.tool_decision(
                        tool_name.as_ref(),
                        &tool_ctx.call_id,
                        &decision,
                        ToolDecisionSource::Config,
                    );
                    return Err(ToolError::Denied(message));
                }
                None => {}
            }
        }

        let otel_source = ToolDecisionSource::User;
        let decision = tool.start_approval_async(req, approval_ctx).await;
        let tool_name = flat_tool_name(&tool_ctx.tool_name);
        otel.tool_decision(
            tool_name.as_ref(),
            &tool_ctx.call_id,
            &decision,
            otel_source,
        );
        Ok(decision)
    }

    async fn reject_if_not_approved(decision: ReviewDecision) -> Result<(), ToolError> {
        match decision {
            ReviewDecision::Denied | ReviewDecision::Abort => {
                let reason = "rejected by user".to_string();
                Err(ToolError::Denied(reason))
            }
            ReviewDecision::Approved
            | ReviewDecision::ApprovedExecpolicyAmendment { .. }
            | ReviewDecision::ApprovedForSession => Ok(()),
            ReviewDecision::NetworkPolicyAmendment { .. } => Err(ToolError::Denied(
                "managed network proxy support was removed; network policy amendments cannot be enforced".to_string(),
            )),
        }
    }
}

fn sandbox_outcome_from_tool_error(err: &ToolError) -> Option<&'static str> {
    match err {
        ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied { .. })) => Some("denied"),
        ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { .. })) => Some("timed_out"),
        ToolError::Codex(CodexErr::Sandbox(SandboxErr::Signal(_))) => Some("signal"),
        ToolError::Denied(_) | ToolError::Rejected(_) | ToolError::Codex(_) => None,
    }
}

fn build_denial_reason_from_output(output: &ExecToolCallOutput) -> String {
    let reason = "command failed; retry without sandbox?";
    match bounded_denial_diagnostic(output) {
        Some(excerpt) => format!("{reason}\nCommand output:\n{excerpt}"),
        None => reason.to_string(),
    }
}

fn bounded_denial_diagnostic(output: &ExecToolCallOutput) -> Option<String> {
    let diagnostic = [
        &output.stderr.text,
        &output.aggregated_output.text,
        &output.stdout.text,
    ]
    .into_iter()
    .map(|text| text.trim())
    .find(|text| !text.is_empty());
    let diagnostic = diagnostic?;
    let mut chars = diagnostic.chars();
    let mut excerpt: String = chars.by_ref().take(1_024).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    Some(excerpt)
}

