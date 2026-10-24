use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_update_plan_tool() -> ToolSpec {
    let status = || JsonSchema::string_enum(
        vec![json!("pending"), json!("in_progress"), json!("completed")], None,
    );
    let item = JsonSchema::object(BTreeMap::from([
        ("step".into(), JsonSchema::string(Some("Task step text.".into()))),
        ("status".into(), status()),
        ("continues".into(), JsonSchema::array(JsonSchema::string(None),
            Some("Optional single prior step ID to preserve identity when renaming; not an obligation link.".into()))),
    ]), Some(vec!["step".into(), "status".into()]), Some(false.into()));
    let mut updates = JsonSchema::array(JsonSchema {
        one_of: Some(["index", "step_id"].into_iter().map(|key| JsonSchema {
            required: Some(vec![key.into()]),
            ..JsonSchema::object(BTreeMap::new(), None, None)
        }).collect()),
        ..JsonSchema::object(BTreeMap::from([
            ("index".into(), JsonSchema { minimum: Some(0.into()), ..JsonSchema::integer(None) }),
            ("step_id".into(), JsonSchema::string(Some("Full stable step ID or unique prefix of at least eight characters.".into()))),
            ("status".into(), status()),
        ]), Some(vec!["status".into()]), Some(false.into()))
    }, Some("Update statuses without replacing other rows. Provide exactly one of index or step_id per entry.".into()));
    updates.min_items = Some(1);
    let properties = BTreeMap::from([
        ("plan".into(), JsonSchema::array(item,
            Some("Replace the checklist. Rows may be renamed or removed; the user request, not this checklist, defines scope.".into()))),
        ("set".into(), updates),
        ("explanation".into(), JsonSchema::string(None)),
        ("expected_revision".into(), JsonSchema::string(Some(
            "Revision returned by the last update. Required after the plan changed since this sampling step; rejects stale updates without mutation.".into()))),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".into(),
        description: "Update a progress checklist for multi-step work. Use plan or set, not both; at most one step may be in_progress. Checklist statuses are model reports, not proof of task correctness.".into(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(["plan", "set"].into_iter().map(|key| JsonSchema {
                required: Some(vec![key.into()]),
                ..JsonSchema::object(BTreeMap::new(), None, None)
            }).collect()),
            ..JsonSchema::object(properties, None, Some(false.into()))
        },
        output_schema: Some(json!({
            "type":"object",
            "properties":{
                "current_plan":{"type":"object","properties":{
                    "explanation":{"type":["string","null"]},
                    "plan":{"type":"array","items":{"type":"object","properties":{
                        "step":{"type":"string"},
                        "status":{"enum":["pending","in_progress","completed"]}
                    },"required":["step","status"],"additionalProperties":false}}
                },"required":["explanation","plan"],"additionalProperties":false},
                "revision":{"type":"string"},
                "step_ids":{"type":"array","items":{"type":"string"}},
                "message":{"type":"string"},
                "effect":{"enum":["initial","structural_revision","status_only","no_op"]},
                "no_progress":{"type":"boolean"},
                "completion_authority":{"const":"checklist_only"},

            },
            "required":["current_plan","revision","step_ids","message","effect","no_progress","completion_authority"],
            "additionalProperties":false
        }).into()),
    })
}
