use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const READ_TOOL_OUTPUT_TOOL_NAME: &str = "read_tool_output";

pub(crate) fn create_read_tool_output_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: READ_TOOL_OUTPUT_TOOL_NAME.to_string(),
        description: "Read a bounded line range from an opaque command-output artifact retained for the current task. Artifact IDs come from command results; filesystem paths are not accepted."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "artifact_id".to_string(),
                    JsonSchema::string(Some(
                        "Opaque UUID returned by a command tool.".to_string(),
                    )),
                ),
                (
                    "start_line".to_string(),
                    JsonSchema::integer(Some(
                        "First 1-based line to return. Defaults to 1.".to_string(),
                    )),
                ),
                (
                    "end_line".to_string(),
                    JsonSchema::integer(Some(
                        "Inclusive last line to return. Defaults to start_line + 199; the span may not exceed 2000 lines."
                            .to_string(),
                    )),
                ),
                (
                    "max_bytes".to_string(),
                    JsonSchema::integer(Some(
                        "Maximum output bytes to retain, from 1 through 16384. Defaults to 16384."
                            .to_string(),
                    )),
                ),
            ]),
            Some(vec!["artifact_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
