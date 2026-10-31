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
            Some("Use exact IDs from the previous result's step_ids or durable lineage, not invented labels such as step-1. These carry forward unfinished requirements, including splits/merges; a single one-to-one continuation preserves identity.".into()))),
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
    let reference = JsonSchema::object(BTreeMap::from([
        ("input_id".into(), JsonSchema::string(None)),
        ("sha256".into(), JsonSchema::string(None)),
    ]), Some(vec!["input_id".into(), "sha256".into()]), Some(false.into()));
    let mut pins = JsonSchema::array(JsonSchema::object(BTreeMap::from([
        ("text".into(), JsonSchema::string(Some("Original requirement text; preserved independently of checklist rows.".into()))),
        ("source".into(), reference.clone()),
    ]), Some(vec!["text".into()]), Some(false.into())), Some(
        "Add requirements without replacing any checklist or outstanding scope. Source defaults to the latest accepted user text; explicit input_id/sha256 must authenticate against accepted input. Repeating the same text/source preserves its stable ID and state.".into()));
    pins.min_items = Some(1);
    let properties = BTreeMap::from([
        ("plan".into(), JsonSchema::array(item,
            Some("Replace the checklist. Rows may be renamed or removed, but unfinished requirements must be retained via continues or retired with accepted user authorization.".into()))),
        ("set".into(), updates),
        ("pin".into(), pins),
        ("explanation".into(), JsonSchema::string(None)),
        ("expected_revision".into(), JsonSchema::string(Some(
            "Revision returned by the last update. Required after the plan changed since this sampling step; rejects stale updates without mutation.".into()))),
        ("superseded".into(), JsonSchema::array(JsonSchema::object(BTreeMap::from([
            ("step_id".into(), JsonSchema::string(None)),
            ("reason".into(), JsonSchema::string(None)),
        ]), Some(vec!["step_id".into(), "reason".into()]), Some(false.into())),
            Some("Unfinished removed steps/orphan requirements retired by a user scope change. Each needs an authentic input citation.".into()))),
        ("scope_change_instructions".into(), JsonSchema {
            description: Some("Map superseded IDs to exact accepted user text authorizing the scope change; explanation alone is not authorization.".into()),
            ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::string(None).into()))
        }),
        ("scope_change_refs".into(), JsonSchema {
            description: Some("Compact alternative to exact text: map superseded IDs to authentic input_id/sha256 references returned on reconciliation.".into()),
            ..JsonSchema::object(BTreeMap::new(), None, Some(reference.into()))
        }),
        ("scope_change_reviewed_revision".into(), JsonSchema::string(Some(
            "When citing earlier input, reconcile all later user instructions and supply the input revision returned on reconciliation.".into()))),
        ("resolve".into(), JsonSchema::array(JsonSchema::string(None), Some(
            "Explicitly complete original or pinned requirement IDs; every mapped descendant must be completed. For an unmapped pin, submit the unchanged plan plus resolve. Not validation receipts; summary/checklist rewrites alone never complete pins.".into()))),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".into(),
        description: format!("{}\n\n{}", "Update a progress checklist or pin task requirements through its existing owner. Unavailable in Plan mode. Use exactly one of plan, set or pin; at most one checklist step may be in_progress. Pins survive compaction with stable IDs, authentic input provenance and explicit states; they do not discard other requirements. Checklist statuses are model reports, not proof of task correctness.", include_str!("../../../templates/plan_progress.md").trim()),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(["plan", "set", "pin"].into_iter().map(|key| JsonSchema {
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
                "lineage_complete":{"const":false,"description":"This compact model result omits durable requirement lineage and is not a complete replay snapshot."},
                "remaining_requirements":{"type":"object","additionalProperties":{"type":"string"}},
                "pinned_requirements":{"type":"object","additionalProperties":{
                    "type":"object","properties":{
                        "text":{"type":"string"},
                        "state":{"enum":["pending","in_progress","completed","superseded"]},
                        "source":{"type":"object","properties":{
                            "input_id":{"type":"string"},"sha256":{"type":"string"}
                        },"required":["input_id","sha256"],"additionalProperties":false}
                    },"required":["text","state","source"],"additionalProperties":false
                }},
                "obligations":{"type":"object","description":"Derived from the existing requirement owner. Summary rewrites cannot close these IDs; use an explicit checklist completion/resolve or authorized supersession.","properties":{
                    "completed":{"type":"integer","minimum":0},
                    "superseded":{"type":"integer","minimum":0},
                    "unresolved":{"type":"array","items":{"type":"string"}}
                },"required":["completed","superseded","unresolved"],"additionalProperties":false},

            },
            "required":["current_plan","revision","step_ids","message","effect","no_progress","completion_authority","lineage_complete","obligations"],
            "additionalProperties":false
        }).into()),
    })
}
