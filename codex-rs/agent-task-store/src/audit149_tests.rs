use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn correction_rejects_unbound_policy_without_consuming_slot_and_resets_verification() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("audit149", "src")).await.unwrap();
    fixture.store.submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new())).await.unwrap();
    fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
        GateKind::Verification, GateStatus::Passed, "original attempt verified".into()).await.unwrap();
    fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
        GateKind::Review, GateStatus::ChangesRequested, "correct source".into()).await.unwrap();
    let valid = AttemptAmendment { reason: "correct source".into(), objective: None, acceptance_criteria: None, stop_condition: None };
    let mut invalid = valid.clone();
    invalid.acceptance_criteria = Some(vec![AcceptanceCriterion {
        id: "unbound".into(), text: "inspect unavailable observation".into(),
        evidence_policy: Some(CriterionEvidencePolicy { kind: CriterionEvidenceRequirement::SourceInspection,
            required_evidence: Some("inspect:other.rs".into()), freshness: EvidenceFreshness::Current }),
    }]);
    assert!(matches!(fixture.store.amend_agent_task(TaskActor::Root, assignment.assignment_id, invalid).await,
        Err(StoreError::InvalidAssignment(_))));
    let correction = fixture.store.amend_agent_task(TaskActor::Root, assignment.assignment_id, valid).await.unwrap();
    assert_eq!(correction.ordinal, 1);
    let task = fixture.store.get_agent_task(assignment.assignment_id, Some(0)).await.unwrap();
    for kind in [GateKind::Review, GateKind::Verification] {
        assert!(task.gates.iter().any(|gate| gate.kind == kind && gate.status == GateStatus::Pending));
    }
    assert!(fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
        GateKind::Verification, GateStatus::Passed, "stale attempt".into()).await.is_err());
    fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, correction.attempt_id,
        GateKind::Verification, GateStatus::Passed, "correction verified".into()).await.unwrap();
}

#[tokio::test]
async fn distinct_terminal_turns_survive_old_acknowledgment_and_identical_retries() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("terminal149", "src")).await.unwrap();
    let binding = bind_test_agent(&fixture.store, assignment.assignment_id, attempt.attempt_id, "terminal149").await;
    for (turn, status) in [("A", "partial"), ("B", "completed"), ("A", "partial")] {
        assert!(fixture.store.retain_terminal_outcome(binding.clone(), turn.into(), status.into(), turn.into()).await.unwrap());
    }
    assert!(fixture.store.retain_terminal_outcome(binding.clone(), "B".into(), "conflicting".into(), "B".into()).await.is_err());
    fixture.store.acknowledge_terminal_outcome(attempt.attempt_id, "A").await.unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
    let pending = reopened.pending_terminal_outcomes("terminal149").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.into_iter().next().unwrap().unwrap().turn_id, "B");
    reopened.acknowledge_terminal_outcome(attempt.attempt_id, "B").await.unwrap();
    assert!(reopened.pending_terminal_outcomes("terminal149").await.unwrap().is_empty());
    reopened.close().await;
}

#[tokio::test]
async fn current_source_capacity_is_checked_at_admission_by_distinct_path() {
    let fixture = Fixture::new().await;
    let draft = |count| {
        let mut draft = worker_draft("capacity149", "src");
        draft.required_evidence = (0..count).map(|i| format!("inspect:src/file{i}.rs")).collect();
        draft.acceptance_criteria = draft.required_evidence.iter().enumerate().map(|(i, required)| AcceptanceCriterion {
            id: format!("c{i}"), text: "inspect current source".into(),
            evidence_policy: Some(CriterionEvidencePolicy { kind: CriterionEvidenceRequirement::SourceInspection,
                required_evidence: Some(required.clone()), freshness: EvidenceFreshness::Current }),
        }).collect();
        draft
    };
    assert!(draft(64).normalize(fixture.repo.path()).is_ok());
    assert!(matches!(draft(65).normalize(fixture.repo.path()), Err(StoreError::InvalidAssignment(message)) if message.contains("64 distinct")));
    let mut shared = draft(1);
    shared.acceptance_criteria = (0..65).map(|i| {
        let mut criterion = shared.acceptance_criteria[0].clone(); criterion.id = format!("shared{i}"); criterion
    }).collect();
    assert!(shared.normalize(fixture.repo.path()).is_ok());
}

#[tokio::test]
async fn verifier_gate_uses_the_same_current_source_assessor_as_its_receipt() {
    use sha2::Digest;
    for drift in [false, true] {
        let fixture = Fixture::new().await;
        initialize_validation_repository(fixture.repo.path());
        let (worker, worker_attempt) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("verify149", "src")).await.unwrap();
        fixture.store.submit_agent_receipt_with_review(worker_attempt.attempt_id, completed_receipt(Vec::new()), "cross-owner".into()).await.unwrap();
        fixture.store.set_agent_gate(TaskActor::Root, worker.assignment_id, worker_attempt.attempt_id,
            GateKind::Review, GateStatus::Passed, "review passed".into()).await.unwrap();
        let mut draft = relation_draft("verify149", AgentRole::Verifier, worker.assignment_id);
        draft.required_evidence = vec!["inspect:src/lib.rs".into()];
        draft.acceptance_criteria[0].evidence_policy = Some(CriterionEvidencePolicy {
            kind: CriterionEvidenceRequirement::SourceInspection, required_evidence: Some("inspect:src/lib.rs".into()), freshness: EvidenceFreshness::Current,
        });
        let (_, verifier) = fixture.store.create_assignment(fixture.repo.path(), draft).await.unwrap();
        let path = fixture.repo.path().join("src/lib.rs");
        let pending = fixture.store.prepare_source_inspection(verifier.attempt_id, path.to_string_lossy().into_owned()).await.unwrap().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let reference = fixture.store.record_source_inspection(pending, "verifier-read".into(), hex::encode(sha2::Sha256::digest(&bytes)), bytes.len() as u64).await.unwrap();
        if drift { std::fs::write(&path, "pub fn changed() {}\n").unwrap(); }
        let gate = fixture.store.set_agent_gate(TaskActor::Attempt(verifier.attempt_id), worker.assignment_id, worker_attempt.attempt_id,
            GateKind::Verification, GateStatus::Passed, "source checked".into()).await;
        assert_eq!(gate.is_ok(), !drift);
        if drift {
            let mut receipt = completed_receipt(vec![reference.call_id.clone()]);
            receipt.criterion_results[0].evidence_ref = Some(reference);
            assert!(fixture.store.submit_agent_receipt(verifier.attempt_id, receipt).await.is_err());
        }
    }
}
