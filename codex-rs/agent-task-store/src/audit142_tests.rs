use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn audit142_receipt_projection_and_dependency_reject_foreign_body() {
    let fixture = Fixture::new().await;
    let report_only = |root, scope| {
        let mut draft = worker_draft(root, scope);
        draft.acceptance_criteria[0].evidence_policy = Some(CriterionEvidencePolicy {
            kind: CriterionEvidenceRequirement::ReportOnly, required_evidence: None, freshness: Default::default(),
        });
        draft
    };
    let (a, aa) = fixture.store.create_assignment(fixture.repo.path(), report_only("a", "src")).await.unwrap();
    let (b, ba) = fixture.store.create_assignment(fixture.repo.path(), report_only("b", "other")).await.unwrap();
    fixture.store.submit_agent_receipt(aa.attempt_id, completed_receipt(Vec::new())).await.unwrap();
    let foreign = fixture.store.submit_agent_receipt(ba.attempt_id, completed_receipt(Vec::new())).await.unwrap();
    let mut dependent = report_only("a", "new");
    dependent.required_evidence.push("focused dependent proof".into());
    dependent.dependencies.push(a.assignment_id);
    let admitted = fixture.store.create_admitted_assignment(fixture.repo.path(), dependent.clone(), false).await.unwrap();
    let task = fixture.store.get_agent_task(admitted.assignment.assignment_id, Some(0)).await.unwrap();
    assert_eq!(task.dependency_admissions[0].attempt_id, aa.attempt_id);
    let pool = audit_pool(&fixture).await;
    let foreign_bytes = serde_json::to_string(&foreign).unwrap();
    assert!(sqlx::query("UPDATE receipts SET body_json = ? WHERE attempt_id = ?")
        .bind(&foreign_bytes).bind(aa.attempt_id.to_string()).execute(&pool).await.is_err(),
        "normal storage must reject mutation of a sealed receipt");
    // Inject persisted corruption below the immutable mutation boundary so
    // projection and dependency readers must independently reject it.
    sqlx::query("DROP TRIGGER receipts_sealed_update").execute(&pool).await.unwrap();
    sqlx::query("UPDATE receipts SET body_json = ? WHERE attempt_id = ?").bind(&foreign_bytes).bind(aa.attempt_id.to_string()).execute(&pool).await.unwrap();
    assert!(matches!(fixture.store.get_agent_task(a.assignment_id, Some(0)).await, Err(StoreError::CorruptData(_))));
    assert!(matches!(fixture.store.create_admitted_assignment(fixture.repo.path(), dependent, false).await, Err(StoreError::CorruptData(_))));
    let retained: String = sqlx::query_scalar("SELECT body_json FROM receipts WHERE attempt_id = ?").bind(aa.attempt_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(retained, foreign_bytes);
    assert_eq!(foreign.assignment_id, b.assignment_id);
}

#[tokio::test]
async fn audit142_capsule_semantics_apply_to_attachment_and_recovery() {
    let fixture = Fixture::new().await;
    let (a, aa) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("capsule", "src")).await.unwrap();
    let (_, ba) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("other", "src")).await.unwrap();
    let mut capsule = audit_capsule(&a, &aa);
    capsule.role = AgentRole::Explorer;
    assert!(matches!(fixture.store.attach_task_capsule(a.assignment_id, aa.attempt_id, serde_json::to_string(&capsule).unwrap()).await, Err(StoreError::InvalidTaskCapsule(_))));
    capsule = audit_capsule(&a, &aa);
    capsule.attempt_id = ba.attempt_id;
    let payload = serde_json::to_string(&capsule).unwrap();
    let stage = fixture.state.codex_home().join("agent-task-coordination/task_capsules").join(format!(".{}.staged.json", a.assignment_id));
    std::fs::write(&stage, &payload).unwrap();
    fixture.store.close().await;
    let reopened = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
    let task = reopened.get_agent_task(a.assignment_id, Some(0)).await.unwrap();
    assert!(task.assignment.task_capsule.is_none());
    assert_eq!(std::fs::read_to_string(stage.with_extension("invalid")).unwrap(), payload);
    let valid = serde_json::to_string(&audit_capsule(&a, &aa)).unwrap();
    reopened.attach_task_capsule(a.assignment_id, aa.attempt_id, valid.clone()).await.unwrap();
    assert_eq!(reopened.get_agent_task(a.assignment_id, Some(0)).await.unwrap().assignment.task_capsule, Some(valid));
}

#[tokio::test]
async fn audit142_corrupt_derived_wake_returns_authoritative_cursor_and_rebuilds() {
    let fixture = Fixture::new().await;
    fixture.store.create_assignment(fixture.repo.path(), worker_draft("wake", "src")).await.unwrap();
    let original = fixture.store.read_wake_events("wake".into(), None).await.unwrap();
    let pool = audit_pool(&fixture).await;
    sqlx::query("UPDATE wake_events SET body_json = '{'").execute(&pool).await.unwrap();
    let recovered = fixture.store.read_wake_events("wake".into(), None).await.unwrap();
    assert_eq!(recovered.updated_agents, original.updated_agents);
    assert!(fixture.store.read_wake_events("wake".into(), recovered.latest_event_id).await.unwrap().updated_agents.is_empty());
    fixture.store.close().await;
    let reopened = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
    let cached: String = sqlx::query_scalar("SELECT body_json FROM wake_events LIMIT 1").fetch_one(&pool).await.unwrap();
    let cached: WakeEvent = serde_json::from_str(&cached).unwrap();
    assert_eq!(Some(cached.event_id), original.latest_event_id);
    assert!(reopened.read_wake_events("foreign".into(), original.latest_event_id).await.is_err());
}

#[tokio::test]
async fn audit142_sealed_damage_does_not_block_active_admission() {
    let fixture = Fixture::new().await;
    let (old, _) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("history", "src")).await.unwrap();
    fixture.store.abandon_agent_task(TaskActor::RootSession("history".into()), old.assignment_id, "done".into()).await.unwrap();
    let pool = audit_pool(&fixture).await;
    sqlx::query("UPDATE assignments SET body_json = '{' WHERE assignment_id = ?").bind(old.assignment_id.to_string()).execute(&pool).await.unwrap();
    let active = fixture.store.create_admitted_assignment(fixture.repo.path(), explorer_draft("history", "src", "new inquiry"), false).await.unwrap();
    assert_ne!(active.assignment.assignment_id, old.assignment_id);
    assert!(fixture.store.get_agent_task(old.assignment_id, Some(0)).await.is_err());
    sqlx::query("UPDATE assignments SET body_json = '{' WHERE assignment_id = ?").bind(active.assignment.assignment_id.to_string()).execute(&pool).await.unwrap();
    assert!(fixture.store.create_admitted_assignment(fixture.repo.path(), explorer_draft("history", "src", "another inquiry"), false).await.is_err());
}

#[tokio::test]
async fn audit142_non_git_history_resolves_each_root_once_per_initialization() {
    let fixture = Fixture::new().await;
    let mut ids = Vec::new();
    for _ in 0..32 {
        let (assignment, _) = fixture.store.create_assignment(fixture.repo.path(), worker_draft("history", "src")).await.unwrap();
        fixture.store.abandon_agent_task(TaskActor::RootSession("history".into()), assignment.assignment_id, "sealed history".into()).await.unwrap();
        ids.push(assignment.assignment_id);
    }
    fixture.store.close().await;
    for _ in 0..2 {
        crate::local::TEST_LEGACY_IDENTITY_RESOLUTIONS.scope(std::cell::Cell::new(0), async {
            let reopened = LocalAgentTaskStore::initialize(&fixture.state).await.unwrap();
            assert_eq!(crate::local::TEST_LEGACY_IDENTITY_RESOLUTIONS.with(std::cell::Cell::get), 1);
            for id in &ids {
                let task = reopened.get_agent_task(*id, Some(0)).await.unwrap();
                assert_eq!(task.assignment.repository_id, task.assignment.workspace_id);
            }
            reopened.close().await;
        }).await;
    }
}

async fn audit_pool(fixture: &Fixture) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(
                fixture
                    .state
                    .codex_home()
                    .join("agent-task-coordination/agent_tasks.sqlite"),
            ),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn audit142_unstarted_recovery_retires_only_the_fenced_name_reservation() {
    let fixture = Fixture::new().await;
    let admitted = fixture.store.create_admitted_assignment_for_agent(fixture.repo.path(), worker_draft("launch", "src"), false, "/root/retry".into(), "retry".into(), "dead-owner".into()).await.unwrap();
    let before = Utc::now() + Duration::seconds(1);
    assert!(matches!(fixture.store.recover_unstarted_assignment(admitted.assignment.assignment_id, before, Some("different-owner".into())).await.unwrap(), NonproductiveRecovery::NotEligible));
    assert!(matches!(fixture.store.recover_unstarted_assignment(admitted.assignment.assignment_id, before, Some("dead-owner".into())).await.unwrap(), NonproductiveRecovery::Recovered { .. }));
    let old = fixture.store.get_agent_task(admitted.assignment.assignment_id, Some(0)).await.unwrap();
    assert_eq!(old.receipt.unwrap().status, AgentStatusClaim::Abandoned);
    assert!(fixture.store.get_agent_task_binding(admitted.assignment.assignment_id).await.unwrap().is_none());
    let new = fixture.store.create_admitted_assignment_for_agent(fixture.repo.path(), worker_draft("launch", "src"), false, "/root/retry".into(), "retry".into(), "new-owner".into()).await.unwrap();
    fixture.store.bind_agent_task(AgentTaskBindingDraft {
        assignment_id: new.assignment.assignment_id, attempt_id: new.attempt.attempt_id,
        agent_path: "/root/retry".into(), task_name: "retry".into(), thread_id: Some(Uuid::now_v7().to_string()),
    }).await.unwrap();
    assert!(matches!(fixture.store.recover_unstarted_assignment(new.assignment.assignment_id, before, Some("new-owner".into())).await.unwrap(), NonproductiveRecovery::NotEligible));
    assert!(fixture.store.get_agent_task_binding(new.assignment.assignment_id).await.unwrap().is_some());
}

#[tokio::test]
async fn audit142_concurrent_legacy_upgrade_accepts_identical_committed_identity() {
    let fixture = Fixture::new().await;
    let (assignment, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("legacy", "src"))
        .await
        .unwrap();
    fixture.store.close().await;
    std::fs::create_dir(fixture.repo.path().join(".git")).unwrap();
    let lineage = repository_lineage_id(fixture.repo.path()).unwrap();
    assert_ne!(lineage, assignment.workspace_id);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(
            crate::local::with_test_legacy_upgrade_barrier(
                barrier.clone(),
                LocalAgentTaskStore::initialize(&fixture.state)
            ),
            crate::local::with_test_legacy_upgrade_barrier(
                barrier,
                LocalAgentTaskStore::initialize(&fixture.state)
            ),
        )
    })
    .await
    .expect("upgrade contenders cannot deadlock");
    for store in [first.unwrap(), second.unwrap()] {
        let task = store
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap();
        assert_eq!(task.assignment.repository_id, lineage);
        assert_eq!(task.assignment.workspace_id, assignment.workspace_id);
        store.close().await;
    }
}

#[tokio::test]
async fn audit142_bad_pending_binding_retains_healthy_sibling_and_original_event() {
    let fixture = Fixture::new().await;
    let mut identities = Vec::new();
    for name in ["bad", "good"] {
        let (assignment, attempt) = fixture
            .store
            .create_assignment(fixture.repo.path(), worker_draft("pending", "src"))
            .await
            .unwrap();
        let binding = fixture
            .store
            .bind_agent_task(AgentTaskBindingDraft {
                assignment_id: assignment.assignment_id,
                attempt_id: attempt.attempt_id,
                agent_path: format!("/root/{name}"),
                task_name: name.into(),
                thread_id: Some(Uuid::now_v7().to_string()),
            })
            .await
            .unwrap();
        fixture
            .store
            .retain_terminal_outcome(
                binding,
                "original-turn".into(),
                "{}".into(),
                "{\"original_event\":true}".into(),
            )
            .await
            .unwrap();
        identities.push(attempt.attempt_id);
    }
    let pool = audit_pool(&fixture).await;
    sqlx::query("UPDATE pending_terminal_outcomes SET binding_json = '{' WHERE attempt_id = ?")
        .bind(identities[0].to_string())
        .execute(&pool)
        .await
        .unwrap();
    let pending = fixture
        .store
        .pending_terminal_outcomes("pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.iter().filter(|row| row.is_err()).count(), 1);
    let healthy = pending.iter().find_map(|row| row.as_ref().ok()).unwrap();
    assert_eq!(healthy.binding.attempt_id, identities[1]);
    assert_eq!(healthy.turn_id, "original-turn");
    assert_eq!(healthy.event_json, "{\"original_event\":true}");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_terminal_outcomes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 2,
        "reading a damaged batch cannot acknowledge either obligation"
    );
    pool.close().await;
    fixture.store.close().await;
}

#[tokio::test]
async fn audit142_uncommitted_published_capsule_is_not_an_attachment_receipt() {
    let fixture = Fixture::new().await;
    let (assignment, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("publication", "src"))
        .await
        .unwrap();
    let payload = serde_json::to_string(&audit_capsule(&assignment, &attempt)).unwrap();
    let path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination/task_capsules")
        .join(format!("{}.json", assignment.assignment_id));
    std::fs::write(&path, &payload).unwrap();
    let task = fixture
        .store
        .get_agent_task(assignment.assignment_id, Some(0))
        .await
        .unwrap();
    assert!(
        task.assignment.task_capsule.is_none(),
        "a sidecar cannot certify an uncommitted attachment"
    );
    fixture.store.close().await;
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .unwrap()
            .assignment
            .task_capsule
            .as_deref(),
        Some(payload.as_str()),
        "startup may reconcile a crash leftover"
    );
    reopened.close().await;
}

#[tokio::test]
#[cfg(windows)]
async fn audit142_unreadable_capsule_does_not_block_unrelated_store_start() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new().await;
    let (damaged, attempt) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("inaccessible", "src"))
        .await
        .unwrap();
    let (healthy, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("healthy", "other"))
        .await
        .unwrap();
    let payload = serde_json::to_string(&audit_capsule(&damaged, &attempt)).unwrap();
    fixture
        .store
        .attach_task_capsule(damaged.assignment_id, attempt.attempt_id, payload.clone())
        .await
        .unwrap();
    fixture.store.close().await;
    let path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination/task_capsules")
        .join(format!("{}.json", damaged.assignment_id));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("one inaccessible capsule must not disable healthy tasks");
    assert!(
        reopened
            .get_agent_task(healthy.assignment_id, Some(0))
            .await
            .is_ok()
    );
    assert!(
        reopened
            .get_agent_task(damaged.assignment_id, Some(0))
            .await
            .is_err()
    );
    drop(lock);
    assert_eq!(
        reopened
            .get_agent_task(damaged.assignment_id, Some(0))
            .await
            .unwrap()
            .assignment
            .task_capsule
            .as_deref(),
        Some(payload.as_str())
    );
    reopened.close().await;
}

#[tokio::test]
#[cfg(windows)]
async fn audit142_quarantine_access_failure_does_not_disable_healthy_tasks() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = Fixture::new().await;
    let (assignment, _) = fixture
        .store
        .create_assignment(fixture.repo.path(), worker_draft("quarantine", "src"))
        .await
        .unwrap();
    fixture.store.close().await;
    let path = fixture
        .state
        .codex_home()
        .join("agent-task-coordination/task_capsules")
        .join(format!(".{}.staged.json", assignment.assignment_id));
    std::fs::write(&path, b"{").unwrap();
    // FILE_SHARE_READ lets reconciliation inspect the invalid payload, but
    // denies the subsequent quarantine rename until this handle is released.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    let reopened = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .expect("diagnostic quarantine failure is assignment-local");
    assert!(
        reopened
            .get_agent_task(assignment.assignment_id, Some(0))
            .await
            .is_ok()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"{");
    reopened.close().await;
    drop(lock);
    let recovered = LocalAgentTaskStore::initialize(&fixture.state)
        .await
        .unwrap();
    assert!(!path.exists());
    assert_eq!(std::fs::read(path.with_extension("invalid")).unwrap(), b"{");
    recovered.close().await;
}
