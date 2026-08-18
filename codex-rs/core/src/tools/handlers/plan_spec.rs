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
                    "Step status. `passed` requires applicable fresh proof; `completed` is a legacy alias. Only mutations relevant to that proof reopen passed work."
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
        ("validation_route".to_string(), validation_route_schema()),
    ]);

    let properties = BTreeMap::from([
        (
            "explanation".to_string(),
            JsonSchema::string(Some(
                "Optional explanation for this plan update.".to_string(),
            )),
        ),
        (
            "tier".to_string(),
            JsonSchema::string_enum(
                vec![json!("focused"), json!("medium"), json!("complex")],
                Some(
                    "Internal planning representation; it never changes collaboration mode."
                        .to_string(),
                ),
            ),
        ),
        (
            "facts".to_string(),
            JsonSchema::array(
                JsonSchema::object(
                    BTreeMap::from([
                        (
                            "id".to_string(),
                            JsonSchema::string(Some("Stable fact id.".to_string())),
                        ),
                        (
                            "value".to_string(),
                            JsonSchema::string(Some(
                                "Established evidence-backed fact.".to_string(),
                            )),
                        ),
                        (
                            "source".to_string(),
                            JsonSchema::string(Some(
                                "Concrete source locator or bounded observation supporting the fact."
                                    .to_string(),
                            )),
                        ),
                        (
                            "provenance".to_string(),
                            JsonSchema::string_enum(
                                vec![
                                    json!("direct_file_read"),
                                    json!("search_hit"),
                                    json!("generated_summary"),
                                    json!("cached_observation"),
                                    json!("inferred_relationship"),
                                    json!("test_result"),
                                ],
                                Some(
                                    "Why the fact is believed; durable storage does not strengthen this provenance."
                                        .to_string(),
                                ),
                            ),
                        ),
                        (
                            "depends_on_paths".to_string(),
                            JsonSchema::array(
                                JsonSchema::string(Some(
                                    "Repository-relative or absolute path whose content supports this fact."
                                        .to_string(),
                                )),
                                Some(
                                    "Concrete file or directory dependencies used to invalidate this fact after mutations."
                                        .to_string(),
                                ),
                            ),
                        ),
                    ]),
                    Some(vec![
                        "id".to_string(),
                        "value".to_string(),
                        "provenance".to_string(),
                        "source".to_string(),
                        "depends_on_paths".to_string(),
                    ]),
                    Some(false.into()),
                ),
                Some("Stable facts to add or patch; omitted facts remain active.".to_string()),
            ),
        ),
        (
            "removed_facts".to_string(),
            reasoned_removals_schema("facts"),
        ),
        (
            "removed_steps".to_string(),
            reasoned_removals_schema("steps"),
        ),
        (
            "source_owner".to_string(),
            JsonSchema::string(Some("Authoritative owner for focused work.".to_string())),
        ),
        (
            "implementation_surfaces".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some(
                    "Bounded implementation file, region, or contract.".to_string(),
                )),
                Some("Focused implementation surfaces.".to_string()),
            ),
        ),
        (
            "acceptance_criteria".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Stable focused acceptance criterion.".to_string())),
                Some("Acceptance criteria for the focused work unit.".to_string()),
            ),
        ),
        (
            "mutation_obligations".to_string(),
            mutation_obligations_schema(
                "Focused mutation obligations; focused tier accepts at most one.",
            ),
        ),
        (
            "validation_disposition".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("executable"),
                    json!("unresolved_discoverable"),
                    json!("unavailable_blocked"),
                    json!("not_required"),
                ],
                Some("Validation feasibility, distinct from proof identity.".to_string()),
            ),
        ),
        ("validation_route".to_string(), validation_route_schema()),
        (
            "external_validation_route".to_string(),
            external_validation_route_schema(),
        ),
        ("step_evidence".to_string(), step_evidence_schema()),
        (
            "plan".to_string(),
            JsonSchema::array(
                JsonSchema::object(
                    plan_item_properties,
                    Some(vec!["step".to_string(), "status".to_string()]),
                    Some(false.into()),
                ),
                Some(
                    "Stable-ID step patches. Omitted active steps remain until explicitly removed."
                        .to_string(),
                ),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "update_plan".to_string(),
        description: r#"Updates the task plan.
Minimal valid update: send only `plan`. When adding facts, every fact requires `id`, `value`, `provenance`, `source`, and `depends_on_paths`. When adding mutation obligations, every obligation requires `id` and `description`; `paths` is optional.
Use focused only for one atomic owner/scope with no cross-owner contract, one mutation obligation, and one feasible validation route; it uses a stable internal work unit and an empty plan.
Use a short evidence-first medium plan for bounded multi-surface work. Use the complete complex representation for multi-owner, architectural, generated-contract, migration, high-risk, or dependent-validation work.
Internal tier selection never changes collaboration mode. Complexity escalation in Default mode upgrades only this representation. Omitted facts and steps remain active; removals need reasons.
Stop exploration once owner, call path, affected contract/scope, validation route, and material risks are established. Keep focused work checklist-free and medium plans short.
At most one step can be in_progress at a time.
Complete one coherent contract before starting the next. State each unresolved uncertainty, then select the cheapest non-overlapping validation leaves that resolve it. Do not run a separate compile or check when the exact behavioral test already compiles the same owner and configuration; add another leaf only when it proves a distinct contract. Record that every focused test leaf selected at least one test.
Structured validation routes accept only direct cargo, just, python, or python3 leaves; run formatting and diff checks separately outside the route.
Use stable ids, owners, bounded surfaces, dependencies, obligations, and acceptance criteria. Edits
record partial obligation progress. Set a step to passed only when applicable fresh proof exists; completion
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

fn mutation_obligations_schema(description: &str) -> JsonSchema {
    JsonSchema::array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "id".to_string(),
                    JsonSchema::string(Some("Stable mutation-obligation id.".to_string())),
                ),
                (
                    "description".to_string(),
                    JsonSchema::string(Some("Required implementation mutation.".to_string())),
                ),
                (
                    "paths".to_string(),
                    JsonSchema::array(
                        JsonSchema::string(Some("Bounded obligation path.".to_string())),
                        Some("Paths that must all receive matching successful edits.".to_string()),
                    ),
                ),
            ]),
            Some(vec!["id".to_string(), "description".to_string()]),
            Some(false.into()),
        ),
        Some(description.to_string()),
    )
}

fn external_validation_route_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "server_name".to_string(),
                JsonSchema::string(Some("Canonical external server identity.".to_string())),
            ),
            (
                "tool_name".to_string(),
                JsonSchema::string(Some("Canonical external tool identity.".to_string())),
            ),
        ]),
        Some(vec!["server_name".to_string(), "tool_name".to_string()]),
        Some(false.into()),
    )
}

fn step_evidence_schema() -> JsonSchema {
    JsonSchema::array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "step_id".to_string(),
                    JsonSchema::string(Some("Stable plan step id.".to_string())),
                ),
                (
                    "source_owner".to_string(),
                    JsonSchema::string(Some("Authoritative source owner.".to_string())),
                ),
                (
                    "implementation_surfaces".to_string(),
                    JsonSchema::array(
                        JsonSchema::string(Some("Bounded implementation surface.".to_string())),
                        Some("Files, regions, or contracts owned by this step.".to_string()),
                    ),
                ),
                (
                    "mutation_obligations".to_string(),
                    mutation_obligations_schema("Declared step mutation obligations."),
                ),
                (
                    "validation_disposition".to_string(),
                    JsonSchema::string_enum(
                        vec![
                            json!("executable"),
                            json!("unresolved_discoverable"),
                            json!("unavailable_blocked"),
                            json!("not_required"),
                        ],
                        Some("Step validation feasibility, distinct from proof.".to_string()),
                    ),
                ),
                (
                    "external_validation_route".to_string(),
                    external_validation_route_schema(),
                ),
            ]),
            Some(vec!["step_id".to_string()]),
            Some(false.into()),
        ),
        Some("Stable-ID evidence patches for medium and complex steps.".to_string()),
    )
}

fn validation_route_schema() -> JsonSchema {
    let mut timeout = JsonSchema::integer(Some(
        "Bounded execution timeout in milliseconds; it affects proof identity only when semantic_timeout is true."
            .to_string(),
    ));
    timeout.minimum = Some(1.into());
    timeout.maximum = Some(codex_protocol::plan_tool::MAX_STRUCTURED_VALIDATION_TIMEOUT_MS.into());
    let leaf = JsonSchema::object(
        BTreeMap::from([
            (
                "argv".to_string(),
                JsonSchema::array(
                    JsonSchema::string(Some("One exact direct-argv element.".to_string())),
                    Some("Canonical direct argv using cargo, just, python, or python3; shell compounds and formatting or diff checks are not accepted.".to_string()),
                ),
            ),
            (
                "uncertainty".to_string(),
                JsonSchema::string(Some(
                    "Specific uncertainty this command resolves and why this coverage is sufficient."
                        .to_string(),
                )),
            ),
            (
                "covered_paths".to_string(),
                JsonSchema::array(
                    JsonSchema::string(Some("Repository-relative covered path.".to_string())),
                    Some("Non-empty repository-relative coverage used to scope proof reuse.".to_string()),
                ),
            ),
            (
                "covered_contracts".to_string(),
                JsonSchema::array(
                    JsonSchema::string(Some("Explicit covered contract.".to_string())),
                    Some("Covered validation contracts.".to_string()),
                ),
            ),
            ("timeout_ms".to_string(), timeout),
            (
                "semantic_timeout".to_string(),
                JsonSchema::boolean(Some(
                    "True only when the validation contract makes timeout part of proof semantics."
                        .to_string(),
                )),
            ),
        ]),
        Some(vec![
            "argv".to_string(),
            "uncertainty".to_string(),
            "covered_paths".to_string(),
            "covered_contracts".to_string(),
            "timeout_ms".to_string(),
        ]),
        Some(false.into()),
    );
    JsonSchema::object(
        BTreeMap::from([
            (
                "leaves".to_string(),
                JsonSchema::array(
                    leaf,
                    Some("Ordered independently executed validation leaves.".to_string()),
                ),
            ),
            (
                "ordering".to_string(),
                JsonSchema::string_enum(
                    vec![json!("stop_on_failure"), json!("run_all")],
                    Some("Declared route short-circuit policy.".to_string()),
                ),
            ),
        ]),
        Some(vec!["leaves".to_string(), "ordering".to_string()]),
        Some(false.into()),
    )
}

fn reasoned_removals_schema(kind: &str) -> JsonSchema {
    JsonSchema::array(
        JsonSchema::object(
            BTreeMap::from([
                (
                    "id".to_string(),
                    JsonSchema::string(Some(format!("Stable {kind} id."))),
                ),
                (
                    "reason".to_string(),
                    JsonSchema::string(Some("Required audit reason.".to_string())),
                ),
            ]),
            Some(vec!["id".to_string(), "reason".to_string()]),
            Some(false.into()),
        ),
        Some(format!("Explicit reasoned removal of active {kind}.")),
    )
}
