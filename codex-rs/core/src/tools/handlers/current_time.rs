use crate::FunctionCallError;
use crate::context::CurrentTimeReminder;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

const NAMESPACE: &str = "clock";
const TOOL_NAME: &str = "curr_time";

pub struct CurrentTimeHandler;

impl ToolExecutor<ToolInvocation> for CurrentTimeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Namespace(ResponsesApiNamespace {
            name: NAMESPACE.to_string(),
            description: "Tools for reading and waiting on time.".to_string(),
            tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
                name: TOOL_NAME.to_string(),
                description: "Return the current time in UTC.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::new(),
                    /*required*/ None,
                    /*additional_properties*/ Some(false.into()),
                ),
                output_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "current_time": {
                            "type": "string",
                            "description": "Current UTC time formatted as YYYY-MM-DD HH:MM:SS UTC."
                        }
                    },
                    "required": ["current_time"],
                    "additionalProperties": false
                }).into()),
            })],
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            if !matches!(invocation.payload, ToolPayload::Function { .. }) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            }

            let current_time = invocation
                .session
                .services
                .time_provider
                .current_time(invocation.session.thread_id)
                .await
                .map_err(|err| {
                    FunctionCallError::Fatal(format!("failed to read current time: {err:#}"))
                })?;
            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "current_time": CurrentTimeReminder::new(current_time).formatted_time(),
            }))))
        })
    }
}

impl CoreToolRuntime for CurrentTimeHandler {}
