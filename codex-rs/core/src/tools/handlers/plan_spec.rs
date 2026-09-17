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
        description: "Updates the task checklist for work with multiple substantive steps. Skip plans for straightforward work; do not create single-step plans. At most one step can be in_progress at a time."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "explanation".to_string(),
                    JsonSchema::string(Some("Optional explanation for this update, including blockers or cancelled work. Do not mark unfinished work completed.".to_string())),
                ),
                (
                    "plan".to_string(),
                    JsonSchema::array(plan_item.clone(), Some("Complete task checklist, replacing the previous plan. Omitted steps are removed; include every step you want to retain.".to_string())),
                ),
            ]),
            Some(vec!["plan".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "current_plan": {
                    "type": "object",
                    "description": "The complete stored checklist after this update.",
                    "properties": {
                        "explanation": { "type": ["string", "null"] },
                        "plan": { "type": "array", "items": plan_item }
                    },
                    "required": ["explanation", "plan"],
                    "additionalProperties": false
                },
                "message": { "type": "string" },
                "effect": {
                    "type": "string",
                    "enum": ["initial", "structural_revision", "status_only", "no_op"],
                    "description": "Whether this created the first plan, revised its step text or order, changed only statuses or explanation, or left the plan unchanged."
                },
                "no_progress": {
                    "type": "boolean",
                    "description": "True when this update left the stored plan unchanged; this does not assess progress on the underlying work."
                }
            },
            "required": ["current_plan", "message", "effect", "no_progress"],
            "additionalProperties": false
        })),
    })
}
