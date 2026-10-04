use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::request_user_input_spec::REQUEST_USER_INPUT_TOOL_NAME;
use crate::tools::handlers::request_user_input_spec::create_request_user_input_tool;
use crate::tools::handlers::request_user_input_spec::normalize_request_user_input_args;
use crate::tools::handlers::request_user_input_spec::request_user_input_tool_description;
use crate::tools::handlers::request_user_input_spec::request_user_input_unavailable_message;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use codex_protocol::config_types::ModeKind;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_tools::JsonToolOutput;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::sync::Arc;

pub struct RequestUserInputHandler {
    pub available_modes: Vec<ModeKind>,
}

impl ToolExecutor<ToolInvocation> for RequestUserInputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_request_user_input_tool(request_user_input_tool_description(&self.available_modes))
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl RequestUserInputHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            call_id,
            payload,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{REQUEST_USER_INPUT_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        if turn.session_source.is_non_root_agent() {
            return Err(FunctionCallError::RespondToModel(
                "request_user_input can only be used by the root thread".to_string(),
            ));
        }

        let mode = session.collaboration_mode().await.mode;
        if let Some(message) = request_user_input_unavailable_message(mode, &self.available_modes) {
            return Err(FunctionCallError::RespondToModel(message));
        }

        let args = parse_user_input_with_header_fallback(&arguments)?;
        let args =
            normalize_request_user_input_args(args).map_err(FunctionCallError::RespondToModel)?;
        let response = session
            .request_user_input(turn.as_ref(), call_id, args)
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "{REQUEST_USER_INPUT_TOOL_NAME} was cancelled before receiving a response"
                ))
            })?;

        let content = serde_json::to_value(&response).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to serialize {REQUEST_USER_INPUT_TOOL_NAME} response: {err}"
            ))
        })?;

        Ok(boxed_tool_output(JsonToolOutput::new(content)))
    }
}

/// A display label must not buy a model round to regenerate the whole question.
/// Keep the public protocol strict; repair only this tool's presentation field,
/// preserving its complete text in the question before validating again.
fn parse_user_input_with_header_fallback(
    arguments: &str,
) -> Result<RequestUserInputArgs, FunctionCallError> {
    let original_error = match parse_arguments::<RequestUserInputArgs>(arguments) {
        Ok(args) => return Ok(args),
        Err(error) => error,
    };
    let mut value: serde_json::Value = parse_arguments(arguments)?;
    let Some(questions) = value.get_mut("questions").and_then(serde_json::Value::as_array_mut) else {
        return Err(original_error);
    };
    let mut repaired = false;
    for question in questions {
        let (Some(header), Some(prompt)) = (
            question.get("header").and_then(serde_json::Value::as_str),
            question.get("question").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        if header.chars().nth(12).is_none() {
            continue;
        }
        let short = format!("{}…", header.chars().take(11).collect::<String>());
        let full_prompt = format!("{header}\n\n{prompt}");
        question["header"] = short.into();
        question["question"] = full_prompt.into();
        repaired = true;
    }
    if !repaired {
        return Err(original_error);
    }
    // Never relax IDs, choice counts, unknown fields, or approval semantics.
    serde_json::from_value(value).map_err(|error| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {error}"))
    })
}

impl CoreToolRuntime for RequestUserInputHandler {
    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        ToolExecutionTiming::Interactive
    }
}

#[cfg(test)]
#[path = "request_user_input_tests.rs"]
mod tests;
