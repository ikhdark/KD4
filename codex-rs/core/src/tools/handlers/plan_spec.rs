use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub fn create_update_plan_tool() -> ToolSpec {
    let item_properties = || {
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
        ])
    };
    let item_required = || Some(vec!["step".to_string(), "status".to_string()]);
    let plan_item = JsonSchema::object(item_properties(), item_required(), Some(false.into()));
    let mut revision_item_properties = item_properties();
    revision_item_properties.insert(
        "continues".to_string(),
        JsonSchema::array(
            JsonSchema::string(None),
            Some("Step IDs from the last result whose unfinished obligations this step carries forward after rewording, splitting, or merging. A step continuing unfinished work cannot be completed in the same update.".to_string()),
        ),
    );
    let revision_item =
        JsonSchema::object(revision_item_properties, item_required(), Some(false.into()));
    let superseded = JsonSchema::array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "step_id".to_string(),
                    JsonSchema::string(Some("ID of an unfinished step this revision removes.".to_string())),
                ),
                (
                    "reason".to_string(),
                    JsonSchema::string(Some("Why it is dropped, such as the user's authorization.".to_string())),
                ),
            ]),
            Some(vec!["step_id".to_string(), "reason".to_string()]),
            Some(false.into()),
        ),
        Some("Unfinished steps this plan revision intentionally drops; recorded in the stored explanation and persistent requirement lineage. Each unfinished step a revision removes must be continued or superseded, or nothing changes.".to_string()),
    );
    let mut status_updates = JsonSchema::array(
        JsonSchema {
            one_of: Some(["index", "step_id"].into_iter().map(|field| JsonSchema {
                required: Some(vec![field.to_string()]),
                ..JsonSchema::object(BTreeMap::new(), None, None)
            }).collect()),
            ..JsonSchema::object(
            BTreeMap::from([
                ("index".to_string(), JsonSchema {
                    minimum: Some(0.into()),
                    ..JsonSchema::integer(Some("Zero-based index in the latest current_plan.plan. Requires expected_revision; prefer step_id.".to_string()))
                }),
                ("step_id".to_string(), JsonSchema::string(Some(
                    "Stable ID from step_ids in the previous result; survives reordering and one-to-one continues renames. New split/merge steps get new IDs. Provide step_id or index, not both.".into()
                ))),
                ("status".to_string(), JsonSchema::string_enum(
                    vec![json!("pending"), json!("in_progress"), json!("completed")], None,
                )),
            ]),
            Some(vec!["status".to_string()]),
            Some(false.into()),
        )},
        Some("Change only these statuses atomically, preserving step text and omitted steps. Use either set or plan. Omitted explanation is preserved with set. Prefer step_id from the last result; index-based updates are rejected without expected_revision.".to_string()),
    );
    status_updates.min_items = Some(1);
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: "Updates the task checklist for work with multiple substantive dependent steps. Execute bounded read-only inventories directly. Skip plans for straightforward work; do not create single-step plans. At most one step can be in_progress at a time."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema {
            one_of: Some(["plan", "set"].into_iter().map(|field| JsonSchema {
                required: Some(vec![field.to_string()]),
                ..JsonSchema::object(BTreeMap::new(), None, None)
            }).collect()),
            ..JsonSchema::object(
            BTreeMap::from([
                ("expected_revision".to_string(), JsonSchema::string(Some(
                    "Revision from the last update_plan result. Checked atomically before any change; required for index-based updates.".into()
                ))),
                (
                    "explanation".to_string(),
                    JsonSchema::string(Some("Explanation for this update, including blockers or cancelled work and any user-authorized scope change. Do not mark unfinished work completed.".to_string())),
                ),
                (
                    "plan".to_string(),
                    JsonSchema::array(revision_item, Some("Complete task checklist, replacing the previous plan. Preserve the user's acceptance criteria; rewriting a step does not complete its old obligations, so carry each removed unfinished step forward with continues or drop it with superseded. A completed checklist is not proof that the request is satisfied.".to_string())),
                ),
                ("set".to_string(), status_updates),
                ("superseded".to_string(), superseded),
                ("workflow".to_string(), JsonSchema {
                    description: Some("Optional map of at most 128 stable step IDs to existing task-coordinator assignment IDs in this root session. Dependencies and capabilities are resolved by the host; checklist order does not schedule execution.".to_string()),
                    ..JsonSchema::object(
                        BTreeMap::new(),
                        None,
                        Some(JsonSchema::string(None).into()),
                    )
                }),
            ]),
            None,
            Some(false.into()),
        )},
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "completion_authority": { "const": "checklist_only", "description": "Model-declared checklist state, not a deliverable publication receipt or host confirmation that the task is complete. A final result must still be published." },
                "revision": { "type": "string", "description": "Content revision for optimistic concurrency; preserved across resume." },
                "step_ids": { "type": "array", "items": { "type": "string" }, "description": "Stable step IDs in current plan order." },
                "lineage": {
                    "type": "object",
                    "description": "Persistent original requirements, independent of checklist wording; supersession records survive subsequent updates and resume. Printed results omit entries restating their sole current step (same ID, text and status).",
                    "properties": {
                        "workflow": {
                            "type": "object",
                            "description": "Stable step ID to existing task-coordinator node. Dependencies and capabilities are host-resolved; list order never schedules execution.",
                            "additionalProperties": {
                                "type": "object",
                                "properties": {
                                    "assignment_id": { "type": "string" },
                                    "dependencies": { "type": "array", "items": { "type": "string" } },
                                    "capability_profile": { "type": "string" }
                                },
                                "required": ["assignment_id", "dependencies", "capability_profile"],
                                "additionalProperties": false
                            }
                        },
                        "requirements": { "type": "object", "additionalProperties": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                "superseded_reason": { "type": "string" }
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }},
                        "step_requirements": { "type": "object", "additionalProperties": {
                            "type": "array", "items": { "type": "string" }
                        }},
                        "step_identities": { "type": "object", "additionalProperties": {
                            "type": "string"
                        }}
                    },
                    "required": ["requirements", "step_requirements"],
                    "additionalProperties": false
                },
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
            "required": ["current_plan", "message", "effect", "no_progress", "revision", "step_ids", "completion_authority"],
            "additionalProperties": false
        }).into()),
    })
}
