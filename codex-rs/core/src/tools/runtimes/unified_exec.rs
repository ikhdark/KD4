/*
Runtime: unified exec

Handles approval + sandbox orchestration for unified exec requests, delegating to
the process manager to spawn PTYs once an ExecRequest is prepared.
*/
use crate::command_canonicalization::canonicalize_command_for_approval;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecServerEnvConfig;
use crate::sandboxing::SandboxPermissions;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::runtimes::ShellCommandPreparation;
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
use crate::unified_exec::ObservedSpawnLifecycle;
use crate::unified_exec::PendingSpawnRegistration;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxablePreference;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::Arc;


/// Request payload used by the unified-exec runtime after approvals and
/// sandbox preferences have been resolved for the current turn.
#[derive(Clone, Debug)]
pub struct UnifiedExecRequest {
    pub command: Vec<String>,
    /// Semantically equivalent, inspectable command used for approvals and
    /// approval caching when `command` contains an encoded runtime payload.
    pub command_for_approval: Vec<String>,
    pub shell_wrapper_is_owned: bool,

    pub normalization_cwd: Option<std::path::PathBuf>,

    pub approved_powershell_direct_argv: Option<Vec<String>>,
    pub raw_output_artifact: RawOutputArtifact,
    pub shell_type: ShellType,
    pub hook_command: String,
    pub process_id: u32,
    pub cwd: PathUri,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub env: HashMap<String, String>,
    pub exec_server_env_config: Option<ExecServerEnvConfig>,
    pub explicit_env_overrides: HashMap<String, String>,
    pub has_command_env_overrides: bool,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_uri: Option<UriAdditionalPermissionProfile>,
    pub justification: Option<String>,
    pub exec_approval_requirement: ExecApprovalRequirement,
}

impl UnifiedExecRequest {
    fn canonical_additional_permissions(&self) -> Option<UriAdditionalPermissionProfile> {
        self.additional_permissions_uri
            .clone()
            .or_else(|| self.additional_permissions.clone().map(Into::into))
    }
}

fn unified_exec_approval_reason(
    reason: Option<String>,
    permissions: Option<&UriAdditionalPermissionProfile>,
) -> Option<String> {
    let Some(permissions) = permissions else {
        return reason;
    };
    #[expect(
        clippy::expect_used,
        reason = "URI permission profiles contain only JSON-serializable scalars, paths serialized as strings, and sequences; never silently omit approval grants"
    )]
    let profile =
        serde_json::to_string(permissions).expect("URI permission profiles are JSON serializable");
    Some(format!(
        "{}Additional permissions (canonical URI profile): {profile}",
        reason
            .map(|reason| format!("{reason}\n"))
            .unwrap_or_default()
    ))
}

/// Cache key for approval decisions that can be reused across equivalent
/// unified-exec launches.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnifiedExecApprovalKey {
    pub environment_id: String,
    pub approval_scope_id: String,
    pub command: Vec<String>,
    pub environment_overrides: std::collections::BTreeMap<String, String>,
    pub cwd: PathUri,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<UriAdditionalPermissionProfile>,
}

/// Runtime adapter that keeps policy and sandbox orchestration on the
/// unified-exec side while delegating process startup to the manager.
pub struct UnifiedExecRuntime<'a> {
    manager: &'a UnifiedExecProcessManager,
    pending_spawns: PendingSpawnRegistration,
}

fn unified_exec_options() -> ExecOptions {
    ExecOptions {
        expiration: ExecExpiration::DefaultTimeout,
        capture_policy: ExecCapturePolicy::default(),
    }
}

fn build_unified_exec_sandbox_command(
    command: &[String],
    cwd: &PathUri,
    env: &HashMap<String, String>,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> Result<SandboxCommand, ToolError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| ToolError::Rejected("command args are empty".to_string()))?;
    Ok(SandboxCommand {
        program: program.clone().into(),
        args: args.to_vec(),
        cwd: cwd.clone(),
        env: env.clone(),
        additional_permissions,
    })
}

impl<'a> UnifiedExecRuntime<'a> {

    pub(crate) fn new_with_pending_spawns(
        manager: &'a UnifiedExecProcessManager,
        pending_spawns: PendingSpawnRegistration,
    ) -> Self {
        Self {
            manager,
            pending_spawns,
        }
    }
}

impl Sandboxable for UnifiedExecRuntime<'_> {
    fn sandbox_preference(&self) -> SandboxablePreference {
        SandboxablePreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        true
    }
}

impl Approvable<UnifiedExecRequest> for UnifiedExecRuntime<'_> {
    type ApprovalKey = UnifiedExecApprovalKey;

    fn approval_keys(&self, req: &UnifiedExecRequest) -> Vec<Self::ApprovalKey> {
        vec![UnifiedExecApprovalKey {
            environment_id: req.turn_environment.environment_id.clone(),
            approval_scope_id: req
                .turn_environment
                .environment
                .approval_scope_id()
                .to_string(),
            command: canonicalize_command_for_approval(&req.command_for_approval),
            environment_overrides: req.explicit_env_overrides.clone().into_iter().collect(),
            cwd: req.cwd.clone(),
            tty: req.tty,
            sandbox_permissions: req.sandbox_permissions,
            additional_permissions: req.canonical_additional_permissions(),
        }]
    }

    fn start_approval_async<'b>(
        &'b mut self,
        req: &'b UnifiedExecRequest,
        ctx: ApprovalCtx<'b>,
    ) -> BoxFuture<'b, ReviewDecision> {
        let keys = self.approval_keys(req);
        let session = ctx.session;
        let turn = ctx.turn;
        let call_id = ctx.call_id.to_string();
        let command = req.command_for_approval.clone();
        let environment_id = Some(req.turn_environment.environment_id.clone());
        let reason = ctx
            .retry_reason
            .clone()
            .or_else(|| req.justification.clone());
        let permissions = req.canonical_additional_permissions();
        // Legacy structured approvals are host-native. The existing reason is
        // preserved by all clients and must show every URI grant, including foreign paths.
        let reason = unified_exec_approval_reason(reason, permissions.as_ref());
        let reason = if req.has_command_env_overrides {
            let mut names = req
                .explicit_env_overrides
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            names.sort();
            Some(format!(
                "{}Command-scoped environment overrides: {}",
                reason
                    .map(|reason| format!("{reason}\n"))
                    .unwrap_or_default(),
                names.join(", ")
            ))
        } else {
            reason
        };
        Box::pin(async move {
            with_cached_approval(&session.services, "unified_exec", keys, || async move {
                let available_decisions = permissions
                    .as_ref()
                    .map(|_| vec![ReviewDecision::Approved, ReviewDecision::Abort]);
                session
                    .request_command_approval(
                        turn,
                        call_id,
                        /*approval_id*/ None,
                        environment_id,
                        command,
                        req.cwd.clone(),
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

    fn exec_approval_requirement(
        &self,
        req: &UnifiedExecRequest,
    ) -> Option<ExecApprovalRequirement> {
        Some(req.exec_approval_requirement.clone())
    }

    fn permission_request_payload(
        &self,
        req: &UnifiedExecRequest,
    ) -> Option<PermissionRequestPayload> {
        Some(PermissionRequestPayload::bash(
            req.hook_command.clone(),
            req.justification.clone(),
        ))
    }

    fn sandbox_permissions(&self, req: &UnifiedExecRequest) -> SandboxPermissions {
        req.sandbox_permissions
    }
}


impl<'a> ToolRuntime<UnifiedExecRequest, Arc<UnifiedExecProcess>> for UnifiedExecRuntime<'a> {
    fn sandbox_cwd<'b>(&self, req: &'b UnifiedExecRequest) -> Option<&'b PathUri> {
        Some(&req.sandbox_cwd)
    }


    async fn run(
        &mut self,
        req: &UnifiedExecRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<Arc<UnifiedExecProcess>, ToolError> {
        // Only a sandbox enforces requested restrictions, and the orchestrator selects
        // `SandboxType::None` when this host has no sandbox backend. Refuse such a
        // local launch like `build_exec_request` instead of running it without
        // enforcement. Remote executors select and enforce their own sandbox, and
        // launches whose policy requested no sandbox are unchanged.
        if attempt.sandbox_requested
            && attempt.sandbox == codex_sandboxing::SandboxType::None
            && !req.turn_environment.environment.is_remote()
        {
            return Err(ToolError::Codex(crate::exec::unavailable_sandbox_error()));
        }
        let pending_spawns = self.pending_spawns.clone();
        let setup_timing = ctx
            .turn
            .turn_timing_state
            .begin_local_phase(crate::turn_timing::TurnLocalPhase::ExecutorReadinessWait);
        let setup_phase = crate::tools::tool_dispatch_trace::begin_tool_phase("command_setup");
        let native_cwd = req.cwd.to_abs_path().ok();

        let base_command = &req.command;
        let session_shell = ctx
            .session
            .shell_for_environment(&req.turn_environment)
            .await
            .map_err(|error| ToolError::Rejected(error.to_string()))?;
        let shell = session_shell.as_ref();
        let shell_snapshot_location = req
            .turn_environment
            .shell_snapshot_for_config(&req.cwd, &ctx.turn.config)
            .await;
        let env = req.env.clone();
        let mut env = env;
        let command = prepare_shell_command(ShellCommandPreparation {
            command: base_command,
            command_for_approval: &req.command_for_approval,
            shell,
            shell_snapshot: shell_snapshot_location.as_deref(),
            explicit_env_overrides: &req.explicit_env_overrides,
            env: &mut env,
            shell_type: &req.shell_type,
            sandbox_shell_type: Some(&req.shell_type),
            sandbox: attempt.sandbox,
            windows_sandbox_level: attempt.windows_sandbox_level,
            enforce_managed_network: false,
            approved_powershell_direct_argv: req.approved_powershell_direct_argv.as_ref(),
            proof_cwd: req.normalization_cwd.as_deref(),
        })
        .await;

        let command = super::recheck_readonly_command_before_launch(
            &req.command_for_approval,
            req.shell_wrapper_is_owned,
            req.normalization_cwd.as_deref(),
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
            && let Some(cwd) = native_cwd.as_ref()
        {
            crate::tools::handlers::command_preflight::check_managed_cargo_lane(
                &req.command_for_approval,
                cwd.as_path(),
                &env,
            )
            .await
            .map_err(ToolError::Rejected)?;
        }

        let additional_read_roots = shell_snapshot_additional_read_roots(
            shell_snapshot_location.as_deref(),
            attempt.sandbox,
        );
        let permissions = req.canonical_additional_permissions();
        let command = build_unified_exec_sandbox_command(
            &command,
            &req.cwd,
            &env,
            permissions
                .clone()
                .and_then(|permissions| AdditionalPermissionProfile::try_from(permissions).ok()),
        )
        .map_err(|error| match error {
            ToolError::Rejected(_) => {
                ToolError::Rejected("missing command line for PTY".to_string())
            }
            error @ (ToolError::Denied(_) | ToolError::Codex(_)) => error,
        })?;
        let options = unified_exec_options();
        drop(setup_phase);
        drop(setup_timing);
        let validation_resource = crate::validation_task::acquire_execution_resources(
            ctx, &req.command_for_approval,
            native_cwd.as_ref().filter(|_| !req.turn_environment.environment.is_remote())
                .map(codex_utils_absolute_path::AbsolutePathBuf::as_path),
        ).await.map_err(ToolError::Rejected)?;
        crate::validation_task::begin_policy_attempt(ctx).await.map_err(ToolError::Rejected)?;
        let spawned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = self
            .manager
            .open_session_with_exec_env(
                req.process_id,
                command,
                additional_read_roots,
                options,
                permissions.as_ref(),
                attempt,
                req.exec_server_env_config.clone(),
                req.tty,
                Box::new(ObservedSpawnLifecycle::new(Arc::clone(&spawned), validation_resource)),
                Some(req.raw_output_artifact.clone()),
                req.turn_environment.environment.as_ref(),
                Some(req.turn_environment.exec_backend()),
                &pending_spawns,
            )
            .await;
        // Every launcher reports creation through `after_spawn`. A failed
        // Windows sandbox launch may still leave a helper-owned child, so it
        // stays spent like any possible start.
        if result.is_err()
            && !spawned.load(std::sync::atomic::Ordering::Acquire)
            && attempt.sandbox != codex_sandboxing::SandboxType::WindowsRestrictedToken
        {
            crate::validation_task::policy_attempt_not_started(ctx).await;
        }
        result
    }
}


