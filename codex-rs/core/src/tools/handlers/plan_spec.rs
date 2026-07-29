use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_update_plan_tool() -> ToolSpec {
    let plan_item_properties = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "Stable step id used by dependencies and durable task evidence.".to_string(),
            )),
        ),
        (
            "step".to_string(),
            JsonSchema::string(Some("Task step text.".to_string())),
        ),
        (
            "status".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("pending"),
                    json!("in_progress"),
                    json!("implemented"),
                    json!("passed"),
                    json!("blocked"),
                    json!("skipped"),
                    json!("completed"),
                ],
                Some(
                    "Step status. `passed` is the caller's explicit completion acknowledgement; `completed` is a legacy alias canonicalized to `passed`. Later file or command mutations reopen passed work as implemented."
                        .to_string(),
                ),
            ),
        ),
        (
            "depends_on".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Stable prerequisite step id.".to_string())),
                Some("Step ids that must pass or be skipped first.".to_string()),
            ),
        ),
        (
            "acceptance_criteria".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("A concrete acceptance criterion.".to_string())),
                Some("Evidence-backed acceptance criteria for this step.".to_string()),
            ),
        ),
        (
            "runtime_paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Intended runtime or call-site path.".to_string())),
                Some("Runtime paths this step must reach.".to_string()),
            ),
        ),
        (
            "generated_artifacts".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some(
                    "Required repository-relative generated artifact path.".to_string(),
                )),
                Some(
                    "Repository-relative generated artifacts that must remain inside the repository and currently exist, be readable, and be hashable for completion."
                        .to_string(),
                ),
            ),
        ),
        (
            "risks".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Known unresolved risk.".to_string())),
                Some("Risks that must remain visible in durable evidence.".to_string()),
            ),
        ),
        (
            "requires_desktop_activation".to_string(),
            JsonSchema::boolean(Some(
                "Require a fresh Desktop runtime activation receipt before passing."
                    .to_string(),
            )),
        ),
    ]);

    let properties = BTreeMap::from([
        (
            "explanation".to_string(),
            JsonSchema::string(Some(
                "Optional explanation for this plan update.".to_string(),
            )),
        ),
        (
            "plan".to_string(),
            JsonSchema::array(
                JsonSchema::object(
                    plan_item_properties,
                    Some(vec!["step".to_string(), "status".to_string()]),
                    Some(false.into()),
                ),
                Some("The list of steps".to_string()),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: r#"Updates the task plan.
Provide an optional explanation and a list of plan items, each with a step and status.
At most one step can be in_progress at a time.
Use stable ids, dependencies, and acceptance criteria for implementation work. Editing can make a
step implemented. Set a step to passed only when its acceptance criteria are satisfied; completion
still checks declared artifacts, Desktop activation, plan structure, and blocking risks.
"#
        .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["plan".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
