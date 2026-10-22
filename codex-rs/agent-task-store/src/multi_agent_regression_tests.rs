use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn ma001_root_scope_denies_foreign_mutations_without_side_effects() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("owner", "src"))
        .await
        .unwrap();
    let before = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(100))
        .await
        .unwrap();
    let actor = TaskActor::RootSession("foreign".into());
    assert!(matches!(
        fixture
            .store
            .set_agent_gate(
                actor.clone(),
                assignment.assignment_id,
                attempt.attempt_id,
                GateKind::Review,
                GateStatus::Pending,
                "foreign".into()
            )
            .await,
        Err(StoreError::RootSessionMismatch { .. })
    ));
    assert!(matches!(
        fixture
            .store
            .waive_agent_gate(
                actor.clone(),
                assignment.assignment_id,
                GateKind::Review,
                "foreign".into()
            )
            .await,
        Err(StoreError::RootSessionMismatch { .. })
    ));
    assert!(matches!(
        fixture
            .store
            .amend_agent_task(
                actor.clone(),
                assignment.assignment_id,
                AttemptAmendment {
                    reason: "foreign".into(),
                    objective: None,
                    acceptance_criteria: None,
                    stop_condition: None
                }
            )
            .await,
        Err(StoreError::RootSessionMismatch { .. })
    ));
    assert!(matches!(
        fixture
            .store
            .abandon_agent_task(actor, assignment.assignment_id, "foreign".into())
            .await,
        Err(StoreError::RootSessionMismatch { .. })
    ));
    assert_eq!(
        before,
        fixture
            .store
            .get_agent_task(assignment.assignment_id, Some(100))
            .await
            .unwrap()
    );
    fixture
        .store
        .abandon_agent_task(
            TaskActor::RootSession("owner".into()),
            assignment.assignment_id,
            "owner".into(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn ma002_verifier_proof_is_required_before_positive_gate() {
    use sha2::Digest;
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let (target, target_attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("proof", "src"))
        .await
        .unwrap();
    fixture
        .store
        .submit_agent_receipt(target_attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .unwrap();
    let mut verifier = relation_draft("proof", AgentRole::Verifier, target.assignment_id);
    verifier.required_evidence = vec!["inspect:src/lib.rs".into()];
    let (_, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), verifier)
        .await
        .unwrap();
    let pass = || {
        fixture.store.set_agent_gate(
            TaskActor::Attempt(attempt.attempt_id),
            target.assignment_id,
            target_attempt.attempt_id,
            GateKind::Verification,
            GateStatus::Passed,
            "verified".into(),
        )
    };
    assert!(matches!(
        pass().await,
        Err(StoreError::RequiredEvidenceMissing { .. })
    ));
    assert!(
        fixture
            .store
            .get_agent_task(target.assignment_id, Some(0))
            .await
            .unwrap()
            .gates
            .is_empty()
    );
    let path = fixture.repo.path().join("src/lib.rs");
    let bytes = std::fs::read(&path).unwrap();
    let inspection = fixture
        .store
        .prepare_source_inspection(attempt.attempt_id, path.to_string_lossy().into_owned())
        .await
        .unwrap()
        .unwrap();
    let proof = fixture
        .store
        .record_source_inspection(
            inspection,
            "verification-proof".into(),
            hex::encode(sha2::Sha256::digest(&bytes)),
            bytes.len() as u64,
        )
        .await
        .unwrap();
    pass()
        .await
        .expect("owned complete evidence permits an active verifier to pass the gate");
    let mut receipt = completed_receipt(vec![proof.call_id]);
    receipt.criterion_results[0].evidence_ref = None;
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, receipt)
        .await
        .unwrap();
}

#[tokio::test]
async fn ma003_dependency_admission_keeps_the_exact_accepted_receipt() {
    let fixture = Fixture::new().await;
    let mut readiness = worker_draft("versions", "src");
    readiness.objective = "report prerequisite readiness".into();
    readiness.acceptance_criteria[0].text = "report the readiness observation".into();
    readiness.acceptance_criteria[0].evidence_policy = Some(CriterionEvidencePolicy {
        kind: CriterionEvidenceRequirement::ReportOnly,
        required_evidence: None,
        freshness: EvidenceFreshness::Current,
    });
    let (prerequisite, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), readiness)
        .await
        .unwrap();
    fixture
        .store
        .submit_agent_receipt(attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .unwrap();
    let mut dependent = worker_draft("versions", "docs");
    dependent.dependencies = vec![prerequisite.assignment_id];
    let (dependent, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), dependent)
        .await
        .unwrap();
    let before = fixture
        .store
        .get_agent_task(dependent.assignment_id, Some(0))
        .await
        .unwrap()
        .dependency_admissions;
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].attempt_id, attempt.attempt_id);
    assert_eq!(before[0].receipt_sha256.len(), 64);
    fixture
        .store
        .set_agent_gate(
            TaskActor::RootSession("versions".into()),
            prerequisite.assignment_id,
            attempt.attempt_id,
            GateKind::Review,
            GateStatus::Failed,
            "retracted".into(),
        )
        .await
        .unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    assert_eq!(
        before,
        reopened
            .get_agent_task(dependent.assignment_id, Some(0))
            .await
            .unwrap()
            .dependency_admissions
    );
}

#[tokio::test]
async fn ma004_launch_binding_is_committed_with_admission() {
    let fixture = Fixture::new().await;
    let admitted = fixture
        .store
        .create_admitted_assignment_for_agent(
            fixture.repo.path(),
            selective_worker_draft("launch", "src/lib.rs", &[]),
            true,
            "/root/worker".into(),
            "worker".into(),
            fixture.state.agent_job_runner_instance_id().into(),
        )
        .await
        .unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    let bindings = reopened
        .list_agent_task_bindings("launch".into(), None)
        .await
        .unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].assignment_id, admitted.assignment.assignment_id);
    assert_eq!(bindings[0].attempt_id, admitted.attempt.attempt_id);
    assert_eq!(bindings[0].thread_id, None);
    let unstarted = reopened.unstarted_assignments("launch").await.unwrap();
    assert_eq!(
        unstarted[0].1.as_deref(),
        Some(fixture.state.agent_job_runner_instance_id())
    );
}

#[tokio::test]
async fn ma009_unpublished_writing_files_are_not_recovery_candidates() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("capsule", "src"))
        .await
        .unwrap();
    let dir = fixture
        .state
        .codex_home()
        .join("agent-task-coordination")
        .join("task_capsules");
    std::fs::create_dir_all(&dir).unwrap();
    let torn = dir.join(format!(".{}.torn.writing", assignment.assignment_id));
    std::fs::write(&torn, [0xff, 0]).unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    let payload = serde_json::to_string(&audit_capsule(&assignment, &attempt)).unwrap();
    reopened
        .attach_task_capsule(assignment.assignment_id, attempt.attempt_id, payload)
        .await
        .unwrap();
    assert_eq!(std::fs::read(torn).unwrap(), [0xff, 0]);
}

#[tokio::test]
async fn ma016_revision_fences_changes_from_independent_store_instances() {
    let fixture = Fixture::new().await;
    let other = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    let before = fixture.store.task_state_revision().await.unwrap();
    let (assignment, attempt) = other
        .create_assignment(fixture.repo.path(), worker_draft("revision", "src"))
        .await
        .unwrap();
    let admitted = fixture.store.task_state_revision().await.unwrap();
    assert!(admitted > before);
    other
        .set_agent_gate(
            TaskActor::RootSession("revision".into()),
            assignment.assignment_id,
            attempt.attempt_id,
            GateKind::Review,
            GateStatus::Pending,
            "revision fence".into(),
        )
        .await
        .unwrap();
    assert!(fixture.store.task_state_revision().await.unwrap() > admitted);
}
#[tokio::test]
async fn ma011_architect_reviewers_are_not_forced_to_consume_worker_contracts() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("architecture", "src");
    draft.role = AgentRole::Architect;
    draft.capability_profile = CapabilityProfile::ReadSearch;
    draft.read_scope = draft.write_scope.clone();
    draft.write_scope.clear();
    let admitted = fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), draft, true)
        .await
        .unwrap();
    let mut receipt = completed_receipt(Vec::new());
    receipt.architecture_contract = Some(architecture_contract_for_worker("src"));
    fixture
        .store
        .submit_agent_receipt_with_review(
            admitted.attempt.attempt_id,
            receipt,
            "independent architecture review".into(),
        )
        .await
        .unwrap();
    let reviewer = fixture
        .store
        .create_admitted_assignment(
            fixture.repo.path(),
            relation_draft(
                "architecture",
                AgentRole::Reviewer,
                admitted.assignment.assignment_id,
            ),
            true,
        )
        .await
        .expect("reviewing an Architect is not Worker contract consumption");
    fixture
        .store
        .set_agent_gate(
            TaskActor::Attempt(reviewer.attempt.attempt_id),
            admitted.assignment.assignment_id,
            admitted.attempt.attempt_id,
            GateKind::Review,
            GateStatus::Passed,
            "architecture reviewed".into(),
        )
        .await
        .unwrap();
    fixture
        .store
        .submit_agent_receipt(reviewer.attempt.attempt_id, completed_receipt(Vec::new()))
        .await
        .unwrap();
    let mut verifier = relation_draft(
        "architecture",
        AgentRole::Verifier,
        admitted.assignment.assignment_id,
    );
    verifier.required_evidence = vec!["focused architecture proof".into()];
    fixture
        .store
        .create_admitted_assignment(fixture.repo.path(), verifier, true)
        .await
        .expect("the Verifier can be admitted without an illegal Worker-only reference");
    let mut worker = worker_draft("architecture", "src");
    worker.dependencies = vec![admitted.assignment.assignment_id];
    assert!(
        fixture
            .store
            .create_assignment(fixture.repo.path(), worker)
            .await
            .is_err(),
        "Worker contract and pending Verification checks are not weakened"
    );
}
