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
        let args: ListAgentsArgs = parse_arguments(&arguments)?;
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
                    FunctionCallError::RespondToModel(format!(
                        "list_agents: durable typed-task state could not initialize: {error}"
                    ))
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
        let mut typed_tasks_truncated = false;
        if let (Some(store), Some(root_session_id)) =
            (coordinator.store(), coordinator.root_session_id())
        {
            let bindings = store
                .list_agent_task_bindings(root_session_id, None)
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "list_agents: durable typed-task state is unavailable: {error}"
                    ))
                })?;
            typed_tasks_truncated = false;
            for binding in bindings {
                if resolved_prefix
                    .as_ref()
                    .is_some_and(|prefix| !agent_path_matches_prefix(&binding.agent_path, prefix))
                {
                    continue;
                }
                let task = coordinator
                    .get_agent_task(binding.assignment_id, Some(0))
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "list_agents: assignment {} is unavailable: {error}",
                            binding.assignment_id
                        ))
                    })?;
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

        Ok(boxed_tool_output(ListAgentsResult {
            agents,
            typed_tasks,
            typed_tasks_truncated,
        }))
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
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    agents: Vec<ListedAgent>,
    typed_tasks: Vec<JsonValue>,
    typed_tasks_truncated: bool,
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
