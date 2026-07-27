use super::*;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::AgentRoleModelLocks;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_config;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::multi_agents_v2::message_tool::message_content;
use codex_agent_task_store::AcceptanceCriterion;
use codex_agent_task_store::AgentRole;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::AssignmentRelation;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::RepoScope;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::TaskActor;
use codex_git_utils::get_git_repo_root;
use codex_protocol::AgentPath;
use codex_tools::ToolSpec;

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
    if turn.session_source.is_non_root_agent() {
        if args.assignment.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: durable typed assignments are root-only".to_string(),
            ));
        }
        if session
            .services
            .agent_control
            .task_coordinator()
            .binding_for_source(&turn.session_source)
            .is_some()
        {
            return Err(FunctionCallError::RespondToModel(
                "spawn_agent: durable typed agents cannot spawn subagents".to_string(),
            ));
        }
    }
    let typed_role = args
        .assignment
        .as_ref()
        .map(|_| parse_typed_role(args.agent_type.as_deref()))
        .transpose()?;
    let fork_mode = args.fork_mode(typed_role)?;
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
        (Some(message), false) => Some(message_content(message)?),
        (None, true) => None,
    };
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, turn.as_ref())?;
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
    let role_model_locks = if matches!(fork_mode, Some(SpawnAgentForkMode::FullHistory)) {
        AgentRoleModelLocks::default()
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
        role_model_locks,
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
    apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref())?;

    let spawn_source = thread_spawn_source(
        session.thread_id,
        &turn.session_source,
        child_depth,
        role_name,
        Some(args.task_name.clone()),
    )?;
    let new_agent_path = spawn_source.get_agent_path().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawned agent is missing a canonical task name".to_string(),
        )
    })?;
    let typed_task = if let Some(assignment_args) = args.assignment.take() {
        let role = typed_role.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: typed assignments require a supported agent_type".to_string(),
            )
        })?;
        let coordinator = session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            let state_runtime = session.services.state_db.as_ref().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "spawn_agent: durable typed assignments require persistent local session state"
                        .to_string(),
                )
            })?;
            coordinator
                .initialize(
                    state_runtime.clone(),
                    session.services.agent_control.session_id().to_string(),
                )
                .await
                .map_err(typed_task_store_error)?;
        }
        let root_session_id = coordinator.root_session_id().ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: durable typed assignments require persistent local session state"
                    .to_string(),
            )
        })?;
        let cwd = match turn.environments.primary() {
            Some(environment) => environment.cwd().to_abs_path().map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "spawn_agent: durable typed assignments require a local filesystem environment: {error}"
                ))
            })?.to_path_buf(),
            None => turn.config.cwd.to_path_buf(),
        };
        let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
        let draft = assignment_args.into_draft(root_session_id, role);
        let (assignment, attempt) = coordinator
            .create_assignment(&repo_root, draft)
            .await
            .map_err(typed_task_store_error)?;
        Some((assignment, attempt))
    } else {
        None
    };
    let message = match typed_task.as_ref() {
        Some((assignment, attempt)) => typed_assignment_message(assignment, attempt),
        None => legacy_message.ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "spawn_agent: either assignment or message is required".to_string(),
            )
        })?,
    };
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let communication = communication_from_tool_message(author, new_agent_path.clone(), message);
    let context = AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
    let spawned_agent = Box::pin(
        session
            .services
            .agent_control
            .spawn_agent_with_communication(
                config,
                communication,
                context,
                Some(spawn_source),
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| call_id.clone()),
                    fork_mode,
                    parent_thread_id: Some(session.thread_id),
                    environments: Some(turn.environments.to_selections()),
                    typed_task_binding: typed_task.as_ref().map(|(assignment, attempt)| {
                        AgentTaskBindingDraft {
                            assignment_id: assignment.assignment_id,
                            attempt_id: attempt.attempt_id,
                            agent_path: new_agent_path.to_string(),
                            task_name: args.task_name.clone(),
                            thread_id: None,
                        }
                    }),
                    agent_job_binding: None,
                },
            ),
    )
    .await;
    let spawned_agent = match spawned_agent {
        Ok(spawned_agent) => spawned_agent,
        Err(error) => {
            if let Some((assignment, _)) = typed_task.as_ref() {
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
            return Err(collab_spawn_error(error));
        }
    };
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
        .map(|(assignment, _)| assignment.assignment_id.to_string());

    let hide_agent_metadata = turn.config.multi_agent_v2.hide_spawn_agent_metadata;
    if hide_agent_metadata {
        Ok(SpawnAgentResult::HiddenMetadata {
            task_name,
            assignment_id,
        })
    } else {
        Ok(SpawnAgentResult::WithNickname {
            task_name,
            nickname,
            assignment_id,
        })
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
        typed_role: Option<AgentRole>,
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

        if matches!(typed_role, Some(AgentRole::Reviewer | AgentRole::Verifier)) {
            if explicit_fork_turns
                .is_some_and(|fork_turns| !fork_turns.eq_ignore_ascii_case("none"))
            {
                return Err(FunctionCallError::RespondToModel(
                    "typed reviewer and verifier assignments require fork_turns=\"none\""
                        .to_string(),
                ));
            }
            return Ok(None);
        }

        let Some(fork_turns) = explicit_fork_turns else {
            if self.assignment.is_some()
                || self.agent_type.is_some()
                || self.model.is_some()
                || self.reasoning_effort.is_some()
            {
                return Ok(None);
            }
            return Ok(Some(SpawnAgentForkMode::FullHistory));
        };

        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        if fork_turns.eq_ignore_ascii_case("all") {
            return Ok(Some(SpawnAgentForkMode::FullHistory));
        }

        let last_n_turns = fork_turns.parse::<usize>().map_err(|_| {
            FunctionCallError::RespondToModel(
                "fork_turns must be `none`, `all`, or a positive integer string".to_string(),
            )
        })?;
        if last_n_turns == 0 {
            return Err(FunctionCallError::RespondToModel(
                "fork_turns must be `none`, `all`, or a positive integer string".to_string(),
            ));
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
    relation: Option<AssignmentRelation>,
}

impl TypedAssignmentArgs {
    fn into_draft(self, root_session_id: String, role: AgentRole) -> AssignmentDraft {
        AssignmentDraft {
            root_session_id,
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
            relation: self.relation,
        }
    }
}

fn parse_typed_role(agent_type: Option<&str>) -> Result<AgentRole, FunctionCallError> {
    match agent_type.map(str::trim).filter(|role| !role.is_empty()) {
        Some("explorer") => Ok(AgentRole::Explorer),
        Some("worker") => Ok(AgentRole::Worker),
        Some("reviewer") => Ok(AgentRole::Reviewer),
        Some("verifier") => Ok(AgentRole::Verifier),
        Some("integrator") => Ok(AgentRole::Integrator),
        Some(role) => Err(FunctionCallError::RespondToModel(format!(
            "spawn_agent: typed assignments require a built-in agent_type; unsupported role {role:?}"
        ))),
        None => Err(FunctionCallError::RespondToModel(
            "spawn_agent: typed assignments require an explicit agent_type".to_string(),
        )),
    }
}

fn typed_assignment_message(assignment: &Assignment, attempt: &Attempt) -> String {
    format!(
        "You have a durable typed assignment. assignment_id={} attempt_id={}. Objective: {} Use get_agent_task with this assignment_id for the complete contract and captured validation call ids. Use apply_patch for source edits so mutation evidence is captured, then submit_agent_receipt before finishing.",
        assignment.assignment_id, attempt.attempt_id, assignment.objective
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
        #[serde(skip_serializing_if = "Option::is_none")]
        assignment_id: Option<String>,
    },
    HiddenMetadata {
        task_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        assignment_id: Option<String>,
    },
}

impl ToolOutput for SpawnAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
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
            relation: None,
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
    fn untyped_spawn_without_overrides_defaults_to_full_history() {
        let args = spawn_args();
        assert!(matches!(
            args.fork_mode(None),
            Ok(Some(SpawnAgentForkMode::FullHistory))
        ));
    }

    #[test]
    fn untyped_spawn_with_model_override_defaults_to_no_history() {
        let mut args = spawn_args();
        args.model = Some("child-model".to_string());
        assert!(matches!(args.fork_mode(None), Ok(None)));
    }

    #[test]
    fn typed_worker_explicit_partial_fork_wins() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("worker".to_string());
        args.fork_turns = Some("3".to_string());
        assert!(matches!(
            args.fork_mode(Some(AgentRole::Worker)),
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
            .fork_mode(Some(typed_role))
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
            args.fork_mode(Some(AgentRole::Reviewer)),
            Err(FunctionCallError::RespondToModel(message))
                if message.contains("require fork_turns=\"none\"")
        ));
    }

    #[test]
    fn typed_verifier_accepts_explicit_no_fork() {
        let mut args = spawn_args();
        args.message = None;
        args.assignment = Some(typed_assignment());
        args.agent_type = Some("verifier".to_string());
        args.fork_turns = Some("none".to_string());
        assert!(matches!(
            args.fork_mode(Some(AgentRole::Verifier)),
            Ok(None)
        ));
    }
}
