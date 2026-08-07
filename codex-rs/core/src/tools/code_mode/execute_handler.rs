use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::collections::HashSet;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::handle_runtime_response;
use super::is_exec_tool_name;

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    direct_nested_tool_specs: Vec<ToolSpec>,
    deferred_nested_tool_specs: Vec<ToolSpec>,
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(
        spec: ToolSpec,
        direct_nested_tool_specs: Vec<ToolSpec>,
        deferred_nested_tool_specs: Vec<ToolSpec>,
    ) -> Self {
        Self {
            spec,
            direct_nested_tool_specs,
            deferred_nested_tool_specs,
        }
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        code: String,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext { session, turn };
        let activated = exec.turn.activated_deferred_tools();
        let mut nested_tool_specs = self.direct_nested_tool_specs.clone();
        nested_tool_specs.extend(
            self.deferred_nested_tool_specs
                .iter()
                .filter_map(|spec| filter_deferred_spec(spec, &activated)),
        );
        let enabled_tools = codex_tools::collect_code_mode_tool_definitions(&nested_tool_specs);
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(codex_code_mode::ExecuteRequest {
                tool_call_id: call_id.clone(),
                enabled_tools,
                source: args.code.clone(),
                yield_time_ms: args.yield_time_ms,
                max_output_tokens: args.max_output_tokens,
            })
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        let runtime_cell_id = cell_id.to_string();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .start_code_cell_trace(
                exec.turn.sub_id.as_str(),
                runtime_cell_id.as_str(),
                call_id.as_str(),
                args.code.as_str(),
            );
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        let response = started_cell
            .initial_response()
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        code_cell_trace.record_initial_response(&response);
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        if !matches!(response, codex_code_mode::RuntimeResponse::Yielded { .. }) {
            code_cell_trace.record_ended(&response);
            exec.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&cell_id);
        }
        exec.session.services.elicitations.wait_until_clear().await;
        handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .await
            .map_err(FunctionCallError::RespondToModel)
    }
}

fn filter_deferred_spec(spec: &ToolSpec, activated: &HashSet<ToolName>) -> Option<ToolSpec> {
    match spec {
        ToolSpec::Namespace(namespace) => {
            let mut namespace = namespace.clone();
            let namespace_name = namespace.name.clone();
            namespace.tools.retain(|tool| match tool {
                codex_tools::ResponsesApiNamespaceTool::Function(tool) => activated.contains(
                    &ToolName::namespaced(namespace_name.clone(), tool.name.clone()),
                ),
            });
            (!namespace.tools.is_empty()).then_some(ToolSpec::Namespace(namespace))
        }
        ToolSpec::Function(tool) => activated
            .contains(&ToolName::plain(tool.name.clone()))
            .then(|| spec.clone()),
        ToolSpec::Freeform(tool) => activated
            .contains(&ToolName::plain(tool.name.clone()))
            .then(|| spec.clone()),
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => activated
            .contains(&ToolName::plain(spec.name()))
            .then(|| spec.clone()),
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(session, turn, call_id, input)
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_tools::JsonSchema;
    use codex_tools::ResponsesApiNamespace;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::ResponsesApiTool;
    use std::collections::BTreeMap;

    fn function(name: &str) -> ResponsesApiTool {
        ResponsesApiTool {
            name: name.to_string(),
            description: format!("{name} description"),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
            output_schema: None,
        }
    }

    #[test]
    fn deferred_namespace_exposes_only_current_turn_activations() {
        let spec = ToolSpec::Namespace(ResponsesApiNamespace {
            name: "example".to_string(),
            description: "example tools".to_string(),
            tools: vec![
                ResponsesApiNamespaceTool::Function(function("first")),
                ResponsesApiNamespaceTool::Function(function("second")),
            ],
        });

        assert_eq!(filter_deferred_spec(&spec, &HashSet::new()), None);

        let activated = HashSet::from([ToolName::namespaced("example", "second")]);
        let ToolSpec::Namespace(filtered) =
            filter_deferred_spec(&spec, &activated).expect("selected namespace tool")
        else {
            panic!("expected namespace tool");
        };
        assert_eq!(filtered.tools.len(), 1);
        let ResponsesApiNamespaceTool::Function(tool) = &filtered.tools[0];
        assert_eq!(tool.name, "second");
    }

    #[test]
    fn deferred_plain_tool_requires_current_turn_activation() {
        let spec = ToolSpec::Function(function("inspect"));
        assert_eq!(filter_deferred_spec(&spec, &HashSet::new()), None);
        assert_eq!(
            filter_deferred_spec(&spec, &HashSet::from([ToolName::plain("inspect")])),
            Some(spec)
        );
    }
}
