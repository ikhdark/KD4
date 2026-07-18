use chrono::Duration;
use chrono::Utc;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

use super::*;

struct Fixture {
    _codex_home: TempDir,
    repo: TempDir,
    state: Arc<StateRuntime>,
    store: LocalAgentTaskStore,
}

impl Fixture {
    async fn new() -> Self {
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime initializes");
        let store = LocalAgentTaskStore::initialize(&state)
            .await
            .expect("task store initializes");
        Self {
            _codex_home: codex_home,
            repo,
            state,
            store,
        }
    }
}

fn criterion() -> AcceptanceCriterion {
    AcceptanceCriterion {
        id: "criterion-1".to_string(),
        text: "the requested behavior is proven".to_string(),
    }
}

fn worker_draft(root_session_id: &str, scope: &str) -> AssignmentDraft {
    AssignmentDraft {
        root_session_id: root_session_id.to_string(),
        role: AgentRole::Worker,
        capability_profile: CapabilityProfile::ScopedSourceWrite,
        objective: "implement the bounded change".to_string(),
        acceptance_criteria: vec![criterion()],
        read_scope: Vec::new(),
        write_scope: vec![RepoScope {
            path: scope.to_string(),
            recursive: true,
        }],
        stop_condition: "stop after focused validation".to_string(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        relation: None,
    }
}

fn completed_receipt(validation_call_ids: Vec<String>) -> ReceiptDraft {
    ReceiptDraft {
        status: AgentStatusClaim::Completed,
        summary: "completed and validated".to_string(),
        criterion_results: vec![CriterionResult {
            criterion_id: criterion().id,
            status: CriterionStatus::Passed,
            evidence: Some("focused validation passed".to_string()),
        }],
        declared_changes: Vec::new(),
        validation_call_ids,
        blockers: Vec::new(),
        risks: Vec::new(),
        next_action: None,
    }
}

#[test]
fn ids_and_scope_validation_are_strict() {
    assert_eq!(AssignmentId::new().as_uuid().get_version_num(), 7);
    assert!(AssignmentId::try_from(Uuid::new_v4()).is_err());
    let repo = TempDir::new().expect("repository tempdir");
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[RepoScope {
                path: repo.path().display().to_string(),
                recursive: false,
            }]
        )
        .is_err()
    );
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[RepoScope {
                path: "../outside".to_string(),
                recursive: false,
            }]
        )
        .is_err()
    );
    assert!(
        normalize_repo_scopes(
            repo.path(),
            &[
                RepoScope {
                    path: "src".to_string(),
                    recursive: false,
                },
                RepoScope {
                    path: "src".to_string(),
                    recursive: true,
                },
            ]
        )
        .is_err()
    );
}

#[test]
fn reviewer_and_verifier_invariants_are_enforced() {
    let repo = TempDir::new().expect("repository tempdir");
    let target = AssignmentId::new();
    let mut draft = worker_draft("root", "src");
    draft.role = AgentRole::Reviewer;
    draft.capability_profile = CapabilityProfile::ReadSearchDiff;
    draft.dependencies = vec![target];
    draft.relation = Some(AssignmentRelation {
        kind: RelationKind::Review,
        target_assignment_ids: vec![target],
    });
    assert!(draft.clone().normalize(repo.path()).is_err());
    draft.write_scope.clear();
    assert!(draft.normalize(repo.path()).is_ok());
}

#[tokio::test]
async fn dependency_validation_returns_every_blocker() {
    let fixture = Fixture::new().await;
    let (incomplete, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "first"))
        .await
        .expect("incomplete dependency assignment");
    let unknown = AssignmentId::new();
    let error = fixture
        .store
        .validate_dependencies(AssignmentId::new(), &[incomplete.assignment_id, unknown])
        .await
        .expect_err("both dependencies block");
    let StoreError::DependencyBlocked { blockers } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(blockers.len(), 2);
    assert_eq!(
        blockers
            .iter()
            .map(|blocker| blocker.state)
            .collect::<Vec<_>>(),
        vec![DependencyState::Incomplete, DependencyState::Unknown]
    );
}

#[tokio::test]
async fn receipts_are_sealed_and_validation_calls_are_attempt_owned() {
    let fixture = Fixture::new().await;
    let (first, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "first"))
        .await
        .expect("first assignment");
    let (_, second_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "second"))
        .await
        .expect("second assignment");
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "call-1".to_string(),
            attempt_id: first_attempt.attempt_id,
            command_summary: "focused test".to_string(),
            status: ValidationCallStatus::Succeeded,
            recorded_at: Utc::now(),
        })
        .await
        .expect("validation call records");
    assert!(
        matches!(
            fixture
                .store
                .submit_agent_receipt(
                    second_attempt.attempt_id,
                    completed_receipt(vec!["call-1".to_string()]),
                )
                .await,
            Err(StoreError::ValidationCallOwnership { .. })
        ),
        "cross-attempt validation call must be rejected"
    );
    fixture
        .store
        .submit_agent_receipt(
            first_attempt.attempt_id,
            completed_receipt(vec!["call-1".to_string()]),
        )
        .await
        .expect("owned validation call seals receipt");
    assert!(
        fixture
            .store
            .submit_agent_receipt(first_attempt.attempt_id, completed_receipt(Vec::new()))
            .await
            .is_err()
    );
    let task = fixture
        .store
        .get_agent_task(first.assignment_id, Some(100))
        .await
        .expect("task reloads");
    assert_eq!(
        task.receipt.expect("sealed receipt").status,
        AgentStatusClaim::Completed
    );
}

#[tokio::test]
async fn validation_calls_allow_only_running_to_terminal_transitions() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    let started_at = Utc::now();
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "transition".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            status: ValidationCallStatus::Running,
            recorded_at: started_at,
        })
        .await
        .expect("running call records");
    fixture
        .store
        .record_validation_call(ValidationCall {
            call_id: "transition".to_string(),
            attempt_id: attempt.attempt_id,
            command_summary: "focused test".to_string(),
            status: ValidationCallStatus::Succeeded,
            recorded_at: started_at + Duration::seconds(1),
        })
        .await
        .expect("running call becomes terminal");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "transition".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "focused test".to_string(),
                status: ValidationCallStatus::Failed,
                recorded_at: started_at + Duration::seconds(2),
            })
            .await,
        Err(StoreError::ValidationCallImmutable(_))
    ));

    for (call_id, status) in [
        ("still-running", ValidationCallStatus::Running),
        ("failed", ValidationCallStatus::Failed),
    ] {
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: call_id.to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "additional focused test".to_string(),
                status,
                recorded_at: started_at + Duration::seconds(3),
            })
            .await
            .expect("additional validation call records");
    }
    let error = fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["still-running".to_string(), "failed".to_string()]),
        )
        .await
        .expect_err("completed receipt rejects non-successful calls");
    let StoreError::ValidationCallStatusInvalid { call_ids } = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(
        call_ids,
        vec!["still-running".to_string(), "failed".to_string()]
    );
    fixture
        .store
        .submit_agent_receipt(
            attempt.attempt_id,
            completed_receipt(vec!["transition".to_string()]),
        )
        .await
        .expect("successful terminal call seals receipt");
    assert!(matches!(
        fixture
            .store
            .record_validation_call(ValidationCall {
                call_id: "after-seal".to_string(),
                attempt_id: attempt.attempt_id,
                command_summary: "too late".to_string(),
                status: ValidationCallStatus::Succeeded,
                recorded_at: Utc::now(),
            })
            .await,
        Err(StoreError::AttemptNotActive(_))
    ));
}

#[tokio::test]
async fn agent_task_bindings_persist_and_are_root_session_scoped() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("binding-root", "src"))
        .await
        .expect("worker assignment");
    let expected = fixture
        .store
        .bind_agent_task(AgentTaskBindingDraft {
            assignment_id: assignment.assignment_id,
            attempt_id: attempt.attempt_id,
            agent_path: "/root/worker".to_string(),
            task_name: "worker".to_string(),
            thread_id: Some("thread-1".to_string()),
        })
        .await
        .expect("binding persists");
    assert_eq!(
        fixture
            .store
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .expect("binding lookup"),
        Some(expected.clone())
    );
    assert_eq!(
        fixture
            .store
            .list_agent_task_bindings("binding-root".to_string(), None)
            .await
            .expect("binding list"),
        vec![expected.clone()]
    );

    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("store restarts");
    assert_eq!(
        restarted
            .get_agent_task_binding(assignment.assignment_id)
            .await
            .expect("binding survives restart"),
        Some(expected)
    );
}

#[tokio::test]
async fn correction_attempt_is_immutable_and_bounded_to_one() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    fixture
        .store
        .set_agent_gate(
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "cold review required".to_string(),
        )
        .await
        .expect("pending gate");
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("worker receipt");
    fixture
        .store
        .set_agent_gate(
            assignment.assignment_id,
            GateKind::Review,
            GateStatus::ChangesRequested,
            "one correction is required".to_string(),
        )
        .await
        .expect("changes requested");
    let amendment = AttemptAmendment {
        reason: "address cold review finding".to_string(),
        objective: None,
        acceptance_criteria: None,
        stop_condition: None,
    };
    let correction = fixture
        .store
        .amend_agent_task(TaskActor::Root, assignment.assignment_id, amendment.clone())
        .await
        .expect("single correction attempt");
    assert_eq!(correction.ordinal, 1);
    assert_eq!(correction.amendment, Some(amendment.clone()));
    assert!(matches!(
        fixture
            .store
            .begin_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                "src/repaired.rs".to_string(),
                AttributionConfidence::Definitive,
            )
            .await,
        Err(StoreError::AttemptNotActive(_))
    ));
    fixture
        .store
        .begin_mutation(
            correction.attempt_id,
            fixture.repo.path(),
            "src/repaired.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("correction atomically reacquires the write claim");
    assert!(matches!(
        fixture
            .store
            .amend_agent_task(TaskActor::Root, assignment.assignment_id, amendment)
            .await,
        Err(StoreError::AmendmentLimitReached(_))
    ));
}

#[tokio::test]
async fn wake_stream_is_bounded_non_draining_and_rebuilt() {
    let fixture = Fixture::new().await;
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("wake-root", "src"))
        .await
        .expect("assignment");
    for index in 0..260 {
        fixture
            .store
            .append_observation(
                attempt.attempt_id,
                ObservationKind::Reading,
                format!("observation {index}"),
                None,
            )
            .await
            .expect("observation appends");
    }
    let first = fixture
        .store
        .read_wake_events("wake-root".to_string(), None)
        .await
        .expect("wake read");
    assert_eq!(first.updated_agents.len(), MAX_WAKE_EVENTS_PER_READ);
    assert!(first.truncated_count > 0);
    let watermark = first.updated_agents.last().expect("event").event_id;
    let repeated = fixture
        .store
        .read_wake_events("wake-root".to_string(), None)
        .await
        .expect("non-draining reread");
    assert_eq!(first.updated_agents, repeated.updated_agents);

    fixture.store.close().await;
    let restarted = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("restart reconstruction");
    let after = restarted
        .read_wake_events("wake-root".to_string(), Some(watermark))
        .await
        .expect("watermarked read after restart");
    assert!(!after.updated_agents.is_empty());
    assert_ne!(after.updated_agents[0].event_id, watermark);

    let mut cursor = None;
    let mut retained_ids = HashSet::new();
    let mut retained_events = 0;
    for _ in 0..10 {
        let page = restarted
            .read_wake_events("wake-root".to_string(), cursor)
            .await
            .expect("retained page reads");
        if page.updated_agents.is_empty() {
            assert!(page.timed_out);
            break;
        }
        for event in &page.updated_agents {
            assert!(
                retained_ids.insert(event.event_id),
                "wake pagination must not duplicate events"
            );
        }
        retained_events += page.updated_agents.len();
        cursor = page.latest_event_id;
    }
    assert_eq!(retained_events, MAX_WAKE_EVENTS_PER_ROOT);
    assert_eq!(retained_ids.len(), MAX_WAKE_EVENTS_PER_ROOT);
}

#[tokio::test]
async fn integrator_supersedes_only_targeted_successful_claims() {
    let fixture = Fixture::new().await;
    let (worker, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared"))
        .await
        .expect("worker assignment");
    assert!(matches!(
        fixture
            .store
            .create_assignment(fixture.repo.path(), worker_draft("root", "shared/file.rs"))
            .await,
        Err(StoreError::WriteClaimConflict { .. })
    ));
    fixture
        .store
        .set_agent_gate(
            worker.assignment_id,
            GateKind::Review,
            GateStatus::Pending,
            "retain claim for cold review".to_string(),
        )
        .await
        .expect("pending gate");
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .expect("successful dependency receipt");
    let mut integrator = worker_draft("root", "shared");
    integrator.role = AgentRole::Integrator;
    integrator.capability_profile = CapabilityProfile::IntegratorSourceWrite;
    integrator.dependencies = vec![worker.assignment_id];
    integrator.relation = Some(AssignmentRelation {
        kind: RelationKind::Integration,
        target_assignment_ids: vec![worker.assignment_id],
    });
    fixture
        .store
        .create_assignment(fixture.repo.path(), integrator)
        .await
        .expect("targeted integrator supersedes retained worker claim");
}

#[tokio::test]
async fn write_claims_and_mutations_are_bound_to_exact_repositories() {
    let fixture = Fixture::new().await;
    let other_repo = TempDir::new().expect("second repository tempdir");
    let (_, first_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "shared"))
        .await
        .expect("first repository claim");
    fixture
        .store
        .create_assignment(other_repo.path(), worker_draft("root", "shared"))
        .await
        .expect("same relative scope in another repository does not conflict");
    assert!(matches!(
        fixture
            .store
            .begin_mutation(
                first_attempt.attempt_id,
                other_repo.path(),
                "shared/file.rs".to_string(),
                AttributionConfidence::Definitive,
            )
            .await,
        Err(StoreError::RepositoryMismatch(_))
    ));
}

#[tokio::test]
async fn mutation_evidence_keeps_private_prewrite_snapshot() {
    let fixture = Fixture::new().await;
    tokio::fs::create_dir_all(fixture.repo.path().join("src"))
        .await
        .expect("source directory");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"before")
        .await
        .expect("prewrite file");
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("root", "src"))
        .await
        .expect("worker assignment");
    let event_id = fixture
        .store
        .begin_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
            AttributionConfidence::Definitive,
        )
        .await
        .expect("mutation begins");
    tokio::fs::write(fixture.repo.path().join("src/file.rs"), b"after")
        .await
        .expect("mutated file");
    let evidence = fixture
        .store
        .finalize_mutation(
            attempt.attempt_id,
            fixture.repo.path(),
            "src/file.rs".to_string(),
        )
        .await
        .expect("mutation finalizes");
    assert_eq!(evidence.mutation_event_ids, vec![event_id]);
    assert_ne!(evidence.pre_write_hash, evidence.final_hash);
    assert!(evidence.snapshot_retained);
    tokio::fs::write(
        fixture.repo.path().join("src/file.rs"),
        b"later live contents",
    )
    .await
    .expect("live file changes after evidence finalization");
    let pre_first = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::PreWrite,
            0,
            Some(2),
        )
        .await
        .expect("first prewrite snapshot chunk");
    assert_eq!(pre_first.bytes, b"be");
    assert_eq!(pre_first.total_bytes, 6);
    let pre_rest = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::PreWrite,
            pre_first.next_offset.expect("prewrite continuation"),
            Some(16),
        )
        .await
        .expect("remaining prewrite snapshot chunk");
    assert_eq!(pre_rest.bytes, b"fore");
    assert_eq!(pre_rest.next_offset, None);
    let final_snapshot = fixture
        .store
        .read_mutation_snapshot(
            attempt.attempt_id,
            "src/file.rs".to_string(),
            MutationSnapshotVersion::Final,
            0,
            None,
        )
        .await
        .expect("final snapshot remains stable");
    assert_eq!(final_snapshot.bytes, b"after");
    assert_eq!(
        fixture
            .store
            .list_mutation_evidence(attempt.attempt_id, Some(1))
            .await
            .expect("bounded evidence list"),
        vec![evidence.clone()]
    );
    assert!(matches!(
        fixture
            .store
            .finalize_mutation(
                attempt.attempt_id,
                fixture.repo.path(),
                "src/file.rs".to_string(),
            )
            .await,
        Err(StoreError::MutationAlreadyFinalized { .. })
    ));
    assert!(matches!(
        fixture
            .store
            .garbage_collect_snapshots(assignment.assignment_id, true)
            .await,
        Err(StoreError::SnapshotRetentionRequired)
    ));
}

#[test]
fn assignments_without_repository_identity_still_deserialize() {
    let repo = TempDir::new().expect("repository tempdir");
    let assignment = worker_draft("root", "src")
        .normalize(repo.path())
        .expect("assignment normalizes");
    let mut value = serde_json::to_value(assignment).expect("assignment serializes");
    value
        .as_object_mut()
        .expect("assignment object")
        .remove("repository_id");
    let decoded: Assignment = serde_json::from_value(value).expect("legacy assignment decodes");
    assert!(decoded.repository_id.is_empty());
}

#[test]
fn risk_gate_and_waiver_rules_are_deterministic() {
    let facts = RiskFacts {
        domains: BTreeSet::from([RiskDomain::Persistence]),
        non_generated_changed_files: 6,
        non_generated_changed_lines: 401,
        focused_validation_succeeded: false,
        ..RiskFacts::default()
    };
    let decision = evaluate_risk_gate(&facts);
    assert!(decision.review_required);
    assert_eq!(decision.reasons.len(), 4);
    assert!(GateKind::Review.is_waivable());
    assert!(GateKind::Verification.is_waivable());
    assert!(!GateKind::Mutation.is_waivable());
    assert!(!GateKind::Ownership.is_waivable());
}
