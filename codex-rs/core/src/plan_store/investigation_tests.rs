use super::*;
use crate::plan_store::PlanStatusUpdate;
use crate::plan_store::PlanStore;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use serde_json::json;

pub(crate) fn report() -> Investigation {
    serde_json::from_value(json!({
        "symptom": "completed turn retains a live timer",
        "phase": "investigating",
        "hypotheses": [
            {"id":"backend", "explanation":"completion is not delivered", "status":"open"},
            {"id":"desktop", "explanation":"correct completion is ignored", "status":"open"}
        ],
        "unknowns":["which side loses completion"],
        "next_observation": {
            "id":"boundary",
            "question":"what completion crosses the live transport?",
            "hypothesis_ids":["backend","desktop"],
            "expected_outcomes":["missing completion","correct matching completion"],
            "obtainable":true
        }
    }))
    .unwrap()
}

fn active() -> InvestigationState {
    let mut state = InvestigationState::default();
    let report = report();
    let progress = state.validate(&report).unwrap();
    state.commit(report, progress);
    state
}

fn cause(state: &mut InvestigationState) -> Investigation {
    state.record_observation(state.target().unwrap());
    let mut next = state.current.clone().unwrap();
    next.hypotheses[0].status = HypothesisStatus::RuledOut;
    next.hypotheses[1].status = HypothesisStatus::Established;
    next.unknowns.clear();
    next.finding = Some(Finding {
        kind: FindingKind::CauseEstablished,
        observation_id: "boundary".into(),
        hypothesis_ids: vec!["backend".into(), "desktop".into()],
        uncertainty: "which side loses completion".into(),
        evidence: "matching completion reached the receiver; its reducer retained inProgress"
            .into(),
        conclusion: "the reducer ignores terminal state for this existing turn".into(),
    });
    next.phase = Phase::Implementing;
    next
}

#[test]
fn investigation_new_observations_do_not_reset_saturation() {
    let mut state = active();
    for i in 1..=6 {
        state.record_observation(state.target().unwrap());
        let (progress, directive) = state.finish_generation().unwrap();
        assert!(!progress);
        assert_eq!(directive.is_some(), i % 3 == 0);
        if i % 3 == 0 {
            let directive = directive.unwrap();
            for field in ["hypotheses", "ruled-out", "unknowns", "obtainable"] {
                assert!(directive.contains(field), "{directive}");
            }
            assert!(state.admit("read_file", &json!({}), false, false).is_err());
            // A complete checkpoint permits another targeted observation, but
            // cannot turn the accumulated reads into causal progress.
            let report = state.current.clone().unwrap();
            let progress = state.validate(&report).unwrap();
            state.commit(report, progress);
        }
    }
    assert_eq!(state.generations_without_narrowing, 6);
}

#[test]
fn investigation_gate_requires_observed_discriminating_cause() {
    let mut state = active();
    assert!(
        state
            .admit("apply_patch", &json!("patch"), true, false)
            .is_err()
    );
    let mut plausible = report();
    plausible.phase = Phase::Implementing;
    assert!(state.validate(&plausible).is_err());
    let next = cause(&mut state);
    // Shape and plausible prose alone are insufficient.
    let mut without_observation = active();
    assert!(without_observation.validate(&next).is_err());
    let progress = state.validate(&next).unwrap();
    assert!(progress);
    state.commit(next, progress);
    assert!(
        state
            .admit("apply_patch", &json!("patch"), true, false)
            .is_ok()
    );
    assert_eq!(state.finish_generation(), Some((true, None)));
    assert_eq!(state.finish_generation(), None);
    assert!(!without_observation.finish_generation().unwrap().0);
}

#[test]
fn investigation_unavailable_boundary_stops_tools_without_claiming_a_cause() {
    let mut state = active();
    let mut next = report();
    next.phase = Phase::Blocked;
    next.next_observation.obtainable = false;
    assert!(state.validate(&next).is_err());
    next.next_observation.blocker =
        Some("the affected Desktop has no live event capture access".into());
    let progress = state.validate(&next).unwrap();
    state.commit(next, progress);
    assert!(state.admit("read_file", &json!({}), false, false).is_err());
    assert!(state.admit("update_plan", &json!({}), false, true).is_ok());
    // A later user reply may supply the missing evidence: reads resume in that
    // turn, but implementation still requires an established cause.
    state.accepted_user_input();
    assert!(state.admit("read_file", &json!({}), false, false).is_ok());
    assert!(
        state
            .admit("apply_patch", &json!("patch"), true, false)
            .is_err()
    );
}

#[test]
fn investigation_saturation_checkpoint_does_not_block_the_next_user_turn() {
    let mut state = active();
    for _ in 0..SATURATION_GENERATIONS {
        state.finish_generation();
    }
    assert!(state.admit("read_file", &json!({}), false, false).is_err());
    state.accepted_user_input();
    assert!(state.admit("read_file", &json!({}), false, false).is_ok());
    assert!(
        state
            .admit("apply_patch", &json!("patch"), true, false)
            .is_err()
    );
}

#[test]
fn investigation_exact_diagnostic_experiment_is_not_a_blanket_write_permit() {
    let mut state = active();
    state
        .current
        .as_mut()
        .unwrap()
        .next_observation
        .diagnostic_action = Some(DiagnosticAction {
        tool: "exec_command".into(),
        input: json!({"cmd":"run-isolated-boundary-probe"}),
    });
    assert!(
        state
            .admit(
                "exec_command",
                &json!({"cmd":"patch-production"}),
                true,
                false
            )
            .is_err()
    );
    assert!(
        state
            .admit(
                "exec_command",
                &json!({"cmd":"run-isolated-boundary-probe"}),
                true,
                false
            )
            .is_ok()
    );
    assert!(
        state
            .admit(
                "exec_command",
                &json!({"cmd":"run-isolated-boundary-probe"}),
                true,
                false
            )
            .is_err()
    );
}

#[test]
fn investigation_reports_cannot_rename_away_or_silently_eliminate_unknowns() {
    let state = active();
    let mut next = report();
    next.hypotheses[0].id = "another-backend".into();
    assert!(state.validate(&next).is_err());
    next = report();
    next.unknowns.clear();
    assert!(state.validate(&next).is_err());
    next = report();
    next.hypotheses[0].status = HypothesisStatus::RuledOut;
    assert!(state.validate(&next).is_err());
}

#[test]
fn investigation_schema_matches_persisted_report_and_rejects_bad_shapes() {
    let validator = jsonschema::validator_for(&schema()).unwrap();
    let good = serde_json::to_value(report()).unwrap();
    assert!(validator.is_valid(&good));
    for field in ["hypotheses", "unknowns", "next_observation"] {
        let mut invalid = good.clone();
        invalid.as_object_mut().unwrap().remove(field);
        assert!(!validator.is_valid(&invalid));
    }
}

#[tokio::test]
async fn investigation_plan_commit_is_atomic_and_restores_the_gate() {
    let store = PlanStore::default();
    let initial = store
        .update_tool(Some(Vec::new()), None, None, Some(report()))
        .await
        .unwrap();
    let invalid = store
        .update_tool(
            None,
            Some(vec![PlanStatusUpdate {
                index: 9,
                status: StepStatus::Completed,
            }]),
            None,
            Some(report()),
        )
        .await;
    assert!(invalid.is_err());
    assert_eq!(store.snapshot().await, Some(initial.current.clone()));
    let persisted = serde_json::to_string(&initial.current).unwrap();
    let resumed = PlanStore::default();
    resumed
        .restore(Some(serde_json::from_str(&persisted).unwrap()))
        .await;
    let mut state = resumed.investigation.lock().unwrap();
    assert_eq!(state.current, Some(report()));
    assert!(
        state
            .admit("apply_patch", &json!("production patch"), true, false)
            .is_err()
    );
    drop(state);
    let legacy: UpdatePlanArgs = serde_json::from_value(json!({"plan":[]})).unwrap();
    assert!(legacy.investigation.is_none());
}

#[test]
fn investigation_without_an_active_report_does_not_restrict_normal_work() {
    let mut state = InvestigationState::default();
    assert!(
        state
            .admit("apply_patch", &json!("patch"), true, false)
            .is_ok()
    );
    assert_eq!(state.finish_generation(), None);
}

#[test]
fn investigation_cancelled_work_does_not_claim_resolution_or_block_a_new_task() {
    let mut state = active();
    let mut cancelled = report();
    cancelled.phase = Phase::Cancelled;
    let progress = state.validate(&cancelled).unwrap();
    assert!(!progress);
    state.commit(cancelled, progress);
    assert!(state.admit("apply_patch", &json!("different task"), true, false).is_ok());
    assert_eq!(state.finish_generation(), None);
    let mut next = report();
    next.symptom = "a different user-requested investigation".into();
    assert!(!state.validate(&next).unwrap());
}
