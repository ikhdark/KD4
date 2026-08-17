use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn update_plan_rejects_multiple_in_progress_items() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [
            {"step": "one", "status": "in_progress"},
            {"step": "two", "status": "in_progress"}
        ]
    }))
    .expect_err("multiple active items should fail");

    assert!(err.to_string().contains("at most one in_progress"));
}

#[test]
fn update_plan_accepts_final_plan_with_no_in_progress_items() {
    let args = serde_json::from_value::<UpdatePlanArgs>(json!({
        "explanation": "finished",
        "plan": [
            {"step": "one", "status": "completed"},
            {"step": "two", "status": "completed"}
        ]
    }))
    .expect("final plan should deserialize");

    assert_eq!(args.explanation.as_deref(), Some("finished"));
    assert_eq!(args.plan.len(), 2);
    assert!(
        args.plan
            .iter()
            .all(|item| item.status == StepStatus::Completed)
    );
}

#[test]
fn update_plan_rejects_blank_steps() {
    for step in ["", " ", "\n\t"] {
        let err = serde_json::from_value::<UpdatePlanArgs>(json!({
            "plan": [{"step": step, "status": "pending"}]
        }))
        .expect_err("blank step should fail");

        assert!(err.to_string().contains("plan step cannot be empty"));
    }
}

#[test]
fn update_plan_accepts_empty_plan_for_existing_clear_semantics() {
    let args = serde_json::from_value::<UpdatePlanArgs>(json!({"plan": []}))
        .expect("empty plans remain wire-compatible");

    assert!(args.plan.is_empty());
}

#[test]
fn update_plan_rejects_unknown_root_fields() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [],
        "explaination": "typo"
    }))
    .expect_err("unknown root field should fail");

    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn update_plan_rejects_unknown_item_fields() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [{"step": "one", "status": "pending", "state": "pending"}]
    }))
    .expect_err("unknown item field should fail");

    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn update_plan_accepts_evidence_backed_step_metadata_and_states() {
    let args = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [
            {
                "id": "inspect",
                "step": "Inspect the owner",
                "status": "passed",
                "acceptance_criteria": ["owner identified"],
                "runtime_paths": ["src/owner.rs"]
            },
            {
                "id": "implement",
                "step": "Implement the change",
                "status": "implemented",
                "depends_on": ["inspect"],
                "generated_artifacts": ["schema/generated.json"],
                "risks": ["Desktop restart required"],
                "requires_desktop_activation": true
            }
        ]
    }))
    .expect("evidence metadata should deserialize");

    assert_eq!(args.plan[0].status, StepStatus::Passed);
    assert_eq!(args.plan[1].status, StepStatus::Implemented);
    assert_eq!(args.plan[1].depends_on, ["inspect"]);
    assert!(args.plan[1].requires_desktop_activation);
}

#[test]
fn structured_validation_route_round_trips_direct_argv_leaves() {
    let value = json!({
        "explanation": null,
        "plan": [{
            "id": "implement",
            "step": "Implement the change",
            "status": "implemented",
            "validation_route": {
                "ordering": "run_all",
                "leaves": [{
                    "argv": ["cargo", "test", "-p", "codex-core", "focused_case"],
                    "uncertainty": "the focused validation contract remains satisfied",
                    "covered_paths": ["core/src/validation_admission.rs"],
                    "covered_contracts": ["focused-validation-v1"],
                    "timeout_ms": 30000
                }]
            }
        }]
    });
    let args = serde_json::from_value::<UpdatePlanArgs>(value.clone())
        .expect("structured validation route should deserialize");

    assert_eq!(serde_json::to_value(args).expect("serialize route"), value);
}

#[test]
fn structured_validation_route_rejects_empty_argv_and_unbounded_timeouts() {
    for leaf in [
        json!({"argv": [], "timeout_ms": 1000}),
        json!({"argv": ["cargo", "test"], "timeout_ms": 0}),
        json!({
            "argv": ["cargo", "test"],
            "timeout_ms": MAX_STRUCTURED_VALIDATION_TIMEOUT_MS + 1
        }),
    ] {
        let error = serde_json::from_value::<ValidationRoute>(json!({"leaves": [leaf]}))
            .expect_err("inadmissible route leaf should fail");
        assert!(
            error.to_string().contains("validation route"),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn update_plan_rejects_missing_dependency_targets() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [{
            "id": "implement",
            "step": "Implement",
            "status": "pending",
            "depends_on": ["inspect"]
        }]
    }))
    .expect_err("missing dependency should fail");

    assert!(
        err.to_string()
            .contains("does not name an explicit step id")
    );
}

#[test]
fn update_plan_rejects_active_self_dependency() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [{
            "id": "implement",
            "step": "Implement",
            "status": "pending",
            "depends_on": ["implement"]
        }]
    }))
    .expect_err("active self dependency should fail");

    assert!(err.to_string().contains("cannot depend on itself"));
}

#[test]
fn update_plan_rejects_dependency_cycles() {
    let err = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [
            {"id": "one", "step": "one", "status": "pending", "depends_on": ["two"]},
            {"id": "two", "step": "two", "status": "pending", "depends_on": ["one"]}
        ]
    }))
    .expect_err("dependency cycle should fail");

    assert!(err.to_string().contains("dependency cycle"));
}

#[test]
fn update_plan_accepts_dependencies_declared_only_by_skipped_items() {
    let args = serde_json::from_value::<UpdatePlanArgs>(json!({
        "plan": [
            {
                "id": "one",
                "step": "one",
                "status": "skipped",
                "depends_on": ["two"]
            },
            {
                "id": "two",
                "step": "two",
                "status": "skipped",
                "depends_on": ["one"]
            },
            {
                "id": "self",
                "step": "self",
                "status": "skipped",
                "depends_on": ["self"]
            },
            {
                "id": "missing",
                "step": "missing",
                "status": "skipped",
                "depends_on": ["not-present"]
            }
        ]
    }))
    .expect("skipped dependencies should not constrain completion");

    assert_eq!(args.plan.len(), 4);
    assert!(
        args.plan
            .iter()
            .all(|item| item.status == StepStatus::Skipped)
    );
}
