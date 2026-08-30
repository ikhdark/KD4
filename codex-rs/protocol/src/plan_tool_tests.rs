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
