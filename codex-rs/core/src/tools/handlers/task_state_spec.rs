use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_task_state_tool() -> ToolSpec {
    let string_array = |description: &str| {
        JsonSchema::array(JsonSchema::string(None), Some(description.to_string()))
    };
    let properties = BTreeMap::from([
        (
            "operation".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("classify"),
                    json!("submit_investigation_checkpoint"),
                    json!("submit_closure"),
                    json!("inspect_status"),
                ],
                Some("Explicit lifecycle operation.".to_string()),
            ),
        ),
        (
            "exhaustive".to_string(),
            JsonSchema::boolean(Some(
                "Whether the task requires an exhaustive read-only investigation checkpoint."
                    .to_string(),
            )),
        ),
        (
            "risk_domains".to_string(),
            string_array("Risk domains used by the runtime to determine review policy."),
        ),
        (
            "supported_non_git_roots".to_string(),
            string_array("Explicitly supported non-Git local mutation roots."),
        ),
        (
            "summary".to_string(),
            JsonSchema::string(Some("Investigation checkpoint summary.".to_string())),
        ),
        (
            "paths_reviewed".to_string(),
            string_array("Paths reviewed during exhaustive investigation."),
        ),
        (
            "path_review".to_string(),
            string_array("Fresh post-mutation path review evidence."),
        ),
        (
            "competing_paths_checked".to_string(),
            string_array("Competing or replaced runtime paths checked."),
        ),
        (
            "validation_receipt_ids".to_string(),
            string_array("Durable validation receipt identifiers."),
        ),
        (
            "runtime_evidence".to_string(),
            string_array(
                "Successful command receipt identifiers returned by inspect_status that prove applicable runtime or wiring behavior.",
            ),
        ),
        (
            "missing_requirement_ids".to_string(),
            string_array("Stable identifiers for missing correctness evidence."),
        ),
        (
            "actionable_findings".to_string(),
            string_array("Defects that can still be repaired in this task."),
        ),
        (
            "blocked_reasons".to_string(),
            string_array("Unavailable access, external state, ownership, or user-action blockers."),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "task_state".to_string(),
        description: "Classify a task before any mutation, submit an exhaustive investigation checkpoint, submit fresh post-mutation closure evidence, or inspect runtime-computed lifecycle status and usable receipt IDs. The runtime owns phase transitions, revisions, drift checks, review policy, and completion outcomes. Closing is atomic; actionable findings return to Fixing.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["operation".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
