use super::*;
use crate::agent::control::ListedAgent;
use crate::tools::handlers::multi_agents_spec::create_list_agents_tool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::sync::Arc;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("list_agents")
    }

    fn spec(&self) -> ToolSpec {
        create_list_agents_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // A read-only agent/task snapshot must not take the global exclusive
        // workspace gate: that serializes independent siblings and stalls the
        // shared polls write_stdin issues in the same batch. Mirrors wait.rs.
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            payload,
            call_id,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);
        let arguments = function_arguments(payload)?;
        let mut args: ListAgentsArgs = parse_arguments(&arguments)?;
        // Normalize once so live agents and durable tasks use the same filter.
        args.path_prefix = args
            .path_prefix
            .map(|prefix| prefix.trim_end_matches('/').to_string());
        session
            .services
            .agent_control
            .register_session_root(session.thread_id, turn.parent_thread_id);
        let (agents, mut task_snapshots) = session
            .services
            .agent_control
            .list_agents_with_task_snapshots(&turn.session_source, args.path_prefix.as_deref())
            .await
            .map_err(collab_spawn_error)?;
        let coordinator = session.services.agent_control.task_coordinator();
        if coordinator.store().is_none() {
            coordinator
                .initialize_for_workspace_coordination(
                    session.services.state_db.clone(),
                    turn.config.sqlite_home.clone(),
                    turn.config.model_provider_id.clone(),
                    session.services.agent_control.session_id().to_string(),
                    turn.config.ephemeral,
                )
                .await
                .map_err(|error| {
                    task_store_context_error(
                        "list_agents",
                        "list_agents: durable typed-task state could not initialize",
                        error,
                    )
                })?;
        }
        let resolved_prefix = args
            .path_prefix
            .as_deref()
            .map(|prefix| {
                turn.session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(FunctionCallError::RespondToModel)
            })
            .transpose()?;
        let mut typed_tasks = Vec::new();
        if let (Some(store), Some(root_session_id)) =
            (coordinator.store(), coordinator.root_session_id())
        {
            let bindings = store
                .list_agent_task_bindings(root_session_id, None)
                .await
                .map_err(|error| {
                    task_store_context_error(
                        "list_agents",
                        "list_agents: durable typed-task state is unavailable",
                        error,
                    )
                })?;
            for binding in bindings {
                if resolved_prefix
                    .as_ref()
                    .is_some_and(|prefix| !agent_path_matches_prefix(&binding.agent_path, prefix))
                {
                    continue;
                }
                let snapshot = task_snapshots.remove(&binding.assignment_id)
                    .filter(|task| task.current_attempt.attempt_id == binding.attempt_id);
                let loaded = match snapshot {
                    Some(task) => Ok(task),
                    None => coordinator.get_agent_task(binding.assignment_id, Some(0)).await,
                };
                let task = match loaded {
                    Ok(task) => task,
                    Err(error) if assignment_local_failure(&error) => {
                        typed_tasks.push(json!({
                            "assignment_id": binding.assignment_id, "attempt_id": binding.attempt_id,
                            "agent_path": binding.agent_path, "task_name": binding.task_name,
                            "health": "unavailable", "obligation_resolved": false,
                            "error": task_store_error_detail("list_agents", error),
                        }));
                        continue;
                    }
                    Err(error) => return Err(FunctionCallError::RespondToModel(task_store_error_detail("list_agents", error))),
                };
                typed_tasks.push(json!({
                    "assignment_id": binding.assignment_id,
                    "attempt_id": binding.attempt_id,
                    "agent_path": binding.agent_path,
                    "task_name": binding.task_name,
                    "workspace_id": task.assignment.workspace_id,
                    "workspace_strategy": task.assignment.workspace_strategy,
                    "epoch": task.workspace_status.epoch,
                    "last_progress_at": task.workspace_status.last_progress_at,
                    "lease_state": task.workspace_status.lease_state,
                    "pending_gates": task.workspace_status.pending_gates,
                    "stale_reason": task.workspace_status.stale_reason,
                    "next_required_action": task.workspace_status.next_required_action,
                    "nudge_sent_at": task.workspace_status.nudge_sent_at,
                }));
            }
        }

        let mut result = ListAgentsResult {
            agents: agents.into_iter().map(ListedAgentView::from).collect(),
            typed_tasks,
            unresolved_descendants: coordinator.unavailable_descendants(),
            revision: String::new(),
            unchanged: false,
            task_text_recovery: None,
            completed_report_recovery: None,
        };
        if args.status_only && let Some(reports) = result.completed_report_snapshot() {
            // Retain the exact omitted bodies, not a later current-state reread.
            // The existing snapshot owner protects and registers the recovery
            // before any completed payload is removed from this response.
            let canonical = Arc::new(codex_tools::CanonicalToolResult::json(reports));
            let artifact = session
                .create_source_snapshot_artifact(call_id, Arc::clone(&canonical), None)
                .await;
            if let Some(artifact_id) = artifact.artifact_id().filter(|_| {
                artifact.complete
                    && artifact.retained_bytes == canonical.exact_bytes
                    && artifact.unavailable_ranges.is_empty()
            }) {
                result.omit_completed_reports();
                result.completed_report_recovery = Some(json!({
                    "tool": "read_tool_output",
                    "arguments": {
                        "artifact_id": artifact_id,
                        "selectors": [{"kind": "json_pointer", "pointer": "/agents"}],
                    },
                    "canonical_sha256": canonical.sha256,
                    "canonical_bytes": canonical.exact_bytes,
                }));
            } else {
                // Fail open with complete reports rather than hide content
                // behind an incomplete or unprotected recovery artifact.
                result.task_text_recovery.get_or_insert_with(String::new)
                    .push_str(&format!(" Status-only report retention failed; completed reports remain inline: {}", artifact.error.as_deref().unwrap_or("incomplete snapshot")));
            }
        }
        result.apply_view(&args, &session.thread_id.to_string())?;
        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListAgentsArgs {
    path_prefix: Option<String>,
    #[serde(default)]
    compact: bool,
    #[serde(default)]
    status_only: bool,
    since_revision: Option<String>,
}

#[derive(Debug, Serialize)]
struct ListedAgentView {
    agent_name: String,
    agent_status: ListedAgentStatus,
    last_task_message: Option<String>,
    runtime_loaded: bool,
}

impl From<ListedAgent> for ListedAgentView {
    fn from(agent: ListedAgent) -> Self {
        Self {
            agent_name: agent.agent_name,
            agent_status: ListedAgentStatus::Full(agent.agent_status),
            last_task_message: agent.last_task_message,
            runtime_loaded: agent.runtime_loaded,
        }
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
enum ListedAgentStatus {
    Full(AgentStatus),
    StatusOnly(CompletedAgentStatus),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CompletedAgentStatus {
    Completed,
    CompletedWithSurface,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    agents: Vec<ListedAgentView>,
    typed_tasks: Vec<JsonValue>,
    unresolved_descendants: Vec<(String, String)>,
    revision: String,
    unchanged: bool,
    task_text_recovery: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_report_recovery: Option<JsonValue>,
}

impl ListAgentsResult {
    fn apply_view(&mut self, args: &ListAgentsArgs, root: &str) -> Result<(), FunctionCallError> {
        use sha2::Digest;
        if args.compact || args.status_only {
            for agent in &mut self.agents {
                agent.last_task_message = None;
            }
            let failure = self.task_text_recovery.take().unwrap_or_default();
            self.task_text_recovery = Some(format!("Call the advertised list_agents entrypoint with compact:false, status_only:false, the same path_prefix and no since_revision to retrieve current task messages and completed reports. Use completed_report_recovery for the exact omitted report snapshot. A changed/unknown revision returns a replacement snapshot for the requested view; unchanged:true preserves the prior visible snapshot, including its failures. Hidden report recovery can change independently.{failure}"));
        }
        // Hash only the delivered view. Omitted report bytes have their own
        // canonical recovery identity, which is returned even on unchanged
        // views. Retention failures leave full reports inline and therefore
        // still invalidate this fingerprint. This is never a freshness witness.
        let bytes = serde_json::to_vec(&json!([
            root, args.path_prefix, args.compact, args.status_only, self.agents, self.typed_tasks,
            self.unresolved_descendants, self.task_text_recovery,
        ]))
        .map_err(|error| FunctionCallError::RespondToModel(format!("list_agents: snapshot serialization failed: {error}")))?;
        self.revision = hex::encode(sha2::Sha256::digest(bytes));
        if args.since_revision.as_ref() == Some(&self.revision) {
            self.unchanged = true;
            self.agents.clear();
            self.typed_tasks.clear();
            self.unresolved_descendants.clear();
        }
        Ok(())
    }

    fn completed_report_snapshot(&self) -> Option<JsonValue> {
        let completed_reports = self.agents.iter().filter(|agent| matches!(
            &agent.agent_status,
            ListedAgentStatus::Full(AgentStatus::Completed(_) | AgentStatus::CompletedWithSurface { .. })
        )).map(|agent| json!({
            "agent_name": agent.agent_name,
            "agent_status": agent.agent_status,
        })).collect::<Vec<_>>();
        (!completed_reports.is_empty()).then(|| json!({
            "agents": completed_reports,
        }))
    }

    fn omit_completed_reports(&mut self) {
        for agent in &mut self.agents {
            // Opt in only: preserve errors and capacity-stop reports, and never
            // fabricate an empty authoritative surfaced value.
            let status = match &agent.agent_status {
                ListedAgentStatus::Full(AgentStatus::Completed(_)) => {
                    Some(CompletedAgentStatus::Completed)
                }
                ListedAgentStatus::Full(AgentStatus::CompletedWithSurface { .. }) => {
                    Some(CompletedAgentStatus::CompletedWithSurface)
                }
                _ => None,
            };
            if let Some(status) = status {
                agent.agent_status = ListedAgentStatus::StatusOnly(status);
            }
        }
    }
}

fn agent_path_matches_prefix(agent_path: &str, prefix: &AgentPath) -> bool {
    prefix.is_root()
        || agent_path == prefix.as_str()
        || agent_path.starts_with(&format!("{}/", prefix.as_str()))
}

impl ToolOutput for ListAgentsResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "list_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        let mut metadata = crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)?;
        metadata.essential_inline["revision"] = json!(self.revision);
        metadata.essential_inline["unchanged"] = json!(self.unchanged);
        if let Some(recovery) = &self.task_text_recovery {
            metadata.essential_inline["task_text_recovery"] = json!(recovery);
        }
        if let Some(recovery) = &self.completed_report_recovery {
            metadata.essential_inline["completed_report_recovery"] = recovery.clone();
        }
        Some(metadata)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}
