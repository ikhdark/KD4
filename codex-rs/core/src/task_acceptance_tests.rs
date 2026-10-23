use super::*;
#[path = "task_acceptance_audit149_tests.rs"]
mod audit149_regressions;
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
        passed_tests: Default::default(),
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
                    passed_tests: Default::default(),
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
async fn owned_advisory_uses_only_matching_evidence_and_does_not_repeat_resolved_blockers() {
    let store = criterion_store(false).await;
    {
        let mut current = store.current.lock().await;
        current.lineage.acceptance.get_mut("consumer").unwrap().blocker = Some("Await user-owned credentials".into());
    }
    let (before, blockers) = store.completion_advisory("task", 0).await;
    assert_eq!(before.request_state, RequestState::Blocked);
    assert_eq!(blockers, ["consumer: Await user-owned credentials"]);
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(), mutation_revision: 0,
        observations: BTreeMap::from([("check:consumer".into(), validation(false))]),
    });
    let (after, blockers) = store.completion_advisory("task", 0).await;
    assert_eq!(after.request_state, RequestState::Complete);
    assert!(blockers.is_empty(), "an old blocker is not newly unmet work");
    assert_eq!(store.completion_advisory("other turn", 0).await.0.request_state, RequestState::Blocked);
    assert_eq!(store.completion_advisory("task", 1).await.0.request_state, RequestState::Blocked);
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
                passed_tests: Default::default(),
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
async fn mandatory_child_reads_refill_a_slot_while_the_first_read_is_blocked() {
    let store = PlanStore::default();
    let input = AcceptedPlanInput::from_texts("task", ["Integrate children"].into_iter());
    store
        .record_accepted_input("task", ["Integrate children"].into_iter())
        .await;
    let ids = (1..=3)
        .map(|index| {
            format!("00000000-0000-7000-8000-{index:012}")
                .parse::<AssignmentId>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let attempt_id: AttemptId = "00000000-0000-7000-8000-000000000010".parse().unwrap();
    let acceptance = ids.iter().enumerate().map(|(index, assignment_id)| {
        (format!("child-{index}"), json!({
            "text":format!("Integrate child {index}"), "origin":input.references()[0],
            "requirement":{"kind":"assignment", "assignment_id":assignment_id, "attempt_id":attempt_id}
        }))
    }).collect::<serde_json::Map<_, _>>();
    update(
        &store,
        json!({"plan":[{"step":"Integrate", "status":"completed"}],
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":acceptance}),
    )
    .await;
    let release_first = tokio::sync::Notify::new();
    let third_started = tokio::sync::Notify::new();
    let reads = store.task_closure_with_reader("task", 0, |assignment_id| {
        let release_first = &release_first;
        let third_started = &third_started;
        let first = ids[0];
        let third = ids[2];
        async move {
            if assignment_id == first {
                release_first.notified().await;
            }
            if assignment_id == third {
                third_started.notify_one();
            }
            Err::<codex_agent_task_store::AgentTask, _>(())
        }
    });
    tokio::pin!(reads);
    tokio::select! {
        result = tokio::time::timeout(std::time::Duration::from_secs(1), third_started.notified()) => {
            result.expect("third read must start before the first read is released");
        }
        _ = &mut reads => panic!("first read must still be blocked"),
    }
    release_first.notify_one();
    let decision = reads.await;
    assert_eq!(
        decision.unmet_obligations,
        ["child-0", "child-1", "child-2"]
    );
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
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

#[tokio::test]
async fn audit149_stale_scope_review_requires_reconciliation_not_delivery() {
    let store = criterion_store(false).await;
    store
        .record_accepted_input("correction", ["Inspect only; do not deploy"].into_iter())
        .await;
    let decision = store
        .task_closure("correction", 0, &AgentTaskCoordinator::default())
        .await;
    assert_eq!(decision.request_state, RequestState::Invalidated);
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
    assert_eq!(decision.unmet_obligations, ["consumer"]);
}

#[tokio::test]
async fn audit149_invalid_inspection_ranges_are_rejected_before_publication() {
    let store = criterion_store(false).await;
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    let before = store.execution_snapshot().await.unwrap();
    for (start, end) in [(100, 10), (10, 10)] {
        let args = json!({
            "set":[{"index":0,"status":"completed"}],
            "expected_revision":before.revision,
            "acceptance_reviewed_revision":input.review_revision(),
            "acceptance":{"inspection":{
                "text":"Inspect the consumer", "origin":input.references()[0],
                "requirement":{"kind":"inspection","environment_id":"local",
                    "path":"consumer.rs","start":start,"end":end}
            }}
        });
        assert!(
            store
                .stage_tool(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
        assert_eq!(store.execution_snapshot().await.unwrap(), before);
    }
    update(
        &store,
        json!({
            "set":[{"index":0,"status":"completed"}],
            "expected_revision":before.revision,
            "acceptance_reviewed_revision":input.review_revision(),
            "acceptance":{"inspection":{
                "text":"Inspect the consumer", "origin":input.references()[0],
                "requirement":{"kind":"inspection","environment_id":"local",
                    "path":"consumer.rs","start":0,"end":10}
            }}
        }),
    )
    .await;
}
fn audit149_test_identity(name: &str) -> crate::validation::RunnerTestIdentity {
    crate::validation::RunnerTestIdentity {
        binary: "codex-core".into(),
        helpers: Vec::new(),
        test: name.into(),
    }
}

#[tokio::test]
async fn audit149_exact_required_tests_reject_unrelated_passes_and_compile_only_evidence() {
    let store = criterion_store(true).await;
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    update(&store, json!({
        "set":[{"index":0,"status":"completed"}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "acceptance_reviewed_revision":input.review_revision(),
        "acceptance":{"selected-tests":{
            "text":"Run the consumer regression", "origin":input.references()[0],
            "requirement":{"kind":"validation","environment_id":"local","path":"consumer.rs",
                "test_execution":true,"required_tests":[audit149_test_identity("consumer_regression")]},
            "evidence_ref":"check:consumer"
        }}
    })).await;
    for (executed, names, expected) in [
        (true, vec!["unrelated"], RequestState::Partial),
        (false, vec!["consumer_regression"], RequestState::Partial),
        (true, vec!["consumer_regression"], RequestState::Complete),
    ] {
        let AcceptedEvidence::Validation {
            environment_id,
            paths,
            ..
        } = validation(true)
        else {
            unreachable!();
        };
        store.publish_acceptance_evidence(AcceptanceEvidence {
            turn_id: "task".into(),
            mutation_revision: 0,
            observations: BTreeMap::from([(
                "check:consumer".into(),
                AcceptedEvidence::Validation {
                    environment_id,
                    paths,
                    test_execution: executed,
                    passed_tests: names.into_iter().map(audit149_test_identity).collect(),
                },
            )]),
        });
        assert_eq!(
            store
                .task_closure("task", 0, &AgentTaskCoordinator::default())
                .await
                .request_state,
            expected
        );
    }
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    store
        .restore_reconstructed(Some(plan), Some(lineage), Some(input), &[])
        .await;
    assert_eq!(
        store
            .task_closure("task", 0, &AgentTaskCoordinator::default())
            .await
            .terminal_action,
        TerminalAction::Continue,
        "resume must retain the selection but not restore passing proof"
    );
}

fn audit149_integrated_child() -> codex_agent_task_store::AgentTask {
    use codex_agent_task_store::*;
    let assignment_id = AssignmentId::new();
    let attempt_id = AttemptId::new();
    let now = chrono::Utc::now();
    AgentTask {
        assignment: Assignment {
            assignment_id,
            root_session_id: "root".into(),
            admission_origin: AssignmentAdmissionOrigin::Typed,
            repository_id: "repo".into(),
            workspace_id: "workspace".into(),
            role: AgentRole::Worker,
            capability_profile: CapabilityProfile::ScopedSourceWrite,
            objective: "Integrate consumer".into(),
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "consumer".into(),
                text: "Report integrated consumer".into(),
                evidence_policy: Some(CriterionEvidencePolicy {
                    kind: CriterionEvidenceRequirement::ReportOnly,
                    required_evidence: None,
                    freshness: EvidenceFreshness::Historical,
                }),
            }],
            read_scope: Vec::new(),
            write_scope: Vec::new(),
            stop_condition: "Integrated".into(),
            dependencies: Vec::new(),
            risk_hints: Vec::new(),
            required_evidence: Vec::new(),
            prohibited_changes: Vec::new(),
            contract_claims: Vec::new(),
            workspace_strategy: WorkspaceStrategy::Shared,
            start_epoch: 0,
            relation: None,
            architecture_contract_ref: None,
            integration_plan: IntegrationPlan::SingleWriter,
            benign_read_overlap_count: 0,
            task_capsule: None,
            created_at: now,
        },
        dependency_admissions: Vec::new(),
        current_attempt: Attempt {
            assignment_id,
            attempt_id,
            ordinal: 0,
            amendment: None,
            state: AttemptState::Completed,
            created_at: now,
            sealed_at: Some(now),
        },
        gates: [GateKind::Review, GateKind::Verification]
            .into_iter()
            .map(|kind| AgentGate {
                assignment_id,
                kind,
                status: GateStatus::Passed,
                reason: "Verified".into(),
                waiver_reason: None,
                evidence_epoch: 0,
                updated_at: now,
                sealed_at: Some(now),
            })
            .collect(),
        receipt: Some(AgentReceipt {
            assignment_id,
            attempt_id,
            status: AgentStatusClaim::Completed,
            summary: "Integrated".into(),
            criterion_results: vec![CriterionResult {
                criterion_id: "consumer".into(),
                status: CriterionStatus::Passed,
                evidence: Some("Integrated and reviewed".into()),
                evidence_ref: None,
            }],
            declared_changes: Vec::new(),
            validation_call_ids: Vec::new(),
            blockers: Vec::new(),
            risks: Vec::new(),
            next_action: None,
            architecture_contract: None,
            evidence_epoch: 0,
            read_scope_fingerprint: None,
            sealed_at: now,
        }),
        validation_calls: Vec::new(),
        criterion_assessments: vec![CriterionAssessment {
            criterion_id: "consumer".into(),
            satisfied: true,
            reason: "Current evidence".into(),
        }],
        workspace_status: Default::default(),
        isolation_handoff: None,
        integration_handoffs: Vec::new(),
        observations: Vec::new(),
    }
}

#[tokio::test]
async fn audit149_integrated_child_binds_host_evidence_without_a_model_round() {
    use codex_agent_task_store::{AgentStatusClaim, AssignmentAdmissionOrigin, GateStatus};
    let store = criterion_store(false).await;
    let input = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    let child = audit149_integrated_child();
    update(
        &store,
        json!({
            "set":[{"index":0,"status":"completed"}],
            "expected_revision":store.execution_snapshot().await.unwrap().revision,
            "acceptance_reviewed_revision":input.review_revision(),
            "acceptance":{"child":{
                "text":"Integrate the consumer", "origin":input.references()[0],
                "requirement":{"kind":"assignment","assignment_id":child.assignment.assignment_id,
                    "attempt_id":child.current_attempt.attempt_id}
            }}
        }),
    )
    .await;
    store.publish_acceptance_evidence(AcceptanceEvidence {
        turn_id: "task".into(),
        mutation_revision: 0,
        observations: BTreeMap::from([("check:consumer".into(), validation(true))]),
    });
    assert!(child_integrated(&child, child.current_attempt.attempt_id));
    let decision = store
        .task_closure_with_reader("task", 0, |_| {
            std::future::ready(Ok::<_, ()>(child.clone()))
        })
        .await;
    assert_eq!(decision.request_state, RequestState::Complete);
    assert_eq!(decision.evidence_refs.len(), 2);
    assert!(
        decision
            .evidence_refs
            .iter()
            .any(|reference| reference.starts_with("assignment:"))
    );
    for case in 0..4 {
        let mut changed = child.clone();
        match case {
            0 => {
                changed.assignment.admission_origin = AssignmentAdmissionOrigin::LegacyMessage {
                    parent_assignment_id: None,
                }
            }
            1 => changed.receipt.as_mut().unwrap().status = AgentStatusClaim::NeedsMain,
            2 => changed.gates[0].status = GateStatus::Pending,
            _ => changed.current_attempt.attempt_id = AttemptId::new(),
        }
        let decision = store
            .task_closure_with_reader("task", 0, |_| {
                std::future::ready(Ok::<_, ()>(changed.clone()))
            })
            .await;
        assert_eq!(decision.terminal_action, TerminalAction::Continue);
        assert_eq!(decision.unmet_obligations, ["child"]);
    }
}
#[tokio::test]
async fn audit149_retired_contract_does_not_force_review_on_a_new_reporting_turn() {
    let store = criterion_store(false).await;
    store
        .record_accepted_input("cancel", ["Cancel consumer work; explain only"].into_iter())
        .await;
    let input = store.accepted_input.lock().unwrap().clone().unwrap();
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    let mut retired = lineage.acceptance["consumer"].clone();
    retired.retired_by = Some(input.references()[0].clone());
    retired.retirement_reason = Some("User cancelled consumer work".into());
    update(
        &store,
        json!({
            "set":[{"index":0,"status":"completed"}],
            "expected_revision":store.execution_snapshot().await.unwrap().revision,
            "acceptance_reviewed_revision":input.review_revision(),
            "acceptance":{"consumer":retired}
        }),
    )
    .await;
    store
        .record_accepted_input("status", ["Explain the result"].into_iter())
        .await;
    let decision = store
        .task_closure("status", 0, &AgentTaskCoordinator::default())
        .await;
    assert_eq!(decision.request_state, RequestState::Unknown);
    assert_eq!(decision.terminal_action, TerminalAction::Deliver);
    assert!(decision.unmet_obligations.is_empty());
}

#[tokio::test]
async fn audit149_invalidated_scope_lists_only_active_criteria() {
    let store = criterion_store(false).await;
    let task = AcceptedPlanInput::from_texts(
        "task",
        ["Update producer and generated consumer"].into_iter(),
    );
    update(&store, json!({"set":[{"index":0,"status":"completed"}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "acceptance_reviewed_revision":task.review_revision(), "acceptance":{
            "docs":{"text":"Verify the generated docs", "origin":task.references()[0],
                "requirement":{"kind":"validation", "environment_id":"local", "path":"docs.md", "test_execution":false}}
        }})).await;
    store
        .record_accepted_input("cancel", ["Cancel the docs check"].into_iter())
        .await;
    let input = store.accepted_input.lock().unwrap().clone().unwrap();
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    let mut retired = lineage.acceptance["docs"].clone();
    retired.retired_by = Some(input.references()[0].clone());
    retired.retirement_reason = Some("User cancelled the docs check".into());
    update(&store, json!({"set":[{"index":0,"status":"completed"}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision,
        "acceptance_reviewed_revision":input.review_revision(), "acceptance":{"docs":retired}})).await;
    store
        .record_accepted_input("status", ["What is the progress?"].into_iter())
        .await;
    let decision = store
        .task_closure("status", 0, &AgentTaskCoordinator::default())
        .await;
    // Active scope still needs reconciliation against the newer input.
    assert_eq!(decision.request_state, RequestState::Invalidated);
    assert_eq!(decision.terminal_action, TerminalAction::Continue);
    // Retired history is not reported as open work.
    assert_eq!(decision.unmet_obligations, ["consumer"]);
}

#[tokio::test]
async fn audit149_step_id_scope_links_store_requirement_ids_and_survive_splits() {
    let store = PlanStore::default();
    let text = "Implement and document the consumer";
    store.record_accepted_input("task", [text].into_iter()).await;
    let input = AcceptedPlanInput::from_texts("task", [text].into_iter());
    update(&store, json!({"plan":[{"step":"Implement", "status":"in_progress"}]})).await;
    update(&store, json!({"plan":[{"step":"Implement", "status":"completed"},
        {"step":"Document", "status":"pending"}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision})).await;
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    let step = lineage.step_id("Document");
    let requirement = super::super::plan_step_id("Document");
    assert_ne!(step, requirement, "a step added by a revision has its own step ID");
    let criterion = |links: Vec<String>| json!({"text":"Verify the documented consumer",
        "origin":input.references()[0],
        "requirement":{"kind":"validation", "environment_id":"local", "path":"consumer.rs", "test_execution":false},
        "plan_requirement_ids":links});
    let revision = store.execution_snapshot().await.unwrap().revision;
    // Negative control: an unknown ID is still rejected without changes.
    let rejected: PlanToolArgs = serde_json::from_value(json!({"set":[{"index":0,"status":"completed"}],
        "expected_revision":revision, "acceptance_reviewed_revision":input.review_revision(),
        "acceptance":{"docs":criterion(vec!["unknown".into()])}})).unwrap();
    assert!(store.stage_tool(rejected).await.is_err());
    update(&store, json!({"set":[{"index":0,"status":"completed"}],
        "expected_revision":revision, "acceptance_reviewed_revision":input.review_revision(),
        "acceptance":{"docs":criterion(vec![step.clone()])}})).await;
    // Stored links use the namespace readiness and archive reconcile.
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    assert_eq!(lineage.acceptance["docs"].plan_requirement_ids, [requirement.clone()]);
    assert!(store.execution_snapshot().await.unwrap().obligations.unresolved.contains(&requirement));
    // Splitting the linked row carries its requirement, so the plan stays revisable.
    update(&store, json!({"plan":[{"step":"Implement", "status":"completed"},
        {"step":"Document API", "status":"pending", "continues":[step]},
        {"step":"Document CLI", "status":"pending", "continues":[step]}],
        "expected_revision":store.execution_snapshot().await.unwrap().revision})).await;
    let (_, lineage) = store.snapshot_with_lineage().await.unwrap();
    assert_eq!(lineage.acceptance["docs"].plan_requirement_ids, [requirement]);
}

#[tokio::test]
async fn audit149_archive_resolves_mapped_renamed_requirements_only() {
    let draft = super::super::plan_step_id("Draft consumer");
    let implemented = super::super::plan_step_id("Implement consumer");
    // Negative control: the unmapped original requirement stays current debt.
    for (links, unresolved) in [
        (vec![draft.clone(), implemented.clone()], Vec::<String>::new()),
        (vec![implemented.clone()], vec![draft.clone()]),
    ] {
        let store = PlanStore::default();
        store.record_accepted_input("task", ["Implement the consumer"].into_iter()).await;
        let input = AcceptedPlanInput::from_texts("task", ["Implement the consumer"].into_iter());
        update(&store, json!({"plan":[{"step":"Draft consumer", "status":"in_progress"}]})).await;
        update(&store, json!({"plan":[{"step":"Implement consumer", "status":"completed", "continues":[draft]}],
            "expected_revision":store.execution_snapshot().await.unwrap().revision})).await;
        // A rename completes the new wording only; the original needs accounting.
        assert_eq!(store.execution_snapshot().await.unwrap().obligations.unresolved, [draft.clone()]);
        update(&store, json!({"set":[{"index":0,"status":"completed"}],
            "expected_revision":store.execution_snapshot().await.unwrap().revision,
            "acceptance_reviewed_revision":input.review_revision(), "acceptance":{
                "consumer":{"text":"Verify the generated consumer", "origin":input.references()[0],
                    "requirement":{"kind":"validation", "environment_id":"local", "path":"consumer.rs", "test_execution":false},
                    "plan_requirement_ids":links, "evidence_ref":"check:consumer"}
            }})).await;
        store.publish_acceptance_evidence(AcceptanceEvidence { turn_id: "task".into(), mutation_revision: 0,
            observations: BTreeMap::from([("check:consumer".into(), validation(false))]) });
        let decision = store.task_closure("task", 0, &AgentTaskCoordinator::default()).await;
        assert_eq!(decision.request_state, RequestState::Complete);
        store.archive_completed_scope(&decision, |_| std::future::ready(Ok(()))).await.unwrap();
        assert_eq!(store.execution_snapshot().await.unwrap().obligations.unresolved, unresolved);
    }
}

#[tokio::test]
async fn audit149_archive_reports_only_changed_checklist_presentation() {
    for lagging in [true, false] {
        let store = PlanStore::default();
        store.record_accepted_input("task", ["Implement the consumer"].into_iter()).await;
        let input = AcceptedPlanInput::from_texts("task", ["Implement the consumer"].into_iter());
        let status = if lagging { "in_progress" } else { "completed" };
        let row = super::super::plan_step_id("Implementation");
        update(&store, json!({"plan":[{"step":"Implementation", "status":status}],
            "acceptance_reviewed_revision":input.review_revision(), "acceptance":{
                "consumer":{"text":"Verify the generated consumer", "origin":input.references()[0],
                    "requirement":{"kind":"validation", "environment_id":"local", "path":"consumer.rs", "test_execution":false},
                    "plan_requirement_ids":[row], "evidence_ref":"check:consumer"}
            }})).await;
        store.publish_acceptance_evidence(AcceptanceEvidence { turn_id: "task".into(), mutation_revision: 0,
            observations: BTreeMap::from([("check:consumer".into(), validation(false))]) });
        let decision = store.task_closure("task", 0, &AgentTaskCoordinator::default()).await;
        assert_eq!(decision.request_state, RequestState::Complete);
        let published = store
            .archive_completed_scope(&decision, |_| std::future::ready(Ok(())))
            .await
            .unwrap();
        // Only a changed checklist is republished, and it is the durable one.
        assert_eq!(published.is_some(), lagging);
        if let Some(plan) = published {
            assert_eq!(plan.plan[0].status, codex_protocol::plan_tool::StepStatus::Completed);
            assert_eq!(Some(plan), store.snapshot().await);
        }
    }
}
