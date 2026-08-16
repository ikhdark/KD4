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
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[test]
fn validation_route_ids_are_stable_and_contract_sensitive() {
    let route = ValidationRoute {
        leaves: vec![ValidationRouteLeaf {
            argv: vec![
                "cargo".into(),
                "test".into(),
                "-p".into(),
                "codex-core".into(),
            ],
            covered_paths: vec!["core/src/tools".into()],
            covered_contracts: vec!["tool routing".into()],
            timeout_ms: 60_000,
            semantic_timeout: false,
        }],
        ordering: ValidationRouteOrdering::StopOnFailure,
    };
    let first = stable_validation_route_id(&route);
    assert_eq!(first, stable_validation_route_id(&route));

    let mut changed = route;
    changed.leaves[0].covered_contracts.push("output".into());
    assert_ne!(first, stable_validation_route_id(&changed));
}

#[test]
fn validation_route_binding_errors_identify_owner_and_leaf() {
    let repository = tempfile::tempdir().expect("repository fixture");
    let invalid_route = ValidationRoute {
        leaves: vec![ValidationRouteLeaf {
            argv: Vec::new(),
            covered_paths: Vec::new(),
            covered_contracts: vec!["plan validation admission".to_string()],
            timeout_ms: 1_000,
            semantic_timeout: false,
        }],
        ordering: ValidationRouteOrdering::StopOnFailure,
    };

    let focused = PlanningUpdateInput {
        validation_route: Some(invalid_route.clone()),
        ..Default::default()
    };
    let FunctionCallError::RespondToModel(focused_error) =
        validate_requested_validation_routes(&focused, repository.path())
            .expect_err("empty focused argv should be rejected")
    else {
        panic!("expected model-visible focused route error");
    };
    assert_eq!(
        focused_error,
        "focused validation route leaf 1 could not be bound: the validation route argv cannot be empty"
    );

    let step = PlanningUpdateInput {
        plan: vec![PlanItemArg {
            id: Some("validate-adapter".to_string()),
            step: "Validate the adapter".to_string(),
            validation_route: Some(invalid_route),
            ..Default::default()
        }],
        ..Default::default()
    };
    let FunctionCallError::RespondToModel(step_error) =
        validate_requested_validation_routes(&step, repository.path())
            .expect_err("empty step argv should be rejected")
    else {
        panic!("expected model-visible step route error");
    };
    assert_eq!(
        step_error,
        "plan step `validate-adapter` leaf 1 could not be bound: the validation route argv cannot be empty"
    );
}

#[test]
fn plan_output_always_signals_mutation_obligation_state() {
    let output = PlanToolOutput {
        normalized_plan: None,
        governor_plan: None,
        effect: PlanUpdateEffect::NoOp,
        unfinished_mutation_obligation: None,
        source_closure_established: false,
        source_closure_receipt: None,
        validation_results: Vec::new(),
        finalization_requested: false,
        finalized: false,
        missing_evidence: Vec::new(),
    };
    let signal = output
        .sampling_request_signal()
        .expect("successful status-only and no-op plan updates must emit obligation state");
    assert_eq!(signal["kind"], "plan_update");
    assert!(signal["unfinished_mutation_obligation"].is_null());
    assert_eq!(signal["source_closure_established"], false);
    assert!(signal["source_closure"].is_null());
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
    let output = PlanHandler
        .handle(ToolInvocation {
            session,
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
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
        normalized_plan: None,
        governor_plan: Some(UpdatePlanArgs {
            explanation: None,
            plan: Vec::new(),
        }),
        effect: PlanUpdateEffect::NoOp,
        unfinished_mutation_obligation: Some(false),
        source_closure_established: false,
        source_closure_receipt: None,
        validation_results: Vec::new(),
        finalization_requested: false,
        finalized: false,
        missing_evidence: Vec::new(),
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

    let result: serde_json::Value = serde_json::from_str(
        response
            .body
            .to_text()
            .as_deref()
            .expect("plan result text"),
    )
    .expect("structured plan result");
    assert_eq!(result, output.code_mode_result(&payload));
    assert_eq!(result["effect"], "no_op");
    assert_eq!(result["no_op"], true);
    assert_eq!(result["finalization"]["requested"], false);
}

#[test]
fn finalization_reports_terminal_state_and_missing_evidence() {
    let terminal = UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: Some("done".to_string()),
            step: "Validated".to_string(),
            status: StepStatus::Passed,
            ..Default::default()
        }],
    };
    assert!(finalization_missing_evidence(&terminal).is_empty());

    let incomplete = UpdatePlanArgs {
        explanation: None,
        plan: vec![PlanItemArg {
            id: Some("needs-proof".to_string()),
            step: "Validate".to_string(),
            status: StepStatus::Implemented,
            validation_route: Some(ValidationRoute {
                ordering: ValidationRouteOrdering::StopOnFailure,
                leaves: vec![ValidationRouteLeaf {
                    argv: vec!["cargo".to_string(), "test".to_string()],
                    covered_paths: Vec::new(),
                    covered_contracts: vec!["plan finalization".to_string()],
                    timeout_ms: 60_000,
                    semantic_timeout: false,
                }],
            }),
            ..Default::default()
        }],
    };
    assert_eq!(
        finalization_missing_evidence(&incomplete),
        vec!["step needs-proof: missing fresh successful validation evidence"]
    );
}

#[test]
fn update_plan_schema_exposes_transactional_finalization() {
    let tool = serde_json::to_value(create_update_plan_tool()).expect("serialize update_plan");
    let finalize = tool
        .pointer("/parameters/properties/finalize")
        .expect("finalize schema");
    assert_eq!(finalize["type"], "boolean");
    assert!(
        finalize["description"]
            .as_str()
            .is_some_and(|description| description.contains("newly admissible"))
    );
}

#[test]
fn generated_artifact_schema_requires_repository_relative_paths() {
    let tool = serde_json::to_value(create_update_plan_tool()).expect("serialize update_plan");
    let artifact_schema = tool
        .pointer("/parameters/properties/plan/items/properties/generated_artifacts")
        .expect("generated_artifacts schema");
    let description = artifact_schema
        .get("description")
        .and_then(serde_json::Value::as_str)
        .expect("generated_artifacts description");
    let item_description = artifact_schema
        .pointer("/items/description")
        .and_then(serde_json::Value::as_str)
        .expect("generated_artifacts item description");

    assert!(description.contains("Repository-relative"));
    assert!(description.contains("remain inside the repository"));
    assert!(item_description.contains("repository-relative"));
}

fn closed_architecture_slice() -> ArchitectureSliceInput {
    let not_applicable = ArchitectureEvidenceFacetInput {
        status: ArchitectureEvidenceStatus::NotApplicable,
        relationships: Vec::new(),
        not_applicable_reason: Some("The focused fixture has no such relationship.".to_string()),
    };
    ArchitectureSliceInput {
        snapshot: "fixture-snapshot".to_string(),
        control_and_data_flow: ArchitectureEvidenceFacetInput {
            status: ArchitectureEvidenceStatus::Established,
            relationships: vec![crate::task_evidence::ArchitectureRelationshipInput {
                kind: crate::task_evidence::ArchitectureRelationshipKind::ControlFlow,
                source: "owner".to_string(),
                target: "runtime".to_string(),
                evidence: "core/src/runtime.rs#run".to_string(),
                provenance: ArchitectureEvidenceProvenance::Exact,
            }],
            not_applicable_reason: None,
        },
        callers_and_consumers: not_applicable.clone(),
        configuration_and_gates: not_applicable.clone(),
        registration_and_entrypoints: not_applicable.clone(),
        tests_and_contracts: not_applicable.clone(),
        generated_artifacts: not_applicable.clone(),
        invariants: not_applicable,
        truncated: false,
        omitted_relationships: 0,
        material_unknowns: Vec::new(),
        limitations: Vec::new(),
        metrics: ArchitectureExplorationMetricsInput::default(),
    }
}

#[test]
fn source_closure_requires_complete_architecture_and_resolved_validation() {
    let mut focused = PlanningUpdateInput {
        tier: Some(PlanningTier::Focused),
        source_owner: Some("core/src/owner.rs".to_string()),
        implementation_surfaces: vec!["core/src/runtime.rs".to_string()],
        validation_disposition: Some(ValidationDisposition::Executable),
        ..Default::default()
    };
    assert!(!planning_update_source_closure(&focused, None).0);

    focused.validation_route = Some(codex_protocol::plan_tool::ValidationRoute {
        ordering: ValidationRouteOrdering::StopOnFailure,
        leaves: vec![ValidationRouteLeaf {
            argv: vec!["cargo".to_string(), "test".to_string()],
            covered_contracts: vec!["source closure".to_string()],
            covered_paths: vec!["core/src/runtime.rs".to_string()],
            timeout_ms: 1_000,
            semantic_timeout: false,
        }],
    });
    let (closed, receipt, _) = planning_update_source_closure(&focused, None);
    assert!(!closed);
    assert_eq!(
        receipt
            .expect("attempted closure receipt")
            .missing_requirements,
        vec!["architecture_slice"]
    );

    focused.architecture_slice = Some(closed_architecture_slice());
    let (closed, receipt, _) = planning_update_source_closure(&focused, None);
    assert!(closed);
    assert!(receipt.expect("complete closure receipt").established);

    focused.validation_disposition = Some(ValidationDisposition::UnresolvedDiscoverable);
    assert!(!planning_update_source_closure(&focused, None).0);
}

#[test]
fn source_closure_signal_reports_compact_wiring_evidence() {
    let mut slice = closed_architecture_slice();
    slice.callers_and_consumers = ArchitectureEvidenceFacetInput {
        status: ArchitectureEvidenceStatus::Established,
        relationships: vec![crate::task_evidence::ArchitectureRelationshipInput {
            kind: crate::task_evidence::ArchitectureRelationshipKind::DirectBuilder,
            source: "builder".to_string(),
            target: "owner".to_string(),
            evidence: "core/src/builder.rs#build".to_string(),
            provenance: ArchitectureEvidenceProvenance::Exact,
        }],
        not_applicable_reason: None,
    };
    let receipt = architecture_slice_receipt(&slice, None);
    let output = PlanToolOutput {
        normalized_plan: None,
        governor_plan: None,
        effect: PlanUpdateEffect::StatusOnly,
        unfinished_mutation_obligation: None,
        source_closure_established: true,
        source_closure_receipt: Some(receipt),
        validation_results: Vec::new(),
        finalization_requested: false,
        finalized: false,
        missing_evidence: Vec::new(),
    };

    let signal = output.sampling_request_signal().expect("plan signal");
    assert_eq!(signal["source_closure"]["total_relationships"], 2);
    assert_eq!(
        signal["source_closure"]["relationship_kinds"],
        serde_json::json!(["control_flow", "direct_builder"])
    );
    assert_eq!(
        signal["source_closure"]["missing_requirements"],
        serde_json::json!([])
    );
}

#[test]
fn architecture_closure_rejects_heuristics_truncation_and_unknowns() {
    let mut slice = closed_architecture_slice();
    slice.control_and_data_flow.relationships[0].provenance =
        ArchitectureEvidenceProvenance::Heuristic;
    let receipt = architecture_slice_receipt(&slice, None);
    assert!(!receipt.established);
    assert!(
        receipt
            .missing_requirements
            .contains(&"architecture_slice.control_and_data_flow".to_string())
    );

    slice.control_and_data_flow.relationships[0].provenance =
        ArchitectureEvidenceProvenance::Declared;
    slice.truncated = true;
    slice.omitted_relationships = 2;
    slice.material_unknowns = vec!["runtime registration".to_string()];
    let receipt = architecture_slice_receipt(&slice, None);
    assert!(!receipt.established);
    assert!(
        receipt
            .missing_requirements
            .iter()
            .any(|item| item.ends_with("not_truncated"))
    );
    assert!(
        receipt
            .missing_requirements
            .iter()
            .any(|item| item.ends_with("zero_omissions"))
    );
    assert!(
        receipt
            .missing_requirements
            .iter()
            .any(|item| item.ends_with("zero_material_unknowns"))
    );
}

#[test]
fn architecture_closure_rejects_a_stale_repository_snapshot() {
    let slice = closed_architecture_slice();
    let receipt = architecture_slice_receipt(&slice, Some("new-head"));

    assert!(!receipt.established);
    assert!(receipt.stale_snapshot);
    assert_eq!(receipt.expected_snapshot.as_deref(), Some("new-head"));
    assert!(
        receipt
            .missing_requirements
            .contains(&"architecture_slice.current_snapshot".to_string())
    );
}

#[test]
fn architecture_closure_accepts_a_current_composite_snapshot() {
    let mut slice = closed_architecture_slice();
    slice.snapshot = "current-head:manifest-digest:source-digest".to_string();
    let receipt = architecture_slice_receipt(&slice, Some("current-head"));

    assert!(receipt.established);
    assert!(!receipt.stale_snapshot);
}

#[test]
fn focused_plan_arguments_require_one_atomic_work_unit_and_reasoned_removals() {
    let focused = parse_update_plan_arguments(
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
        assert!(parse_update_plan_arguments(&invalid.to_string()).is_err());
    }
}

#[test]
fn default_mode_complexity_selects_only_the_complex_internal_tier() {
    let parsed = parse_update_plan_arguments(
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

    let tool = serde_json::to_value(create_update_plan_tool()).expect("serialize update_plan");
    let description = tool["description"].as_str().expect("tool description");
    assert!(description.contains("Default mode upgrades only this representation"));
    assert!(description.contains("never changes collaboration mode"));
}

#[tokio::test]
async fn normalized_plan_output_reports_completed_as_passed() {
    let (result, persisted) = invoke_normalized_plan_update(plan_update_args(
        Some("step"),
        "Implement the step",
        StepStatus::Completed,
    ))
    .await;

    assert_eq!(result["message"], PLAN_UPDATED_MESSAGE);
    assert_eq!(result["normalized_plan"]["plan"][0]["status"], "passed");
    assert_eq!(persisted["plan"][0]["status"], "passed");
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
    assert!(PlanHandler.waits_for_runtime_cancellation());
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
        turn,
        cancellation_token,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: "cancelled-plan".to_string(),
        tool_name: ToolName::plain("update_plan"),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function { arguments },
    };

    let result = PlanHandler.handle(invocation).await;

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
    let handler = Arc::new(PlanHandler) as Arc<dyn CoreToolRuntime>;
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
