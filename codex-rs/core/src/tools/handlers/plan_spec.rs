use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

fn acceptance_schema() -> JsonSchema {
    let reference = || {
        JsonSchema::object(
            BTreeMap::from([
                ("turn_id".into(), JsonSchema::string(None)),
                ("sha256".into(), JsonSchema::string(None)),
            ]),
            Some(vec!["turn_id".into(), "sha256".into()]),
            Some(false.into()),
        )
    };
    let requirement = |kind: &str, fields: Vec<(&str, JsonSchema)>| {
        let mut properties = BTreeMap::from([(
            "kind".into(),
            JsonSchema::string_enum(vec![json!(kind)], None),
        )]);
        let mut required = vec!["kind".into()];
        for (name, schema) in fields {
            required.push(name.into());
            properties.insert(name.into(), schema);
        }
        JsonSchema::object(properties, Some(required), Some(false.into()))
    };
    let proof = JsonSchema {
        one_of: Some(vec![
            requirement(
                "inspection",
                vec![
                    ("environment_id", JsonSchema::string(None)),
                    ("path", JsonSchema::string(None)),
                    (
                        "start",
                        JsonSchema {
                            minimum: Some(0.into()),
                            ..JsonSchema::integer(None)
                        },
                    ),
                    (
                        "end",
                        JsonSchema {
                            minimum: Some(1.into()),
                            ..JsonSchema::integer(None)
                        },
                    ),
                ],
            ),
            JsonSchema::object(
                BTreeMap::from([
                    ("kind".into(), JsonSchema::string_enum(vec![json!("validation")], None)),
                    ("environment_id".into(), JsonSchema::string(None)),
                    ("path".into(), JsonSchema::string(None)),
                    ("test_execution".into(), JsonSchema::boolean(None)),
                    ("required_tests".into(), JsonSchema::array(
                        JsonSchema::object(BTreeMap::from([
                            ("binary".into(), JsonSchema::string(None)),
                            ("helpers".into(), JsonSchema::array(JsonSchema::string(None), None)),
                            ("test".into(), JsonSchema::string(None)),
                        ]), Some(vec!["binary".into(), "helpers".into(), "test".into()]), Some(false.into())),
                        Some("Optional exact host runner identities from one passing execution; requires test_execution=true. Counts, package inputs and unrelated passing tests do not satisfy this selection. Helpers use the runner's canonical order.".into()),
                    )),
                ]),
                Some(vec!["kind".into(), "environment_id".into(), "path".into(), "test_execution".into()]),
                Some(false.into()),
            ),
            requirement(
                "assignment",
                vec![
                    ("assignment_id", JsonSchema::string(None)),
                    ("attempt_id", JsonSchema::string(None)),
                ],
            ),
        ]),
        ..Default::default()
    };
    JsonSchema {
        description: Some("Independent declared acceptance IDs (maximum 128), not checklist statuses. Incremental: omitted criteria persist. Text, origin and proof kind are immutable; Inspection/validation evidence_ref must name current task-state acceptance_evidence; the host resolves assignment evidence from its current typed query. Inspection ranges are byte offsets. Assignment criteria are mandatory and require current typed receipt, gates and parent integration; optional children need no criterion. Retire only with retired_by accepted input and retirement_reason. Declare every required artifact/consumer explicitly: this finite contract cannot certify omitted scope or semantic correctness. Never introduce unauthorized tests.".into()),
        ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::object(BTreeMap::from([
            ("text".into(), JsonSchema::string(None)),
            ("origin".into(), reference()),
            ("requirement".into(), proof),
            ("plan_requirement_ids".into(), {
                let mut ids = JsonSchema::array(JsonSchema::string(None), Some(
                    "Known checklist requirement IDs covered by this criterion (maximum 128). A concrete blocker applies only to explicitly linked requirements; it never completes or retires them, excuses unrelated work, or authorizes tests. Omitted links preserve legacy same-ID matching.".into()
                ));
                ids.max_items = Some(128);
                ids
            }),
            ("evidence_ref".into(), JsonSchema::string(None)),
            ("blocker".into(), JsonSchema::string(Some("Concrete external dependency; do not use a truthful partial report to abandon obtainable work.".into()))),
            ("retired_by".into(), reference()),
            ("retirement_reason".into(), JsonSchema::string(None)),
        ]), Some(vec!["text".into(), "origin".into(), "requirement".into()]), Some(false.into())).into()))
    }
}

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
            Some("IDs (step_ids of the last result, or unresolved orphan requirement IDs) of removed or reworded steps whose remaining work this step carries.".to_string()),
        ),
    );
    let revision_item = JsonSchema::object(
        revision_item_properties,
        item_required(),
        Some(false.into()),
    );
    let superseded = JsonSchema::array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "step_id".to_string(),
                    JsonSchema::string(Some("ID of an unfinished step this revision removes, or an unresolved orphan requirement from lineage.".to_string())),
                ),
                (
                    "reason".to_string(),
                    JsonSchema::string(Some("Proposed retirement reason. Model-authored prose alone is not authorization; use scope_change_instructions to ground a user-authorized retirement.".to_string())),
                ),
            ]),
            Some(vec!["step_id".to_string(), "reason".to_string()]),
            Some(false.into()),
        ),
        Some("Unfinished steps this revision removes without a continuing step. Each unfinished step removed must be continued or listed here, or nothing changes.".to_string()),
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
                    ..JsonSchema::integer(Some("Zero-based index in the current plan as of this sampling step. Needs expected_revision only after the plan changed since then, for example by an earlier update in the same cell.".to_string()))
                }),
                ("step_id".to_string(), JsonSchema::string(Some(
                    "Stable ID from step_ids in the previous result, or a unique prefix of at least 8 characters; survives reordering and one-to-one continues renames. New split/merge steps get new IDs. Provide step_id or index, not both.".into()
                ))),
                ("status".to_string(), JsonSchema::string_enum(
                    vec![json!("pending"), json!("in_progress"), json!("completed")], None,
                )),
            ]),
            Some(vec!["status".to_string()]),
            Some(false.into()),
        )},
        Some("Change only these statuses atomically, preserving step text and omitted steps; prefer this over resending the whole plan. Use either set or plan. Omitted explanation is preserved with set. Once the plan changed since this sampling step began, index updates and further changes to an already changed step are rejected without expected_revision or the step's expected_step_revisions entry.".to_string()),
    );
    status_updates.min_items = Some(1);
    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: "Updates the task checklist for work with multiple substantive dependent steps. Skip plans for straightforward work; use plans for complex tasks, including read-only audits, when dependencies or coverage require tracking. Do not create single-step plans. At most one step can be in_progress at a time. Send a status update in the same cell as the work it reports, never as a cell of its own."
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
                    "Revision from the last update_plan result or conflict reconciliation; in exec pass the returned object's revision directly instead of retyping the hash. Checked atomically before any change. A replacement or index-based set may omit it only if the plan has not changed since this sampling step began; stable-ID deltas may instead use expected_step_revisions.".into()
                ))),
                (
                    "explanation".to_string(),
                    JsonSchema::string(Some("Explanation for this update, including blockers or cancelled work and any user-authorized scope change. Do not mark unfinished work completed.".to_string())),
                ),
                (
                    "plan".to_string(),
                    JsonSchema::array(revision_item, Some("Complete task checklist, replacing the previous plan. Preserve the user's acceptance criteria: each unfinished step you remove must be named in a new step's continues or in superseded, or the update is rejected.".to_string())),
                ),
                ("set".to_string(), status_updates),
                ("expected_step_revisions".to_string(), JsonSchema {
                    description: Some("Full step ID to its last observed lineage.step_revisions value (absent means zero). Protects independent status deltas without a whole-plan conflict. Required for changing an already revised step unless expected_revision is supplied; same-status retries and initial legacy steps remain compatible.".into()),
                    ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema {
                        minimum: Some(0.into()), ..JsonSchema::integer(None)
                    }.into()))
                }),
                ("resolve".to_string(), JsonSchema::array(JsonSchema::string(None), Some(
                    "Original requirement IDs whose acceptance scope is now fully satisfied. Use in the same update as completing renamed/split/merged descendants; at least one mapped descendant must exist and every mapped descendant must be completed. Reattach orphaned scope with continues before resolving it; authorized retirement uses superseded instead.".into()
                ))),
                ("scope_change_instructions".to_string(), JsonSchema {
                    description: Some("Superseded step/orphan ID to the exact complete text of a currently accepted user input authorizing that scope change. The host verifies user-input provenance, not your interpretation; a reason or tool/context quote alone cannot retire scope. Omit for unverified proposals.".into()),
                    ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::string(None).into()))
                }),
                ("superseded".to_string(), superseded),
                ("scope_change_reviewed_revision".to_string(), JsonSchema::string(Some(
                    "Current task-state scope_input_revision. Required when citing an earlier accepted input: explicitly reconcile every later instruction, including contradictions, before using it. This records interpretation, not host semantic approval.".into()
                ))),
                ("acceptance".to_string(), acceptance_schema()),
                ("acceptance_reviewed_revision".to_string(), JsonSchema::string(Some(
                    "Current scope_input_revision against which the independent acceptance criteria were reconciled. Required with acceptance updates; later input invalidates scoped acceptance until reviewed.".into()
                ))),
                ("scope_change_refs".to_string(), JsonSchema {
                    description: Some("Superseded step/orphan ID to an accepted_input_refs entry from current task state. References include authenticated explicit request_user_input answers bound to their full question/options, session, turn and call; use the host-provided reference, not a hash of the answer label. Interrupted, empty, timed-out, skipped, legacy-disposition or transport-error replies do not mint decision references. Alternative to copying scope_change_instructions for raw user text; identical user provenance checks apply. A reference establishes source identity, not semantic authorization. Conflicting text and reference sources are rejected.".into()),
                    ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::object(
                        BTreeMap::from([
                            ("turn_id".into(), JsonSchema::string(None)),
                            ("sha256".into(), JsonSchema::string(None)),
                        ]),
                        Some(vec!["turn_id".into(), "sha256".into()]),
                        Some(false.into()),
                    ).into()))
                }),
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
                "obligations": {
                    "type": "object",
                    "description": "Model-declared checklist accounting, not verified requirement completion. Continuations do not establish equivalent scope; ungrounded supersessions remain unresolved. Reconcile original acceptance scope against evidence and accepted user instructions; counts do not authorize stopping or overriding scope changes.",
                    "properties": {
                        "completed": { "type": "integer", "minimum": 0 },
                        "superseded": { "type": "integer", "minimum": 0, "description": "Declared retirements grounded in host-accepted user input. The source is verified, not the semantic interpretation or task completion." },
                        "unresolved": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["completed", "superseded", "unresolved"],
                    "additionalProperties": false
                },
                "completion_authority": { "const": "checklist_only", "description": "Model-declared checklist state, not a deliverable publication receipt or host confirmation that the task is complete. A final result must still be published." },
                "lineage_complete": { "type": "boolean", "description": "False for active model projections; full historical lineage remains in durable plan snapshots." },
                "revision": { "type": "string", "description": "Content revision for optimistic concurrency; preserved across resume." },
                "step_ids": { "type": "array", "items": { "type": "string" }, "description": "Stable step IDs in current plan order." },
                "lineage": {
                    "type": "object",
                    "description": "Persistent original requirements, independent of checklist wording; supersession records survive subsequent updates and resume. Printed results show only message, effect, no_progress, revision and obligations; the task-state context carries the stored plan, step IDs and this lineage.",
                    "properties": {
                        "resolved_requirements": { "type": "array", "items": { "type": "string" } },
                        "acceptance": acceptance_schema(),
                        "acceptance_reviewed_revision": { "type": "string" },
                        "step_revisions": { "type": "object", "additionalProperties": { "type": "integer", "minimum": 0 } },
                        "accepted_supersessions": { "type": "object", "additionalProperties": {
                            "type": "object", "properties": { "turn_id": { "type": "string" }, "sha256": { "type": "string" } },
                            "required": ["turn_id", "sha256"], "additionalProperties": false
                        } },
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
                                "superseded_reason": { "type": "string", "description": "Unverified model-proposed retirement; not authority to remove this obligation." }
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
            "required": ["current_plan", "message", "effect", "no_progress", "revision", "step_ids", "completion_authority", "obligations"],
            "additionalProperties": false
        }).into()),
    })
}
#[cfg(test)]
mod audit149_tests {
    use super::*;

    #[test]
    fn audit149_required_test_selection_schema_and_legacy_contract_are_valid() {
        let ToolSpec::Function(spec) = create_update_plan_tool() else {
            panic!("function tool")
        };
        let validator = jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
        let mut args = json!({"plan":[], "acceptance": {"consumer":{
            "text":"Verify consumer", "origin":{"turn_id":"task","sha256":"origin"},
            "requirement":{"kind":"validation","environment_id":"local","path":"consumer.rs",
                "test_execution":true,"required_tests":[{"binary":"codex-core","helpers":[],"test":"consumer_regression"}]}
        }}});
        assert!(validator.is_valid(&args));
        args["acceptance"]["consumer"]["requirement"]["required_tests"][0]
            .as_object_mut()
            .unwrap()
            .remove("test");
        assert!(!validator.is_valid(&args));
        args["acceptance"]["consumer"]["requirement"]
            .as_object_mut()
            .unwrap()
            .remove("required_tests");
        assert!(
            validator.is_valid(&args),
            "legacy path-only criteria remain supported"
        );
    }
}
