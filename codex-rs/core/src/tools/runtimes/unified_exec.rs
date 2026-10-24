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
use crate::tools::context::SharedTurnDiffTracker;
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
use crate::unified_exec::NoopSpawnLifecycle;
use crate::unified_exec::ObservedSpawnLifecycle;
use crate::unified_exec::PendingSpawnRegistration;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
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
    tracker: Option<SharedTurnDiffTracker>,
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
    /// Creates a runtime bound to the shared unified-exec process manager.
    #[cfg(test)]
    pub fn new(manager: &'a UnifiedExecProcessManager) -> Self {
        Self::new_with_pending_spawns(manager, PendingSpawnRegistration::default(), None)
    }

    pub(crate) fn new_with_pending_spawns(
        manager: &'a UnifiedExecProcessManager,
        pending_spawns: PendingSpawnRegistration,
        tracker: Option<SharedTurnDiffTracker>,
    ) -> Self {
        Self {
            manager,
            pending_spawns,
            tracker,
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

        #[cfg(test)]
        test_observation::record_process_launch();
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
        let mut command = build_unified_exec_sandbox_command(
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
                .map(|cwd| cwd.as_path()),
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
                /*environment_id*/ Some(&req.turn_environment.environment_id),
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



    /// Audit 2026-10-22 #24: a raw Cargo validation rejected by the lane check
    /// never started. Through a nested code-mode call into either runtime it
    /// remains a tool failure, not a pending validation or a completion blocker.
    #[tokio::test]
    async fn raw_cargo_rejected_nested_validation_does_not_create_pending_work() {
        use crate::session::step_context::StepContext;
        use crate::session::turn_execution::SamplingRequestSettledState;
        use crate::session::turn_execution::TurnExecutionControl;
        use crate::tools::context::ToolInvocation;
        use crate::tools::context::ToolPayload;
        use crate::tools::registry::CoreToolRuntime;
        use codex_tools::ToolExecutor;
        use codex_tools::ToolOutput;

        let temp = tempdir().unwrap();
        // A KD4 tree reserves its core test lane against raw Cargo builds.
        let checkout = temp.path().join("codex-rs");
        std::fs::create_dir_all(checkout.join(".config")).unwrap();
        std::fs::write(checkout.join(".config/kd4-rust-tests.toml"), "").unwrap();
        let root = AbsolutePathBuf::try_from(checkout.clone()).unwrap();
        let cwd = PathUri::from_abs_path(&root);
        for (tool, handler) in [
            (
                "exec_command",
                Arc::new(crate::tools::handlers::ExecCommandHandler::default())
                    as Arc<dyn CoreToolRuntime>,
            ),
            (
                "shell_command",
                Arc::new(crate::tools::handlers::ShellCommandHandler::default())
                    as Arc<dyn CoreToolRuntime>,
            ),
        ] {
            let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
            turn.approval_policy
                .set(codex_protocol::protocol::AskForApproval::Never)
                .unwrap();
            turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
            let config = Arc::make_mut(&mut turn.config);
            config.cwd = root.clone();
            config
                .features
                .enable(codex_features::Feature::Kd4Runtime)
                .unwrap();
            // Only the fixture tree may decide the target: no inherited Cargo state.
            let policy = &mut config.permissions.shell_environment_policy;
            policy.inherit = codex_protocol::config_types::ShellEnvironmentPolicyInherit::None;
            policy.r#set = std::env::vars()
                .filter(|(key, _)| !key.to_ascii_uppercase().starts_with("CARGO_"))
                .collect();
            policy.r#set.insert(
                "CARGO_HOME".into(),
                temp.path().join("cargo-home").display().to_string(),
            );
            turn.environments.turn_environments = vec![test_turn_environment(cwd.clone())];
            turn.model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
            session.services.code_mode_service = crate::tools::code_mode::CodeModeService::new(
                Arc::new(codex_code_mode_runtime::InProcessCodeModeSessionProvider),
            );
            let session = Arc::new(session);
            let execute = crate::tools::code_mode::CodeModeExecuteHandler::new(
                crate::tools::code_mode::execute_spec::create_code_mode_tool(false, false, &[], &[]),
                vec![handler.spec()],
                Vec::new(),
            )
            .unwrap();
            let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
                crate::tools::registry::ToolRegistry::from_tools([handler]),
                Vec::new(),
            ));
            let step = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
            let tracker = Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            ));
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            let collector = control.collector(&baseline);
            let _worker = session
                .services
                .code_mode_service
                .start_turn_worker(
                    &session,
                    Arc::clone(&step),
                    Arc::clone(&tracker),
                    collector.clone(),
                )
                .unwrap();
            let arguments = serde_json::json!({
                "program": "cargo",
                "args": ["check", "--target-dir", "target/lanes/core-tests"],
                "workdir": checkout,
            });
            let output = tokio::time::timeout(
                Duration::from_secs(20),
                execute.handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: step,
                    tracker,
                    cancellation_token: CancellationToken::new(),
                    call_id: format!("raw-cargo-{tool}"),
                    tool_name: codex_tools::ToolName::plain("exec"),
                    source: crate::tools::router::ToolCallSource::Direct,
                    payload: ToolPayload::Custom {
                        input: format!(
                            "try {{ text(await tools.{tool}({arguments})); }} catch (error) {{ text(String(error)); }}"
                        ),
                    },
                }),
            )
            .await
            .expect("the rejected cell settles")
            .unwrap();
            let codex_protocol::models::ResponseInputItem::CustomToolCallOutput { output, .. } =
                output.to_response_item(
                    "raw-cargo-output",
                    &ToolPayload::Custom {
                        input: String::new(),
                    },
                )
            else {
                panic!("{tool}: exec must produce a custom tool output");
            };
            let visible = output.body.to_text().unwrap();
            assert!(
                visible.contains("Raw Cargo launch rejected; no build started"),
                "{tool}: {visible}"
            );
            control.settle(
                &baseline,
                &collector,
                &SamplingRequestSettledState {
                    mutation_revision: 0,
                    attributed_mutation_revision: 0,
                    tool_exposure_revision: 0,
                },
            );
            let work = control.completion_readiness(0).validation_work;
            assert!(work.is_empty(), "{tool}: {work:?}");
            let assessment = control.completion_assessment_with_changed_paths(
                0,
                Some(&[(LOCAL_ENVIRONMENT_ID.into(), checkout.join("changed.rs"))]),
                false,
            );
            assert!(assessment.failed_checks.is_empty(), "{tool}: {assessment:?}");
            assert!(assessment.verification_gaps.is_empty(), "{tool}: {assessment:?}");
            assert!(assessment.validation_uncertainty.is_none(), "{tool}: {assessment:?}");
        }
    }

    /// Like `build_exec_request`, a local launch under managed network whose policy requested a
    /// sandbox this host cannot provide is refused before any setup.
    #[tokio::test]
    async fn restricted_launch_without_available_sandbox_is_refused_before_setup() {
        let temp = tempdir().unwrap();
        let root = AbsolutePathBuf::try_from(temp.path().to_path_buf()).unwrap();
        let cwd = PathUri::from_abs_path(&root);
        let manager = UnifiedExecProcessManager::default();
        let mut runtime = UnifiedExecRuntime::new(&manager);
        let mut request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        );
        request.command = vec!["must-not-launch-sandbox-fixture".into()];
        request.command_for_approval = vec!["cargo".into(), "test".into()];
        request.cwd = cwd.clone();
        request.sandbox_cwd = cwd.clone();
        request.turn_environment = test_turn_environment(cwd.clone());
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        turn.approval_policy
            .set(codex_protocol::protocol::AskForApproval::Never)
            .unwrap();
        turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
        Arc::make_mut(&mut turn.config).cwd = root.clone();
        turn.environments.turn_environments = vec![test_turn_environment(cwd.clone())];
        let ctx = ToolCtx {
            session: Arc::new(session),
            turn: Arc::new(turn),
            call_id: "managed-network-sandbox".into(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
        };
        let permissions = codex_protocol::models::PermissionProfile::Disabled;
        for (sandbox_requested, refused) in [(true, true), (false, false)] {
            let case = sandbox_requested;
            let attempt = SandboxAttempt {
                codex_home: &root,
                sandbox: codex_sandboxing::SandboxType::None,
                sandbox_requested,
                permissions: &permissions,
                exec_server_permissions: &permissions,
                sandbox_cwd: &cwd,
                workspace_roots: std::slice::from_ref(&root),
                windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
                windows_sandbox_private_desktop: false,
            };
            let (result, observed) = test_observation::observe(tokio::time::timeout(
                Duration::from_secs(1),
                runtime.run(&request, &attempt, &ctx),
            ))
            .await;
            let result = result.expect("sandbox admission must remain bounded");
            if refused {
                let Err(ToolError::Codex(CodexErr::Io(error))) = result else {
                    panic!("{case:?}: expected the unavailable-sandbox refusal");
                };
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{case:?}");
                assert_eq!(
                    error.to_string(),
                    "requested restrictions require a sandbox, but no sandbox backend is available; refusing to run unsandboxed",
                    "{case:?}"
                );
            } else {
                assert!(result.is_err(), "{case:?}: the fixture executable does not exist");
            }
            assert_eq!(observed.process_launches, if refused { 0 } else { 1 }, "{case:?}");
        }
    }

    fn test_turn_environment(cwd: PathUri) -> TurnEnvironment {
        TurnEnvironment::new(
            LOCAL_ENVIRONMENT_ID.to_string(),
            Arc::new(Environment::default_for_tests()),
            cwd,
            /*shell*/ None,
        )
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
            vec![
                "__codex_powershell_script__",
                "pwsh",
                "profiles-enabled",
                "Get-ChildItem"
            ]
        );
        assert_ne!(
            keys[0].command,
            canonicalize_command_for_approval(&request.command)
        );
    }

    #[tokio::test]
    async fn approval_key_and_prompt_preserve_mixed_uri_permissions() {
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        let mut request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        );
        let native = PathUri::from_abs_path(&AbsolutePathBuf::current_dir().unwrap()).to_string();
        let profile = |foreign: &str| {
            serde_json::from_value::<UriAdditionalPermissionProfile>(
                serde_json::json!({"file_system": {"read": [native, foreign]}}),
            )
            .unwrap()
        };
        request.additional_permissions_uri = Some(profile("file:///foreign/a"));
        let first = runtime.approval_keys(&request);
        request.additional_permissions_uri = Some(profile("file:///foreign/b"));
        assert_ne!(first, runtime.approval_keys(&request));
        let reason = unified_exec_approval_reason(
            Some("reason".into()),
            request.additional_permissions_uri.as_ref(),
        )
        .unwrap();
        assert!(reason.starts_with("reason\nAdditional permissions (canonical URI profile): "));
        assert!(reason.contains("file:///foreign/b"));
        assert!(reason.contains(&native));
    }

    #[tokio::test]
    async fn approval_cache_distinguishes_command_environment_values() {
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        let mut request = test_request(
            SandboxPermissions::UseDefault,
            ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        );
        let original = runtime.approval_keys(&request);
        request
            .explicit_env_overrides
            .insert("PATH".into(), "first".into());
        let first = runtime.approval_keys(&request);
        assert_ne!(original, first);
        request
            .explicit_env_overrides
            .insert("PATH".into(), "second".into());
        assert_ne!(first, runtime.approval_keys(&request));
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
            has_command_env_overrides: false,
            tty: false,
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            additional_permissions_uri: None,
            justification: None,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        };

        assert_eq!(
            runtime.sandbox_cwd(&request),
            Some(&PathUri::from_abs_path(&sandbox_cwd))
        );
    }

    #[tokio::test]
    async fn first_attempt_preserves_requested_sandbox_permissions() {
        let manager = UnifiedExecProcessManager::default();
        let runtime = UnifiedExecRuntime::new(&manager);
        for permissions in [
            SandboxPermissions::UseDefault,
            SandboxPermissions::RequireEscalated,
            SandboxPermissions::WithAdditionalPermissions,
        ] {
            let request = test_request(
                permissions,
                ExecApprovalRequirement::NeedsApproval {
                    reason: None,
                    proposed_execpolicy_amendment: None,
                },
            );
            assert_eq!(runtime.sandbox_permissions(&request), permissions);
        }
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
            has_command_env_overrides: false,
            tty: false,
            sandbox_permissions,
            additional_permissions: None,
            additional_permissions_uri: None,
            justification: None,
            exec_approval_requirement,
        }
    }
}
