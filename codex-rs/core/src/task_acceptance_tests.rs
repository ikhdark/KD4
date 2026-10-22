use super::*;
use crate::agent::task_coordinator::AgentTaskCoordinator;
use crate::plan_store::AcceptedPlanInput;
use crate::plan_store::PlanStore;
use crate::plan_store::PlanToolArgs;
use serde_json::json;

fn validation(test_execution: bool) -> AcceptedEvidence {
    AcceptedEvidence::Validation {
        environment_id: "local".into(),
        paths: std::collections::BTreeSet::from([crate::tool_history::SourceDependencyV1::new(
            std::path::Path::new("consumer.rs"),
            false,
        )]),
        test_execution,
    }
}

#[tokio::test]
async fn current_but_unrelated_path_or_environment_cannot_prove_a_consumer() {
    let store = criterion_store(false).await;
    for (environment_id, path) in [("foreign", "consumer.rs"), ("local", "unrelated.rs")] {
        store.publish_acceptance_evidence(AcceptanceEvidence {
            turn_id: "task".into(),
            mutation_revision: 0,
            observations: BTreeMap::from([(
                "check:consumer".into(),
                AcceptedEvidence::Validation {
                    environment_id: environment_id.into(),
                    paths: std::collections::BTreeSet::from([
                        crate::tool_history::SourceDependencyV1::new(
                            std::path::Path::new(path),
                            false,
                        ),
                    ]),
                    test_execution: true,
                },
            )]),
        });
        assert_eq!(
            store
                .task_closure("task", 0, &AgentTaskCoordinator::default())
                .await
                .terminal_action,
            TerminalAction::Continue
        );
    }
}

async fn update(store: &PlanStore, value: serde_json::Value) {
    let args: PlanToolArgs = serde_json::from_value(value).unwrap();
    store.stage_tool(args).await.unwrap().commit();
}

async fn criterion_store(test_execution: bool) -> PlanStore {
    let store = PlanStore::default();
    store
        .record_accepted_input(
            "task",
            ["Update producer and generated consumer"].into_iter(),
        )
        .await;
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    update(&store, json!({"plan":[{"step":"Implementation", "status":"completed"}],
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":{
            "consumer":{"text":"Verify the generated consumer", "origin":input.references()[0],
                "requirement":{"kind":"validation", "environment_id":"local", "path":"consumer.rs", "test_execution":test_execution}, "evidence_ref":"check:consumer"}
        }})).await;
    store
}

#[tokio::test]
async fn completed_checklist_cannot_discharge_independent_consumer_or_compile_as_tests() {
    let store = criterion_store(true).await;
    assert!(
        store
            .execution_snapshot()
            .await
            .unwrap()
            .obligations
            .unresolved
            .is_empty()
    );
    let coordinator = AgentTaskCoordinator::default();
    let decision = store.task_closure("task", 3, &coordinator).await;
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
    assert_eq!(decision.unmet_obligations, ["consumer"]);
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(),
        mutation_revision: 3,
        observations: BTreeMap::from([("check:consumer".into(), validation(false))]),
    });
    assert_eq!(
        store
            .task_closure("task", 3, &coordinator)
            .await
            .request_state,
        RequestState::Partial
    );
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(),
        mutation_revision: 3,
        observations: BTreeMap::from([("check:consumer".into(), validation(true))]),
    });
    assert_eq!(
        store
            .task_closure("task", 3, &coordinator)
            .await
            .request_state,
        RequestState::Complete
    );
    assert_eq!(
        store
            .task_closure("task", 4, &coordinator)
            .await
            .terminal_action,
        TerminalAction::Continue
    );
}

#[tokio::test]
async fn validation_scope_compares_locators_by_platform_key() {
    let store = criterion_store(true).await;
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(),
        mutation_revision: 0,
        observations: BTreeMap::from([(
            "check:consumer".into(),
            AcceptedEvidence::Validation {
                environment_id: "local".into(),
                paths: std::collections::BTreeSet::from([
                    crate::tool_history::SourceDependencyV1::new(
                        std::path::Path::new("Consumer.rs"),
                        false,
                    ),
                ]),
                test_execution: true,
            },
        )]),
    });
    // Stored locators keep their case; only Windows comparison keys fold it.
    let expected = if cfg!(windows) {
        RequestState::Complete
    } else {
        RequestState::Partial
    };
    assert_eq!(
        store
            .task_closure("task", 0, &AgentTaskCoordinator::default())
            .await
            .request_state,
        expected
    );
}

#[tokio::test]
async fn foreign_turn_forged_pointer_and_resume_do_not_restore_passing_proof() {
    let store = criterion_store(false).await;
    let coordinator = AgentTaskCoordinator::default();
    for (turn, reference) in [("foreign", "check:consumer"), ("task", "check:invented")] {
        store.publish_acceptance_evidence(AcceptanceEvidence {
            turn_id: turn.into(),
            mutation_revision: 0,
            observations: BTreeMap::from([(reference.into(), validation(true))]),
        });
        assert_eq!(
            store
                .task_closure("task", 0, &coordinator)
                .await
                .terminal_action,
            TerminalAction::Continue
        );
    }
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    store
        .restore_reconstructed(Some(plan), Some(lineage), Some(input), &[])
        .await;
    assert_eq!(
        store
            .task_closure("task", 0, &coordinator)
            .await
            .terminal_action,
        TerminalAction::Continue
    );
}

#[tokio::test]
async fn truthful_partial_report_cannot_abandon_an_obtainable_required_branch() {
    let store = criterion_store(false).await;
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    let mut blocked = serde_json::to_value(&lineage.acceptance["consumer"]).unwrap();
    blocked["blocker"] = json!("Provider unavailable");
    let mut obtainable = blocked.clone();
    obtainable["text"] = json!("Inspect independent source");
    obtainable.as_object_mut().unwrap().remove("blocker");
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    update(&store, json!({"plan":plan.plan, "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":{"consumer":blocked,"independent":obtainable}})).await;
    let decision = store
        .task_closure("task", 0, &AgentTaskCoordinator::default())
        .await;
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
    assert_eq!(decision.unmet_obligations.len(), 2);
}

#[tokio::test]
async fn earlier_correction_survives_status_query_but_needs_current_interpretation() {
    let store = PlanStore::default();
    update(
        &store,
        json!({"plan":[{"step":"Deploy", "status":"pending"}]}),
    )
    .await;
    store
        .record_accepted_input("correction", ["Do not deploy; review only"].into_iter())
        .await;
    let correction =
        AcceptedPlanInput::from_texts("correction", ["Do not deploy; review only"].into_iter());
    store
        .record_accepted_input("status", ["What is your progress?"].into_iter())
        .await;
    let input = store.accepted_input.lock().unwrap().clone().unwrap();
    assert!(input.references().contains(&correction.references()[0]));
    let id = super::super::plan_step_id("Deploy");
    let mut args = json!({"plan":[], "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "superseded":[{"step_id":id,"reason":"User cancelled deployment"}],
        "scope_change_refs":{id.clone():correction.references()[0]}});
    assert!(
        store
            .stage_tool(serde_json::from_value(args.clone()).unwrap())
            .await
            .is_err()
    );
    args["scope_change_reviewed_revision"] = json!(input.review_revision());
    store
        .stage_tool(serde_json::from_value(args).unwrap())
        .await
        .unwrap()
        .commit();
    assert_eq!(
        store
            .execution_snapshot()
            .await
            .unwrap()
            .obligations
            .superseded,
        1
    );
}

#[tokio::test]
async fn explanation_without_contract_is_deliverable_but_not_host_certified_success() {
    let store = PlanStore::default();
    let decision = store
        .task_closure("explanation", 0, &AgentTaskCoordinator::default())
        .await;
    assert_eq!(decision.request_state, RequestState::Unknown);
    assert_eq!(decision.terminal_action, TerminalAction::Deliver);
    assert_eq!(decision.artifact_state, ArtifactState::Unverified);
}

#[tokio::test]
async fn mandatory_child_is_requeried_and_old_acceptance_cannot_survive_unavailable_current_state()
{
    let store = PlanStore::default();
    let input = AcceptedPlanInput::from_texts("task", ["Integrate required child"].into_iter());
    store
        .record_accepted_input("task", ["Integrate required child"].into_iter())
        .await;
    let assignment_id: codex_agent_task_store::AssignmentId =
        "00000000-0000-7000-8000-000000000001".parse().unwrap();
    let attempt_id: codex_agent_task_store::AttemptId =
        "00000000-0000-7000-8000-000000000002".parse().unwrap();
    update(&store, json!({"plan":[{"step":"Integrate", "status":"completed"}],
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":{
            "child":{"text":"Integrate current child receipt", "origin":input.references()[0],
                "requirement":{"kind":"assignment", "assignment_id":assignment_id, "attempt_id":attempt_id},
                "evidence_ref":"old-child"}
        }})).await;
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(),
        mutation_revision: 0,
        observations: BTreeMap::from([(
            "old-child".into(),
            AcceptedEvidence::Assignment {
                assignment_id,
                attempt_id,
            },
        )]),
    });
    let decision = store
        .task_closure("task", 0, &AgentTaskCoordinator::default())
        .await;
    assert_eq!(decision.unmet_obligations, ["child"]);
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
    let retry: codex_agent_task_store::AttemptId =
        "00000000-0000-7000-8000-000000000003".parse().unwrap();
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    let mut obligation = lineage.acceptance["child"].clone();
    obligation.requirement = EvidenceRequirement::Assignment {
        assignment_id,
        attempt_id: retry,
    };
    update(
        &store,
        json!({"plan":[{"step":"Integrate", "status":"completed"}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":{"child":obligation}}),
    )
    .await;
    assert_eq!(
        store
            .task_closure("task", 0, &AgentTaskCoordinator::default())
            .await
            .terminal_action,
        TerminalAction::Continue
    );
}
