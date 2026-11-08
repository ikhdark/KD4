use super::*;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::context::ContextualUserFragment;
use crate::context::TaskCapsuleFragment;
use crate::isolated_workspace::WorkspaceLaunch;
use crate::isolated_workspace::cleanup_workspace_launch;
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
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
    // Enforce persistence intent even for a stale/deferred tool invocation,
    // before reservations, task admission, capsules or worktrees can be created.
    if turn.config.ephemeral {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: unavailable in ephemeral sessions because task coordination requires disk persistence; no agent was started".to_string(),
        ));
    }
    let arguments = function_arguments(payload)?;
    let mut args: SpawnAgentArgs = parse_arguments(&arguments)?;
    // Reject the canonical lexical domain before configuration, environment
    // observation, task-store startup or workspace preparation can run.
    AgentPath::root()
        .join(&args.task_name)
        .map_err(FunctionCallError::RespondToModel)?;
    args.validate_generic()?;
    let workspace_strategy = args
        .workspace_strategy
        .or_else(|| args.assignment.as_ref().map(|assignment| assignment.workspace_strategy))
        .unwrap_or(WorkspaceStrategy::Shared);
    // Writable work requires a typed contract; prose never grants write authority.
    let typed_role = AgentRole::Worker;
    let fork_mode = args.fork_mode(turn.config.multi_agent_v2.allow_full_history_forks)?;
    // History forks do not attach a capsule, but still need the task's explicit
    // file/symbol pointers. They are hints, not authority beyond the task scopes.
    let history_relevant_handles = args.assignment.as_ref()
        .filter(|_| !matches!(fork_mode, SpawnAgentForkMode::TaskCapsule))
        .map(|assignment| assignment.relevant_handles.clone())
        .unwrap_or_default();

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
        coordinator
            .binding_for_turn(&turn)
            .map(|binding| binding.assignment_id)
    } else {
        None
    };
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    let mut config = build_agent_spawn_config_for_step(&step_context)?;
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
    let admission_cwd = if let Some(cwd) = args.cwd.as_deref() {
        let selected = tokio::fs::canonicalize(cwd).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!("spawn_agent: cannot resolve cwd {cwd}: {error}"))
        })?;
        if !tokio::fs::metadata(&selected).await.is_ok_and(|metadata| metadata.is_dir()) {
            return Err(FunctionCallError::RespondToModel("spawn_agent: cwd must be an existing local directory".into(),
            ));
        }
        if !config.permissions.file_system_sandbox_policy()
            .can_read_path_with_cwd(&selected, admission_cwd.as_path())
        {
            return Err(FunctionCallError::RespondToModel("spawn_agent: selected cwd is outside inherited read permissions".into(),
            ));
        }
        // Changing a task's location is not a permission grant. Pin any implicit
        // project root to the parent's directory before changing the child cwd.
        if config.permissions.workspace_roots().is_empty() {
            config.permissions.set_workspace_roots(vec![admission_cwd.clone()]);
        }
        if config.workspace_roots.is_empty() {
            config.workspace_roots = config.permissions.workspace_roots().to_vec();
        }
        AbsolutePathBuf::from_absolute_path(selected)
            .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?
    } else {
        admission_cwd
    };
    let mut spawn_environments = Some(step_context.environments.to_selections());
    if args.cwd.is_some()
        && let Some(primary) = spawn_environments.as_mut().and_then(|environments| environments.first_mut())
    {
        primary.cwd = codex_utils_path_uri::PathUri::from_abs_path(&admission_cwd);
    }
    let mut isolated_workspace: Option<WorkspaceLaunch>;
    if let Some(service_tier) = args.service_tier.as_ref() {
        config.service_tier = Some(service_tier.clone());
    }
    // Generic subagents inherit runtime authority; configured role profiles cannot
    // replace their model, effort, instructions, or permissions.
    apply_spawn_agent_model_defaults_and_overrides(
        &session,
        turn.as_ref(),
        &mut config,
        args.model.as_deref(),
        args.reasoning_effort.clone(),
    )
    .await?;
    apply_spawn_agent_service_tier(
        &session,
        turn.as_ref(),
        &mut config,
        args.service_tier.as_deref(),
    )
    .await?;
    config.cwd = admission_cwd.clone();
    worker_startup_preflight(&session, &turn, &config, &primary, &cancellation_token).await?;

    // Durable task metadata retains the historical generic Worker tag.
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

    // Typed spawns reserve execution, registry identity, and V2 residency before any
    // worktree or durable assignment preparation. The token holds only logical RAII
    // reservations; all synchronization guards used to create it have already been released.
    let typed_role_metric = agent_role_metric_label(typed_role);
    turn.session_telemetry.counter(
        "codex.multi_agent.typed_spawn_lifecycle",
        1,
        &[("outcome", "attempted"), ("role", typed_role_metric)],
    );
    let prepared_typed_spawn = match session
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
            prepared
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
    let mut typed_reservation_metric = TypedSpawnReservationMetric::new(
        turn.session_telemetry.clone(),
        typed_role_metric,
    );
    let mut consumed_typed_spawn;
    let mut admitted_launch_guard;
    let overlap_warnings;
    let typed_task = {
        let role = typed_role;
        let coordinator = session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    session.services.state_db.clone(),
                    config.sqlite_home.clone(),
                    config.model_provider_id.clone(),
                    session.services.agent_control.session_id().to_string(),
                    config.ephemeral,
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
        let writable = args
            .assignment
            .as_ref()
            .is_some_and(|assignment| !assignment.write_scope.is_empty());
        let mut environments = spawn_environments.take().unwrap_or_default();
        isolated_workspace = crate::isolated_workspace::prepare_workspace_launch(
            &mut config,
            &mut environments,
            writable,
            workspace_strategy == WorkspaceStrategy::Isolated,
            &cancellation_token,
            &session.terminal_tasks,
        )
        .await?;
        spawn_environments = Some(environments);
        let automatic_isolation = isolated_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.is_isolated())
            && workspace_strategy != WorkspaceStrategy::Isolated;
        let repo_root = isolated_workspace
            .as_ref()
            .map_or(main_repo_root, |workspace| workspace.workspace_root.clone());
        let (mut draft, relevant_handles) = match args.assignment.take() {
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
                    workspace_strategy,
                ),
                Vec::new(),
            ),
        };
        draft.independent_verification = args.independent_verification;
        if isolated_workspace
            .as_ref()
            .is_some_and(|workspace| workspace.is_isolated())
        {
            draft.workspace_strategy = WorkspaceStrategy::Isolated;
        }
        if cancellation_token.is_cancelled() {
            if let Some(workspace) = isolated_workspace.take()
                && let Err(error) = cleanup_workspace_launch(&workspace).await
            {
                tracing::warn!(path = %workspace.workspace_root.display(), %error, "failed to clean cancelled isolated worktree");
            }
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent cancelled".to_string(),
            ));
        }
        consumed_typed_spawn = match session
            .services
            .agent_control
            .consume_prepared_typed_spawn(
                prepared_typed_spawn,
                &config,
                &spawn_source,
                Some(session.thread_id),
            )
            .await
        {
            Ok(consumed) => Some(consumed),
            Err(error) => {
                if let Some(workspace) = isolated_workspace.take()
                    && let Err(cleanup_error) = cleanup_workspace_launch(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.workspace_root.display(),
                        %cleanup_error,
                        "failed to clean isolated worktree after typed reservation revalidation failure"
                    );
                }
                return Err(collab_spawn_error(error));
            }
        };
        let admitted = match coordinator
            .create_admitted_assignment(
                &repo_root,
                draft,
                if automatic_isolation {
                    codex_agent_task_store::IsolationIntegrationOwner::Root
                } else {
                    codex_agent_task_store::IsolationIntegrationOwner::Unavailable
                },
                new_agent_path.to_string(),
                args.task_name.clone(),
            )
            .await
        {
            Ok(task) => task,
            Err(error) => {
                emit_admission_rejection_metric(&turn.session_telemetry, &error);
                if let Some(workspace) = isolated_workspace.take()
                    && let Err(cleanup_error) = cleanup_workspace_launch(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.workspace_root.display(),
                        %cleanup_error,
                        "failed to clean isolated worktree after assignment creation failure"
                    );
                }
                return Err(typed_task_store_error(error));
            }
        };
        emit_admission_overlap_metrics(&turn.session_telemetry, &admitted.overlaps);
        overlap_warnings = admitted.overlaps.warnings;
        let mut assignment = admitted.assignment;
        let attempt = admitted.attempt;
        admitted_launch_guard = AdmittedLaunchGuard {
            session: Arc::clone(&session),
            telemetry: turn.session_telemetry.clone(),
            assignment_id: assignment.assignment_id,
            armed: true,
        };
        let canonical_capsule = match construct_and_attach_task_capsule(
                store.as_ref(),
                &repo_root,
                &assignment,
                &attempt,
                relevant_handles,
            )
            .await
            {
                Ok(payload) => payload,
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
                        && let Err(cleanup_error) = cleanup_workspace_launch(&workspace).await
                    {
                        tracing::warn!(
                            path = %workspace.workspace_root.display(),
                            %cleanup_error,
                            "failed to clean isolated worktree after TaskCapsule failure"
                        );
                    }
                    return Err(typed_task_store_error(error));
                }
            };
        assignment.task_capsule = Some(canonical_capsule.clone());
        let task_capsule = matches!(fork_mode, SpawnAgentForkMode::TaskCapsule).then_some(canonical_capsule);
        (assignment, attempt, task_capsule)
    };
    if typed_task.0.admission_origin == AssignmentAdmissionOrigin::Typed {
        // Capsule and history forks share the same receipt lifecycle. Keep it in the
        // child configuration so the canonical capsule payload remains unchanged.
        let instructions = config.developer_instructions.get_or_insert_default();
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(TYPED_RECEIPT_LIFECYCLE);
    }
    let prepared_workspace = Arc::new(std::sync::Mutex::new(isolated_workspace.take()));
    let options = SpawnAgentOptions {
        prepared_workspace: Some(Arc::clone(&prepared_workspace)),
        fork_parent_spawn_call_id: Some(call_id.clone()),
        fork_mode: Some(fork_mode),
        parent_thread_id: Some(session.thread_id),
        parent_turn_id: Some(turn.sub_id.clone()),
        environments: spawn_environments,
        captured_environments: Some(step_context.environments.clone()),
        typed_task_binding: Some({
            let (assignment, attempt, _) = &typed_task;
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
    let mut effective_capabilities = JsonValue::Null;
    let spawned_agent = async {
    // Recheck after workspace/capsule preparation, through the same selected
    // backend. Errors enter the normal admitted-launch cleanup below.
    let launch_environment = crate::tools::handlers::wait_for_observed_tool_environment(
        &step_context.environments, None, &cancellation_token,
    ).await.map_err(|error| CodexErr::InvalidRequest(error.to_string()))?
        .ok_or_else(|| CodexErr::InvalidRequest("worker launch environment disappeared".into()))?;
    effective_capabilities = worker_startup_preflight(
        &session, &turn, &config, &launch_environment, &cancellation_token,
    ).await.map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
    if let Some(canonical_payload) = typed_task
        .2.as_ref()
    {
        let prepared = consumed_typed_spawn.take().ok_or_else(|| {
            CodexErr::InvalidRequest(
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
        let (assignment, attempt, _) = &typed_task;
        let communication = communication_from_plaintext_message(
            author,
            new_agent_path.clone(),
            typed_assignment_message(assignment, attempt, &history_relevant_handles),
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
        let prepared = consumed_typed_spawn.take().ok_or_else(|| {
            CodexErr::InvalidRequest(
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
    }
    }.await;
    let spawned_agent = match spawned_agent {
        Ok(spawned_agent) => spawned_agent,
        Err(error) => {
            let assignment_id = typed_task.0.assignment_id;
            let control = session.services.agent_control.clone();
            let child_thread_id = control.task_coordinator().binding_for_assignment(assignment_id)
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
                {
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
                    && let Err(cleanup_error) = cleanup_workspace_launch(&workspace).await
                {
                    tracing::warn!(
                        path = %workspace.workspace_root.display(),
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
    admitted_launch_guard.armed = false;
    drop(admitted_launch_guard);

    typed_reservation_metric.mark_retained();
    let new_thread_id = spawned_agent.thread_id;
    if codex_protocol::timing::observations::enabled() {
        turn.turn_timing_state.observe_runtime("agent_spawned", &call_id, serde_json::json!({
            "parent_thread_id":session.thread_id, "parent_turn_id":turn.sub_id,
            "child_thread_id":new_thread_id, "assignment_id":typed_task.0.assignment_id.to_string(),
            "agent_path":new_agent_path.to_string()}));
    }
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
    let role_tag = DEFAULT_ROLE_NAME;
    turn.session_telemetry.counter(
        "codex.multi_agent.spawn",
        /*inc*/ 1,
        &[("role", role_tag), ("version", "v2")],
    );
    let task_name = String::from(new_agent_path);
    let assignment_id = typed_task.0.assignment_id.to_string();
    let integration_plan = typed_task.0.integration_plan;
    effective_capabilities["task_contract"] = serde_json::json!({
        "mode": if typed_task.0.admission_origin == AssignmentAdmissionOrigin::Typed { "structured" } else { "legacy_message" },
        "write_scope": if typed_task.0.admission_origin == AssignmentAdmissionOrigin::Typed { "native_patch_enforced" } else { "read_only_enforced" },
        "typed_prohibitions": typed_task.0.prohibited_changes.iter().filter(|value| matches!(value, codex_agent_task_store::WriteProhibition::Typed(_))).count(),
        "prose_restrictions": "unverified",
        "acceptance": "unverified_until_evidence_assessed",
        "source_identity": "see_saved_task_capsule",
    });

    let hide_agent_metadata = turn.config.multi_agent_v2.hide_spawn_agent_metadata;
    if hide_agent_metadata {
        Ok(SpawnAgentResult::HiddenMetadata {
            effective_capabilities,
            task_name,
            assignment_id,
            integration_plan,
            overlap_warnings,
        })
    } else {
        Ok(SpawnAgentResult::WithNickname {
            effective_capabilities,
            task_name,
            nickname,
            assignment_id,
            integration_plan,
            overlap_warnings,
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
    overlaps: &codex_agent_task_store::AdmissionOverlapSummary,
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
    // Required for writable tasks. Message-only calls are enforced read-only.
    assignment: Option<TypedAssignmentArgs>,
    task_name: String,
    cwd: Option<String>,
    #[serde(default)]
    independent_verification: bool,
    workspace_strategy: Option<WorkspaceStrategy>,
    agent_type: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    fork_turns: Option<String>,
    fork_context: Option<bool>,
}

impl SpawnAgentArgs {
    fn validate_generic(&self) -> Result<(), FunctionCallError> {
        if self.cwd.as_deref().is_some_and(|cwd| !Path::new(cwd).is_absolute()) {
            return Err(FunctionCallError::RespondToModel("spawn_agent: cwd must be an absolute local directory; assignment scopes remain repository-relative".into()));
        }
        if self.agent_type.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: specialized roles are no longer supported; use an explicit typed assignment for writable work or message for read-only work".to_string(),
            ));
        }
        if self.model.is_some() || self.reasoning_effort.is_some() {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: model and reasoning overrides are not supported; subagents use {DEFAULT_SPAWN_AGENT_MODEL} with {DEFAULT_SPAWN_AGENT_REASONING_EFFORT} reasoning"
            )));
        }
        if self.workspace_strategy == Some(WorkspaceStrategy::Auto) {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: workspace_strategy must be shared or isolated; omit it to use shared".to_string(),
            ));
        }
        if self.workspace_strategy.is_some() && self.assignment.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: workspace_strategy cannot be combined with assignment; use assignment.workspace_strategy".to_string(),
            ));
        }
        Ok(())
    }

    fn fork_mode(
        &self,
        allow_full_history_forks: bool,
    ) -> Result<SpawnAgentForkMode, FunctionCallError> {
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

        let Some(fork_turns) = explicit_fork_turns else {
            return Ok(SpawnAgentForkMode::TaskCapsule);
        };

        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(SpawnAgentForkMode::TaskCapsule);
        }
        if fork_turns.eq_ignore_ascii_case("all") {
            if allow_full_history_forks {
                return Ok(SpawnAgentForkMode::FullHistory);
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

        Ok(SpawnAgentForkMode::LastNTurns(last_n_turns))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedAssignmentArgs {
    objective: String,
    acceptance_criteria: Vec<AcceptanceCriterion>,
    read_scope: Vec<RepoScope>,
    write_scope: Vec<RepoScope>,
    stop_condition: String,
    #[serde(default)]
    dependencies: Vec<AssignmentId>,
    #[serde(default)]
    risk_hints: Vec<String>,
    required_evidence: Vec<String>,
    prohibited_changes: Vec<codex_agent_task_store::WriteProhibition>,
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
            independent_verification: false,
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



fn legacy_message_draft(
    root_session_id: String,
    message: &str,
    parent_assignment_id: Option<AssignmentId>,
    workspace_strategy: WorkspaceStrategy,
) -> AssignmentDraft {
    let role = AgentRole::Explorer;
    let read_scope = vec![RepoScope { path: ".".into(), recursive: true }];
    let write_scope = Vec::new();
    AssignmentDraft {
        root_session_id,
        independent_verification: false,
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
        workspace_strategy,
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

    // Source identities are diagnostic evidence, never an extra spawn gate.
    // Do not inventory a legacy message's synthetic repository-wide claim.
    let manifest_scopes = if assignment.write_scope.is_empty() {
        normalized_handles.iter().map(|handle| RepoScope { path: match handle {
            TaskCapsuleHandle::File { path, .. } | TaskCapsuleHandle::Symbol { path, .. } => path.clone(),
        }, recursive: false }).collect::<Vec<_>>()
    } else { assignment.write_scope.clone() };
    let manifest_root = repo_root.to_path_buf();
    let source_manifest = Some(tokio::task::spawn_blocking(move ||
        codex_agent_task_store::capture_source_manifest(&manifest_root, &manifest_scopes))
        .await.map_err(|error| StoreError::CorruptData(error.to_string()))??);
    let source_manifest_error = None;
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
        workspace_epoch: assignment.start_epoch,
        workspace_manifest_hash: source_manifest.as_ref().map(|manifest| manifest.sha256.clone()).unwrap_or_default(),
        source_manifest,
        source_manifest_error,
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

const TYPED_RECEIPT_LIFECYCLE: &str = "This is an explicit typed assignment. Before acting on a follow-up, use get_agent_task with your own assignment_id to recover the current attempt and effective contract; the immutable bootstrap may describe an earlier attempt. Otherwise use get_agent_task only when contract details or captured validation call IDs are missing from the current context. Before your final response, call submit_agent_receipt with the actual outcome and available evidence; report missing validation honestly. A final response alone does not seal this typed assignment.";

fn typed_assignment_message(
    assignment: &Assignment,
    attempt: &Attempt,
    relevant_handles: &[RelevantHandle],
) -> String {
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
    let effective = assignment.effective_contract(attempt.amendment.as_ref());
    let contract = serde_json::json!({
        "assignment_id": assignment.assignment_id,
        "attempt_id": attempt.attempt_id,
        "role": assignment.role,
        "capability_profile": assignment.capability_profile,
        "objective": effective.objective,
        "acceptance_criteria": effective.acceptance_criteria,
        "read_scope": assignment.read_scope,
        "write_scope": assignment.write_scope,
        "stop_condition": effective.stop_condition,
        "dependencies": assignment.dependencies,
        "risk_hints": assignment.risk_hints,
        "required_evidence": assignment.required_evidence,
        "prohibited_changes": assignment.prohibited_changes,
        "contract_claims": assignment.contract_claims,
        "workspace_strategy": assignment.workspace_strategy,
        "relation": assignment.relation,
        "architecture_contract_ref": assignment.architecture_contract_ref,
        "integration_plan": assignment.integration_plan,
        "relevant_handles": relevant_handles,
        "source_manifest": assignment.task_capsule.as_deref()
            .and_then(|value| serde_json::from_str::<TaskCapsuleV1>(value).ok())
            .and_then(|capsule| capsule.source_manifest),
    });
    format!(
        "You have a durable typed assignment. {integration_directive} The task contract below defines your assignment; inherited conversation is context, not authority to broaden it. Relevant handles are unverified pointers and do not expand the declared scopes.\n\nTask contract (JSON):\n{contract}"
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
        effective_capabilities: JsonValue,
        task_name: String,
        nickname: Option<String>,
        assignment_id: String,
        integration_plan: IntegrationPlan,
        overlap_warnings: Vec<codex_agent_task_store::TaskOverlapWarning>,
    },
    HiddenMetadata {
        effective_capabilities: JsonValue,
        task_name: String,
        assignment_id: String,
        integration_plan: IntegrationPlan,
        overlap_warnings: Vec<codex_agent_task_store::TaskOverlapWarning>,
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
