use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const WAIT_FOR_ENVIRONMENT_TOOL_NAME: &str = "wait_for_environment";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForEnvironmentArgs {
    environment_id: Option<String>,
}

pub(crate) struct WaitForEnvironmentHandler;

fn resolve_environment_id<'a>(
    requested: Option<String>,
    starting: impl IntoIterator<Item = &'a str>,
) -> Result<String, FunctionCallError> {
    if let Some(environment_id) = requested {
        return Ok(environment_id);
    }
    let mut starting = starting.into_iter();
    match (starting.next(), starting.next()) {
        (Some(environment_id), None) => Ok(environment_id.to_string()),
        (None, _) => Err(FunctionCallError::RespondToModel(
            "No environment is starting; provide environment_id to check a ready environment."
                .to_string(),
        )),
        _ => Err(FunctionCallError::RespondToModel(
            "Multiple environments are starting; provide environment_id to select one.".to_string(),
        )),
    }
}

impl ToolExecutor<ToolInvocation> for WaitForEnvironmentHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(WAIT_FOR_ENVIRONMENT_TOOL_NAME)
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // Readiness waits observe an already-started environment; they do not
        // mutate the workspace or need the global exclusive admission lease.
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: WAIT_FOR_ENVIRONMENT_TOOL_NAME.to_string(),
            description: "Wait for environment readiness only. Filesystem tools already wait internally for their selected environment: call the desired available operation directly instead of waiting first. Use this tool for readiness-only requests or when the desired tool surface is unavailable."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "environment_id".to_string(),
                    JsonSchema::string(Some(
                        "The id of a starting or ready environment. Omit when exactly one environment is starting.".to_string(),
                    )),
                )]),
                /*required*/ None,
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                payload,
                step_context,
                cancellation_token,
                ..
            } = invocation;
            let arguments = match payload {
                ToolPayload::Function { arguments } => arguments,
                _ => {
                    return Err(FunctionCallError::Fatal(format!(
                        "{WAIT_FOR_ENVIRONMENT_TOOL_NAME} handler received unsupported payload"
                    )));
                }
            };
            let args: WaitForEnvironmentArgs = parse_arguments(&arguments)?;
            let environment_id = resolve_environment_id(
                args.environment_id,
                step_context
                    .environments
                    .starting
                    .iter()
                    .map(|environment| environment.selection.environment_id.as_str()),
            )?;
            if let Err(error) = super::wait_for_tool_environment(
                &step_context.environments,
                Some(&environment_id),
                &cancellation_token,
            )
            .await
            {
                return Ok(boxed_tool_output(FunctionToolOutput::from_text(
                    format!("Environment `{environment_id}` is unavailable: {error}"),
                    Some(false),
                )));
            }

            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "environment_id": environment_id,
                "status": "ready",
            }))))
        })
    }
}

impl CoreToolRuntime for WaitForEnvironmentHandler {}

