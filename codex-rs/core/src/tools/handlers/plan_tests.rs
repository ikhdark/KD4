use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolRouter;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::plan_tool::ValidationRouteLeaf;
use codex_protocol::plan_tool::ValidationRouteOrdering;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[test]
fn plan_output_always_signals_mutation_obligation_state() {
    let output = PlanToolOutput {
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        normalized_plan: None,
        effect: PlanUpdateEffect::NoOp,
        normalization_reason: Some(
            "request matched the authoritative plan; no plan state changed".to_string(),
        ),
        governor_plan: None,
        unfinished_mutation_obligation: None,
        validation_results: Vec::new(),
    };
    let signal = output
        .sampling_request_signal()
        .expect("successful status-only and no-op plan updates must emit obligation state");
    assert_eq!(signal["kind"], "plan_update");
    assert_eq!(signal["effect"], "no_op");
    assert_eq!(signal["no_progress"], true);
    assert!(signal["unfinished_mutation_obligation"].is_null());
}

#[test]
fn pending_validation_result_does_not_readmit_a_durable_candidate() {
    let route = ValidationRoute {
        leaves: vec![ValidationRouteLeaf {
            argv: vec!["cargo".to_string(), "test".to_string()],
            uncertainty: String::new(),
            covered_paths: vec!["codex-rs/core/src/tools/handlers/plan.rs".to_string()],
            covered_contracts: vec!["update_plan response rendering".to_string()],
            timeout_ms: 10_000,
            semantic_timeout: false,
        }],
        ordering: ValidationRouteOrdering::StopOnFailure,
    };
    assert!(
        super::super::shell::validate_structured_validation_leaf(
            &route.leaves[0],
            PathBuf::from(".").as_path()
        )
        .is_err(),
        "the fixture must remain semantically inadmissible so this detects a rendering-layer recheck"
    );
    let result = pending_validation_result(AutoValidationCandidate {
        step_id: "step".to_string(),
        step_revision: 1,
        route: route.clone(),
        implementation_revision: 1,
        implementation_identity: "implementation".to_string(),
        leaf_implementation_identities: vec!["leaf".to_string()],
        repository_wide: false,
    });

    assert!(result["unsupported_runner"].is_null());
    assert_eq!(result["validation_route"], serde_json::json!(route));
}

#[tokio::test]
async fn task_evidence_plan_rejects_unadmitted_validation_route_at_input() {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "plan": [{
                "id": "step",
                "step": "Keep validation at the input boundary",
                "status": "pending",
                "validation_route": {
                    "leaves": [{
                        "argv": ["cargo", "test"],
                        "uncertainty": "",
                        "covered_paths": ["codex-rs/core/src/tools/handlers/plan.rs"],
                        "covered_contracts": ["update_plan input admission"],
                        "timeout_ms": 10000
                    }]
                }
            }]
        })
        .to_string(),
    };
    let result = PlanHandler::new(true)
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "invalid-plan-validation-route".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload,
        })
        .await;

    assert!(matches!(
        result,
        Err(FunctionCallError::RespondToModel(message))
            if message == "structured validation route could not be bound: auto-validation must state the uncertainty this command resolves"
    ));
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(evidence["plan"], serde_json::json!([]));
}

async fn enable_task_evidence(
    session: &mut Arc<crate::session::session::Session>,
) -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("task evidence tempdir");
    let repo = temp.path().join("repo");
    let codex_home = temp.path().join("codex-home");
    tokio::fs::create_dir_all(&repo)
        .await
        .expect("task evidence repo fixture");
    tokio::fs::write(repo.join("kd4_features.toml"), "# fixture")
        .await
        .expect("task evidence manifest fixture");
    let evidence_path = codex_home
        .join("task-evidence")
        .join(format!("{}.json", session.thread_id));
    let ledger =
        crate::task_evidence::TaskEvidenceLedger::load_or_new(codex_home, session.thread_id, &repo)
            .await;
    Arc::get_mut(session)
        .expect("single session reference")
        .services
        .task_evidence = ledger;
    (temp, evidence_path)
}

fn plan_arguments(step: &str) -> String {
    serde_json::to_string(&UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: Some("step".to_string()),
            step: step.to_string(),
            status: StepStatus::Pending,
            ..Default::default()
        }],
    })
    .expect("serialize plan arguments")
}

fn plan_update_args(id: Option<&str>, step: &str, status: StepStatus) -> UpdatePlanArgs {
    UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: id.map(str::to_string),
            step: step.to_string(),
            status,
            ..Default::default()
        }],
    }
}

async fn read_persisted_plan(evidence_path: &PathBuf) -> serde_json::Value {
    serde_json::from_slice(
        &tokio::fs::read(evidence_path)
            .await
            .expect("read persisted task evidence"),
    )
    .expect("parse persisted task evidence")
}

async fn invoke_normalized_plan_update(
    args: UpdatePlanArgs,
) -> (serde_json::Value, serde_json::Value) {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let arguments = serde_json::to_string(&args).expect("serialize plan update");
    let payload = ToolPayload::Function { arguments };
    let output = PlanHandler::new(true)
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "normalized-plan-output".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await
        .expect("plan update should succeed");

    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("normalized-plan-output", &payload)
    else {
        panic!("plan update should return function output");
    };
    let FunctionCallOutputBody::Text(text) = response.body else {
        panic!("plan update should return text output");
    };
    let ordinary_result =
        serde_json::from_str(&text).expect("normalized plan output should be JSON");
    let code_mode_result = output.code_mode_result(&payload);
    assert_eq!(ordinary_result, code_mode_result);

    (ordinary_result, read_persisted_plan(&evidence_path).await)
}

#[test]
fn unchanged_plan_output_remains_compact() {
    let output = PlanToolOutput {
        current_plan: UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        },
        normalized_plan: None,
        effect: PlanUpdateEffect::NoOp,
        normalization_reason: Some(
            "request matched the authoritative plan; no plan state changed".to_string(),
        ),
        governor_plan: Some(UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        }),
        unfinished_mutation_obligation: Some(false),
        validation_results: Vec::new(),
    };
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item("unchanged-plan-output", &payload)
    else {
        panic!("plan update should return function output");
    };

    let response = serde_json::from_str::<serde_json::Value>(
        response
            .body
            .to_text()
            .as_deref()
            .expect("plan output text"),
    )
    .expect("plan output JSON");
    assert_eq!(response["message"], PLAN_UNCHANGED_MESSAGE);
    assert_eq!(response["effect"], "no_op");
    assert_eq!(response["no_progress"], true);
    assert_eq!(
        response["normalization_reason"],
        "request matched the authoritative plan; no plan state changed"
    );
    assert_eq!(response["current_plan"]["plan"], serde_json::json!([]));
    assert_eq!(output.code_mode_result(&payload), response);
}

#[test]
fn recommended_fixes_normalization_identifies_only_changed_steps() {
    let requested = UpdatePlanArgs {
        explanation: None,
        plan: vec![
            PlanItemArg {
                id: Some("proven".to_string()),
                step: "proven".to_string(),
                status: StepStatus::Passed,
                ..Default::default()
            },
            PlanItemArg {
                id: Some("missing".to_string()),
                step: "missing".to_string(),
                status: StepStatus::Passed,
                ..Default::default()
            },
        ],
    };
    let mut current = requested.clone();
    current.plan[1].status = StepStatus::InProgress;

    let reason = plan_normalization_reason(
        &requested,
        &current,
        /*was_normalized*/ true,
        PlanUpdateEffect::StatusOnly,
        Some("missing"),
    )
    .expect("one status was normalized");
    assert!(reason.contains("[missing]"));
    assert!(!reason.contains("[proven]"));
}

#[test]
fn precomputed_unchanged_plan_produces_the_noop_reason() {
    let plan = UpdatePlanArgs {
        explanation: None,
        plan: Vec::new(),
    };

    assert_eq!(
        plan_normalization_reason(
            &plan,
            &plan,
            /*was_normalized*/ false,
            PlanUpdateEffect::NoOp,
            None,
        ),
        Some("request matched the authoritative plan; no plan state changed".to_string())
    );
}

#[test]
fn ordinary_update_plan_schema_is_a_small_checklist_contract() {
    let tool = serde_json::to_value(create_update_plan_tool(false)).expect("serialize update_plan");
    let properties = tool["parameters"]["properties"]
        .as_object()
        .expect("top-level checklist properties");
    let item_properties = tool["parameters"]["properties"]["plan"]["items"]["properties"]
        .as_object()
        .expect("checklist item properties");

    assert_eq!(
        properties.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["explanation", "plan"]
    );
    assert_eq!(
        item_properties
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["status", "step"]
    );
    assert!(
        !tool["description"]
            .as_str()
            .expect("description")
            .contains("task evidence")
    );
}

#[tokio::test]
async fn ordinary_plan_updates_use_the_session_checklist_store() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let handler = PlanHandler::new(false);

    let initial_payload = ToolPayload::Function {
        arguments: serde_json::to_string(&plan_update_args(
            None,
            "Implement the change",
            StepStatus::InProgress,
        ))
        .expect("serialize initial checklist"),
    };
    let initial = handler
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "ordinary-plan-initial".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: initial_payload.clone(),
        })
        .await
        .expect("initial checklist update");
    assert_eq!(
        initial.code_mode_result(&initial_payload)["effect"],
        "initial"
    );

    let completed_payload = ToolPayload::Function {
        arguments: serde_json::to_string(&plan_update_args(
            None,
            "Implement the change",
            StepStatus::Completed,
        ))
        .expect("serialize completed checklist"),
    };
    let completed = handler
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "ordinary-plan-completed".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: completed_payload.clone(),
        })
        .await
        .expect("completed checklist update");
    assert_eq!(
        completed.code_mode_result(&completed_payload)["effect"],
        "status_only"
    );
}

#[tokio::test]
async fn task_evidence_plan_does_not_mirror_into_session_store() {
    let (mut session, turn, _events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let payload = ToolPayload::Function {
        arguments: plan_arguments("Keep one authoritative plan owner"),
    };

    PlanHandler::new(true)
        .handle(ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(turn),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "task-evidence-single-plan-owner".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload,
        })
        .await
        .expect("task-evidence plan update");

    assert_eq!(
        read_persisted_plan(&evidence_path).await["plan"][0]["step"],
        "Keep one authoritative plan owner"
    );
    assert!(
        session
            .services
            .plan_store
            .current_for_test()
            .await
            .is_none(),
        "durable task evidence must remain the sole plan owner"
    );
}

#[test]
fn update_plan_schema_exposes_task_evidence_contract() {
    fn property_names<'a>(tool: &'a serde_json::Value, pointer: &str) -> Vec<&'a str> {
        tool.pointer(pointer)
            .and_then(serde_json::Value::as_object)
            .unwrap_or_else(|| panic!("schema properties at {pointer}"))
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn enum_values<'a>(tool: &'a serde_json::Value, pointer: &str) -> Vec<&'a str> {
        tool.pointer(pointer)
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("schema enum at {pointer}"))
            .iter()
            .map(|value| value.as_str().expect("string enum value"))
            .collect()
    }

    let tool = serde_json::to_value(create_update_plan_tool(true)).expect("serialize update_plan");
    let description = tool["description"].as_str().expect("tool description");
    let statuses =
        tool["parameters"]["properties"]["plan"]["items"]["properties"]["status"]["enum"]
            .as_array()
            .expect("plan step status enum");

    assert_eq!(
        property_names(&tool, "/parameters/properties"),
        vec![
            "acceptance_criteria",
            "explanation",
            "external_validation_route",
            "facts",
            "implementation_surfaces",
            "mutation_obligations",
            "plan",
            "removed_facts",
            "removed_steps",
            "source_owner",
            "step_evidence",
            "tier",
            "validation_disposition",
            "validation_route",
        ]
    );
    assert_eq!(
        property_names(&tool, "/parameters/properties/plan/items/properties"),
        vec![
            "acceptance_criteria",
            "depends_on",
            "generated_artifacts",
            "id",
            "risks",
            "runtime_paths",
            "status",
            "step",
            "validation_route",
        ]
    );
    assert_eq!(
        statuses,
        &vec![
            serde_json::json!("pending"),
            serde_json::json!("in_progress"),
            serde_json::json!("implemented"),
            serde_json::json!("passed"),
            serde_json::json!("blocked"),
            serde_json::json!("skipped"),
            serde_json::json!("completed"),
        ]
    );
    assert_eq!(
        enum_values(&tool, "/parameters/properties/tier/enum"),
        vec!["focused", "medium", "complex"]
    );
    assert_eq!(
        enum_values(
            &tool,
            "/parameters/properties/facts/items/properties/provenance/enum"
        ),
        vec![
            "direct_file_read",
            "search_hit",
            "generated_summary",
            "cached_observation",
            "inferred_relationship",
            "test_result",
        ]
    );
    assert_eq!(
        enum_values(&tool, "/parameters/properties/validation_disposition/enum"),
        vec![
            "executable",
            "unresolved_discoverable",
            "unavailable_blocked",
            "not_required",
        ]
    );
    assert_eq!(
        enum_values(
            &tool,
            "/parameters/properties/validation_route/properties/ordering/enum"
        ),
        vec!["stop_on_failure", "run_all"]
    );
    assert_eq!(
        property_names(&tool, "/parameters/properties/facts/items/properties"),
        vec!["depends_on_paths", "id", "provenance", "source", "value"]
    );
    assert!(
        tool.pointer("/parameters/properties/facts/items/properties/dependencies_current")
            .is_none(),
        "dependencies_current is server-owned and must not be caller controlled"
    );
    assert_eq!(
        property_names(
            &tool,
            "/parameters/properties/removed_facts/items/properties"
        ),
        vec!["id", "reason"]
    );
    assert_eq!(
        property_names(
            &tool,
            "/parameters/properties/mutation_obligations/items/properties"
        ),
        vec!["description", "id", "paths"]
    );
    assert_eq!(
        property_names(
            &tool,
            "/parameters/properties/external_validation_route/properties"
        ),
        vec!["server_name", "tool_name"]
    );
    assert_eq!(
        property_names(
            &tool,
            "/parameters/properties/step_evidence/items/properties"
        ),
        vec![
            "external_validation_route",
            "implementation_surfaces",
            "mutation_obligations",
            "source_owner",
            "step_id",
            "surface_roles",
            "validation_asset_paths",
            "validation_disposition",
        ]
    );
    assert_eq!(
        enum_values(
            &tool,
            "/parameters/properties/step_evidence/items/properties/surface_roles/items/enum"
        ),
        vec![
            "lifecycle",
            "persistence",
            "schema",
            "security",
            "packaging",
            "pipeline",
            "validation",
        ]
    );
    assert_eq!(
        property_names(&tool, "/parameters/properties/validation_route/properties"),
        vec!["leaves", "ordering"]
    );
    assert_eq!(
        property_names(
            &tool,
            "/parameters/properties/validation_route/properties/leaves/items/properties"
        ),
        vec![
            "argv",
            "covered_contracts",
            "covered_paths",
            "semantic_timeout",
            "timeout_ms",
            "uncertainty",
        ]
    );

    for pointer in [
        "/parameters/additionalProperties",
        "/parameters/properties/plan/items/additionalProperties",
        "/parameters/properties/facts/items/additionalProperties",
        "/parameters/properties/removed_facts/items/additionalProperties",
        "/parameters/properties/mutation_obligations/items/additionalProperties",
        "/parameters/properties/external_validation_route/additionalProperties",
        "/parameters/properties/step_evidence/items/additionalProperties",
        "/parameters/properties/step_evidence/items/properties/mutation_obligations/items/additionalProperties",
        "/parameters/properties/step_evidence/items/properties/external_validation_route/additionalProperties",
        "/parameters/properties/validation_route/additionalProperties",
        "/parameters/properties/validation_route/properties/leaves/items/additionalProperties",
        "/parameters/properties/plan/items/properties/validation_route/additionalProperties",
        "/parameters/properties/plan/items/properties/validation_route/properties/leaves/items/additionalProperties",
    ] {
        assert_eq!(
            tool.pointer(pointer),
            Some(&serde_json::json!(false)),
            "{pointer}"
        );
    }

    assert!(tool.pointer("/parameters/required").is_none());
    assert_eq!(
        tool.pointer("/parameters/properties/plan/items/required"),
        Some(&serde_json::json!(["step", "status"]))
    );
    assert_eq!(
        tool.pointer("/parameters/properties/validation_route/required"),
        Some(&serde_json::json!(["leaves"]))
    );
    assert_eq!(
        tool.pointer("/parameters/properties/facts/items/required"),
        Some(&serde_json::json!([
            "id",
            "value",
            "provenance",
            "source",
            "depends_on_paths"
        ]))
    );
    assert_eq!(
        tool.pointer("/parameters/properties/validation_route/properties/leaves/items/required"),
        Some(&serde_json::json!([
            "argv",
            "uncertainty",
            "covered_paths",
            "covered_contracts",
            "timeout_ms"
        ]))
    );
    assert!(description.contains("All update fields are optional"));
    assert!(description.contains("Structured validation routes"));
}

#[test]
fn focused_plan_arguments_require_one_atomic_work_unit_and_reasoned_removals() {
    let focused = parse_task_evidence_arguments(
        &serde_json::json!({
            "tier": "focused",
            "source_owner": "codex-core",
            "implementation_surfaces": ["core/src/task_evidence.rs"],
            "mutation_obligations": [{
                "id": "mutation",
                "description": "edit the owner",
                "paths": ["core/src/task_evidence.rs"]
            }],
            "validation_disposition": "not_required",
            "plan": []
        })
        .to_string(),
    )
    .expect("focused work unit");
    assert_eq!(focused.tier, Some(PlanningTier::Focused));
    assert!(focused.plan.is_empty());

    for invalid in [
        serde_json::json!({
            "tier": "focused",
            "plan": [{"id": "step", "step": "not atomic", "status": "pending"}]
        }),
        serde_json::json!({
            "tier": "focused",
            "mutation_obligations": [
                {"id": "one", "description": "one"},
                {"id": "two", "description": "two"}
            ],
            "plan": []
        }),
        serde_json::json!({
            "removed_steps": [{"id": "step", "reason": ""}],
            "plan": []
        }),
    ] {
        assert!(parse_task_evidence_arguments(&invalid.to_string()).is_err());
    }
}

#[test]
fn task_evidence_review_metadata_rejects_unconsumable_values() {
    for invalid in [
        serde_json::json!({
            "step_evidence": [{"step_id": "step", "surface_roles": ["unknown"]}]
        }),
        serde_json::json!({
            "step_evidence": [{"step_id": "step", "validation_asset_paths": ["../golden.snap"]}]
        }),
        serde_json::json!({
            "step_evidence": [{"step_id": "step", "validation_asset_paths": ["C:\\golden.snap"]}]
        }),
    ] {
        assert!(parse_task_evidence_arguments(&invalid.to_string()).is_err());
    }

    let parsed = parse_task_evidence_arguments(
        &serde_json::json!({
            "step_evidence": [{
                "step_id": "step",
                "surface_roles": ["pipeline", "validation"],
                "validation_asset_paths": ["tests/golden.snap"]
            }]
        })
        .to_string(),
    )
    .expect("completion-review metadata");
    assert_eq!(parsed.step_evidence[0].surface_roles.len(), 2);
}

#[test]
fn default_mode_complexity_selects_only_the_complex_internal_tier() {
    let parsed = parse_task_evidence_arguments(
        &serde_json::json!({
            "tier": "complex",
            "plan": [{
                "id": "architecture",
                "step": "Resolve the cross-owner architecture contract",
                "status": "in_progress",
                "risks": ["generated contract compatibility"]
            }]
        })
        .to_string(),
    )
    .expect("complex internal representation");
    assert_eq!(parsed.tier, Some(PlanningTier::Complex));

    let tool = serde_json::to_value(create_update_plan_tool(true)).expect("serialize update_plan");
    let description = tool["description"].as_str().expect("tool description");
    assert!(description.contains("never changes collaboration mode"));
}

#[tokio::test]
async fn normalized_plan_output_reports_proof_free_completed_as_passed() {
    let (result, persisted) = invoke_normalized_plan_update(plan_update_args(
        Some("step"),
        "Implement the step",
        StepStatus::Completed,
    ))
    .await;

    assert_eq!(result["message"], PLAN_UPDATED_MESSAGE);
    assert_eq!(result["normalized_plan"]["plan"][0]["status"], "passed");
    assert_eq!(persisted["plan"][0]["status"], "passed");
    assert_eq!(
        persisted["plan"][0]["validation_disposition"],
        "not_required"
    );
}

#[tokio::test]
async fn skipped_dependency_cycle_reaches_plan_handler_and_persists() {
    let args = serde_json::from_value::<UpdatePlanArgs>(serde_json::json!({
        "plan": [
            {
                "id": "first",
                "step": "Skip the first step",
                "status": "skipped",
                "depends_on": ["second"]
            },
            {
                "id": "second",
                "step": "Skip the second step",
                "status": "skipped",
                "depends_on": ["first"]
            },
            {
                "id": "self-skipped",
                "step": "Skip a self-referential step",
                "status": "skipped",
                "depends_on": ["self-skipped"]
            },
            {
                "id": "missing-skipped",
                "step": "Skip a step with an unavailable prerequisite",
                "status": "skipped",
                "depends_on": ["not-present"]
            },
            {
                "id": "finish",
                "step": "Finish the active work",
                "status": "completed"
            }
        ]
    }))
    .expect("skipped dependency cycle should deserialize");

    let (result, persisted) = invoke_normalized_plan_update(args).await;

    assert_eq!(result["message"], PLAN_UPDATED_MESSAGE);
    assert_eq!(result["normalized_plan"]["plan"][0]["status"], "skipped");
    assert_eq!(result["normalized_plan"]["plan"][1]["status"], "skipped");
    assert_eq!(result["normalized_plan"]["plan"][2]["status"], "skipped");
    assert_eq!(result["normalized_plan"]["plan"][3]["status"], "skipped");
    assert_eq!(result["normalized_plan"]["plan"][4]["status"], "passed");
    assert_eq!(persisted["plan"][0]["status"], "skipped");
    assert_eq!(
        persisted["plan"][0]["depends_on"],
        serde_json::json!(["second"])
    );
    assert_eq!(persisted["plan"][1]["status"], "skipped");
    assert_eq!(
        persisted["plan"][1]["depends_on"],
        serde_json::json!(["first"])
    );
    assert_eq!(
        persisted["plan"][2]["depends_on"],
        serde_json::json!(["self-skipped"])
    );
    assert_eq!(
        persisted["plan"][3]["depends_on"],
        serde_json::json!(["not-present"])
    );
    assert_eq!(persisted["plan"][4]["status"], "passed");
}

#[tokio::test]
async fn normalized_plan_output_returns_the_installed_compatibility_id() {
    let (result, persisted) = invoke_normalized_plan_update(plan_update_args(
        None,
        "Install a stable identifier",
        StepStatus::Pending,
    ))
    .await;

    let returned_id = result["normalized_plan"]["plan"][0]["id"]
        .as_str()
        .expect("normalized plan should return a stable id");
    assert!(returned_id.starts_with("step-"));
    assert_eq!(persisted["plan"][0]["id"], returned_id);
}

#[test]
fn update_plan_waits_for_runtime_cancellation_commit_cleanup() {
    assert!(PlanHandler::new(true).waits_for_runtime_cancellation());
}

#[tokio::test]
async fn plan_persistence_failure_is_reported() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    session
        .services
        .task_evidence
        .set_persistence_failure_for_test(true);
    let arguments = plan_arguments("Do not acknowledge an unpersisted plan");
    let result = PlanHandler::new(true)
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "failed-plan-persistence".to_string(),
            tool_name: ToolName::plain("update_plan"),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function { arguments },
        })
        .await;

    assert!(matches!(
        result,
        Err(FunctionCallError::RespondToModel(message))
            if message
                == "update_plan could not be durably persisted; no plan update was acknowledged"
    ));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::PlanUpdate(_)),
            "a failed persistence attempt must not emit a plan update"
        );
    }
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(evidence["plan"], serde_json::json!([]));
}

#[tokio::test]
async fn cancellation_before_plan_commit_does_not_emit_plan_update() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let arguments = plan_arguments("Do not commit this plan");
    let invocation = ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        cancellation_token,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancelled-plan".to_string(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function { arguments },
    };

    let result = PlanHandler::new(true).handle(invocation).await;

    assert!(matches!(
        result,
        Err(FunctionCallError::RespondToModel(message))
            if message == "update_plan was cancelled before the plan update began"
    ));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::PlanUpdate(_)),
            "a pre-commit cancellation must not emit a plan update"
        );
    }
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(evidence["plan"], serde_json::json!([]));
}

#[tokio::test]
async fn cancellation_after_plan_commit_boundary_waits_for_durable_update() {
    let (mut session, turn, events) = make_session_and_context_with_rx().await;
    let (_temp, evidence_path) = enable_task_evidence(&mut session).await;
    let call_id = "cancelled-after-plan-commit-boundary";
    let hook = PlanCommitBoundaryHook::install(call_id);
    let handler = Arc::new(PlanHandler::new(true)) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step_context = StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        session,
        step_context,
        Arc::new(Mutex::new(TurnDiffTracker::new())),
    );
    let cancellation_token = CancellationToken::new();
    let call = ToolCall {
        tool_name: ToolName::plain("update_plan"),
        call_id: call_id.to_string(),
        payload: ToolPayload::Function {
            arguments: plan_arguments("Commit this plan before returning cancellation"),
        },
    };
    let mut response_task =
        tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
    timeout(Duration::from_secs(2), hook.wait_until_reached())
        .await
        .expect("plan handler should reach its commit boundary");

    cancellation_token.cancel();
    assert!(
        timeout(Duration::from_millis(50), &mut response_task)
            .await
            .is_err(),
        "runtime cancellation must wait for commit cleanup"
    );
    hook.release();

    let response = timeout(Duration::from_secs(2), &mut response_task)
        .await
        .expect("cancelled plan call should finish after commit")
        .expect("plan response task should join")
        .expect("plan runtime should return a response");
    let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
        panic!("cancelled plan tool should return function output");
    };
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("cancelled plan tool output should be text");
    };
    assert!(text.contains("aborted by user"));

    let plan_update = timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("event channel should remain open");
            if let EventMsg::PlanUpdate(update) = event.msg {
                break update;
            }
        }
    })
    .await
    .expect("plan update event should be emitted before cancellation completes");
    assert_eq!(
        plan_update.plan[0].step,
        "Commit this plan before returning cancellation"
    );
    let evidence = read_persisted_plan(&evidence_path).await;
    assert_eq!(
        evidence["plan"][0]["step"],
        "Commit this plan before returning cancellation"
    );
}
