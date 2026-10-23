use super::*;
#[cfg(test)]
#[path = "spawn_audit151_tests.rs"]
mod audit151_tests;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::AgentRoleLocks;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_config;
use crate::agent::role::resolve_role_config;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::context::TaskCapsuleFragment;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::multi_agents_v2::message_tool::message_content;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AdmissionRejectionReason;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::ArchitectureContractRef;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AssignmentRelation;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::IntegrationPlan;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::RelevantHandle;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::TaskCapsuleHandle;
use codex_agent_task_store::TaskCapsuleV1;
use codex_agent_task_store::WorkspaceStrategy;
use codex_agent_task_store::normalize_repo_path;
use codex_agent_task_store::normalize_repo_scopes;
use codex_agent_task_store::repo_path_identity_keys;
use codex_context_fragments::ModelContextBudget;
use codex_git_utils::get_git_repo_root;
use codex_otel::SessionTelemetry;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const MAX_OVERLAY_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORKTREE_GIT_STDOUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORKTREE_GIT_STDERR_BYTES: u64 = 1024 * 1024;
const WORKTREE_GIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(crate) struct Handler {
    options: SpawnAgentToolOptions,
}

impl Handler {
    pub(crate) fn new(options: SpawnAgentToolOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("spawn_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_agent_tool_v2(self.options.clone())
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // V2 admission reserves capacity and identity before asynchronous setup;
        // durable task admission and rollback own their synchronization. Holding
        // the parent tool gate across startup only serializes independent children.
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_spawn_agent(invocation).await.map(boxed_tool_output) })
    }
}

struct AdmittedLaunchGuard {
    session: Arc<crate::session::session::Session>,
    telemetry: SessionTelemetry,
    assignment_id: AssignmentId,
    armed: bool,
}

impl Drop for AdmittedLaunchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let session = Arc::clone(&self.session);
        let assignment_id = self.assignment_id;
        let telemetry = self.telemetry.clone();
        self.session.terminal_tasks.spawn(async move {
            let control = &session.services.agent_control;
            let coordinator = control.task_coordinator();
            if let Some(binding) = coordinator.binding_for_assignment(assignment_id)
                && let Some(thread) = binding
                    .thread_id
                    .as_deref()
                    .and_then(|id| ThreadId::from_string(id).ok())
            {
                // The existing runtime construction guard owns shutdown. Do not
                // erase a binding while its original worker can still report.
                if control.wait_for_agent_termination(thread).await.is_err() {
                    return;
                }
            }
            if let (Some(store), Ok(actor)) = (coordinator.store(), coordinator.root_actor()) {
                let _ = store
                    .abandon_agent_task(
                        actor,
                        assignment_id,
                        "spawn observer ended before launch committed; no work was replayed".into(),
                    )
                    .await;
                let _ = coordinator.remove_agent_task_binding(assignment_id).await;
                coordinator
                    .maybe_emit_terminal_metrics(assignment_id, &telemetry)
                    .await;
            }
        });
    }
}

async fn handle_spawn_agent(
    invocation: ToolInvocation,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        step_context,
        payload,
        call_id,
        cancellation_token,
        ..
    } = invocation;
    let turn = Arc::clone(&step_context.turn);
    let arguments = function_arguments(payload)?;
    let mut args: SpawnAgentArgs = parse_arguments(&arguments)?;
    if turn.session_source.is_non_root_agent() && args.assignment.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: durable typed assignments are root-only".to_string(),
        ));
    }
    let typed_role = if args.assignment.is_some() {
        parse_typed_role(args.agent_type.as_deref())?
    } else {
        // The legacy message shape remains accepted and receives a durable repository-scoped
        // diagnostic claim so its identity and overlap remain visible without blocking work.
        // A read-only agent_type that the compatibility claim can satisfy keeps its role.
        legacy_message_role(args.agent_type.as_deref())
    };
    let fork_mode = args.fork_mode(
        typed_role,
        turn.config.multi_agent_v2.allow_full_history_forks,
    )?;
    let role_name = args
        .agent_type
        .as_deref()
        .map(str::trim)
        .filter(|role| !role.is_empty());
    if args.assignment.is_none() && role_name == Some("reviewer") {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: reviewer requires a typed assignment with a review relation; the compatibility message form cannot establish independent review".to_string(),
        ));
    }

    let legacy_message = match (args.message.take(), args.assignment.is_some()) {
        (Some(_), true) => {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: use either assignment or message, never both".to_string(),
            ));
        }
        (None, false) => {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: either assignment or message is required".to_string(),
            ));
        }
        (Some(message), false) => {
            // Preserve the legacy validation contract before converting the task into a durable
            // assignment. Empty or otherwise invalid message payloads still fail as before.
            let _ = message_content(message.clone())?;
            Some(message)
        }
        (None, true) => None,
    };
    let legacy_parent_assignment_id = if turn.session_source.is_non_root_agent() {
        if matches!(&turn.session_source, SessionSource::SubAgent(SubAgentSource::Other(label)) if label.starts_with("agent_job:")) {
            return Err(FunctionCallError::RespondToModel("CSV workers cannot spawn nested agents until structured ancestry is supported; ask the root agent to delegate".into()));
        }
        let coordinator = session.services.agent_control.task_coordinator();
        match coordinator.binding_for_turn(&turn) {
            Some(binding) => {
                let parent = coordinator
                    .get_agent_task(binding.assignment_id, Some(0))
                    .await
                    .map_err(typed_task_store_error)?;
                if !matches!(
                    parent.assignment.admission_origin,
                    AssignmentAdmissionOrigin::LegacyMessage { .. }
                ) {
                    return Err(FunctionCallError::RespondToModel(
                        "spawn_agent: explicitly typed agents cannot spawn subagents".to_string(),
                    ));
                }
                Some(binding.assignment_id)
            }
            None => None,
        }
    } else {
        None
    };
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    let mut config = build_agent_spawn_config_for_step(&step_context)?;
    let parent_service_tier = inherit_live_service_tier(&session, &mut config).await;
    // Admission and worktree allocation are host-only. Resolve the same step
    // authority used by native tools before touching the local task store.
    let primary = crate::tools::handlers::wait_for_observed_tool_environment(
        &step_context.environments,
        None,
        &cancellation_token,
    )
    .await?
    .ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent: durable assignments require a selected local environment".to_string(),
        )
    })?;
    if cancellation_token.is_cancelled() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent cancelled".to_string(),
        ));
    }
    if primary.environment.is_remote() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: durable assignments and isolated worktrees require a local primary environment; remote native tools remain available".to_string(),
        ));
    }
    let admission_cwd = primary
        .cwd()
        .to_abs_path()
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    let mut spawn_environments = Some(step_context.environments.to_selections());
    let mut isolated_workspace: Option<IsolatedWorkspace> = None;
    if let Some(service_tier) = args.service_tier.as_ref() {
        config.service_tier = Some(service_tier.clone());
    }
    if matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)) {
        reject_full_fork_spawn_overrides(
            role_name,
            args.model.as_deref(),
            args.reasoning_effort.clone(),
        )?;
    }
    let role_locks = if matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)) {
        AgentRoleLocks::default()
    } else {
        apply_role_to_config(&mut config, role_name)
            .await
            .map_err(FunctionCallError::RespondToModel)?
    };
    apply_spawn_agent_model_defaults_and_overrides(
        &session,
        turn.as_ref(),
        &mut config,
        args.model.as_deref(),
        args.reasoning_effort.clone(),
        role_locks,
    )
    .await?;
    apply_spawn_agent_service_tier(
        &session,
        turn.as_ref(),
        &mut config,
        parent_service_tier.as_deref(),
        args.service_tier.as_deref(),
    )
    .await?;
    let runtime_role_locks = AgentRoleLocks {
        permissions: (args.assignment.is_none() || typed_role == AgentRole::Reviewer)
            && role_locks.permissions,
        ..role_locks
    };
    apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref(), runtime_role_locks)?;
    config.cwd = admission_cwd.clone();

    // Every V2 spawn becomes a durable typed assignment. Legacy message-shaped
    // calls have no explicit contract; their durable role is Worker unless
    // legacy_message_role keeps a read-only Explorer, so
    // carry that canonical role through admission instead of constructing a
    // role-less ThreadSpawn source that typed admission must reject.
    // The spawn source stores the durable assignment role, not the optional
    // configuration-profile name. Custom agent profiles can select model and
    // instruction locks, but durable admission accepts canonical typed roles.
    let durable_role_name = agent_role_metric_label(typed_role);
    let spawn_source = thread_spawn_source(
        session.thread_id,
        &turn.session_source,
        child_depth,
        Some(durable_role_name),
        Some(args.task_name.clone()),
    )?;
    let new_agent_path = spawn_source.get_agent_path().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawned agent is missing a canonical task name".to_string(),
        )
    })?;
    if typed_role == AgentRole::Explorer
        && let Some(assignment) = args.assignment.as_ref()
        && assignment.workspace_strategy != WorkspaceStrategy::Isolated
    {
        let coordinator = session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    session.services.state_db.clone(),
                    config.sqlite_home.clone(),
                    config.model_provider_id.clone(),
                    session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(typed_task_store_error)?;
        }
        if let (Some(store), Some(root)) = (coordinator.store(), coordinator.root_session_id()) {
            let cwd = admission_cwd.to_path_buf();
            let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
            if let Some(existing) = store
                .reusable_explorer_assignment(
                    &repo_root,
                    assignment.clone().into_draft(root, typed_role),
                )
                .await
                .map_err(typed_task_store_error)?
            {
                let requested_handles = reuse_request_handles(
                    matches!(fork_mode, Some(SpawnAgentForkMode::TaskCapsule)),
                    &repo_root,
                    &assignment.read_scope,
                    &assignment.write_scope,
                    assignment.relevant_handles.clone(),
                )
                .await?;
                let result = reusable_spawn_result(
                    &session.services.agent_control,
                    existing,
                    requested_handles.as_deref(),
                )
                .await?;
                emit_admission_reuse_metric(&turn.session_telemetry);
                return Ok(result);
            }
        }
    }
    // Typed spawns reserve execution, registry identity, and V2 residency before any
    // worktree or durable assignment preparation. The token holds only logical RAII
    // reservations; all synchronization guards used to create it have already been released.
    let typed_role_metric = agent_role_metric_label(typed_role);
    turn.session_telemetry.counter(
        "codex.multi_agent.typed_spawn_lifecycle",
        1,
        &[("outcome", "attempted"), ("role", typed_role_metric)],
    );
    let mut prepared_typed_spawn = match session
        .services
        .agent_control
        .prepare_typed_spawn(&config, spawn_source.clone(), Some(session.thread_id))
        .await
    {
        Ok(prepared) => {
            turn.session_telemetry.counter(
                "codex.multi_agent.typed_spawn_lifecycle",
                1,
                &[("outcome", "reserved"), ("role", typed_role_metric)],
            );
            Some(prepared)
        }
        Err(error) => {
            turn.session_telemetry.counter(
                "codex.multi_agent.typed_spawn_lifecycle",
                1,
                &[
                    ("outcome", "reservation_rejected"),
                    ("role", typed_role_metric),
                ],
            );
            return Err(collab_spawn_error(error));
        }
    };
    let mut typed_reservation_metric = Some(TypedSpawnReservationMetric::new(
        turn.session_telemetry.clone(),
        typed_role_metric,
    ));
    let mut consumed_typed_spawn = None;
    let mut admitted_launch_guard = None;
    let typed_task = if args.assignment.is_some() || legacy_message.is_some() {
        let role = typed_role;
        let coordinator = session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    session.services.state_db.clone(),
                    config.sqlite_home.clone(),
                    config.model_provider_id.clone(),
                    session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(typed_task_store_error)?;
        }
        let store = coordinator.store().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: durable typed assignment store became unavailable".to_string(),
            )
        })?;
        let root_session_id = coordinator.root_session_id().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: durable typed assignments require persistent local session state"
                    .to_string(),
            )
        })?;
        session
            .services
            .agent_control
            .reconcile_live_typed_actor_heartbeats()
            .await
            .map_err(typed_task_store_error)?;
        let cwd = admission_cwd.to_path_buf();
        let main_repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        let workspace_strategy = args
            .assignment
            .as_ref()
            .map(|assignment| assignment.workspace_strategy)
            .unwrap_or(WorkspaceStrategy::Shared);
        let evaluation_root = if matches!(role, AgentRole::Reviewer | AgentRole::Verifier) {
            let relation = args
                .assignment
                .as_ref()
                .and_then(|assignment| assignment.relation.as_ref())
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "spawn_agent: evaluator requires one target relation".to_string(),
                    )
                })?;
            let expected = if role == AgentRole::Reviewer {
                codex_agent_task_store::RelationKind::Review
            } else {
                codex_agent_task_store::RelationKind::Verification
            };
            if relation.kind != expected || relation.target_assignment_ids.len() != 1 {
                return Err(FunctionCallError::RespondToModel(
                    "spawn_agent: evaluator requires one matching target relation".to_string(),
                ));
            }
            if workspace_strategy == WorkspaceStrategy::Isolated {
                return Err(FunctionCallError::RespondToModel("spawn_agent: evaluators must share their target's existing workspace; isolated would review different source".to_string()));
            }
            Some(
                store
                    .evaluation_target_workspace(
                        &main_repo_root,
                        &root_session_id,
                        relation.target_assignment_ids[0],
                    )
                    .await
                    .map_err(typed_task_store_error)?,
            )
        } else {
            None
        };
        let repo_root = if let Some(repo_root) = evaluation_root {
            let previous_cwd = config.cwd.clone();
            config.cwd = AbsolutePathBuf::from_absolute_path(&repo_root)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
            let mut roots = config.workspace_roots.clone();
            for root in &mut roots {
                if root == &previous_cwd {
                    *root = config.cwd.clone();
                }
            }
            if !roots.contains(&config.cwd) {
                roots.push(config.cwd.clone());
            }
            config.workspace_roots = roots.clone();
            config.permissions.set_workspace_roots(roots);
            if let Some(environments) = &mut spawn_environments
                && let Some(primary) = environments.first_mut()
            {
                primary.cwd = codex_utils_path_uri::PathUri::from_abs_path(&config.cwd);
            }
            repo_root
        } else if workspace_strategy == WorkspaceStrategy::Isolated {
            let workspace = Box::pin(create_isolated_worktree(
                &main_repo_root,
                config.codex_home.as_path(),
                &args.task_name,
                &cancellation_token,
                &session.terminal_tasks,
            ))
            .await?;
            let previous_cwd = config.cwd.clone();
            config.cwd = match AbsolutePathBuf::from_absolute_path(&workspace.path) {
                Ok(path) => path,
                Err(error) => {
                    if let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await {
                        tracing::warn!(
                            path = %workspace.path.display(),
                            %cleanup_error,
                            "failed to clean isolated worktree after path conversion failure"
                        );
                    }
                    return Err(FunctionCallError::RespondToModel(format!(
                        "spawn_agent: isolated worktree path is invalid: {error}"
                    )));
                }
            };
            let mut roots = config.workspace_roots.clone();
            for root in &mut roots {
                if root == &previous_cwd {
                    *root = config.cwd.clone();
                }
            }
            let mut seen = HashSet::new();
            roots.retain(|root| seen.insert(root.clone()));
            config.workspace_roots = roots.clone();
            config.permissions.set_workspace_roots(roots);
            if let Some(environments) = &mut spawn_environments {
                // Retarget only the local primary, never remote secondaries.
                if let Some(primary) = environments.first_mut() {
                    primary.cwd = codex_utils_path_uri::PathUri::from_abs_path(&config.cwd);
                }
            }
            let repo_root = workspace.path.clone();
            isolated_workspace = Some(workspace);
            repo_root
        } else {
            main_repo_root
        };
        let (draft, relevant_handles) = match args.assignment.take() {
            Some(assignment_args) => {
                let relevant_handles = assignment_args.relevant_handles.clone();
                (
                    assignment_args.into_draft(root_session_id, role),
                    relevant_handles,
                )
            }
            None => (
                legacy_message_draft(
                    root_session_id,
                    legacy_message.as_deref().ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "spawn_agent: either assignment or message is required".to_string(),
                        )
                    })?,
                    legacy_parent_assignment_id,
                    role,
                ),
                Vec::new(),
            ),
        };
        if cancellation_token.is_cancelled() {
            if let Some(workspace) = isolated_workspace.take()
                && let Err(error) = cleanup_isolated_worktree(&workspace).await
            {
                tracing::warn!(path = %workspace.path.display(), %error, "failed to clean cancelled isolated worktree");
            }
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent cancelled".to_string(),
            ));
        }
        let isolated_integrator_available = resolve_role_config(&config, "integrator").is_some();
        let prepared = prepared_typed_spawn.take().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: typed spawn reservation became unavailable".to_string(),
            )
        })?;
        consumed_typed_spawn = match session
            .services
            .agent_control
            .consume_prepared_typed_spawn(prepared, &config, &spawn_source, Some(session.thread_id))
            .await
        {
            Ok(consumed) => Some(consumed),
            Err(error) => {
                if let Some(workspace) = isolated_workspace.take()
                    && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.path.display(),
                        %cleanup_error,
                        "failed to clean isolated worktree after typed reservation revalidation failure"
                    );
                }
                return Err(collab_spawn_error(error));
            }
        };
        // A duplicate rejection can offer reuse, whose handles must validate
        // against these requested scopes exactly as this fresh launch would.
        let requested_scopes = (draft.read_scope.clone(), draft.write_scope.clone());
        let admitted = match coordinator
            .create_admitted_assignment(
                &repo_root,
                draft,
                isolated_integrator_available,
                new_agent_path.to_string(),
                args.task_name.clone(),
            )
            .await
        {
            Ok(task) => task,
            Err(error) => {
                if let StoreError::AdmissionRejected {
                    reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
                    reusable_assignment_id: Some(assignment_id),
                } = &error
                {
                    if let Some(workspace) = isolated_workspace.take()
                        && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                    {
                        tracing::warn!(
                            path = %workspace.path.display(),
                            %cleanup_error,
                            "failed to clean isolated worktree after assignment reuse"
                        );
                    }
                    let (read_scope, write_scope) = &requested_scopes;
                    let requested_handles = reuse_request_handles(
                        matches!(fork_mode, Some(SpawnAgentForkMode::TaskCapsule)),
                        &repo_root,
                        read_scope,
                        write_scope,
                        relevant_handles,
                    )
                    .await?;
                    let result = reusable_spawn_result(
                        &session.services.agent_control,
                        *assignment_id,
                        requested_handles.as_deref(),
                    )
                    .await?;
                    emit_admission_reuse_metric(&turn.session_telemetry);
                    return Ok(result);
                }
                emit_admission_rejection_metric(&turn.session_telemetry, &error);
                if let Some(workspace) = isolated_workspace.take()
                    && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.path.display(),
                        %cleanup_error,
                        "failed to clean isolated worktree after assignment creation failure"
                    );
                }
                return Err(typed_task_store_error(error));
            }
        };
        emit_admission_overlap_metrics(&turn.session_telemetry, admitted.overlaps);
        let assignment = admitted.assignment;
        let attempt = admitted.attempt;
        admitted_launch_guard = Some(AdmittedLaunchGuard {
            session: Arc::clone(&session),
            telemetry: turn.session_telemetry.clone(),
            assignment_id: assignment.assignment_id,
            armed: true,
        });
        let task_capsule = if matches!(fork_mode, Some(SpawnAgentForkMode::TaskCapsule)) {
            match construct_and_attach_task_capsule(
                store.as_ref(),
                &repo_root,
                &assignment,
                &attempt,
                relevant_handles,
            )
            .await
            {
                Ok(payload) => Some(payload),
                Err(error) => {
                    if let Err(rollback_error) = store
                        .abandon_agent_task(
                            TaskActor::RootSession(assignment.root_session_id.clone()),
                            assignment.assignment_id,
                            format!("TaskCapsule construction failed before launch: {error}"),
                        )
                        .await
                    {
                        tracing::warn!(
                            assignment_id = %assignment.assignment_id,
                            %rollback_error,
                            "failed to abandon typed assignment after TaskCapsule failure"
                        );
                    }
                    coordinator
                        .maybe_emit_terminal_metrics(
                            assignment.assignment_id,
                            &turn.session_telemetry,
                        )
                        .await;
                    if let Some(workspace) = isolated_workspace.take()
                        && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                    {
                        tracing::warn!(
                            path = %workspace.path.display(),
                            %cleanup_error,
                            "failed to clean isolated worktree after TaskCapsule failure"
                        );
                    }
                    return Err(typed_task_store_error(error));
                }
            }
        } else {
            None
        };
        Some((assignment, attempt, task_capsule))
    } else {
        None
    };
    if let Some((assignment, _, _)) = typed_task.as_ref()
        && assignment.admission_origin == AssignmentAdmissionOrigin::Typed
    {
        // Capsule and history forks share the same receipt lifecycle. Keep it in the
        // child configuration so the canonical capsule payload remains unchanged.
        let instructions = config.developer_instructions.get_or_insert_default();
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(TYPED_RECEIPT_LIFECYCLE);
    }
    let options = SpawnAgentOptions {
        fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| call_id.clone()),
        fork_mode,
        parent_thread_id: Some(session.thread_id),
        environments: spawn_environments,
        captured_environments: Some(step_context.environments.clone()),
        typed_task_binding: typed_task.as_ref().map(|(assignment, attempt, _)| {
            AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: new_agent_path.to_string(),
                task_name: args.task_name.clone(),
                thread_id: None,
            }
        }),
        agent_job_binding: None,
    };
    let spawned_agent = if let Some(canonical_payload) = typed_task
        .as_ref()
        .and_then(|(_, _, capsule)| capsule.as_ref())
    {
        let prepared = consumed_typed_spawn.take().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: typed spawn reservation became unavailable".to_string(),
            )
        })?;
        await_spawn_or_cancel(
            &cancellation_token,
            Box::pin(
                session
                    .services
                    .agent_control
                    .spawn_agent_with_prepared_typed_task_capsule(
                        config,
                        canonical_payload.clone(),
                        spawn_source,
                        options,
                        prepared,
                    ),
            ),
        )
        .await
    } else {
        let author = turn
            .session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let communication = match typed_task.as_ref() {
            Some((assignment, attempt, _)) => communication_from_plaintext_message(
                author,
                new_agent_path.clone(),
                typed_assignment_message(assignment, attempt),
            ),
            None => unreachable!("all MultiAgentV2 spawns are durably admitted"),
        };
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
        if typed_task.is_some() {
            let prepared = consumed_typed_spawn.take().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: typed spawn reservation became unavailable".to_string(),
                )
            })?;
            await_spawn_or_cancel(
                &cancellation_token,
                Box::pin(
                    session
                        .services
                        .agent_control
                        .spawn_agent_with_prepared_typed_communication(
                            config,
                            communication,
                            context,
                            spawn_source,
                            options,
                            prepared,
                        ),
                ),
            )
            .await
        } else {
            await_spawn_or_cancel(
                &cancellation_token,
                Box::pin(
                    session
                        .services
                        .agent_control
                        .spawn_agent_with_communication(
                            config,
                            communication,
                            context,
                            Some(spawn_source),
                            options,
                        ),
                ),
            )
            .await
        }
    };
    let spawned_agent = match spawned_agent {
        Ok(spawned_agent) => spawned_agent,
        Err(error) => {
            let assignment_id = typed_task
                .as_ref()
                .map(|(assignment, _, _)| assignment.assignment_id);
            let control = session.services.agent_control.clone();
            let child_thread_id = assignment_id
                .and_then(|id| control.task_coordinator().binding_for_assignment(id))
                .and_then(|binding| binding.thread_id)
                .and_then(|id| ThreadId::from_string(&id).ok());
            let telemetry = turn.session_telemetry.clone();
            let failure_reason = error.to_string();
            let cleanup = async move {
                if let Some(child_thread_id) = child_thread_id
                    && let Err(error) = control.wait_for_agent_termination(child_thread_id).await
                {
                    tracing::warn!(%child_thread_id, %error, "could not observe failed spawned child termination");
                    return;
                }
                if let Some(assignment_id) = assignment_id {
                    let coordinator = control.task_coordinator();
                    if let Some(store) = coordinator.store() {
                        if let Err(rollback_error) = store
                            .abandon_agent_task(
                                TaskActor::RootSession(control.session_id().to_string()),
                                assignment_id,
                                format!(
                                    "spawn failed before the typed agent was returned: {failure_reason}"
                                ),
                            )
                            .await
                        {
                            tracing::warn!(
                                assignment_id = %assignment_id,
                                %rollback_error,
                                "failed to abandon typed assignment after spawn failure"
                            );
                        }
                        // The child may have sealed its own terminal receipt during shutdown.
                        // Removal checks terminal state independently of explicit abandonment.
                        if let Err(cleanup_error) =
                            coordinator.remove_agent_task_binding(assignment_id).await
                        {
                            tracing::warn!(
                                assignment_id = %assignment_id,
                                %cleanup_error,
                                "failed to remove typed task binding after spawn failure"
                            );
                        }
                        coordinator
                            .maybe_emit_terminal_metrics(assignment_id, &telemetry)
                            .await;
                    }
                }
                if let Some(workspace) = isolated_workspace.take()
                    && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.path.display(),
                        %cleanup_error,
                        "failed to clean isolated worktree after spawn failure"
                    );
                }
            };
            if child_thread_id.is_some() {
                // Dropping the spawn future queues rollback. Its child can still report
                // usage until shutdown completes, so keep binding and metrics alive.
                // Preserve the runtime's commit barrier. If its bounded cancellation
                // supervisor drops this waiter, the tracked cleanup still owns the work.
                if let Err(error) = session.terminal_tasks.spawn(cleanup).await {
                    tracing::warn!(%error, "failed spawned child finalization task failed");
                }
            } else {
                cleanup.await;
            }
            return Err(collab_spawn_error(error));
        }
    };
    if let Some(guard) = admitted_launch_guard.as_mut() {
        guard.armed = false;
    }
    if let Some(workspace) = &isolated_workspace {
        // The successfully started agent now owns the worktree lifetime.
        workspace.cleanup_required.store(false, Ordering::Release);
    }
    if let Some(metric) = typed_reservation_metric.as_mut() {
        metric.mark_retained();
    }
    let new_thread_id = spawned_agent.thread_id;
    let agent_snapshot = session
        .services
        .agent_control
        .get_agent_config_snapshot(new_thread_id)
        .await;
    let nickname = agent_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.session_source.get_nickname())
        .or(spawned_agent.metadata.agent_nickname);
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            id: call_id,
            agent_thread_id: new_thread_id,
            agent_path: new_agent_path.clone(),
            kind: SubAgentActivityKind::Started,
        },
    )
    .await;
    let role_tag = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        /*inc*/ 1,
        &[("role", role_tag), ("version", "v2")],
    );
    let task_name = String::from(new_agent_path);
    let assignment_id = typed_task
        .as_ref()
        .map(|(assignment, _, _)| assignment.assignment_id.to_string())
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: durable admission completed without an assignment".to_string(),
            )
        })?;
    let integration_plan = typed_task
        .as_ref()
        .map(|(assignment, _, _)| assignment.integration_plan)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: durable admission completed without an integration plan".to_string(),
            )
        })?;

    let hide_agent_metadata = turn.config.multi_agent_v2.hide_spawn_agent_metadata;
    if hide_agent_metadata {
        Ok(SpawnAgentResult::HiddenMetadata {
            task_name,
            assignment_id,
            integration_plan,
        })
    } else {
        Ok(SpawnAgentResult::WithNickname {
            task_name,
            nickname,
            assignment_id,
            integration_plan,
        })
    }
}

struct TypedSpawnReservationMetric {
    session_telemetry: SessionTelemetry,
    role: &'static str,
    retained: bool,
}

impl TypedSpawnReservationMetric {
    fn new(session_telemetry: SessionTelemetry, role: &'static str) -> Self {
        Self {
            session_telemetry,
            role,
            retained: false,
        }
    }

    fn mark_retained(&mut self) {
        self.retained = true;
        self.session_telemetry.counter(
            "codex.multi_agent.typed_spawn_lifecycle",
            1,
            &[("outcome", "retained"), ("role", self.role)],
        );
    }
}

impl Drop for TypedSpawnReservationMetric {
    fn drop(&mut self) {
        if !self.retained {
            self.session_telemetry.counter(
                "codex.multi_agent.typed_spawn_lifecycle",
                1,
                &[("outcome", "reservation_released"), ("role", self.role)],
            );
        }
    }
}

const fn agent_role_metric_label(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Architect => "architect",
        AgentRole::Explorer => "explorer",
        AgentRole::Worker => "worker",
        AgentRole::Reviewer => "reviewer",
        AgentRole::Verifier => "verifier",
        AgentRole::Integrator => "integrator",
    }
}

fn emit_admission_overlap_metrics(
    session_telemetry: &SessionTelemetry,
    overlaps: codex_agent_task_store::AdmissionOverlapSummary,
) {
    let count = overlaps.benign_read_overlap_count;
    if count > 0 {
        session_telemetry.counter(
            "codex.multi_agent.admission_overlap",
            i64::from(count),
            &[("kind", "benign_read_read"), ("outcome", "admitted")],
        );
    }
}

fn emit_admission_rejection_metric(session_telemetry: &SessionTelemetry, error: &StoreError) {
    let (reason, overlap_kind, overlap_count) = match error {
        StoreError::AdmissionRejected { reason, .. } => match reason {
            AdmissionRejectionReason::DuplicateExplorerInvestigation => (
                "duplicate_explorer_investigation",
                Some("duplicated_primary_investigation"),
                1,
            ),
            // Counted as duplicated discovery too: the work was avoided because a
            // finished result over identical read-scope bytes already answers it.
            AdmissionRejectionReason::ReusableSealedInvestigation => (
                "reusable_sealed_investigation",
                Some("reusable_sealed_investigation"),
                1,
            ),
            AdmissionRejectionReason::IsolatedIntegratorUnavailable => {
                ("isolated_integrator_unavailable", None, 0)
            }
            AdmissionRejectionReason::ExternalWorkspaceWriter => (
                "external_workspace_writer",
                Some("external_write_overlap"),
                1,
            ),
        },
        _ => ("other", None, 0),
    };
    session_telemetry.counter(
        "codex.multi_agent.admission",
        1,
        &[("outcome", "rejected"), ("reason", reason)],
    );
    if let Some(kind) = overlap_kind {
        session_telemetry.counter(
            "codex.multi_agent.admission_overlap",
            overlap_count,
            &[("kind", kind), ("outcome", "rejected")],
        );
    }
}

fn emit_admission_reuse_metric(session_telemetry: &SessionTelemetry) {
    session_telemetry.counter(
        "codex.multi_agent.admission",
        1,
        &[
            ("outcome", "reused"),
            ("reason", "duplicate_explorer_investigation"),
        ],
    );
    session_telemetry.counter(
        "codex.multi_agent.admission_overlap",
        1,
        &[
            ("kind", "duplicated_primary_investigation"),
            ("outcome", "reused"),
        ],
    );
}

/// `requested_handles` are the canonical capsule handles a fresh launch of the
/// request would attach; `None` when that launch would ignore handles.
async fn reusable_spawn_result(
    control: &crate::agent::AgentControl,
    assignment_id: AssignmentId,
    requested_handles: Option<&[TaskCapsuleHandle]>,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let coordinator = control.task_coordinator();
    let binding = coordinator
        .get_agent_task_binding(assignment_id)
        .await
        .map_err(typed_task_store_error)?;
    let producer = binding
        .as_ref()
        .and_then(|binding| binding.thread_id.as_deref())
        .and_then(|thread| ThreadId::from_string(thread).ok());
    let producer_running = match producer {
        Some(thread) => control.has_running_agent(thread).await,
        None => false,
    };
    // Recheck durable authority after the asynchronous liveness observation,
    // never return an earlier Active snapshot for a superseded attempt.
    let task = coordinator
        .get_agent_task(assignment_id, None)
        .await
        .map_err(typed_task_store_error)?;
    if !producer_running
        || binding
            .as_ref()
            .is_none_or(|binding| binding.attempt_id != task.current_attempt.attempt_id)
        || task.current_attempt.state != codex_agent_task_store::AttemptState::Active
        || task.receipt.is_some()
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: matching assignment {assignment_id} has no verified running producer. Resume or reconcile/abandon that assignment before requesting a replacement. No investigation was reused and no duplicate was started."
        )));
    }
    if let Some(requested) = requested_handles
        && !capsule_handles_match(task.assignment.task_capsule.as_deref(), requested)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: matching assignment {assignment_id} was launched with different relevant_handles. Wait for that assignment or request a distinct investigation. No investigation was reused and no duplicate was started."
        )));
    }
    let task_name = binding
        .as_ref()
        .map(|binding| binding.agent_path.clone())
        .unwrap_or_else(|| assignment_id.to_string());
    Ok(SpawnAgentResult::Reused {
        task_name,
        assignment_id: assignment_id.to_string(),
        attempt_id: task.current_attempt.attempt_id.to_string(),
        agent_path: binding.as_ref().map(|binding| binding.agent_path.clone()),
        thread_id: binding.and_then(|binding| binding.thread_id),
        status: task.current_attempt.state,
        receipt_available: task.receipt.is_some(),
        integration_plan: task.assignment.integration_plan,
        reused: true,
    })
}

#[derive(Debug)]
struct IsolatedWorkspace {
    main_repo_root: PathBuf,
    path: PathBuf,
    cleanup_required: AtomicBool,
    terminal_tasks: TaskTracker,
}

impl Drop for IsolatedWorkspace {
    fn drop(&mut self) {
        if !self.cleanup_required.swap(false, Ordering::AcqRel) {
            return;
        }
        let repo_root = self.main_repo_root.clone();
        let path = self.path.clone();
        // The handler can be dropped between two awaited admission steps.
        // Preserve rollback ownership even when there is no caller left to await it.
        let terminal_tasks = self.terminal_tasks.clone();
        self.terminal_tasks.spawn(async move {
            if let Err(error) = cleanup_isolated_worktree_paths(&repo_root, &path, &terminal_tasks).await {
                tracing::warn!(path = %path.display(), %error, "failed to roll back isolated worktree");
            }
        });
    }
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceOverlay {
    head: String,
    tracked_diff: Vec<u8>,
    working_files: Vec<(PathBuf, Vec<u8>)>,
}

async fn create_isolated_worktree(
    repo_root: &Path,
    codex_home: &Path,
    task_name: &str,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> Result<IsolatedWorkspace, FunctionCallError> {
    let mut initial_overlay =
        capture_workspace_overlay(repo_root, cancellation_token, terminal_tasks).await?;
    let repository_key = hex::encode(Sha256::digest(repo_root.to_string_lossy().as_bytes()));
    let safe_task_name = task_name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(24)
        .collect::<String>();
    let leaf = format!(
        "{}-{}",
        if safe_task_name.is_empty() {
            "task"
        } else {
            &safe_task_name
        },
        Uuid::now_v7()
    );
    let parent = codex_home
        .join("isolated-worktrees")
        .join(&repository_key[..16]);
    tokio::fs::create_dir_all(&parent).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not create isolated-worktree directory: {error}"
        ))
    })?;
    let path = parent.join(leaf);
    let workspace = Arc::new(IsolatedWorkspace {
        main_repo_root: repo_root.to_path_buf(),
        path,
        cleanup_required: AtomicBool::new(true),
        terminal_tasks: terminal_tasks.clone(),
    });
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(repo_root)
        .args(["-c", "checkout.workers=0"])
        .args(["worktree", "add", "--detach"])
        .arg(&workspace.path)
        .arg(&initial_overlay.head);
    let output = run_worktree_git(command, None, cancellation_token, terminal_tasks).await;
    let error = match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(format!(
            "spawn_agent: git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(error) => Some(format!(
            "spawn_agent: could not complete git worktree add: {error}"
        )),
    };
    if let Some(error) = error {
        if let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await {
            tracing::warn!(path = %workspace.path.display(), %cleanup_error, "failed to clean isolated worktree after checkout failure");
        }
        return Err(FunctionCallError::RespondToModel(error));
    }
    let populate_result = Box::pin(async {
        // Nothing has been applied yet. Absorb a short writer burst here rather
        // than spending a model generation recreating the same empty worktree.
        for attempt in 0..=2 {
            let current_overlay = capture_workspace_overlay(repo_root, cancellation_token, terminal_tasks).await?;
            if current_overlay == initial_overlay {
                break;
            }
            if attempt == 2 || current_overlay.head != initial_overlay.head {
                return Err(FunctionCallError::RespondToModel(
                    "spawn_agent: the shared worktree did not stabilize during bounded snapshot retries; retry after the current writer finishes".to_string(),
                ));
            }
            initial_overlay = current_overlay;
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Err(FunctionCallError::RespondToModel(
                    "spawn_agent: cancelled while waiting for a stable workspace snapshot".into())),
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
        if !initial_overlay.tracked_diff.is_empty() {
            let mut command = Command::new(codex_git_utils::git_executable_async().await);
            command.arg("-C")
                .arg(&workspace.path)
                .args(["apply", "--binary", "--whitespace=nowarn", "-"]);
            let output = run_worktree_git(command, Some(&initial_overlay.tracked_diff), cancellation_token, terminal_tasks)
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "spawn_agent: could not apply the shared-worktree snapshot: {error}"
                    ))
                })?;
            if !output.status.success() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "spawn_agent: isolated snapshot apply failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        use futures::StreamExt;
        let copies = futures::stream::iter(initial_overlay.working_files.into_iter().map(|(relative_path, bytes)| {
            let workspace = Arc::clone(&workspace);
            async move {
                tokio::task::spawn_blocking(move || {
                    // Keep rollback ownership in the blocking worker, including
                    // when the caller is cancelled while this write is in flight.
                    let target = workspace.path.join(relative_path);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            FunctionCallError::RespondToModel(format!(
                                "spawn_agent: could not create an isolated snapshot directory: {error}"
                            ))
                        })?;
                    }
                    std::fs::write(&target, bytes).map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "spawn_agent: could not copy a working file into the isolated snapshot: {error}"
                        ))
                    })
                })
                .await
                .map_err(|error| FunctionCallError::RespondToModel(format!(
                    "spawn_agent: isolated snapshot copy worker failed: {error}"
                )))?
            }
        }))
        .buffer_unordered(std::thread::available_parallelism().map_or(4, usize::from))
        .collect::<Vec<_>>()
        .await;
        // Drain every write before rollback: dropping an in-flight Tokio file
        // operation does not cancel the underlying blocking filesystem call.
        for result in copies {
            result?;
        }
        Ok(())
    })
    .await;
    if let Err(error) = populate_result {
        if let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await {
            tracing::warn!(
                path = %workspace.path.display(),
                %cleanup_error,
                "failed to clean isolated worktree after snapshot failure"
            );
        }
        return Err(error);
    }
    Arc::into_inner(workspace).ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawn_agent: isolated snapshot copy is still in progress".to_string(),
        )
    })
}

async fn capture_workspace_overlay(
    repo_root: &Path,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> Result<WorkspaceOverlay, FunctionCallError> {
    let mut index_command = Command::new(codex_git_utils::git_executable_async().await);
    index_command.arg("-C").arg(repo_root).args(["ls-files", "--stage", "-z"]);
    let index = run_worktree_git(index_command, None, cancellation_token, terminal_tasks)
        .await.map_err(|error| FunctionCallError::RespondToModel(format!("spawn_agent: snapshot index capture failed: {error}")))?;
    if !index.status.success() {
        return Err(FunctionCallError::RespondToModel("spawn_agent: could not inspect snapshot index".into()));
    }
    if index.stdout.split(|byte| *byte == 0).any(|entry| entry.starts_with(b"160000 ")) {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: unsupported_snapshot_submodule: isolated snapshots do not materialize submodules; use a shared workspace or an explicitly prepared checkout".into(),
        ));
    }
    let mut head_command = Command::new(codex_git_utils::git_executable_async().await);
    head_command
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--verify", "HEAD"]);
    let head = run_worktree_git(head_command, None, cancellation_token, terminal_tasks)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not capture snapshot HEAD: {error}"
            ))
        })?;
    if !head.status.success() {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: could not resolve snapshot HEAD".into(),
        ));
    }
    let head = String::from_utf8(head.stdout).map_err(|error| {
        FunctionCallError::RespondToModel(format!("spawn_agent: invalid snapshot HEAD: {error}"))
    })?;
    let head = head.trim().to_string();
    let mut diff_command = Command::new(codex_git_utils::git_executable_async().await);
    diff_command
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--binary", "--no-ext-diff", "--no-textconv", "--src-prefix=a/", "--dst-prefix=b/", &head, "--"]);
    let mut untracked_command = Command::new(codex_git_utils::git_executable_async().await);
    untracked_command.arg("-C").arg(repo_root).args([
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
    ]);
    let mut tracked_paths_command = Command::new(codex_git_utils::git_executable_async().await);
    tracked_paths_command.arg("-C").arg(repo_root).args([
        "diff",
        "--name-only",
        "--no-renames",
        "--diff-filter=ACMRT",
        "-z",
        &head,
        "--",
    ]);
    let (diff, untracked, tracked_paths) = tokio::join!(
        run_worktree_git(diff_command, None, cancellation_token, terminal_tasks),
        run_worktree_git(untracked_command, None, cancellation_token, terminal_tasks),
        run_worktree_git(
            tracked_paths_command,
            None,
            cancellation_token,
            terminal_tasks
        ),
    );
    let diff = diff.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not capture tracked workspace changes: {error}"
        ))
    })?;
    if !diff.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git diff failed while creating an isolated snapshot: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        )));
    }
    let untracked = untracked.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not enumerate untracked workspace files: {error}"
        ))
    })?;
    if !untracked.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git ls-files failed while creating an isolated snapshot: {}",
            String::from_utf8_lossy(&untracked.stderr).trim()
        )));
    }
    let tracked_paths = tracked_paths.map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "spawn_agent: could not enumerate changed tracked files: {error}"
        ))
    })?;
    if !tracked_paths.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git diff could not enumerate changed tracked files: {}",
            String::from_utf8_lossy(&tracked_paths.stderr).trim()
        )));
    }
    let repo_root = repo_root.to_path_buf();
    let cancellation = cancellation_token.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // One tracked blocking job per capture, not metadata/open/read jobs for every
    // file. Observer drops cancel read-only work between files.
    let working_files = terminal_tasks
        .spawn_blocking(move || {
            let paths = untracked
                .stdout
                .split(|byte| *byte == 0)
                .map(|path| (path, false))
                .chain(
                    tracked_paths
                        .stdout
                        .split(|byte| *byte == 0)
                        .map(|path| (path, true)),
                );
            capture_working_file_snapshot(&repo_root, paths, &cancellation)
        })
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file snapshot worker failed: {error}"
            ))
        })??;
    Ok(WorkspaceOverlay {
        head,
        tracked_diff: diff.stdout,
        working_files,
    })
}

fn capture_working_file_snapshot<'a>(
    repo_root: &Path,
    paths: impl IntoIterator<Item = (&'a [u8], bool)>,
    cancellation_token: &CancellationToken,
) -> Result<Vec<(PathBuf, Vec<u8>)>, FunctionCallError> {
    use std::io::Read;

    let mut working_files = Vec::new();
    let mut total_file_bytes = 0u64;
    // Git patches handle deletes, modes and tracked symlinks. Preserve the
    // original regular-file bytes, including under clean filters/CRLF settings.
    for (raw_path, tracked) in paths {
        if cancellation_token.is_cancelled() {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: working file snapshot cancelled".to_string(),
            ));
        }
        if raw_path.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8(raw_path.to_vec()).map_err(|_| {
            FunctionCallError::RespondToModel(
                "spawn_agent: a working file path is not valid UTF-8".to_string(),
            )
        })?);
        let source = repo_root.join(&path);
        let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not inspect working file {}: {error}",
                path.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            if tracked {
                continue;
            }
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: isolated snapshots reject untracked symlinks and special files: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_OVERLAY_SNAPSHOT_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file {} is too large for an isolated snapshot ({} bytes, max {})",
                path.display(),
                metadata.len(),
                MAX_OVERLAY_SNAPSHOT_FILE_BYTES
            )));
        }
        total_file_bytes = total_file_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: working file snapshot size overflowed".to_string(),
                )
            })?;
        if total_file_bytes > MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working files exceed the isolated snapshot limit of {MAX_OVERLAY_SNAPSHOT_TOTAL_BYTES} bytes"
            )));
        }
        let file = std::fs::File::open(&source).map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not snapshot working file {}: {error}",
                path.display()
            ))
        })?;
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(MAX_OVERLAY_SNAPSHOT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not snapshot working file {}: {error}",
                    path.display()
                ))
            })?;
        if bytes.len() as u64 != metadata.len() {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: working file {} changed while the isolated snapshot was captured",
                path.display()
            )));
        }
        working_files.push((path, bytes));
    }
    working_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(working_files)
}

#[cfg(unix)]
struct WorktreeProcessGroup(u32);

#[cfg(unix)]
impl Drop for WorktreeProcessGroup {
    fn drop(&mut self) {
        let _ = codex_utils_pty::process_group::kill_process_group(self.0);
    }
}

#[expect(
    clippy::expect_used,
    reason = "Successful spawn creates both handles configured with Stdio::piped"
)]
async fn run_worktree_git(
    mut command: Command,
    input: Option<&[u8]>,
    cancellation_token: &CancellationToken,
    terminal_tasks: &TaskTracker,
) -> std::io::Result<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
    let input = input.map(<[u8]>::to_vec);
    let cancellation = cancellation_token.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // A session-owned worker retains the process through termination and reaping,
    // including cancellation while Command::spawn is on the blocking pool.
    terminal_tasks.spawn(async move {
        #[cfg(windows)]
        let managed = codex_utils_pty::ManagedRootProcess::reserve_with_reclaim().await?;
        #[cfg(windows)]
        managed.require_descendant_containment()?;
        if cancellation.is_cancelled() {
            return Err(std::io::Error::new(ErrorKind::Interrupted, "worktree Git command cancelled"));
        }
        let mut child = tokio::task::spawn_blocking(move || command.spawn())
            .await.map_err(std::io::Error::other)??;
        let pid = child.id().ok_or_else(|| std::io::Error::other("missing worktree Git pid"))?;
        #[cfg(unix)]
        let _process_group = WorktreeProcessGroup(pid);
        #[cfg(windows)]
        if let Err(error) = managed.attach_and_resume(pid) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("piped worktree stdout");
        let mut stderr = child.stderr.take().expect("piped worktree stderr");
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        let result = {
            let operation = async {
                let write_input = async {
                    if let (Some(mut stdin), Some(input)) = (stdin, input) {
                        stdin.write_all(&input).await?;
                        stdin.shutdown().await?;
                    }
                    Ok::<(), std::io::Error>(())
                };
                let (_, _, _, status) = tokio::try_join!(write_input,
                    read_worktree_output(&mut stdout, &mut stdout_bytes, MAX_WORKTREE_GIT_STDOUT_BYTES),
                    read_worktree_output(&mut stderr, &mut stderr_bytes, MAX_WORKTREE_GIT_STDERR_BYTES), child.wait())?;
                Ok(status)
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(std::io::Error::new(ErrorKind::Interrupted, "worktree Git command cancelled")),
                result = tokio::time::timeout(WORKTREE_GIT_TIMEOUT, operation) => {
                    result.unwrap_or_else(|_| Err(std::io::Error::new(ErrorKind::TimedOut, "worktree Git command timed out")))
                }
            }
        };
        if result.is_err() {
            #[cfg(windows)]
            let _ = managed.terminate();
            #[cfg(unix)]
            let _ = codex_utils_pty::process_group::kill_process_group(pid);
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result.map(|status| std::process::Output {status, stdout: stdout_bytes, stderr: stderr_bytes})
    }).await.map_err(std::io::Error::other)?
}

async fn read_worktree_output(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    bytes: &mut Vec<u8>,
    limit: u64,
) -> std::io::Result<()> {
    reader.take(limit + 1).read_to_end(bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("snapshot_git_output_limit: Git output exceeded its independent byte budget"));
    }
    Ok(())
}

async fn await_spawn_or_cancel<T>(
    cancellation_token: &CancellationToken,
    spawn: impl std::future::Future<Output = Result<T, CodexErr>>,
) -> Result<T, CodexErr> {
    tokio::select! {
        biased;
        _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
        result = spawn => result,
    }
}

#[cfg(test)]
#[tokio::test]
async fn cancelled_spawn_never_polls_admission_and_drops_in_flight_reservation() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let entered = AtomicBool::new(false);
    let result = await_spawn_or_cancel(&cancellation, async {
        entered.store(true, Ordering::Release);
        Ok(())
    })
    .await;
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert!(!entered.load(Ordering::Acquire));

    struct Reservation(Arc<AtomicBool>);
    impl Drop for Reservation {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let released = Arc::new(AtomicBool::new(false));
    let worker_released = Arc::clone(&released);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let operation = tokio::spawn(async move {
        await_spawn_or_cancel(&worker_cancellation, async {
            let _reservation = Reservation(worker_released);
            entered_tx.send(()).expect("admission entered");
            std::future::pending::<Result<(), CodexErr>>().await
        })
        .await
    });
    entered_rx.await.expect("in-flight reservation");
    cancellation.cancel();
    let result = operation.await.expect("cancelled admission");
    assert!(matches!(result, Err(CodexErr::TurnAborted)));
    assert!(released.load(Ordering::Acquire));
}

async fn cleanup_isolated_worktree(workspace: &IsolatedWorkspace) -> Result<(), std::io::Error> {
    let result = cleanup_isolated_worktree_paths(
        &workspace.main_repo_root,
        &workspace.path,
        &workspace.terminal_tasks,
    )
    .await;
    if result.is_ok() {
        workspace.cleanup_required.store(false, Ordering::Release);
    }
    result
}

async fn cleanup_isolated_worktree_paths(
    repo_root: &Path,
    path: &Path,
    terminal_tasks: &TaskTracker,
) -> std::io::Result<()> {
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(path);
    let output = run_worktree_git(command, None, &CancellationToken::new(), terminal_tasks).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

impl CoreToolRuntime for Handler {
    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }

    fn cancellation_requires_commit_barrier(&self) -> bool {
        true
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    /// Other agent APIs call this task text `prompt` or `instructions`; the
    /// published schema lists both aliases. Supplying two spellings is rejected.
    #[serde(alias = "prompt", alias = "instructions")]
    message: Option<String>,
    assignment: Option<TypedAssignmentArgs>,
    task_name: String,
    agent_type: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    fork_turns: Option<String>,
    fork_context: Option<bool>,
}

impl SpawnAgentArgs {
    fn fork_mode(
        &self,
        role: AgentRole,
        allow_full_history_forks: bool,
    ) -> Result<Option<SpawnAgentForkMode>, FunctionCallError> {
        if self.fork_context.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string(),
            ));
        }

        let explicit_fork_turns = self
            .fork_turns
            .as_deref()
            .map(str::trim)
            .filter(|fork_turns| !fork_turns.is_empty());

        if matches!(role, AgentRole::Reviewer | AgentRole::Verifier) {
            if explicit_fork_turns
                .is_some_and(|fork_turns| !fork_turns.eq_ignore_ascii_case("none"))
            {
                return Err(FunctionCallError::RespondToModel(
                    "typed reviewer and verifier assignments require fork_turns=\"none\""
                        .to_string(),
                ));
            }
            return Ok(Some(SpawnAgentForkMode::TaskCapsule));
        }

        let Some(fork_turns) = explicit_fork_turns else {
            return Ok(Some(SpawnAgentForkMode::TaskCapsule));
        };

        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(Some(SpawnAgentForkMode::TaskCapsule));
        }
        if fork_turns.eq_ignore_ascii_case("all") {
            if allow_full_history_forks {
                return Ok(Some(SpawnAgentForkMode::FullHistory));
            }
            return Err(FunctionCallError::RespondToModel(
                "fork_turns=\"all\" is disabled. Use `none` or an integer from 1 through 5."
                    .to_string(),
            ));
        }

        let last_n_turns = fork_turns.parse::<usize>().map_err(|_| {
            FunctionCallError::RespondToModel(
                "fork_turns must be `none` or an integer from 1 through 5".to_string(),
            )
        })?;
        if last_n_turns == 0 {
            return Err(FunctionCallError::RespondToModel(
                "fork_turns must be `none` or an integer from 1 through 5".to_string(),
            ));
        }
        if last_n_turns > 5 {
            return Err(FunctionCallError::RespondToModel(format!(
                "requested {last_n_turns} fork turns exceeds configured limit 5; use `none` or an integer from 1 through 5"
            )));
        }

        Ok(Some(SpawnAgentForkMode::LastNTurns(last_n_turns)))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedAssignmentArgs {
    objective: String,
    acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    read_scope: Vec<RepoScope>,
    write_scope: Vec<RepoScope>,
    stop_condition: String,
    #[serde(default)]
    dependencies: Vec<AssignmentId>,
    #[serde(default)]
    risk_hints: Vec<String>,
    #[serde(default)]
    required_evidence: Vec<String>,
    #[serde(default)]
    prohibited_changes: Vec<String>,
    #[serde(default)]
    contract_claims: Vec<String>,
    #[serde(default)]
    relevant_handles: Vec<RelevantHandle>,
    #[serde(default)]
    workspace_strategy: WorkspaceStrategy,
    relation: Option<AssignmentRelation>,
    #[serde(default)]
    architecture_contract_ref: Option<ArchitectureContractRef>,
}

impl TypedAssignmentArgs {
    fn into_draft(self, root_session_id: String, role: AgentRole) -> AssignmentDraft {
        AssignmentDraft {
            root_session_id,
            admission_origin: AssignmentAdmissionOrigin::Typed,
            role,
            capability_profile: role.capability_profile(),
            objective: self.objective,
            acceptance_criteria: self.acceptance_criteria,
            read_scope: self.read_scope,
            write_scope: self.write_scope,
            stop_condition: self.stop_condition,
            dependencies: self.dependencies,
            risk_hints: self.risk_hints,
            required_evidence: self.required_evidence,
            prohibited_changes: self.prohibited_changes,
            contract_claims: self.contract_claims,
            workspace_strategy: self.workspace_strategy,
            relation: self.relation,
            architecture_contract_ref: self.architecture_contract_ref,
        }
    }
}

/// Durable role for the compatibility message shape, which carries no explicit
/// contract. Only Explorer is a read-only role that such a message can satisfy:
/// a successful Architect receipt must seal an ArchitectureContractV1, and
/// reviewers and verifiers need a target relation. Every other agent_type,
/// including absent, custom, and unknown names, keeps the Worker claim.
fn legacy_message_role(agent_type: Option<&str>) -> AgentRole {
    match parse_typed_role(agent_type) {
        Ok(AgentRole::Explorer) => AgentRole::Explorer,
        _ => AgentRole::Worker,
    }
}

fn legacy_message_draft(
    root_session_id: String,
    message: &str,
    parent_assignment_id: Option<AssignmentId>,
    role: AgentRole,
) -> AssignmentDraft {
    // The claim covers the whole repository. Only a Worker claims it for
    // writing; a read-only role reads it and keeps an empty write scope, so it
    // never becomes a conflicting writer during integration planning.
    let repository = vec![RepoScope {
        path: ".".to_string(),
        recursive: true,
    }];
    let (read_scope, write_scope) = if role == AgentRole::Worker {
        (Vec::new(), repository)
    } else {
        (repository, Vec::new())
    };
    AssignmentDraft {
        root_session_id,
        admission_origin: AssignmentAdmissionOrigin::LegacyMessage {
            parent_assignment_id,
        },
        role,
        capability_profile: role.capability_profile(),
        objective: message.to_string(),
        acceptance_criteria: vec![AcceptanceCriterion {
            evidence_policy: None,
            id: "legacy-message-result".to_string(),
            text: "Return a final response for the requested task to the parent agent.".to_string(),
        }],
        read_scope,
        write_scope,
        stop_condition: "Stop after reporting the requested result to the parent agent."
            .to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: None,
        architecture_contract_ref: None,
    }
}

async fn construct_and_attach_task_capsule(
    store: &LocalAgentTaskStore,
    repo_root: &Path,
    assignment: &Assignment,
    attempt: &Attempt,
    handles: Vec<RelevantHandle>,
) -> Result<String, StoreError> {
    let scopes = assignment
        .read_scope
        .iter()
        .chain(&assignment.write_scope)
        .cloned()
        .collect::<Vec<_>>();
    let normalized_handles = normalize_relevant_handles(repo_root, &scopes, handles).await?;

    let capsule = TaskCapsuleV1 {
        schema_version: 1,
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        role: assignment.role,
        capability_profile: assignment.capability_profile,
        requirements: assignment.acceptance_criteria.clone(),
        objective: assignment.objective.clone(),
        read_scope: assignment.read_scope.clone(),
        write_scope: assignment.write_scope.clone(),
        stop_condition: assignment.stop_condition.clone(),
        dependencies: assignment.dependencies.clone(),
        risk_hints: assignment.risk_hints.clone(),
        contract_claims: assignment.contract_claims.clone(),
        workspace_strategy: Some(assignment.workspace_strategy),
        relation: assignment.relation.clone(),
        architecture_contract_ref: assignment.architecture_contract_ref.clone(),
        integration_plan: assignment.integration_plan,
        relevant_handles: normalized_handles,
        workspace_epoch: 0,
        workspace_manifest_hash: String::new(),
        prohibited_changes: assignment.prohibited_changes.clone(),
        required_evidence: assignment.required_evidence.clone(),
    };
    let canonical_payload = serde_json::to_string(&capsule)?;
    let rendered = TaskCapsuleFragment::new(canonical_payload.clone()).render();
    if !ModelContextBudget::default().try_take(&rendered) {
        return Err(StoreError::InvalidTaskCapsule(
            "canonical TaskCapsuleV1 exceeds the model context-size policy".to_string(),
        ));
    }
    store
        .attach_task_capsule(
            assignment.assignment_id,
            attempt.attempt_id,
            canonical_payload.clone(),
        )
        .await?;
    Ok(canonical_payload)
}

/// Normalize relevant handles exactly as a TaskCapsule launch attaches them:
/// covered by the assignment scopes, regular files only, and unique under the
/// store's canonical path identity rather than independent case folding.
async fn normalize_relevant_handles(
    repo_root: &Path,
    scopes: &[RepoScope],
    handles: Vec<RelevantHandle>,
) -> Result<Vec<TaskCapsuleHandle>, StoreError> {
    let mut normalized_handles = Vec::with_capacity(handles.len());
    let mut file_handles = HashSet::new();
    let mut symbol_handles = HashSet::new();

    let normalization_root = repo_root.to_path_buf();
    let handles = tokio::task::spawn_blocking(move || {
        let handles = handles
            .into_iter()
            .map(|handle| {
                let path = normalize_repo_path(&normalization_root, handle.path())?;
                Ok((handle, path))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let paths = handles
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>();
        let keys = repo_path_identity_keys(&normalization_root, &paths)?;
        Ok::<_, StoreError>(
            handles
                .into_iter()
                .zip(keys)
                .map(|((handle, path), key)| (handle, path, key))
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(|error| {
        StoreError::CorruptData(format!("task capsule handle normalization failed: {error}"))
    })??;
    for (handle, path, path_key) in handles {
        if !scopes.iter().any(|scope| scope.covers_path(&path)) {
            return Err(StoreError::InvalidTaskCapsule(format!(
                "relevant handle {path:?} is outside the assignment read/write scope"
            )));
        }
        let existed = match tokio::fs::metadata(repo_root.join(&path)).await {
            Ok(metadata) if metadata.is_dir() => {
                return Err(StoreError::InvalidTaskCapsule(format!(
                    "relevant handle {path:?} resolves to a directory"
                )));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(StoreError::InvalidTaskCapsule(format!(
                    "relevant handle {path:?} is not a regular file"
                )));
            }
            Ok(_) => true,
            Err(error) if error.kind() == ErrorKind::NotFound => false,
            Err(error) => return Err(StoreError::Io(error)),
        };

        match handle {
            RelevantHandle::File { .. } => {
                if !file_handles.insert(path_key) {
                    return Err(StoreError::InvalidTaskCapsule(format!(
                        "duplicate file handle for {path:?}"
                    )));
                }
                normalized_handles.push(TaskCapsuleHandle::File {
                    path,
                    existed,
                    content_hash: None,
                });
            }
            RelevantHandle::Symbol { symbol, .. } => {
                let symbol = symbol.trim().to_string();
                if symbol.is_empty() {
                    return Err(StoreError::InvalidTaskCapsule(format!(
                        "symbol handle for {path:?} has an empty locator"
                    )));
                }
                if !symbol_handles.insert((path_key, symbol.clone())) {
                    return Err(StoreError::InvalidTaskCapsule(format!(
                        "duplicate symbol handle for {path:?} and locator {symbol:?}"
                    )));
                }
                normalized_handles.push(TaskCapsuleHandle::Symbol {
                    path,
                    symbol,
                    existed,
                    content_hash: None,
                });
            }
        }
    }
    Ok(normalized_handles)
}

/// Validate a reuse request's handles before any reuse, exactly as its fresh
/// launch would, and return them for the equivalence check. `None` when the
/// launch forks history instead of a TaskCapsule and so ignores handles.
async fn reuse_request_handles(
    uses_task_capsule: bool,
    repo_root: &Path,
    read_scope: &[RepoScope],
    write_scope: &[RepoScope],
    handles: Vec<RelevantHandle>,
) -> Result<Option<Vec<TaskCapsuleHandle>>, FunctionCallError> {
    if !uses_task_capsule {
        return Ok(None);
    }
    let root = repo_root.to_path_buf();
    let read_scope = read_scope.to_vec();
    let write_scope = write_scope.to_vec();
    // Read and write scopes normalize separately, as admission normalizes them.
    let scopes = tokio::task::spawn_blocking(move || {
        let mut scopes = normalize_repo_scopes(&root, &read_scope)?;
        scopes.extend(normalize_repo_scopes(&root, &write_scope)?);
        Ok::<_, StoreError>(scopes)
    })
    .await
    .map_err(|error| {
        typed_task_store_error(StoreError::CorruptData(format!(
            "task capsule scope normalization failed: {error}"
        )))
    })?
    .map_err(typed_task_store_error)?;
    normalize_relevant_handles(repo_root, &scopes, handles)
        .await
        .map(Some)
        .map_err(typed_task_store_error)
}

/// Reuse equivalence includes the canonical handle set (file, or symbol
/// locator, per path). Existence and content observations are launch-time
/// state, not identity. A producer launched without a capsule has no handles;
/// an unreadable capsule never matches.
fn capsule_handles_match(existing_capsule: Option<&str>, requested: &[TaskCapsuleHandle]) -> bool {
    let existing = match existing_capsule {
        None => Vec::new(),
        Some(payload) => match serde_json::from_str::<TaskCapsuleV1>(payload) {
            Ok(capsule) => capsule.relevant_handles,
            Err(_) => return false,
        },
    };
    capsule_handle_identities(&existing) == capsule_handle_identities(requested)
}

fn capsule_handle_identities(handles: &[TaskCapsuleHandle]) -> BTreeSet<(&str, Option<&str>)> {
    handles
        .iter()
        .map(|handle| match handle {
            TaskCapsuleHandle::File { path, .. } => (path.as_str(), None),
            TaskCapsuleHandle::Symbol { path, symbol, .. } => {
                (path.as_str(), Some(symbol.as_str()))
            }
        })
        .collect()
}

fn parse_typed_role(agent_type: Option<&str>) -> Result<AgentRole, FunctionCallError> {
    match agent_type.map(str::trim).filter(|role| !role.is_empty()) {
        Some(role) => match role {
            "architect" => Ok(AgentRole::Architect),
            "explorer" => Ok(AgentRole::Explorer),
            "worker" => Ok(AgentRole::Worker),
            "reviewer" => Ok(AgentRole::Reviewer),
            "verifier" => Ok(AgentRole::Verifier),
            "integrator" => Ok(AgentRole::Integrator),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: typed assignments require a supported agent_type; unsupported role {role:?}"
            ))),
        },
        None => Err(FunctionCallError::RespondToModel(
            "spawn_agent: typed assignments require an explicit agent_type".to_string(),
        )),
    }
}

const TYPED_RECEIPT_LIFECYCLE: &str = "This is an explicit typed assignment. Use get_agent_task only when contract details or captured validation call IDs are missing from the current context. Before your final response, call submit_agent_receipt with the actual outcome and available evidence; report missing validation honestly. A final response alone does not seal this typed assignment.";

fn typed_assignment_message(assignment: &Assignment, attempt: &Attempt) -> String {
    if matches!(
        assignment.admission_origin,
        AssignmentAdmissionOrigin::LegacyMessage { .. }
    ) {
        return format!(
            "{}\n\nReturn your result in the final response. The host records the outcome; no receipt tool call is required.",
            assignment.objective
        );
    }
    let integration_directive = match assignment.integration_plan {
        IntegrationPlan::SingleWriter => {
            "Integration plan: single_writer; you own the bounded write scope."
        }
        IntegrationPlan::RootOwned => {
            "Integration plan: root_owned; limit changes to the assigned scope and submit a receipt for root reconciliation."
        }
        IntegrationPlan::TypedIntegratorRequired => {
            "Integration plan: typed_integrator_required; work only in the isolated workspace and submit a receipt for the typed integrator to reconcile independently."
        }
    };
    format!(
        "You have a durable typed assignment. assignment_id={} attempt_id={}. Objective: {} {}",
        assignment.assignment_id, attempt.attempt_id, assignment.objective, integration_directive
    )
}

fn typed_task_store_error(error: StoreError) -> FunctionCallError {
    let storage_failure = task_store_failure_kind(&error).is_some();
    let detail = task_store_error_detail("spawn_agent", error);
    FunctionCallError::RespondToModel(if storage_failure {
        // Name the next action once: a vague failure cost a generation per
        // retry and per re-planned batch of spawns in the audited sessions.
        format!(
            "spawn_agent: no new agent was returned: {detail}. Report that delegation was unavailable. Continue locally only if the user's requirements permit solo work; required delegation remains an unmet obligation. Do not claim subagents were used or automatically replay the spawn."
        )
    } else {
        format!("spawn_agent: {detail}")
    })
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SpawnAgentResult {
    WithNickname {
        task_name: String,
        nickname: Option<String>,
        assignment_id: String,
        integration_plan: IntegrationPlan,
    },
    HiddenMetadata {
        task_name: String,
        assignment_id: String,
        integration_plan: IntegrationPlan,
    },
    Reused {
        task_name: String,
        assignment_id: String,
        attempt_id: String,
        agent_path: Option<String>,
        thread_id: Option<String>,
        status: AttemptState,
        receipt_available: bool,
        integration_plan: IntegrationPlan,
        reused: bool,
    },
}

impl ToolOutput for SpawnAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agent")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_store_failures_log_causes_without_exposing_them_to_the_model() {
        super::super::store_error_tests::assert_error_reporting(
            "spawn_agent",
            typed_task_store_error,
        );
    }

    #[test]
    fn storage_failures_preserve_required_delegation() {
        let FunctionCallError::RespondToModel(storage) =
            typed_task_store_error(StoreError::CorruptData("legacy row".to_string()))
        else {
            panic!("store failure must remain a tool response");
        };
        assert!(
            storage.starts_with("spawn_agent: no new agent was returned: "),
            "{storage}"
        );
        assert!(storage.contains("not expected to succeed"), "{storage}");
        assert!(
            storage.contains("required delegation remains an unmet obligation"),
            "{storage}"
        );
        // A rejection of the call itself stays specific and does not tell the
        // model to abandon delegation.
        let FunctionCallError::RespondToModel(rejection) =
            typed_task_store_error(StoreError::InvalidScope("outside".to_string()))
        else {
            panic!("validation rejection must remain a tool response");
        };
        assert_eq!(rejection, "spawn_agent: invalid repository scope: outside");
    }

    #[test]
    fn transient_store_failures_do_not_claim_permanent_unavailability() {
        let FunctionCallError::RespondToModel(message) =
            typed_task_store_error(StoreError::Sql(sqlx::Error::PoolTimedOut))
        else {
            panic!("storage failure must be a tool response");
        };
        assert!(message.contains("temporarily unavailable"));
        assert!(message.contains("inspect retained assignment state"));
        assert!(!message.contains("not expected to succeed"));
        assert!(message.contains("required delegation remains an unmet obligation"));
    }

    #[test]
    fn task_text_aliases_parse_as_message_and_conflicting_spellings_fail() {
        for alias in ["message", "prompt", "instructions"] {
            let mut arguments = serde_json::json!({"task_name": "worker"});
            arguments[alias] = serde_json::json!("inspect the repo");
            let args: SpawnAgentArgs = parse_arguments(&arguments.to_string())
                .unwrap_or_else(|error| panic!("{alias}: {error:?}"));
            assert_eq!(args.message.as_deref(), Some("inspect the repo"), "{alias}");
        }
        let duplicate = serde_json::json!({
            "task_name": "worker",
            "message": "inspect the repo",
            "prompt": "inspect another repo",
        });
        assert!(parse_arguments::<SpawnAgentArgs>(&duplicate.to_string()).is_err());
    }

    fn typed_assignment() -> TypedAssignmentArgs {
        TypedAssignmentArgs {
            objective: "inspect the bounded path".to_string(),
            acceptance_criteria: vec![AcceptanceCriterion {
                evidence_policy: None,
                id: "criterion-1".to_string(),
                text: "report evidence".to_string(),
            }],
            read_scope: Vec::new(),
            write_scope: Vec::new(),
            stop_condition: "stop after reporting evidence".to_string(),
            dependencies: Vec::new(),
            risk_hints: Vec::new(),
            required_evidence: Vec::new(),
            prohibited_changes: Vec::new(),
            contract_claims: Vec::new(),
            relevant_handles: Vec::new(),
            workspace_strategy: WorkspaceStrategy::Auto,
            relation: None,
            architecture_contract_ref: None,
        }
    }

    fn spawn_args() -> SpawnAgentArgs {
        SpawnAgentArgs {
            message: Some("inspect the repo".to_string()),
            assignment: None,
            task_name: "worker".to_string(),
            agent_type: None,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            fork_turns: None,
            fork_context: None,
        }
    }

    #[test]
    fn legacy_message_is_converted_to_a_repository_wide_worker_claim() {
        // Only Explorer is a read-only role a contract-less message can satisfy.
        for agent_type in [
            None,
            Some("worker"),
            Some("architect"),
            Some("verifier"),
            Some("kd4_explorer"),
        ] {
            assert_eq!(
                legacy_message_role(agent_type),
                AgentRole::Worker,
                "{agent_type:?}"
            );
        }
        let draft = legacy_message_draft(
            "root-session".to_string(),
            "inspect the repo",
            None,
            legacy_message_role(None),
        );
        assert_eq!(draft.role, AgentRole::Worker);
        assert_eq!(draft.objective, "inspect the repo");
        assert_eq!(
            draft.write_scope,
            vec![RepoScope {
                path: ".".to_string(),
                recursive: true,
            }]
        );
        assert_eq!(draft.workspace_strategy, WorkspaceStrategy::Shared);
    }

    #[tokio::test]
    async fn legacy_explorer_message_is_a_read_only_explorer_claim_end_to_end() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let coordinator = session.services.agent_control.task_coordinator();
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = codex_state::StateRuntime::init(home.path().into(), "test".into())
            .await
            .unwrap();
        coordinator
            .initialize(Arc::clone(&state), "root".into())
            .await
            .unwrap();
        let role = legacy_message_role(Some(" explorer "));
        assert_eq!(role, AgentRole::Explorer);
        let explorer = coordinator
            .create_admitted_assignment(
                repo.path(),
                legacy_message_draft("root".into(), "inspect the repo", None, role),
                false,
                "/root/explorer".into(),
                "explorer".into(),
            )
            .await
            .expect("selective admission accepts the read-only legacy claim");
        assert_eq!(explorer.assignment.role, AgentRole::Explorer);
        assert_eq!(
            explorer.assignment.capability_profile,
            codex_agent_task_store::CapabilityProfile::ReadSearch
        );
        assert!(explorer.assignment.write_scope.is_empty());
        assert!(!explorer.assignment.read_scope.is_empty());
        assert_eq!(explorer.integration_plan, IntegrationPlan::SingleWriter);
        // The read-only claim is not a conflicting writer for later work.
        let worker = coordinator
            .create_admitted_assignment(
                repo.path(),
                legacy_message_draft(
                    "root".into(),
                    "edit the repo",
                    None,
                    legacy_message_role(None),
                ),
                false,
                "/root/worker".into(),
                "worker".into(),
            )
            .await
            .expect("repository-wide worker admission");
        assert_eq!(worker.integration_plan, IntegrationPlan::SingleWriter);
        // The host seals the turn outcome without an Architect contract, and a
        // plain-message follow-up turn opens the next attempt.
        let store = coordinator.store().unwrap();
        store
            .record_legacy_agent_outcome(
                explorer.attempt.attempt_id,
                codex_agent_task_store::AgentStatusClaim::Completed,
                "reported the findings".into(),
            )
            .await
            .expect("host outcome for the read-only legacy claim");
        let follow_up = store
            .begin_legacy_agent_turn(explorer.assignment.assignment_id)
            .await
            .expect("follow-up turn for a legacy explorer");
        assert_ne!(follow_up.attempt_id, explorer.attempt.attempt_id);
        drop(turn);
        store.close().await;
        state.close().await;
    }

    #[test]
    fn worker_default_fork_modes_use_durable_task_capsule() {
        for typed in [false, true] {
            for model in [None, Some("child-model")] {
                for fork_turns in [None, Some("none"), Some(" NoNe "), Some(" ")] {
                    let mut args = spawn_args();
                    if typed {
                        args.message = None;
                        args.assignment = Some(typed_assignment());
                        args.agent_type = Some("worker".to_string());
                    }
                    args.model = model.map(str::to_string);
                    args.fork_turns = fork_turns.map(str::to_string);
                    assert!(
                        matches!(
                            args.fork_mode(AgentRole::Worker, false),
                            Ok(Some(SpawnAgentForkMode::TaskCapsule))
                        ),
                        "typed={typed}, model={model:?}, fork_turns={fork_turns:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn full_history_requires_opt_in_and_last_n_is_bounded() {
        let mut args = spawn_args();
        args.fork_turns = Some("all".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Err(FunctionCallError::RespondToModel(message)) if message.contains("is disabled")
        ));
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, true),
            Ok(Some(SpawnAgentForkMode::FullHistory))
        ));

        args.fork_turns = Some("1".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::LastNTurns(1)))
        ));
        args.fork_turns = Some("5".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::LastNTurns(5)))
        ));
        args.fork_turns = Some("6".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("requested 6 fork turns")
                    && message.contains("configured limit 5")
                    && message.contains("none")
                    && message.contains("1 through 5")
        ));
    }

    #[test]
    fn typed_worker_explicit_partial_fork_wins() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("worker".to_string());
        args.fork_turns = Some("3".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::LastNTurns(3)))
        ));
    }

    #[test]
    fn typed_worker_full_history_rejects_required_role_override() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("worker".to_string());
        args.fork_turns = Some("all".to_string());
        let typed_role = parse_typed_role(args.agent_type.as_deref()).expect("typed role");
        let fork_mode = args
            .fork_mode(typed_role, true)
            .expect("explicit fork mode");
        assert!(matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)));
        assert!(
            reject_full_fork_spawn_overrides(
                args.agent_type.as_deref(),
                args.model.as_deref(),
                args.reasoning_effort,
            )
            .is_err()
        );
    }

    #[test]
    fn typed_reviewer_rejects_conflicting_explicit_fork() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("reviewer".to_string());
        args.fork_turns = Some("all".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Reviewer, false),
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("require fork_turns=\"none\"")
        ));
    }

    #[test]
    fn custom_role_aliases_are_rejected_for_typed_tasks() {
        for agent_type in [
            "kd4_architect",
            "kd4_explorer",
            "kd4_worker",
            "kd4_reviewer",
            "kd4_verifier",
            "kd4_integrator",
        ] {
            assert!(parse_typed_role(Some(agent_type)).is_err());
        }
    }

    #[test]
    fn typed_architect_is_a_distinct_read_only_role() {
        let role = parse_typed_role(Some("architect")).expect("architect role");

        assert_eq!(role, AgentRole::Architect);
        assert_eq!(
            role.capability_profile(),
            AgentRole::Explorer.capability_profile()
        );
        assert_ne!(role, AgentRole::Explorer);
    }

    #[test]
    fn typed_verifier_accepts_explicit_task_capsule_fork() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("verifier".to_string());
        args.fork_turns = Some("none".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Verifier, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
        ));
    }

    fn file_handle(path: &str) -> RelevantHandle {
        RelevantHandle::File {
            path: path.to_string(),
        }
    }

    fn capsule_file(path: &str, existed: bool) -> TaskCapsuleHandle {
        TaskCapsuleHandle::File {
            path: path.to_string(),
            existed,
            content_hash: None,
        }
    }

    fn source_scope() -> Vec<RepoScope> {
        vec![RepoScope {
            path: "src".to_string(),
            recursive: true,
        }]
    }

    #[tokio::test]
    async fn capsule_handles_dedupe_by_store_identity_not_ascii_case() {
        let repo = tempfile::tempdir().expect("repository");
        let source = repo.path().join("src");
        std::fs::create_dir(&source).expect("source directory");
        std::fs::write(source.join("Owner.rs"), "upper").expect("upper owner");
        // Independent oracle: exclusive creation of the alias collides exactly
        // when this directory ignores case; otherwise both files now exist.
        let ignores_case = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(source.join("owner.rs"))
        {
            Ok(_) => false,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => true,
            Err(error) => panic!("case alias oracle failed: {error}"),
        };
        // Existing case-distinct files, then paths that do not exist yet.
        for pair in [["src/Owner.rs", "src/owner.rs"], ["src/New.rs", "src/new.rs"]] {
            let result = normalize_relevant_handles(
                repo.path(),
                &source_scope(),
                pair.into_iter().map(file_handle).collect(),
            )
            .await;
            if ignores_case {
                let duplicate = matches!(
                    &result,
                    Err(StoreError::InvalidTaskCapsule(message))
                        if message.contains("duplicate file handle")
                );
                assert!(duplicate, "{pair:?}: {result:?}");
            } else {
                let handles = result.expect("case-distinct handles name distinct files");
                assert_eq!(handles.len(), 2, "{pair:?}");
            }
        }
    }

    #[tokio::test]
    async fn reuse_validates_handles_as_a_fresh_capsule_launch_would() {
        let repo = tempfile::tempdir().expect("repository");
        std::fs::create_dir(repo.path().join("src")).expect("source directory");
        std::fs::write(repo.path().join("src/lib.rs"), "source").expect("source file");
        let outside = vec![file_handle("docs/notes.md")];
        let rejected =
            reuse_request_handles(true, repo.path(), &source_scope(), &[], outside.clone()).await;
        let outside_scope = matches!(
            &rejected,
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("outside the assignment")
        );
        assert!(outside_scope, "{rejected:?}");
        // History forks ignore handles, so neither a launch nor reuse checks them.
        assert_eq!(
            reuse_request_handles(false, repo.path(), &source_scope(), &[], outside).await,
            Ok(None)
        );
        let accepted = reuse_request_handles(
            true,
            repo.path(),
            &source_scope(),
            &[],
            vec![file_handle("src/lib.rs")],
        )
        .await
        .expect("a covered handle validates");
        assert_eq!(accepted, Some(vec![capsule_file("src/lib.rs", true)]));
    }

    #[test]
    fn reuse_equivalence_includes_canonical_capsule_handles() {
        let symbol = TaskCapsuleHandle::Symbol {
            path: "src/lib.rs".to_string(),
            symbol: "parse".to_string(),
            existed: true,
            content_hash: None,
        };
        let launched = serde_json::to_string(&TaskCapsuleV1 {
            schema_version: 1,
            assignment_id: AssignmentId::new(),
            attempt_id: codex_agent_task_store::AttemptId::new(),
            role: AgentRole::Explorer,
            capability_profile: AgentRole::Explorer.capability_profile(),
            requirements: Vec::new(),
            objective: "inspect the parser".to_string(),
            read_scope: source_scope(),
            write_scope: Vec::new(),
            stop_condition: String::new(),
            dependencies: Vec::new(),
            risk_hints: Vec::new(),
            contract_claims: Vec::new(),
            workspace_strategy: None,
            relation: None,
            architecture_contract_ref: None,
            integration_plan: IntegrationPlan::SingleWriter,
            relevant_handles: vec![capsule_file("src/lib.rs", true), symbol.clone()],
            workspace_epoch: 0,
            workspace_manifest_hash: String::new(),
            prohibited_changes: Vec::new(),
            required_evidence: Vec::new(),
        })
        .expect("capsule serializes");
        let launched = Some(launched.as_str());
        // Existence observed at launch is state, not identity; order is irrelevant.
        assert!(capsule_handles_match(
            launched,
            &[symbol.clone(), capsule_file("src/lib.rs", false)]
        ));
        assert!(!capsule_handles_match(launched, &[capsule_file("src/lib.rs", true)]));
        assert!(!capsule_handles_match(launched, &[capsule_file("src/main.rs", true), symbol]));
        assert!(capsule_handles_match(None, &[]));
        assert!(!capsule_handles_match(None, &[capsule_file("src/lib.rs", true)]));
        assert!(!capsule_handles_match(Some("not a capsule"), &[]));
    }
}

#[cfg(test)]
mod output_schema_tests {

    #[test]
    fn spawn_agent_schema_accepts_actual_visible_hidden_and_reused_results() {
        use super::SpawnAgentResult;
        use crate::tools::handlers::multi_agents_spec::spawn_agent_output_schema_v2;
        use codex_agent_task_store::AttemptState;
        use codex_agent_task_store::IntegrationPlan;
        for hidden in [false, true] {
            let schema = spawn_agent_output_schema_v2(hidden);
            let validator = jsonschema::validator_for(&schema).unwrap();
            let result = if hidden {
                SpawnAgentResult::HiddenMetadata {
                    task_name: "/root/task".into(),
                    assignment_id: "assignment".into(),
                    integration_plan: IntegrationPlan::SingleWriter,
                }
            } else {
                SpawnAgentResult::WithNickname {
                    task_name: "/root/task".into(),
                    nickname: None,
                    assignment_id: "assignment".into(),
                    integration_plan: IntegrationPlan::SingleWriter,
                }
            };
            let value = serde_json::to_value(result).unwrap();
            assert!(validator.is_valid(&value));
            let mut missing_plan = value.clone();
            missing_plan
                .as_object_mut()
                .unwrap()
                .remove("integration_plan");
            assert!(!validator.is_valid(&missing_plan));
            let reused = serde_json::to_value(SpawnAgentResult::Reused {
                task_name: "/root/task".into(),
                assignment_id: "assignment".into(),
                attempt_id: "attempt".into(),
                agent_path: None,
                thread_id: None,
                status: AttemptState::Active,
                receipt_available: false,
                integration_plan: IntegrationPlan::SingleWriter,
                reused: true,
            })
            .unwrap();
            assert!(validator.is_valid(&reused));
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn audit142_stderr_overflow_stops_and_reaps_the_owned_process() {
        let tasks = TaskTracker::new();
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", "[Console]::Error.Write(('x' * 1048577)); Start-Sleep -Seconds 30"]);
        let result = tokio::time::timeout(Duration::from_secs(5), run_worktree_git(command, None, &CancellationToken::new(), &tasks)).await.expect("overflow must not wait for the blocked process");
        assert!(result.unwrap_err().to_string().contains("snapshot_git_output_limit"));
        tasks.close(); tasks.wait().await;
    }

    fn fixture_git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git").current_dir(repo)
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=unused-hooks"])
            .args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    fn snapshot_fixture() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        fixture_git(repo.path(), &["init", "-q"]);
        fixture_git(repo.path(), &["config", "core.autocrlf", "false"]);
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("file.txt"), b"HELLO\n").unwrap();
        std::fs::write(repo.path().join("src/file.txt"), b"HELLO\n").unwrap();
        fixture_git(repo.path(), &["add", "."]);
        fixture_git(repo.path(), &["-c", "user.name=Snapshot", "-c", "user.email=snapshot@example.invalid", "commit", "-qm", "fixture"]);
        repo
    }

    #[tokio::test]
    async fn audit142_snapshot_protocol_ignores_prefix_and_textconv_settings() {
        for (prefix, textconv) in [(true, false), (false, true), (true, true), (false, false)] {
            let repo = snapshot_fixture();
            let home = tempfile::tempdir().unwrap();
            if prefix { fixture_git(repo.path(), &["config", "diff.noprefix", "true"]); }
            if textconv {
                std::fs::write(repo.path().join(".gitattributes"), "*.txt diff=render\n").unwrap();
                fixture_git(repo.path(), &["config", "diff.render.textconv", "sed -e s/HELLO/RENDERED/g -e s/WORLD/CONVERTED/g"]);
            }
            std::fs::write(repo.path().join("src/file.txt"), b"WORLD\n").unwrap();
            let tasks = TaskTracker::new();
            let workspace = create_isolated_worktree(repo.path(), home.path(), "controlled", &CancellationToken::new(), &tasks).await.unwrap();
            assert_eq!(std::fs::read(workspace.path.join("file.txt")).unwrap(), b"HELLO\n");
            assert_eq!(std::fs::read(workspace.path.join("src/file.txt")).unwrap(), b"WORLD\n");
            cleanup_isolated_worktree(&workspace).await.unwrap();
            tasks.close(); tasks.wait().await;
        }
    }

    #[tokio::test]
    async fn audit142_submodule_snapshot_is_explicitly_unsupported() {
        let repo = snapshot_fixture();
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("git").current_dir(repo.path())
            .args(["rev-parse", "HEAD"]).output().unwrap();
        let head = String::from_utf8(output.stdout).unwrap();
        fixture_git(repo.path(), &["update-index", "--add", "--cacheinfo", &format!("160000,{},module", head.trim())]);
        let tasks = TaskTracker::new();
        let result = create_isolated_worktree(repo.path(), home.path(), "submodule", &CancellationToken::new(), &tasks).await;
        assert!(matches!(result, Err(FunctionCallError::RespondToModel(ref message)) if message.contains("unsupported_snapshot_submodule")));
        tasks.close(); tasks.wait().await;
    }

    #[tokio::test]
    async fn audit142_large_tracked_deletion_is_bounded_before_accepting_a_patch() {
        let repo = snapshot_fixture();
        let bytes = vec![b'x'; MAX_WORKTREE_GIT_STDOUT_BYTES as usize + 1024];
        std::fs::write(repo.path().join("large.txt"), bytes).unwrap();
        fixture_git(repo.path(), &["add", "large.txt"]);
        fixture_git(repo.path(), &["-c", "user.name=Snapshot", "-c", "user.email=snapshot@example.invalid", "commit", "-qm", "large"]);
        let tasks = TaskTracker::new();
        let token = CancellationToken::new();
        assert!(capture_workspace_overlay(repo.path(), &token, &tasks).await.unwrap().tracked_diff.is_empty());
        std::fs::remove_file(repo.path().join("large.txt")).unwrap();
        let result = capture_workspace_overlay(repo.path(), &token, &tasks).await;
        assert!(matches!(result, Err(FunctionCallError::RespondToModel(ref message)) if message.contains("snapshot_git_output_limit")));
        tasks.close(); tasks.wait().await;
    }

    #[test]
    fn working_file_snapshot_stops_between_files_when_cancelled() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("first"), b"exact bytes\n").unwrap();
        let cancellation = CancellationToken::new();
        let paths = [b"first".as_slice(), b"missing".as_slice()]
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                if index == 1 {
                    cancellation.cancel();
                }
                (path, false)
            });
        let error = capture_working_file_snapshot(repo.path(), paths, &cancellation)
            .expect_err("cancellation must stop before inspecting the missing second file");
        assert!(error.to_string().contains("snapshot cancelled"));
    }

    #[tokio::test]
    async fn parallel_snapshot_preserves_tracked_and_untracked_bytes() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-c")
                .arg(format!(
                    "core.hooksPath={}",
                    repo.path().join("no-hooks").display()
                ))
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "core.autocrlf", "true"]);
        std::fs::write(repo.path().join("tracked.txt"), b"before\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&[
            "-c",
            "user.name=Snapshot Test",
            "-c",
            "user.email=snapshot@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        std::fs::write(repo.path().join("tracked.txt"), b"after\n").unwrap();
        std::fs::create_dir_all(repo.path().join("nested/files")).unwrap();
        for index in 0..16_u8 {
            std::fs::write(
                repo.path().join(format!("nested/files/{index}.bin")),
                [0, index, 255, b'\n'],
            )
            .unwrap();
        }
        let tasks = TaskTracker::new();
        let workspace = create_isolated_worktree(
            repo.path(),
            home.path(),
            "snapshot",
            &CancellationToken::new(),
            &tasks,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(workspace.path.join("tracked.txt")).unwrap(),
            b"after\n"
        );
        for index in 0..16_u8 {
            assert_eq!(
                std::fs::read(workspace.path.join(format!("nested/files/{index}.bin"))).unwrap(),
                [0, index, 255, b'\n'],
            );
        }
        cleanup_isolated_worktree(&workspace).await.unwrap();
        assert!(!workspace.path.exists());
        tasks.close();
        tasks.wait().await;
    }
}
