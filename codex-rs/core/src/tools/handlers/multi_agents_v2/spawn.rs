use super::*;
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
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::ArchitectureContractRef;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AssignmentRelation;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::IntegrationPlan;
use codex_agent_task_store::RelevantHandle;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::TaskCapsuleHandle;
use codex_agent_task_store::TaskCapsuleV1;
use codex_agent_task_store::WorkspaceStrategy;
use codex_agent_task_store::normalize_repo_path;
use codex_context_fragments::ModelContextBudget;
use codex_git_utils::get_git_repo_root;
use codex_otel::SessionTelemetry;
use codex_protocol::AgentPath;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

const MAX_UNTRACKED_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_UNTRACKED_SNAPSHOT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

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

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_spawn_agent(invocation).await.map(boxed_tool_output) })
    }
}

async fn handle_spawn_agent(
    invocation: ToolInvocation,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
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
        AgentRole::Worker
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
        let coordinator = session.services.agent_control.task_coordinator();
        match coordinator.binding_for_source(&turn.session_source) {
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
    if !crate::session::multi_agents::spawn_is_authorized(turn.as_ref())
        && legacy_parent_assignment_id.is_none()
    {
        return Err(FunctionCallError::RespondToModel(
            "spawn_agent: this turn is in explicit-request-only mode and the user did not explicitly authorize spawning agents"
                .to_string(),
        ));
    }
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, turn.as_ref())?;
    let mut spawn_environments = Some(turn.environments.to_selections());
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
        turn.config.service_tier.as_deref(),
        args.service_tier.as_deref(),
    )
    .await?;
    let runtime_role_locks = AgentRoleLocks {
        permissions: typed_role == AgentRole::Reviewer && role_locks.permissions,
        ..role_locks
    };
    apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref(), runtime_role_locks)?;

    // Every V2 spawn becomes a durable typed assignment. Legacy message-shaped
    // calls have no explicit agent_type, but their durable role is Worker, so
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
        let cwd = match turn.environments.primary() {
            Some(environment) => environment.cwd().to_abs_path().map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: durable typed assignments require a local filesystem environment: {error}"
                ))
            })?.to_path_buf(),
            None => turn.config.cwd.to_path_buf(),
        };
        let main_repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        let workspace_strategy = args
            .assignment
            .as_ref()
            .map(|assignment| assignment.workspace_strategy)
            .unwrap_or(WorkspaceStrategy::Shared);
        let repo_root = if workspace_strategy == WorkspaceStrategy::Isolated {
            let workspace = Box::pin(create_isolated_worktree(
                &main_repo_root,
                config.codex_home.as_path(),
                &args.task_name,
            ))
            .await?;
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
            spawn_environments = None;
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
                ),
                Vec::new(),
            ),
        };
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
        let admitted = match coordinator
            .create_admitted_assignment(&repo_root, draft, isolated_integrator_available)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                if let StoreError::AdmissionRejected {
                    reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
                    reusable_assignment_id: Some(assignment_id),
                } = &error
                {
                    emit_admission_reuse_metric(&turn.session_telemetry);
                    if let Some(workspace) = isolated_workspace.take()
                        && let Err(cleanup_error) = cleanup_isolated_worktree(&workspace).await
                    {
                        tracing::warn!(
                            path = %workspace.path.display(),
                            %cleanup_error,
                            "failed to clean isolated worktree after assignment reuse"
                        );
                    }
                    return reusable_spawn_result(coordinator, *assignment_id).await;
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
                            TaskActor::Root,
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
    let options = SpawnAgentOptions {
        fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| call_id.clone()),
        fork_mode,
        parent_thread_id: Some(session.thread_id),
        environments: spawn_environments,
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
            )
            .await
        } else {
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
            )
            .await
        }
    };
    let spawned_agent = match spawned_agent {
        Ok(spawned_agent) => spawned_agent,
        Err(error) => {
            if let Some((assignment, _, _)) = typed_task.as_ref() {
                let coordinator = session.services.agent_control.task_coordinator();
                if let Some(store) = coordinator.store() {
                    if let Err(rollback_error) = store
                        .abandon_agent_task(
                            TaskActor::Root,
                            assignment.assignment_id,
                            format!("spawn failed before the typed agent started: {error}"),
                        )
                        .await
                    {
                        tracing::warn!(
                            assignment_id = %assignment.assignment_id,
                            %rollback_error,
                            "failed to abandon typed assignment after spawn failure"
                        );
                    }
                    // A terminal fallback receipt may race the explicit abandonment while the
                    // child is shutting down. Removal performs its own terminal-state check, so
                    // attempt it independently and never delete an active task's binding.
                    if let Err(cleanup_error) = coordinator
                        .remove_agent_task_binding(assignment.assignment_id)
                        .await
                    {
                        tracing::warn!(
                            assignment_id = %assignment.assignment_id,
                            %cleanup_error,
                            "failed to remove typed task binding after spawn failure"
                        );
                    }
                    coordinator
                        .maybe_emit_terminal_metrics(
                            assignment.assignment_id,
                            &turn.session_telemetry,
                        )
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
            return Err(collab_spawn_error(error));
        }
    };
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
    for (kind, count) in [("benign_read_read", overlaps.benign_read_overlap_count)] {
        if count > 0 {
            session_telemetry.counter(
                "codex.multi_agent.admission_overlap",
                i64::from(count),
                &[("kind", kind), ("outcome", "admitted")],
            );
        }
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

async fn reusable_spawn_result(
    coordinator: &crate::agent::task_coordinator::AgentTaskCoordinator,
    assignment_id: AssignmentId,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let task = coordinator
        .get_agent_task(assignment_id, None)
        .await
        .map_err(typed_task_store_error)?;
    let binding = coordinator
        .get_agent_task_binding(assignment_id)
        .await
        .map_err(typed_task_store_error)?;
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
}

#[derive(Debug, Eq, PartialEq)]
struct WorkspaceOverlay {
    tracked_diff: Vec<u8>,
    untracked_files: Vec<(PathBuf, Vec<u8>)>,
}

async fn create_isolated_worktree(
    repo_root: &Path,
    codex_home: &Path,
    task_name: &str,
) -> Result<IsolatedWorkspace, FunctionCallError> {
    let initial_overlay = capture_workspace_overlay(repo_root).await?;
    let repository_key = format!(
        "{:x}",
        Sha256::digest(repo_root.to_string_lossy().as_bytes())
    );
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
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg("HEAD")
        .output()
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not launch git worktree add: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let workspace = IsolatedWorkspace {
        main_repo_root: repo_root.to_path_buf(),
        path,
    };
    let populate_result = Box::pin(async {
        let current_overlay = capture_workspace_overlay(repo_root).await?;
        if current_overlay != initial_overlay {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: the shared worktree changed while its isolated snapshot was being created; retry after the current writer finishes"
                    .to_string(),
            ));
        }
        if !initial_overlay.tracked_diff.is_empty() {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(&workspace.path)
                .args(["apply", "--binary", "--whitespace=nowarn", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "spawn_agent: could not apply the shared-worktree snapshot: {error}"
                    ))
                })?;
            let mut stdin = child.stdin.take().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: git apply stdin was unavailable".to_string(),
                )
            })?;
            stdin
                .write_all(&initial_overlay.tracked_diff)
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "spawn_agent: could not stream the shared-worktree snapshot: {error}"
                    ))
                })?;
            drop(stdin);
            let output = child.wait_with_output().await.map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not finish applying the shared-worktree snapshot: {error}"
                ))
            })?;
            if !output.status.success() {
                return Err(FunctionCallError::RespondToModel(format!(
                    "spawn_agent: isolated snapshot apply failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
        for (relative_path, bytes) in initial_overlay.untracked_files {
            let target = workspace.path.join(relative_path);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "spawn_agent: could not create an isolated snapshot directory: {error}"
                    ))
                })?;
            }
            tokio::fs::write(&target, bytes).await.map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not copy an untracked file into the isolated snapshot: {error}"
                ))
            })?;
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
    Ok(workspace)
}

async fn capture_workspace_overlay(
    repo_root: &Path,
) -> Result<WorkspaceOverlay, FunctionCallError> {
    let diff = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--binary", "--no-ext-diff", "HEAD", "--"])
        .output()
        .await
        .map_err(|error| {
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
    let untracked = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .await
        .map_err(|error| {
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
    let mut untracked_files = Vec::new();
    let mut total_untracked_bytes = 0u64;
    for raw_path in untracked.stdout.split(|byte| *byte == 0) {
        if raw_path.is_empty() {
            continue;
        }
        let path = PathBuf::from(String::from_utf8(raw_path.to_vec()).map_err(|_| {
            FunctionCallError::RespondToModel(
                "spawn_agent: an untracked path is not valid UTF-8".to_string(),
            )
        })?);
        let source = repo_root.join(&path);
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not inspect untracked file {}: {error}",
                    path.display()
                ))
            })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: isolated snapshots reject untracked symlinks and special files: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_UNTRACKED_SNAPSHOT_FILE_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: untracked file {} is too large for an isolated snapshot ({} bytes, max {})",
                path.display(),
                metadata.len(),
                MAX_UNTRACKED_SNAPSHOT_FILE_BYTES
            )));
        }
        total_untracked_bytes = total_untracked_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: untracked snapshot size overflowed".to_string(),
                )
            })?;
        if total_untracked_bytes > MAX_UNTRACKED_SNAPSHOT_TOTAL_BYTES {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: untracked files exceed the isolated snapshot limit of {MAX_UNTRACKED_SNAPSHOT_TOTAL_BYTES} bytes"
            )));
        }
        let file = tokio::fs::File::open(&source).await.map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "spawn_agent: could not snapshot untracked file {}: {error}",
                path.display()
            ))
        })?;
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(MAX_UNTRACKED_SNAPSHOT_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: could not snapshot untracked file {}: {error}",
                    path.display()
                ))
            })?;
        if bytes.len() as u64 != metadata.len() {
            return Err(FunctionCallError::RespondToModel(format!(
                "spawn_agent: untracked file {} changed while the isolated snapshot was captured",
                path.display()
            )));
        }
        untracked_files.push((path, bytes));
    }
    untracked_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(WorkspaceOverlay {
        tracked_diff: diff.stdout,
        untracked_files,
    })
}

async fn cleanup_isolated_worktree(workspace: &IsolatedWorkspace) -> Result<(), std::io::Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&workspace.main_repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(&workspace.path)
        .output()
        .await?;
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
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
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

#[derive(Debug, Deserialize)]
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

fn legacy_message_draft(
    root_session_id: String,
    message: &str,
    parent_assignment_id: Option<AssignmentId>,
) -> AssignmentDraft {
    AssignmentDraft {
        root_session_id,
        admission_origin: AssignmentAdmissionOrigin::LegacyMessage {
            parent_assignment_id,
        },
        role: AgentRole::Worker,
        capability_profile: AgentRole::Worker.capability_profile(),
        objective: message.to_string(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "legacy-message-result".to_string(),
            text: "Return a concrete result for the requested task to the parent agent."
                .to_string(),
        }],
        read_scope: Vec::new(),
        write_scope: vec![RepoScope {
            path: ".".to_string(),
            recursive: true,
        }],
        stop_condition: "Stop after reporting the requested result to the parent agent."
            .to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: vec!["task result reported to the parent agent".to_string()],
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: None,
        architecture_contract_ref: None,
    }
}

async fn construct_and_attach_task_capsule(
    store: &dyn AgentTaskStore,
    repo_root: &Path,
    assignment: &Assignment,
    attempt: &Attempt,
    handles: Vec<RelevantHandle>,
) -> Result<String, StoreError> {
    let scopes = assignment
        .read_scope
        .iter()
        .chain(assignment.write_scope.iter())
        .collect::<Vec<_>>();
    let mut normalized_handles = Vec::with_capacity(handles.len());
    let mut file_handles = HashSet::new();
    let mut symbol_handles = HashSet::new();
    let mut distinct_paths = BTreeMap::new();

    for handle in handles {
        let path = normalize_repo_path(repo_root, handle.path())?;
        if !scopes.iter().any(|scope| scope.covers_path(&path)) {
            return Err(StoreError::InvalidTaskCapsule(format!(
                "relevant handle {path:?} is outside the assignment read/write scope"
            )));
        }
        match std::fs::metadata(repo_root.join(&path)) {
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
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }

        let path_key = if cfg!(windows) {
            path.to_ascii_lowercase()
        } else {
            path.clone()
        };
        distinct_paths
            .entry(path_key.clone())
            .or_insert_with(|| path.clone());
        match handle {
            RelevantHandle::File { .. } => {
                if !file_handles.insert(path_key) {
                    return Err(StoreError::InvalidTaskCapsule(format!(
                        "duplicate file handle for {path:?}"
                    )));
                }
                normalized_handles.push(RelevantHandle::File { path });
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
                normalized_handles.push(RelevantHandle::Symbol { path, symbol });
            }
        }
    }

    let revision = store
        .capture_workspace_revision(repo_root, distinct_paths.into_values().collect())
        .await?;
    let entries = revision
        .files
        .iter()
        .map(|entry| {
            let key = if cfg!(windows) {
                entry.path.to_ascii_lowercase()
            } else {
                entry.path.clone()
            };
            (key, entry)
        })
        .collect::<BTreeMap<_, _>>();
    let relevant_handles = normalized_handles
        .into_iter()
        .map(|handle| {
            let key = if cfg!(windows) {
                handle.path().to_ascii_lowercase()
            } else {
                handle.path().to_string()
            };
            let entry = entries.get(&key).ok_or_else(|| {
                StoreError::InvalidTaskCapsule(format!(
                    "workspace revision omitted relevant handle {:?}",
                    handle.path()
                ))
            })?;
            Ok(match handle {
                RelevantHandle::File { path } => TaskCapsuleHandle::File {
                    path,
                    existed: entry.existed,
                    content_hash: entry.content_hash.clone(),
                },
                RelevantHandle::Symbol { path, symbol } => TaskCapsuleHandle::Symbol {
                    path,
                    symbol,
                    existed: entry.existed,
                    content_hash: entry.content_hash.clone(),
                },
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;

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
        relevant_handles,
        workspace_epoch: revision.epoch,
        workspace_manifest_hash: revision.manifest_hash,
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

fn typed_assignment_message(assignment: &Assignment, attempt: &Attempt) -> String {
    let integration_directive = match assignment.integration_plan {
        IntegrationPlan::SingleWriter => {
            "Integration plan: single_writer; you own the bounded write scope."
        }
        IntegrationPlan::RootOwned => {
            "Integration plan: root_owned; limit changes to the assigned scope and submit a receipt for root reconciliation."
        }
        IntegrationPlan::TypedIntegratorRequired => {
            "Integration plan: typed_integrator_required; work only in the isolated workspace and publish a versioned receipt handoff for the typed integrator."
        }
    };
    format!(
        "You have a durable typed assignment. assignment_id={} attempt_id={}. Objective: {} {} Use get_agent_task with this assignment_id for the complete contract and captured validation call ids. Use apply_patch for source edits so mutation evidence is captured, then submit_agent_receipt before finishing.",
        assignment.assignment_id, attempt.attempt_id, assignment.objective, integration_directive
    )
}

fn typed_task_store_error(error: StoreError) -> FunctionCallError {
    let detail = match error {
        StoreError::Io(_)
        | StoreError::Sql(_)
        | StoreError::Migration(_)
        | StoreError::Json(_)
        | StoreError::CorruptData(_) => {
            "the typed task store is unavailable or contains invalid persisted state".to_string()
        }
        error => error.to_string(),
    };
    FunctionCallError::RespondToModel(format!("spawn_agent: {detail}"))
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

    fn typed_assignment() -> TypedAssignmentArgs {
        TypedAssignmentArgs {
            objective: "inspect the bounded path".to_string(),
            acceptance_criteria: vec![AcceptanceCriterion {
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
        let draft = legacy_message_draft("root-session".to_string(), "inspect the repo", None);
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

    #[test]
    fn legacy_message_without_overrides_uses_durable_task_capsule() {
        let args = spawn_args();
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
        ));
    }

    #[test]
    fn legacy_message_with_model_override_uses_durable_task_capsule() {
        let mut args = spawn_args();
        args.model = Some("child-model".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
        ));
    }

    #[test]
    fn legacy_explicit_none_uses_durable_task_capsule() {
        let mut args = spawn_args();
        args.fork_turns = Some("none".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
        ));
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
    fn typed_worker_omitted_fork_turns_selects_task_capsule() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("worker".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
        ));
    }

    #[test]
    fn typed_worker_explicit_none_selects_task_capsule() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("worker".to_string());
        args.fork_turns = Some("none".to_string());
        assert!(matches!(
            args.fork_mode(AgentRole::Worker, false),
            Ok(Some(SpawnAgentForkMode::TaskCapsule))
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
}
