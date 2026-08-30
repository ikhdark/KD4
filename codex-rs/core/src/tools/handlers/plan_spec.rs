use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_update_plan_tool() -> ToolSpec {
    let plan_item = JsonSchema::object(
        BTreeMap::from([
            (
                "step".to_string(),
                JsonSchema::string(Some("Task step text.".to_string())),
            ),
            (
                "status".to_string(),
                JsonSchema::string_enum(
                    vec![json!("pending"), json!("in_progress"), json!("completed")],
                    Some("Current step status.".to_string()),
                ),
            ),
        ]),
        Some(vec!["step".to_string(), "status".to_string()]),
        Some(false.into()),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: "Updates the task checklist. At most one step can be in_progress at a time."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "explanation".to_string(),
                    JsonSchema::string(Some("Optional explanation for this update.".to_string())),
                ),
                (
                    "plan".to_string(),
                    JsonSchema::array(plan_item, Some("Complete task checklist.".to_string())),
                ),
            ]),
            Some(vec!["plan".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
