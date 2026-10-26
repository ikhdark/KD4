use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::adaptive_output_budget_description;
use std::collections::BTreeMap;

pub(crate) fn create_wait_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "cell_id".to_string(),
            JsonSchema::string(Some("Identifier of the running exec cell.".to_string())),
        ),
        (
            "max_tokens".to_string(),
            JsonSchema {
                minimum: Some(serde_json::Number::from(0_u64)),
                ..JsonSchema::integer(Some(format!(
                    "Cell output budget from this wait onward (omit to keep the cell's current budget). {}.",
                    adaptive_output_budget_description()
                )))
            },
        ),
        (
            "terminate".to_string(),
            JsonSchema::boolean(Some(
                "True stops the running exec cell; false or omitted waits for output.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: codex_code_mode::WAIT_TOOL_NAME.to_string(),
        description: format!(
            "Resumes a genuinely live `{}` cell and returns new output or completion.\n{}",
            codex_code_mode::PUBLIC_TOOL_NAME,
            codex_code_mode::build_wait_tool_description().trim()
        ),
        strict: false,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["cell_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
        defer_loading: None,
    })
}

