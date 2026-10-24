use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn terminal_binding_retries_ignore_refresh_timestamps() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture.store.create_assignment(
        fixture.repo.path(), worker_draft("binding-refresh", "src"),
    ).await.unwrap();
    let first = bind_test_agent(&fixture.store, assignment.assignment_id, attempt.attempt_id, "binding-refresh").await;
    fixture.store.retain_terminal_outcome(first, "A".into(), "partial".into(), "A".into()).await.unwrap();
    fixture.store.acknowledge_terminal_outcome(attempt.attempt_id, "A").await.unwrap();
    let rebound = bind_test_agent(&fixture.store, assignment.assignment_id, attempt.attempt_id, "binding-refresh").await;
    for turn in ["A", "B"] {
        let status = if turn == "A" { "partial" } else { "completed" };
        assert!(fixture.store.retain_terminal_outcome(rebound.clone(), turn.into(), status.into(), turn.into()).await.unwrap());
    }
    assert!(fixture.store.retain_terminal_outcome(rebound, "B".into(), "changed".into(), "B".into()).await.is_err());
}

#[tokio::test]
async fn inspection_publication_never_requires_a_second_transaction() {
    use sha2::Digest;
    let fixture = Fixture::new().await;
    initialize_validation_repository(fixture.repo.path());
    let (_, attempt) = fixture.store.create_assignment(fixture.repo.path(),
        validation_worker_draft("atomic-inspection", "src/lib.rs", "inspect:src/lib.rs")).await.unwrap();
    let pool = sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new()
        .filename(fixture.state.codex_home().join("agent-task-coordination/agent_tasks.sqlite"))).await.unwrap();
    // The former Running -> Succeeded publication fails here after committing
    // Running. A completed acquisition must instead publish one terminal row.
    sqlx::query("CREATE TRIGGER reject_inspection_update BEFORE UPDATE ON validation_calls BEGIN SELECT RAISE(ABORT, 'no second publication'); END")
        .execute(&pool).await.unwrap();
    let path = fixture.repo.path().join("src/lib.rs");
    let bytes = std::fs::read(&path).unwrap();
    let start = fixture.store.prepare_source_inspection(attempt.attempt_id, path.to_string_lossy().into_owned()).await.unwrap().unwrap();
    let reference = fixture.store.record_source_inspection(start.clone(), "atomic".into(),
        hex::encode(sha2::Sha256::digest(&bytes)), bytes.len() as u64).await.unwrap();
    let retry = fixture.store.record_source_inspection(start, "atomic".into(),
        hex::encode(sha2::Sha256::digest(&bytes)), bytes.len() as u64).await.unwrap();
    assert_eq!(reference, retry);
    let call = fixture.store.get_validation_call(reference.call_id.clone()).await.unwrap().unwrap();
    assert_eq!(call.status, ValidationCallStatus::Succeeded);
    assert_eq!(call.evidence.lease_expires_at, None);
    let mut receipt = completed_receipt(vec![reference.call_id.clone()]);
    receipt.criterion_results[0].evidence_ref = Some(reference);
    fixture.store.submit_agent_receipt(attempt.attempt_id, receipt).await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn premature_verification_cannot_seal_before_review() {
    for waive_review in [false, true] {
        let fixture = Fixture::new().await;
        let (assignment, attempt) = fixture.store.create_assignment(
            fixture.repo.path(), worker_draft("review-order", "src")).await.unwrap();
        fixture.store.submit_agent_receipt_with_review(attempt.attempt_id,
            completed_receipt(Vec::new()), "cross-owner".into()).await.unwrap();
        assert!(fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
            GateKind::Verification, GateStatus::Passed, "too early".into()).await.is_err());
        assert!(fixture.store.waive_agent_gate(TaskActor::Root, assignment.assignment_id,
            GateKind::Verification, "too early".into()).await.is_err());
        if waive_review {
            fixture.store.waive_agent_gate(TaskActor::Root, assignment.assignment_id,
                GateKind::Review, "explicit waiver".into()).await.unwrap();
        } else {
            fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
                GateKind::Review, GateStatus::Passed, "reviewed".into()).await.unwrap();
        }
        fixture.store.set_agent_gate(TaskActor::Root, assignment.assignment_id, attempt.attempt_id,
            GateKind::Verification, GateStatus::Passed, "verified".into()).await.unwrap();
    }
}

#[tokio::test]
async fn inspection_paths_keep_internal_space_identity() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("inspection-spaces", "src");
    draft.required_evidence = vec!["inspect:src/a b.rs".into(), "inspect:src/a  b.rs".into()];
    fixture.store.create_assignment(fixture.repo.path(), draft).await.unwrap();
}

#[tokio::test]
async fn architecture_contract_rejects_unusable_evidence_before_sealing() {
    let fixture = Fixture::new().await;
    let mut draft = worker_draft("architecture-policy", "src");
    draft.role = AgentRole::Architect;
    draft.capability_profile = CapabilityProfile::ReadSearch;
    draft.read_scope = std::mem::take(&mut draft.write_scope);
    let (_, attempt) = fixture.store.create_assignment(fixture.repo.path(), draft).await.unwrap();
    for case in 0..3 {
        let mut contract = architecture_contract_for_worker("src");
        let requirements: Vec<String> = if case == 2 {
            (0..65).map(|i| format!("inspect:src/{i}.rs")).collect()
        } else if case == 0 { vec!["cargo test".into()] } else { vec!["inspect:src/lib.rs".into()] };
        contract.acceptance_criteria = requirements.iter().enumerate().map(|(i, requirement)| AcceptanceCriterion {
            id: format!("c{i}"), text: "obtain admissible evidence".into(),
            evidence_policy: Some(CriterionEvidencePolicy {
                kind: if case == 0 { CriterionEvidenceRequirement::ValidationExecution } else { CriterionEvidenceRequirement::SourceInspection },
                required_evidence: Some(requirement.clone()), freshness: EvidenceFreshness::Current,
            }),
        }).collect();
        if case != 1 { contract.required_evidence = requirements; }
        let mut receipt = completed_receipt(Vec::new());
        receipt.architecture_contract = Some(contract);
        assert!(fixture.store.submit_agent_receipt(attempt.attempt_id, receipt).await.is_err());
    }
    let mut receipt = completed_receipt(Vec::new());
    receipt.architecture_contract = Some(architecture_contract_for_worker("src"));
    fixture.store.submit_agent_receipt(attempt.attempt_id, receipt).await.unwrap();
}

#[tokio::test]
async fn corrupt_wake_authority_does_not_block_healthy_cache_rebuild() {
    let fixture = Fixture::new().await;
    for root in ["corrupt-root", "healthy-root"] {
        fixture.store.create_assignment(fixture.repo.path(), worker_draft(root, "src")).await.unwrap();
    }
    let healthy = fixture.store.read_wake_events("healthy-root".into(), None).await.unwrap();
    let pool = sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new()
        .filename(fixture.state.codex_home().join("agent-task-coordination/agent_tasks.sqlite"))).await.unwrap();
    sqlx::query("UPDATE observations SET body_json = '{' WHERE root_session_id = 'corrupt-root'").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM wake_events WHERE root_session_id = 'healthy-root'").execute(&pool).await.unwrap();
    fixture.store.close().await;
    let reopened = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
    assert_eq!(reopened.read_wake_events("healthy-root".into(), None).await.unwrap().updated_agents, healthy.updated_agents);
    assert!(reopened.read_wake_events("corrupt-root".into(), None).await.is_err());
    let preserved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations WHERE root_session_id = 'corrupt-root' AND body_json = '{'").fetch_one(&pool).await.unwrap();
    assert!(preserved > 0);
    reopened.close().await;
    pool.close().await;
}
