/*
Runtime: unified exec

Handles approval + sandbox orchestration for unified exec requests, delegating to
the process manager to spawn PTYs once an ExecRequest is prepared.
*/
use crate::command_canonicalization::canonicalize_command_for_approval;
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::guardian::GuardianNetworkAccessTrigger;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecServerEnvConfig;
use crate::sandboxing::SandboxPermissions;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::flat_tool_name;
use crate::tools::known_delta_store::KnownDeltaHit;
use crate::tools::network_approval::NetworkApprovalMode;
use crate::tools::network_approval::NetworkApprovalSpec;
use crate::tools::runtimes::ShellCommandPreparation;
use crate::tools::runtimes::exec_env_for_sandbox_permissions;
use crate::tools::runtimes::prepare_shell_command;
use crate::tools::runtimes::shell_snapshot_additional_read_roots;
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
use crate::unified_exec::NoopSpawnLifecycle;
use crate::unified_exec::PendingSpawnRegistration;
use crate::unified_exec::SpawnLifecycle;
use crate::unified_exec::SpawnLifecycleHandle;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_network_proxy::NetworkProxy;
use codex_protocol::error::CodexErr;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxablePreference;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::sync::OwnedRwLockReadGuard;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(crate) mod test_observation {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[derive(Clone, Default)]
    struct Counters {
        process_launches: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) struct Snapshot {
        pub process_launches: usize,
    }

    tokio::task_local! {
        static COUNTERS: Counters;
    }

    pub(crate) async fn observe<F: Future>(future: F) -> (F::Output, Snapshot) {
        let counters = Counters::default();
        let output = COUNTERS.scope(counters.clone(), future).await;
        (
            output,
            Snapshot {
                process_launches: counters.process_launches.load(Ordering::Relaxed),
            },
        )
    }

    pub(super) fn record_process_launch() {
        let _ = COUNTERS.try_with(|counters| {
            counters.process_launches.fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Request payload used by the unified-exec runtime after approvals and
/// sandbox preferences have been resolved for the current turn.
#[derive(Clone, Debug)]
pub struct UnifiedExecRequest {
    pub command: Vec<String>,
    /// Semantically equivalent, inspectable command used for approvals and
    /// approval caching when `command` contains an encoded runtime payload.
    pub command_for_approval: Vec<String>,

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
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_uri: Option<UriAdditionalPermissionProfile>,
    pub justification: Option<String>,
    pub exec_approval_requirement: ExecApprovalRequirement,
    pub validation_launch: Option<crate::validation_admission::ValidationLaunchPlan>,
    pub(crate) known_delta_hit: Option<KnownDeltaHit>,
}

#[derive(Debug)]
pub(crate) enum UnifiedExecLaunch {
    Process(Arc<UnifiedExecProcess>),
    KnownDelta(KnownDeltaHit),
}

#[derive(Debug)]
struct ValidationSpawnLifecycle {
    inner: SpawnLifecycleHandle,
    authorization_guard:
        Option<OwnedRwLockReadGuard<crate::validation_admission::ValidationAuthorization>>,
}

impl SpawnLifecycle for ValidationSpawnLifecycle {
    fn inherited_fds(&self) -> Vec<i32> {
        self.inner.inherited_fds()
    }

    fn after_spawn(&mut self) {
        self.inner.after_spawn();
        self.authorization_guard.take();
    }
}

async fn validation_spawn_lifecycle(
    req: &UnifiedExecRequest,
    ctx: &ToolCtx,
    inner: SpawnLifecycleHandle,
) -> Result<SpawnLifecycleHandle, ToolError> {
    let Some(launch) = req.validation_launch.as_ref() else {
        return Ok(inner);
    };
    let guard = Arc::clone(&ctx.turn.validation_authorization)
        .read_owned()
        .await;
    if let Some(skipped) = crate::validation_admission::recheck_validation_launch(&guard, launch) {
        return Err(ToolError::ValidationSkipped(skipped));
    }
    Ok(Box::new(ValidationSpawnLifecycle {
        inner,
        authorization_guard: Some(guard),
    }))
}

/// Cache key for approval decisions that can be reused across equivalent
/// unified-exec launches.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnifiedExecApprovalKey {
    pub environment_id: String,
    pub approval_scope_id: String,
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
}

/// Runtime adapter that keeps policy and sandbox orchestration on the
/// unified-exec side while delegating process startup to the manager.
pub struct UnifiedExecRuntime<'a> {
    manager: &'a UnifiedExecProcessManager,
    pending_spawns: PendingSpawnRegistration,
}

fn unified_exec_options(
    network_denial_cancellation_token: Option<CancellationToken>,
) -> ExecOptions {
    let mut expiration = ExecExpiration::DefaultTimeout;
    if let Some(cancellation) = network_denial_cancellation_token {
        expiration = expiration.with_cancellation(cancellation);
    }
    ExecOptions {
        expiration,
        capture_policy: ExecCapturePolicy::ShellTool,
    }
}

fn build_unified_exec_sandbox_command(
    command: &[String],
    cwd: &PathUri,
    env: &HashMap<String, String>,
    managed_network: Option<ManagedNetworkSandboxContext>,
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
        managed_network,
        additional_permissions,
    })
}

impl<'a> UnifiedExecRuntime<'a> {
    /// Creates a runtime bound to the shared unified-exec process manager.
    #[cfg(test)]
    pub fn new(manager: &'a UnifiedExecProcessManager) -> Self {
        Self::new_with_pending_spawns(manager, PendingSpawnRegistration::default())
    }

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
            cwd: req.cwd.clone(),
            tty: req.tty,
            sandbox_permissions: req.sandbox_permissions,
            additional_permissions: req.additional_permissions.clone(),
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
        Box::pin(async move {
            with_cached_approval(&session.services, "unified_exec", keys, || async move {
                let available_decisions = None;
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

    fn approval_action(
        &self,
        req: &UnifiedExecRequest,
        ctx: &ApprovalCtx<'_>,
    ) -> std::io::Result<ApprovalAction> {
        Ok(Self::build_guardian_review_request(req, ctx.call_id))
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

impl UnifiedExecRuntime<'_> {
    fn build_guardian_review_request(req: &UnifiedExecRequest, call_id: &str) -> ApprovalAction {
        ApprovalAction::ExecCommand {
            id: call_id.to_string(),
            command: req.command_for_approval.clone(),
            cwd: req.cwd.clone(),
            sandbox_permissions: req.sandbox_permissions,
            additional_permissions: req.additional_permissions.clone(),
            justification: req.justification.clone(),
            tty: req.tty,
        }
    }
}

impl<'a> ToolRuntime<UnifiedExecRequest, UnifiedExecLaunch> for UnifiedExecRuntime<'a> {
    fn sandbox_cwd<'b>(&self, req: &'b UnifiedExecRequest) -> Option<&'b PathUri> {
        Some(&req.sandbox_cwd)
    }

    fn network_approval_spec(
        &self,
        req: &UnifiedExecRequest,
        ctx: &ToolCtx,
    ) -> Option<NetworkApprovalSpec> {
        if req.known_delta_hit.is_some() {
            return None;
        }
        let file_system_sandbox_policy = ctx.turn.file_system_sandbox_policy();
        let sandbox_permissions = sandbox_permissions_preserving_denied_reads(
            req.sandbox_permissions,
            &file_system_sandbox_policy,
        );
        let network =
            managed_network_for_sandbox_permissions(req.network.as_ref(), sandbox_permissions)?;
        Some(NetworkApprovalSpec {
            network: Some(network.clone()),
            mode: NetworkApprovalMode::Deferred,
            trigger: GuardianNetworkAccessTrigger {
                call_id: ctx.call_id.clone(),
                tool_name: flat_tool_name(&ctx.tool_name).into_owned(),
                command: req.command.clone(),
                cwd: req.cwd.to_abs_path().ok()?,
                sandbox_permissions: req.sandbox_permissions,
                additional_permissions: req.additional_permissions.clone(),
                justification: req.justification.clone(),
                tty: Some(req.tty),
            },
            command: req.hook_command.clone(),
            environment_id: req.turn_environment.environment_id.clone(),
            approval_scope_id: req
                .turn_environment
                .environment
                .approval_scope_id()
                .to_string(),
        })
    }

    async fn run(
        &mut self,
        req: &UnifiedExecRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<UnifiedExecLaunch, ToolError> {
        if let Some(hit) = req.known_delta_hit.as_ref() {
            return Ok(UnifiedExecLaunch::KnownDelta(hit.clone()));
        }
        let native_cwd = req.cwd.to_abs_path().ok();
        let mutation = crate::turn_diff_tracker::command_mutation(
            &req.command_for_approval,
            native_cwd
                .as_ref()
                .map(codex_utils_absolute_path::AbsolutePathBuf::as_path),
        );
        crate::tools::events::begin_exec_mutation_evidence(
            crate::tools::events::ToolEventCtx::new(
                ctx.session.as_ref(),
                ctx.turn.as_ref(),
                &ctx.call_id,
                None,
            ),
            native_cwd.as_ref(),
            &mutation,
        )
        .await;
        #[cfg(test)]
        test_observation::record_process_launch();
        let base_command = &req.command;
        let session_shell = ctx.session.user_shell();
        let shell = req
            .turn_environment
            .shell
            .as_ref()
            .unwrap_or(session_shell.as_ref());
        let shell_snapshot_location = req.turn_environment.shell_snapshot(&req.cwd).await;
        let (file_system_sandbox_policy, _) = attempt.permissions.to_runtime_permissions();
        let launch_sandbox_permissions = sandbox_permissions_preserving_denied_reads(
            req.sandbox_permissions,
            &file_system_sandbox_policy,
        );
        let managed_network = attempt.network_proxy(managed_network_for_sandbox_permissions(
            req.network.as_ref(),
            launch_sandbox_permissions,
        ));
        let env = exec_env_for_sandbox_permissions(&req.env, launch_sandbox_permissions);
        let (mut env, managed_network_context) = match managed_network {
            Some(network) => {
                let prepared = network
                    .prepare_for_optional_environment(
                        env,
                        Some(&req.turn_environment.environment_id),
                    )
                    .map_err(|err| {
                        ToolError::Codex(CodexErr::Io(io::Error::other(format!(
                            "failed to prepare network proxy for environment `{}`: {err}",
                            req.turn_environment.environment_id
                        ))))
                    })?;
                (prepared.env, Some(prepared.sandbox_context))
            }
            None => (env, None),
        };
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
            enforce_managed_network: attempt.enforce_managed_network,
            approved_powershell_direct_argv: req.approved_powershell_direct_argv.as_ref(),
            proof_cwd: req.normalization_cwd.as_deref(),
        })
        .await;

        let additional_read_roots = shell_snapshot_additional_read_roots(
            shell_snapshot_location.as_deref(),
            attempt.sandbox,
        );
        let command = build_unified_exec_sandbox_command(
            &command,
            &req.cwd,
            &env,
            managed_network_context,
            req.additional_permissions.clone(),
        )
        .map_err(|error| match error {
            ToolError::Rejected(_) => {
                ToolError::Rejected("missing command line for PTY".to_string())
            }
            error @ (ToolError::Denied(_)
            | ToolError::Codex(_)
            | ToolError::ValidationSkipped(_)) => error,
        })?;
        let options = unified_exec_options(attempt.network_denial_cancellation_token.clone());
        let spawn_lifecycle =
            validation_spawn_lifecycle(req, ctx, Box::new(NoopSpawnLifecycle)).await?;
        self.manager
            .open_session_with_exec_env(
                req.process_id,
                command,
                additional_read_roots,
                options,
                req.additional_permissions_uri.as_ref(),
                attempt,
                managed_network,
                /*environment_id*/ Some(&req.turn_environment.environment_id),
                req.exec_server_env_config.clone(),
                req.tty,
                spawn_lifecycle,
                Some(req.raw_output_artifact.clone()),
                req.turn_environment.environment.as_ref(),
                &self.pending_spawns,
            )
            .await
            .map(UnifiedExecLaunch::Process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::DEFAULT_EXEC_COMMAND_TIMEOUT_MS;
    use crate::tools::sandboxing::ToolRuntime;
    use codex_exec_server::Environment;
    use codex_exec_server::LOCAL_ENVIRONMENT_ID;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn test_turn_environment(cwd: PathUri) -> TurnEnvironment {
        TurnEnvironment::new(
            LOCAL_ENVIRONMENT_ID.to_string(),
            Arc::new(Environment::default_for_tests()),
            cwd,
            /*shell*/ None,
        )
    }

    #[test]
    fn unified_exec_options_combines_default_timeout_with_network_denial_cancellation() {
        let cancellation = CancellationToken::new();
        let options = unified_exec_options(Some(cancellation.clone()));

        assert_eq!(options.capture_policy, ExecCapturePolicy::ShellTool);
        match options.expiration {
            ExecExpiration::TimeoutOrCancellation {
                timeout,
                cancellation: actual,
            } => {
                assert_eq!(
                    timeout,
                    Duration::from_millis(DEFAULT_EXEC_COMMAND_TIMEOUT_MS)
                );
                cancellation.cancel();
                assert!(actual.is_cancelled());
            }
            other => panic!("expected timeout-or-cancellation expiration, got {other:?}"),
        }
    }

    #[test]
    fn guardian_review_request_preserves_foreign_cwd() {
        let foreign_cwd =
            PathUri::parse("file:///tmp/remote-workspace").expect("POSIX remote workspace URI");
        let mut request = test_request(
            SandboxPermissions::RequireEscalated,
            ExecApprovalRequirement::NeedsApproval {
                reason: None,
                proposed_execpolicy_amendment: None,
            },
        );
        request.cwd = foreign_cwd.clone();

        let action =
            UnifiedExecRuntime::build_guardian_review_request(&request, "remote-exec-call");

        assert!(matches!(
            action,
            ApprovalAction::ExecCommand { id, cwd, .. }
                if id == "remote-exec-call" && cwd == foreign_cwd
        ));
    }

    #[tokio::test]
    async fn approval_key_includes_environment_id_and_approval_scope() {
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        let mut request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        );
        request.turn_environment.environment_id = "remote".to_string();
        let original_key = runtime.approval_keys(&request);
        request.turn_environment.environment = Arc::new(Environment::default_for_tests());
        let replacement_key = runtime.approval_keys(&request);
        assert_ne!(original_key, replacement_key);

        request.turn_environment.environment_id = "other".to_string();
        let other_key = runtime.approval_keys(&request);

        assert_ne!(replacement_key, other_key);
    }

    #[tokio::test]
    async fn approval_key_uses_inspectable_command_instead_of_encoded_payload() {
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        let mut request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        );
        request.command = vec![
            "pwsh".to_string(),
            "-EncodedCommand".to_string(),
            "RwBlAHQALQBDAGgAaQBsAGQASQB0AGUAbQA=".to_string(),
        ];
        request.command_for_approval = vec![
            "pwsh".to_string(),
            "-Command".to_string(),
            "Get-ChildItem".to_string(),
        ];

        let keys = runtime.approval_keys(&request);
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].command,
            canonicalize_command_for_approval(&request.command_for_approval)
        );
        assert_ne!(
            keys[0].command,
            canonicalize_command_for_approval(&request.command)
        );
    }

    #[tokio::test]
    async fn unified_exec_uses_the_trusted_sandbox_cwd() {
        let cwd_dir = tempdir().expect("create process temp dir");
        let sandbox_dir = tempdir().expect("create sandbox temp dir");
        let cwd =
            AbsolutePathBuf::try_from(cwd_dir.path().to_path_buf()).expect("absolute temp dir");
        let sandbox_cwd = AbsolutePathBuf::try_from(sandbox_dir.path().to_path_buf())
            .expect("absolute sandbox temp dir");
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        let request = UnifiedExecRequest {
            command: vec!["pwd".to_string()],
            command_for_approval: vec!["pwd".to_string()],

            normalization_cwd: None,

            approved_powershell_direct_argv: None,
            raw_output_artifact: RawOutputArtifact::Failed {
                id: None,
                message: "test fixture".to_string(),
                owned_path: None,
                bytes: 0,
            },
            shell_type: ShellType::Sh,
            hook_command: "pwd".to_string(),
            process_id: 1000,
            cwd: cwd.into(),
            sandbox_cwd: sandbox_cwd.clone().into(),
            turn_environment: test_turn_environment(sandbox_cwd.clone().into()),
            env: HashMap::new(),
            exec_server_env_config: None,
            explicit_env_overrides: HashMap::new(),
            network: None,
            tty: false,
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            additional_permissions_uri: None,
            justification: None,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
            validation_launch: None,
            known_delta_hit: None,
        };

        assert_eq!(
            runtime.sandbox_cwd(&request),
            Some(&PathUri::from_abs_path(&sandbox_cwd))
        );
    }

    #[tokio::test]
    async fn first_attempt_preserves_parent_sandbox_override() {
        let manager = UnifiedExecProcessManager::default();
        let request = test_request(
            SandboxPermissions::RequireEscalated,
            ExecApprovalRequirement::NeedsApproval {
                reason: None,
                proposed_execpolicy_amendment: None,
            },
        );
        let runtime = UnifiedExecRuntime::new(&manager);

        assert_eq!(
            runtime.sandbox_permissions(&request),
            SandboxPermissions::RequireEscalated,
            "unified exec should preserve a parent require_escalated request"
        );
    }

    #[tokio::test]
    async fn first_attempt_preserves_additional_permissions_request() {
        let manager = UnifiedExecProcessManager::default();
        let request = test_request(
            SandboxPermissions::WithAdditionalPermissions,
            ExecApprovalRequirement::NeedsApproval {
                reason: None,
                proposed_execpolicy_amendment: None,
            },
        );
        let runtime = UnifiedExecRuntime::new(&manager);

        assert_eq!(
            runtime.sandbox_permissions(&request),
            SandboxPermissions::WithAdditionalPermissions,
            "unified exec should keep bounded additional-permissions requests sandboxed"
        );
    }

    #[tokio::test]
    async fn execpolicy_allow_preserves_parent_sandbox_override() {
        let manager = UnifiedExecProcessManager::default();
        let request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: true,
                proposed_execpolicy_amendment: None,
            },
        );
        let runtime = UnifiedExecRuntime::new(&manager);

        assert_eq!(
            runtime.exec_approval_requirement(&request),
            Some(ExecApprovalRequirement::Skip {
                bypass_sandbox: true,
                proposed_execpolicy_amendment: None,
            }),
            "unified exec should preserve exec-policy allow decisions that bypass the sandbox"
        );
    }

    fn test_request(
        sandbox_permissions: SandboxPermissions,
        exec_approval_requirement: ExecApprovalRequirement,
    ) -> UnifiedExecRequest {
        let cwd = AbsolutePathBuf::try_from(std::env::current_dir().unwrap())
            .expect("current dir is absolute");
        UnifiedExecRequest {
            command: vec!["zsh".to_string(), "-c".to_string(), "echo hi".to_string()],
            command_for_approval: vec!["zsh".to_string(), "-c".to_string(), "echo hi".to_string()],

            normalization_cwd: None,

            approved_powershell_direct_argv: None,
            raw_output_artifact: RawOutputArtifact::Failed {
                id: None,
                message: "test fixture".to_string(),
                owned_path: None,
                bytes: 0,
            },
            shell_type: ShellType::Zsh,
            hook_command: "echo hi".to_string(),
            process_id: 1000,
            cwd: cwd.clone().into(),
            sandbox_cwd: cwd.clone().into(),
            turn_environment: test_turn_environment(cwd.into()),
            env: HashMap::new(),
            exec_server_env_config: None,
            explicit_env_overrides: HashMap::new(),
            network: None,
            tty: false,
            sandbox_permissions,
            additional_permissions: None,
            additional_permissions_uri: None,
            justification: None,
            exec_approval_requirement,
            validation_launch: None,
            known_delta_hit: None,
        }
    }
}
