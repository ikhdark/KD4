use crate::function_tool::FunctionCallError;
use crate::tools::command_output_artifact::read_tool_output_artifact;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::read_tool_output_spec::READ_TOOL_OUTPUT_TOOL_NAME;
use crate::tools::handlers::read_tool_output_spec::create_read_tool_output_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

const DEFAULT_MAX_BYTES: usize = 16_384;
const DEFAULT_LINE_COUNT: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadToolOutputArgs {
    artifact_id: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub struct ReadToolOutputHandler;

impl ToolExecutor<ToolInvocation> for ReadToolOutputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TOOL_OUTPUT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_read_tool_output_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_read_tool_output(invocation))
    }
}

impl CoreToolRuntime for ReadToolOutputHandler {}

async fn handle_read_tool_output(
    invocation: ToolInvocation,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let ToolPayload::Function { ref arguments } = invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "read_tool_output received unsupported payload".to_string(),
        ));
    };
    let args: ReadToolOutputArgs = serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to parse read_tool_output arguments: {err}"
        ))
    })?;
    let max_bytes = resolved_max_bytes(args.max_bytes)?;
    let (start_line, end_line) = resolved_line_range(&args)?;
    let output = read_tool_output_artifact(
        invocation.turn.config.codex_home.as_path(),
        &invocation.session.thread_id.to_string(),
        &args.artifact_id,
        start_line,
        end_line,
        max_bytes,
    )
    .await
    .map_err(|err| FunctionCallError::RespondToModel(err.for_model()))?;

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        output,
        Some(true),
    )))
}

fn resolved_line_range(args: &ReadToolOutputArgs) -> Result<(usize, usize), FunctionCallError> {
    let start_line = args.start_line.unwrap_or(1);
    let end_line = match args.end_line {
        Some(end_line) => end_line,
        None => start_line
            .checked_add(DEFAULT_LINE_COUNT - 1)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel("start_line is too large".to_string())
            })?,
    };
    Ok((start_line, end_line))
}

fn resolved_max_bytes(max_bytes: Option<usize>) -> Result<usize, FunctionCallError> {
    match max_bytes {
        Some(max_bytes) if max_bytes == 0 || max_bytes > DEFAULT_MAX_BYTES => {
            Err(FunctionCallError::RespondToModel(format!(
                "max_bytes must be between 1 and {DEFAULT_MAX_BYTES}"
            )))
        }
        Some(max_bytes) => Ok(max_bytes),
        None => Ok(DEFAULT_MAX_BYTES),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_range_is_exactly_two_hundred_lines() {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            start_line: Some(17),
            end_line: None,
            max_bytes: None,
        };
        assert_eq!(resolved_line_range(&args).unwrap(), (17, 216));
    }

    #[test]
    fn max_bytes_is_hard_limited_to_sixteen_kibibytes() {
        assert_eq!(resolved_max_bytes(None).unwrap(), 16_384);
        assert_eq!(resolved_max_bytes(Some(1)).unwrap(), 1);
        assert_eq!(resolved_max_bytes(Some(16_384)).unwrap(), 16_384);
        for invalid in [0, 16_385, usize::MAX] {
            let error = resolved_max_bytes(Some(invalid)).unwrap_err();
            assert_eq!(
                error,
                FunctionCallError::RespondToModel(
                    "max_bytes must be between 1 and 16384".to_string()
                )
            );
        }
    }
}
