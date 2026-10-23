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
        let agents = session
            .services
            .agent_control
            .list_agents(&turn.session_source, args.path_prefix.as_deref())
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
                let task = match coordinator
                    .get_agent_task(binding.assignment_id, Some(0))
                    .await
                {
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
            agents,
            typed_tasks,
            unresolved_descendants: coordinator.unavailable_descendants(),
            revision: String::new(),
            unchanged: false,
            task_text_recovery: None,
        };
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
    since_revision: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    agents: Vec<ListedAgent>,
    typed_tasks: Vec<JsonValue>,
    unresolved_descendants: Vec<(String, String)>,
    revision: String,
    unchanged: bool,
    task_text_recovery: Option<String>,
}

impl ListAgentsResult {
    fn apply_view(&mut self, args: &ListAgentsArgs, root: &str) -> Result<(), FunctionCallError> {
        use sha2::Digest;
        // Hash the complete fresh projection, before removing task prose. Lease
        // expiry, a changed answer, deletion, failed delivery and attempt/gate
        // transitions must invalidate an unchanged response. This is a display
        // fingerprint, never a task-store or source-freshness witness.
        let bytes = serde_json::to_vec(&json!([
            root, args.path_prefix, args.compact, self.agents, self.typed_tasks,
            self.unresolved_descendants,
        ]))
        .map_err(|error| FunctionCallError::RespondToModel(format!("list_agents: snapshot serialization failed: {error}")))?;
        self.revision = hex::encode(sha2::Sha256::digest(bytes));
        if args.since_revision.as_ref() == Some(&self.revision) {
            self.unchanged = true;
            self.agents.clear();
            self.typed_tasks.clear();
            self.unresolved_descendants.clear();
        } else if args.compact {
            for agent in &mut self.agents {
                agent.last_task_message = None;
            }
            // Keep lifecycle status (including final results), failures,
            // blockers and required actions intact even in the compact view.
            self.task_text_recovery = Some("Call list_agents with compact:false and the same path_prefix to retrieve task messages. A changed/unknown revision returns a full replacement snapshot; unchanged:true preserves the prior snapshot, including its failures.".into());
        }
        Ok(())
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
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}
#[cfg(test)]
mod view_tests {
    use super::*;

    fn snapshot() -> ListAgentsResult {
        ListAgentsResult {
            agents: vec![ListedAgent {
                agent_name: "/worker".into(), agent_status: AgentStatus::Completed(Some("retained final answer".into())),
                last_task_message: Some("λ task instructions ".repeat(500)), runtime_loaded: false,
            }],
            typed_tasks: vec![json!({"assignment_id":"assignment", "attempt_id":"attempt-one",
                "workspace_status":{"lease_state":"active", "pending_gates":["review"], "next_required_action":"resolve review"}})],
            unresolved_descendants: vec![("/unavailable".into(), "delivery failed".into())],
            revision: String::new(), unchanged: false, task_text_recovery: None,
        }
    }

    #[test]
    fn compact_and_unchanged_views_preserve_failures_results_and_exact_full_recovery() {
        let mut compact = snapshot();
        let args = ListAgentsArgs { path_prefix: None, compact: true, since_revision: None };
        compact.apply_view(&args, "root").unwrap();
        assert!(compact.agents[0].last_task_message.is_none());
        assert_eq!(compact.agents[0].agent_status, AgentStatus::Completed(Some("retained final answer".into())));
        assert_eq!(compact.typed_tasks[0]["workspace_status"]["next_required_action"], "resolve review");
        assert_eq!(compact.unresolved_descendants[0].1, "delivery failed");
        assert!(compact.task_text_recovery.as_deref().unwrap().contains("compact:false"));
        let mut unchanged = snapshot();
        unchanged.apply_view(&ListAgentsArgs { since_revision: Some(compact.revision.clone()), ..args }, "root").unwrap();
        assert!(unchanged.unchanged);
        assert!(unchanged.agents.is_empty() && unchanged.typed_tasks.is_empty());
        let mut full = snapshot();
        full.apply_view(&ListAgentsArgs { path_prefix: None, compact: false, since_revision: Some(compact.revision) }, "root").unwrap();
        assert!(!full.unchanged, "changing view requires a replacement snapshot");
        assert_eq!(full.agents[0].last_task_message, snapshot().agents[0].last_task_message);
        assert!(serde_json::to_vec(&full).unwrap().len() > serde_json::to_vec(&unchanged).unwrap().len() * 4);
    }

    #[test]
    fn changes_deletions_attempts_leases_and_required_actions_invalidate_display_identity() {
        let mut original = snapshot();
        let args = || ListAgentsArgs { path_prefix: None, compact: true, since_revision: None };
        original.apply_view(&args(), "root").unwrap();
        for change in 0..6 {
            let mut changed = snapshot();
            match change {
                0 => changed.agents.clear(),
                1 => changed.agents[0].agent_status = AgentStatus::Completed(Some("new answer".into())),
                2 => changed.typed_tasks[0]["attempt_id"] = json!("attempt-two"),
                3 => changed.typed_tasks[0]["workspace_status"]["lease_state"] = json!("expired"),
                4 => changed.typed_tasks[0]["workspace_status"]["next_required_action"] = json!("repair failure"),
                _ => changed.unresolved_descendants.push(("/another".into(), "new delivery failure".into())),
            }
            changed.apply_view(&ListAgentsArgs { since_revision: Some(original.revision.clone()), ..args() }, "root").unwrap();
            assert!(!changed.unchanged, "change {change} must return a complete replacement");
        }
        for (root, prefix) in [("other root", None), ("root", Some("/worker".into()))] {
            let mut scoped = snapshot();
            scoped.apply_view(&ListAgentsArgs { path_prefix: prefix, since_revision: Some(original.revision.clone()), ..args() }, root).unwrap();
            assert!(!scoped.unchanged);
        }
    }
}
