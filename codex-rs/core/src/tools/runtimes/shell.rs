/*
Runtime: shell

Executes shell requests under the orchestrator: asks for approval when needed,
builds sandbox transform inputs, and runs them under the current SandboxAttempt.
*/
use crate::command_canonicalization::canonicalize_command_for_approval;
use crate::exec::CommandProgress;
use crate::exec::ExecCapturePolicy;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::SandboxPermissions;
use crate::sandboxing::execute_exec_request_with_after_spawn;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::runtimes::ShellCommandPreparation;
use crate::tools::runtimes::build_sandbox_command;
use crate::tools::runtimes::prepare_shell_command;
use crate::tools::runtimes::shell_snapshot_additional_read_roots;
use crate::tools::sandboxing::Approvable;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::PermissionRequestPayload;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::Sandboxable;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::with_cached_approval;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxablePreference;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ShellRequest {
    pub command: Vec<String>,
    /// Semantically equivalent, inspectable command used for approvals and
    /// approval caching when `command` contains an encoded runtime payload.
    pub command_for_approval: Vec<String>,
    pub shell_wrapper_is_owned: bool,

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
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub justification: Option<String>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub(crate) validation_launch: bool,
}

pub struct ShellRuntime {
    last_validation_attempt_output: Option<ExecToolCallOutput>,
    last_validation_attempt_started: bool,
    raw_output: Option<crate::tools::command_output_artifact::capture::CapturedShellOutput>,
    validation_raw_output:
        Option<crate::tools::command_output_artifact::capture::CapturedShellOutput>,
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ApprovalKey {
    environment_id: String,
    approval_scope_id: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
}

impl ShellRuntime {
    pub(crate) fn for_shell_command() -> Self {
        Self {
            last_validation_attempt_output: None,
            last_validation_attempt_started: false,
            raw_output: None,
            validation_raw_output: None,
        }
    }

    pub(crate) fn take_last_validation_attempt_output(&mut self) -> Option<ExecToolCallOutput> {
        self.last_validation_attempt_output.take()
    }

    pub(crate) fn take_last_validation_attempt_started(&mut self) -> bool {
        std::mem::take(&mut self.last_validation_attempt_started)
    }

    pub(crate) fn take_raw_output(
        &mut self,
        restored_validation: bool,
    ) -> Option<crate::tools::command_output_artifact::capture::CapturedShellOutput> {
        if restored_validation {
            self.validation_raw_output.take()
        } else {
            self.raw_output.take()
        }
    }

    fn remember_validation_attempt_error(
        &mut self,
        is_validation: bool,
        attempt_started: bool,
        error: &CodexErr,
    ) {
        if !is_validation {
            return;
        }
        self.last_validation_attempt_started |= attempt_started;
        let output = match error {
            CodexErr::Sandbox(SandboxErr::Denied { output, .. })
            | CodexErr::Sandbox(SandboxErr::Timeout { output }) => output.as_ref(),
            _ => return,
        };
        self.last_validation_attempt_output = Some(output.clone());
        self.validation_raw_output = self.raw_output.clone();
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
            delivery_recovery: None,
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
            approval_scope_id: req
                .turn_environment
                .environment
                .approval_scope_id()
                .to_string(),
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

    async fn run(
        &mut self,
        req: &ShellRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecToolCallOutput, ToolError> {
        // Only a sandbox enforces requested restrictions, and the orchestrator selects
        // `SandboxType::None` when this host has no sandbox backend. Refuse such a
        // launch like `build_exec_request` and unified exec instead of running it
        // without enforcement. This runtime always launches on this host, so a
        // remote-labelled environment is not exempt. Launches whose policy
        // requested no sandbox are unchanged.
        if attempt.sandbox_requested && attempt.sandbox == codex_sandboxing::SandboxType::None {
            return Err(ToolError::Codex(crate::exec::unavailable_sandbox_error()));
        }
        let session_shell = ctx
            .session
            .shell_for_environment(&req.turn_environment)
            .await
            .map_err(|error| ToolError::Rejected(error.to_string()))?;
        let shell = session_shell.as_ref();
        let shell_snapshot_location = req
            .turn_environment
            .shell_snapshot_for_config(
                &codex_utils_path_uri::PathUri::from_abs_path(&req.cwd),
                &ctx.turn.config,
            )
            .await;
        let mut env = req.env.clone();
        let command = prepare_shell_command(ShellCommandPreparation {
            command: &req.command,
            command_for_approval: &req.command_for_approval,
            shell,
            shell_snapshot: shell_snapshot_location.as_deref(),
            explicit_env_overrides: &req.explicit_env_overrides,
            env: &mut env,
            shell_type: &shell.shell_type,
            sandbox_shell_type: req.shell_type.as_ref(),
            sandbox: attempt.sandbox,
            windows_sandbox_level: attempt.windows_sandbox_level,
            enforce_managed_network: false,
            approved_powershell_direct_argv: req.approved_powershell_direct_argv.as_ref(),
            proof_cwd: Some(req.cwd.as_path()),
        })
        .await;

        let command = super::recheck_readonly_command_before_launch(
            &req.command_for_approval,
            req.shell_wrapper_is_owned,
            (!req.turn_environment.environment.is_remote()).then_some(req.cwd.as_path()),
            &env,
            req.sandbox_permissions,
            &req.exec_approval_requirement,
            ctx,
        ).await?.unwrap_or(command);

        if ctx
            .turn
            .config
            .features
            .enabled(codex_features::Feature::Kd4Runtime)
            && !req.turn_environment.environment.is_remote()
        {
            crate::tools::handlers::command_preflight::check_managed_cargo_lane(
                &req.command_for_approval,
                req.cwd.as_path(),
                &env,
            )
            .await
            .map_err(ToolError::Rejected)?;
        }

        let additional_read_roots = shell_snapshot_additional_read_roots(
            shell_snapshot_location.as_deref(),
            attempt.sandbox,
        );
        let command =
            build_sandbox_command(&command, &req.cwd, &env, req.additional_permissions.clone())?;
        let mut expiration: crate::exec::ExecExpiration = req.timeout_ms.into();
        expiration = expiration.with_cancellation(req.cancellation_token.clone());
        let stall_cancellation = req.stall_timeout_ms.map(|_| CancellationToken::new());
        if let Some(cancellation) = stall_cancellation.as_ref() {
            expiration = expiration.with_cancellation(cancellation.clone());
        }
        let options = ExecOptions {
            expiration,
            capture_policy: ExecCapturePolicy::ShellTool,
        };
        let mut env = attempt
            .env_for(
                command,
                options,
            )
            .await
            .map_err(ToolError::Codex)?;
        env.windows_sandbox_additional_read_roots = additional_read_roots;
        let validation_resource = crate::validation_task::acquire_execution_resources(
            ctx, &req.command_for_approval, Some(req.cwd.as_path()),
        ).await.map_err(ToolError::Rejected)?;
        crate::validation_task::begin_policy_attempt(ctx).await.map_err(ToolError::Rejected)?;
        let validation_attempt_started = Arc::new(AtomicBool::new(false));
        let after_spawn = req.validation_launch.then(|| {
            let validation_attempt_started = Arc::clone(&validation_attempt_started);
            Box::new(move || {
                validation_attempt_started.store(true, Ordering::Release);
            }) as Box<dyn FnOnce() + Send>
        });
        let progress = req.stall_timeout_ms.map(|_| CommandProgress::new());
        let progress_observer = progress.as_ref().map(CommandProgress::subscribe);
        let thread_id = ctx.session.thread_id.to_string();
        let capture_home = ctx.turn.config.codex_home.clone();
        let stdout_stream = Self::stdout_stream(ctx, progress);
        let owns_resource = validation_resource.is_some();
        let execution = async move {
            let _validation_resource = validation_resource;
            crate::tools::command_output_artifact::capture::capture_legacy_output(
                &capture_home,
                &thread_id,
                execute_exec_request_with_after_spawn(env, stdout_stream, after_spawn),
            ).await
        };
        let terminal_tasks = ctx.session.terminal_tasks.clone();
        let dispatch_timing = crate::tools::tool_dispatch_trace::active_tool_dispatch_timing();
        let execution = async move {
            if owns_resource {
                // A dropped tool observer must not release a running child's
                // resource. Existing session shutdown owns this task's cleanup.
                terminal_tasks.spawn(async move {
                    match dispatch_timing {
                        Some(timing) => crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing(
                            timing, execution,
                        ).await,
                        None => execution.await,
                    }
                }).await.map_err(|error|
                    ToolError::Rejected(format!("Python test execution owner failed: {error}")))
            } else {
                Ok(execution.await)
            }
        };
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
        let (out, raw_output) = out?;
        self.raw_output = Some(raw_output);
        let mut out = match out {
            Ok(out) => out,
            Err(CodexErr::Sandbox(SandboxErr::Timeout { output })) if stalled => *output,
            Err(err) => {
                let attempt_started = validation_attempt_started.load(Ordering::Acquire);
                // `exec` reports creation through `after_spawn`. The Windows
                // sandbox launcher never does, so its failures stay spent.
                if !attempt_started
                    && attempt.sandbox != codex_sandboxing::SandboxType::WindowsRestrictedToken
                {
                    crate::validation_task::policy_attempt_not_started(ctx).await;
                }
                self.remember_validation_attempt_error(
                    req.validation_launch,
                    attempt_started,
                    &err,
                );
                return Err(ToolError::Codex(err));
            }
        };
        if stalled && let Some(stall_timeout_ms) = req.stall_timeout_ms {
            mark_command_stalled(&mut out, stall_timeout_ms);
        }
        if stalled {
            return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Timeout {
                output: Box::new(out),
            })));
        }
        Ok(out)
    }
}

