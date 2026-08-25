/*
Runtime: shell

Executes shell requests under the orchestrator: asks for approval when needed,
builds sandbox transform inputs, and runs them under the current SandboxAttempt.
*/
use crate::command_canonicalization::canonicalize_command_for_approval;
use crate::exec::CommandProgress;
use crate::exec::ExecCapturePolicy;
use crate::guardian::GuardianNetworkAccessTrigger;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::SandboxPermissions;
use crate::sandboxing::execute_exec_request_with_after_spawn;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::flat_tool_name;
use crate::tools::known_delta_store::KnownDeltaHit;
use crate::tools::known_delta_store::PreparedKnownDelta;
use crate::tools::network_approval::NetworkApprovalMode;
use crate::tools::network_approval::NetworkApprovalSpec;
use crate::tools::runtimes::RuntimePathPrepends;
use crate::tools::runtimes::build_sandbox_command;
use crate::tools::runtimes::disable_powershell_profile_for_elevated_windows_sandbox;
use crate::tools::runtimes::exec_env_for_sandbox_permissions;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot_file;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalAction;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::managed_network_for_sandbox_permissions;
use crate::tools::sandboxing::sandbox_permissions_preserving_denied_reads;
use crate::tools::sandboxing::with_cached_approval;
use codex_network_proxy::NetworkProxy;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxablePreference;
use codex_shell_command::powershell::prefix_powershell_script_with_utf8;

use codex_shell_command::powershell::prove_noprofile_powershell_command_as_direct_argv;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ShellRequest {
    pub command: Vec<String>,
    /// Semantically equivalent, inspectable command used for approvals and
    /// approval caching when `command` contains an encoded runtime payload.
    pub command_for_approval: Vec<String>,

    pub approved_powershell_direct_argv: Option<Vec<String>>,
    pub turn_environment: TurnEnvironment,
    pub shell_type: Option<ShellType>,
    pub hook_command: String,
    pub cwd: AbsolutePathBuf,
    pub timeout_ms: Option<u64>,
    pub stall_timeout_ms: Option<u64>,
    pub cancellation_token: CancellationToken,
    pub env: HashMap<String, String>,
    pub explicit_env_overrides: HashMap<String, String>,
    pub network: Option<NetworkProxy>,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub justification: Option<String>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub(crate) known_delta: Option<PreparedKnownDelta>,
    pub(crate) validation_launch: Option<crate::validation_admission::ValidationLaunchPlan>,
    pub(crate) workspace_operation_root: Option<PathBuf>,
}

pub struct ShellRuntime;

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ApprovalKey {
    environment_id: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
}

impl ShellRuntime {
    pub(crate) fn for_shell_command() -> Self {
        Self
    }

    fn stdout_stream(
        ctx: &ToolCtx,
        progress: Option<CommandProgress>,
    ) -> Option<crate::exec::StdoutStream> {
        Some(crate::exec::StdoutStream {
            sub_id: ctx.turn.sub_id.clone(),
            call_id: ctx.call_id.clone(),
            tx_event: ctx.session.get_tx_event(),
            progress,
        })
    }
}

async fn wait_for_command_stall(
    mut progress: tokio::sync::watch::Receiver<u64>,
    stall_timeout: std::time::Duration,
) {
    loop {
        let deadline = tokio::time::sleep(stall_timeout);
        tokio::pin!(deadline);
        tokio::select! {
            biased;
            changed = progress.changed() => {
                if changed.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            _ = &mut deadline => return,
        }
    }
}

fn mark_command_stalled(output: &mut ExecToolCallOutput, stall_timeout_ms: u64) {
    let notice =
        format!("command stalled after {stall_timeout_ms} milliseconds without stdout or stderr");
    if !output.aggregated_output.text.is_empty() && !output.aggregated_output.text.ends_with('\n') {
        output.aggregated_output.text.push('\n');
    }
    output.aggregated_output.text.push_str(&notice);
    if !output.stderr.text.is_empty() && !output.stderr.text.ends_with('\n') {
        output.stderr.text.push('\n');
    }
    output.stderr.text.push_str(&notice);
    output.exit_code = crate::exec::EXEC_TIMEOUT_EXIT_CODE;
    output.timed_out = true;
}

impl Sandboxable for ShellRuntime {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }
    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<ShellRequest> for ShellRuntime {
    type ApprovalKey = ApprovalKey;

    fn approval_keys(&self, req: &ShellRequest) -> Vec<Self::ApprovalKey> {
        vec![ApprovalKey {
            environment_id: req.turn_environment.environment_id.clone(),
            command: canonicalize_command_for_approval(&req.command_for_approval),
            cwd: req.cwd.clone(),
            sandbox_permissions: req.sandbox_permissions,
            additional_permissions: req.additional_permissions.clone(),
        }]
    }

    fn start_approval_async<'a>(
        &'a mut self,
        req: &'a ShellRequest,
        ctx: ApprovalCtx<'a>,
    ) -> BoxFuture<'a, ReviewDecision> {
        let keys = self.approval_keys(req);
        let command = req.command_for_approval.clone();
        let cwd = req.cwd.clone();
        let environment_id = Some(req.turn_environment.environment_id.clone());
        let reason = ctx
            .retry_reason
            .clone()
            .or_else(|| req.justification.clone());
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        Box::pin(async move {
            with_cached_approval(&session.services, "shell", keys, move || async move {
                let available_decisions = None;
                session
                    .request_command_approval(
                        turn,
                        call_id,
                        /*approval_id*/ None,
                        environment_id,
                        command,
                        cwd,
                        reason,
                        ctx.network_approval_context.clone(),
                        req.exec_approval_requirement
                            .proposed_execpolicy_amendment()
                            .cloned(),
                        req.additional_permissions.clone(),
                        available_decisions,
                    )
                    .await
            })
            .await
        })
    }

    fn approval_action(
        &self,
        req: &ShellRequest,
        ctx: &ApprovalCtx<'_>,
    ) -> std::io::Result<ApprovalAction> {
        Ok(ApprovalAction::Shell {
            id: ctx.call_id.to_string(),
            command: req.command_for_approval.clone(),
            cwd: req.cwd.clone(),
            sandbox_permissions: req.sandbox_permissions,
            additional_permissions: req.additional_permissions.clone(),
            justification: req.justification.clone(),
        })
    }

    fn exec_approval_requirement(&self, req: &ShellRequest) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }

    fn permission_request_payload(&self, req: &ShellRequest) -> Option<PermissionRequestPayload> {
        Some(PermissionRequestPayload::bash(
            req.hook_command.clone(),
            req.justification.clone(),
        ))
    }

    fn sandbox_permissions(&self, req: &ShellRequest) -> SandboxPermissions {
        req.sandbox_permissions
    }
}

impl ToolRuntime<ShellRequest, ExecToolCallOutput> for ShellRuntime {
    fn network_approval_spec(
        &self,
        req: &ShellRequest,
        ctx: &ToolCtx,
    ) -> Option<NetworkApprovalSpec> {
        let file_system_sandbox_policy = ctx.turn.file_system_sandbox_policy();
        let sandbox_permissions = sandbox_permissions_preserving_denied_reads(
            req.sandbox_permissions,
            &file_system_sandbox_policy,
        );
        let network =
            managed_network_for_sandbox_permissions(req.network.as_ref(), sandbox_permissions)?;
        Some(NetworkApprovalSpec {
            network: Some(network.clone()),
            mode: NetworkApprovalMode::Immediate,
            trigger: GuardianNetworkAccessTrigger {
                call_id: ctx.call_id.clone(),
                tool_name: flat_tool_name(&ctx.tool_name).into_owned(),
                command: req.command.clone(),
                cwd: req.cwd.clone(),
                sandbox_permissions: req.sandbox_permissions,
                additional_permissions: req.additional_permissions.clone(),
                justification: req.justification.clone(),
                tty: None,
            },
            command: req.hook_command.clone(),
            environment_id: req.turn_environment.environment_id.clone(),
        })
    }

    async fn run(
        &mut self,
        req: &ShellRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecToolCallOutput, ToolError> {
        if let Some(hit_output) = req
            .known_delta
            .as_ref()
            .and_then(PreparedKnownDelta::hit)
            .map(KnownDeltaHit::rendered_output)
        {
            return Ok(ExecToolCallOutput {
                stdout: codex_protocol::exec_output::StreamOutput::new(hit_output.to_string()),
                aggregated_output: codex_protocol::exec_output::StreamOutput::new(
                    hit_output.to_string(),
                ),
                ..Default::default()
            });
        }
        let _workspace_operation_permit = match req.workspace_operation_root.as_deref() {
            Some(root) => {
                Some(crate::workspace_operation_gate::acquire_workspace_operation(root).await)
            }
            None => None,
        };
        let mutation = crate::turn_diff_tracker::command_mutation(
            &req.command_for_approval,
            Some(req.cwd.as_path()),
        );
        crate::tools::events::begin_exec_mutation_evidence(
            crate::tools::events::ToolEventCtx::new(
                ctx.session.as_ref(),
                ctx.turn.as_ref(),
                &ctx.call_id,
                None,
            ),
            Some(&req.cwd),
            &mutation,
        )
        .await;
        let session_shell = ctx.session.user_shell();
        let shell = req
            .turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        let shell_snapshot_location = req
            .turn_environment
            .shell_snapshot(&codex_utils_path_uri::PathUri::from_abs_path(&req.cwd))
            .await;
        let (file_system_sandbox_policy, _) = attempt.permissions.to_runtime_permissions();
        let sandbox_permissions = sandbox_permissions_preserving_denied_reads(
            req.sandbox_permissions,
            &file_system_sandbox_policy,
        );
        let managed_network =
            managed_network_for_sandbox_permissions(req.network.as_ref(), sandbox_permissions);
        let mut env = exec_env_for_sandbox_permissions(&req.env, sandbox_permissions);
        let explicit_env_overrides = req.explicit_env_overrides.clone();
        let runtime_path_prepends = RuntimePathPrepends;
        let command = maybe_wrap_shell_lc_with_snapshot_file(
            &req.command,
            shell,
            shell_snapshot_location.as_deref(),
            &explicit_env_overrides,
            &mut env,
            &runtime_path_prepends,
        );
        let command = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            req.shell_type.as_ref(),
            attempt.sandbox,
            attempt.windows_sandbox_level,
        );
        let command = if matches!(shell.shell_type, ShellType::PowerShell) {
            {
                if let Some(approved_command) = req.approved_powershell_direct_argv.as_ref()
                    && let Some(proof) = prove_noprofile_powershell_command_as_direct_argv(
                        &command,
                        req.cwd.as_path(),
                        &env,
                    )
                    && let Some(proven_command) =
                        proof.into_command_for_state(&command, req.cwd.as_path(), &env)
                    && &proven_command == approved_command
                {
                    proven_command
                } else {
                    prefix_powershell_script_with_utf8(&command)
                }
            }
        } else {
            command
        };

        let command =
            build_sandbox_command(&command, &req.cwd, &env, req.additional_permissions.clone())?;
        let mut expiration: crate::exec::ExecExpiration = req.timeout_ms.into();
        expiration = expiration.with_cancellation(req.cancellation_token.clone());
        let stall_cancellation = req.stall_timeout_ms.map(|_| CancellationToken::new());
        if let Some(cancellation) = stall_cancellation.as_ref() {
            expiration = expiration.with_cancellation(cancellation.clone());
        }
        if let Some(cancellation) = attempt.network_denial_cancellation_token.clone() {
            expiration = expiration.with_cancellation(cancellation);
        }
        let options = ExecOptions {
            expiration,
            capture_policy: ExecCapturePolicy::ShellTool,
        };
        let env = attempt
            .env_for(
                command,
                options,
                managed_network,
                Some(&req.turn_environment.environment_id),
            )
            .map_err(ToolError::Codex)?;
        let (authorization_guard, observation_token) = if let Some(launch) =
            req.validation_launch.as_ref()
        {
            let guard = Arc::clone(&ctx.turn.validation_authorization)
                .read_owned()
                .await;
            if guard.revision != launch.authorization_revision
                && !crate::validation_admission::admission_still_authorized(
                    &guard,
                    &launch.invocation,
                )
            {
                let Some(skipped) =
                    crate::validation_admission::prohibited_skip_for(&guard, &launch.invocation)
                else {
                    return Err(ToolError::Rejected(
                        "validation launch plan did not contain a validation command".to_string(),
                    ));
                };
                return Err(ToolError::ValidationSkipped(skipped));
            }
            let token = match (
                launch.observation.clone(),
                ctx.session.services.state_db.clone(),
            ) {
                (Some(observation), Some(state)) => Some(
                    crate::validation_admission::ValidationObservationToken::new(
                        observation,
                        state,
                    ),
                ),
                _ => None,
            };
            (Some(guard), token)
        } else {
            (None, None)
        };
        let arm_token = observation_token.clone();
        let after_spawn = authorization_guard.map(|guard| {
            Box::new(move || {
                if let Some(token) = arm_token {
                    token.arm();
                }
                drop(guard);
            }) as Box<dyn FnOnce() + Send>
        });
        let progress = req.stall_timeout_ms.map(|_| CommandProgress::new());
        let progress_observer = progress.as_ref().map(CommandProgress::subscribe);
        let execution = execute_exec_request_with_after_spawn(
            env,
            Self::stdout_stream(ctx, progress),
            after_spawn,
        );
        tokio::pin!(execution);
        let (out, stalled) = match (req.stall_timeout_ms, progress_observer, stall_cancellation) {
            (Some(stall_timeout_ms), Some(progress_observer), Some(stall_cancellation)) => {
                let stall = wait_for_command_stall(
                    progress_observer,
                    std::time::Duration::from_millis(stall_timeout_ms),
                );
                tokio::pin!(stall);
                tokio::select! {
                    biased;
                    result = &mut execution => (result, false),
                    _ = &mut stall => {
                        stall_cancellation.cancel();
                        (execution.await, true)
                    }
                }
            }
            _ => (execution.await, false),
        };
        let mut out = match out {
            Ok(out) => out,
            Err(CodexErr::Sandbox(SandboxErr::Timeout { output })) if stalled => *output,
            Err(err) => return Err(ToolError::Codex(err)),
        };
        if stalled && let Some(stall_timeout_ms) = req.stall_timeout_ms {
            mark_command_stalled(&mut out, stall_timeout_ms);
        }
        if let Some(token) = observation_token {
            let elapsed_ms = u64::try_from(out.duration.as_millis()).unwrap_or(u64::MAX);
            if req.cancellation_token.is_cancelled() || out.timed_out {
                token.record_cancelled(elapsed_ms).await;
            } else {
                token.record_completed(elapsed_ms).await;
            }
        }
        if stalled {
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout {
                output: Box::new(out),
            })));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
