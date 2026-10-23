use super::*;

fn explorer(root: &Path) -> Assignment {
    AssignmentDraft {
        root_session_id: "audit151-root".into(),
        admission_origin: crate::AssignmentAdmissionOrigin::Typed,
        role: AgentRole::Explorer,
        capability_profile: AgentRole::Explorer.capability_profile(),
        objective: "Inspect concurrency ownership".into(),
        acceptance_criteria: vec![crate::AcceptanceCriterion {
            id: "report".into(),
            text: "Report findings".into(),
            evidence_policy: None,
        }],
        read_scope: vec![RepoScope {
            path: ".".into(),
            recursive: true,
        }],
        write_scope: Vec::new(),
        stop_condition: "Read the complete assigned scope".into(),
        dependencies: Vec::new(),
        risk_hints: Vec::new(),
        required_evidence: Vec::new(),
        prohibited_changes: Vec::new(),
        contract_claims: Vec::new(),
        workspace_strategy: WorkspaceStrategy::Shared,
        relation: None,
        architecture_contract_ref: None,
    }
    .normalize(root)
    .unwrap()
}

#[tokio::test]
async fn audit151_root_revision_ignores_foreign_progress_and_tracks_owned_mutations() {
    let (_home, repo, state, store) = store_fixture().await;
    let own = store.create_admitted_assignment(repo.path(), assignment_draft(repo.path(), "root-a", None), false).await.unwrap();
    let before = store.task_state_revision_for_root("root-a").await.unwrap();
    store.create_admitted_assignment(repo.path(), assignment_draft(repo.path(), "root-b", None), false).await.unwrap();
    assert_eq!(store.task_state_revision_for_root("root-a").await.unwrap(), before);
    store.abandon_agent_task(TaskActor::RootSession("root-a".into()), own.assignment.assignment_id, "explicit abandonment".into()).await.unwrap();
    assert!(store.task_state_revision_for_root("root-a").await.unwrap() > before);
    store.close().await;
    state.close().await;
}

#[test]
fn audit151_explorer_identity_preserves_controlling_constraints() {
    let root = tempfile::tempdir().unwrap();
    let assignment = explorer(root.path());
    let original = assignment.primary_investigation_identity().unwrap();
    let mut variants = Vec::new();
    let mut changed = assignment.clone();
    changed.stop_condition = "Stop after the first finding".into();
    variants.push(changed);
    let mut changed = assignment.clone();
    changed
        .prohibited_changes
        .push("Do not inspect private inputs".into());
    variants.push(changed);
    let mut changed = assignment.clone();
    changed.risk_hints.push("storage contention".into());
    variants.push(changed);
    for changed in variants {
        assert_ne!(original, changed.primary_investigation_identity().unwrap());
    }
    let mut ordered = assignment.clone();
    ordered.risk_hints = vec!["first risk".into(), "second risk".into()];
    ordered.prohibited_changes = vec!["first restriction".into(), "second restriction".into()];
    let mut reordered = ordered.clone();
    reordered.risk_hints.reverse();
    reordered.prohibited_changes.reverse();
    assert_eq!(
        ordered.primary_investigation_identity(),
        reordered.primary_investigation_identity()
    );
    let mut dependent = assignment.clone();
    dependent.dependencies.push(AssignmentId::new());
    assert!(
        dependent.primary_investigation_identity().is_none(),
        "dependency identities without admitted versions must never certify reuse"
    );
    let mut equivalent = assignment.clone();
    equivalent.assignment_id = AssignmentId::new();
    equivalent.created_at = Utc::now();
    assert_eq!(
        original,
        equivalent.primary_investigation_identity().unwrap()
    );
}

async fn store_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Arc<StateRuntime>,
    LocalAgentTaskStore,
) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "source").unwrap();
    let state = StateRuntime::init(home.path().into(), "test".into())
        .await
        .unwrap();
    let store = LocalAgentTaskStore::initialize(&state).await.unwrap();
    (home, repo, state, store)
}

fn assignment_draft(root: &Path, owner: &str, write_scope: Option<&str>) -> AssignmentDraft {
    let mut value = serde_json::to_value(explorer(root)).unwrap();
    value["root_session_id"] = owner.into();
    if let Some(scope) = write_scope {
        value["role"] = serde_json::to_value(AgentRole::Worker).unwrap();
        value["capability_profile"] =
            serde_json::to_value(AgentRole::Worker.capability_profile()).unwrap();
        value["write_scope"] = serde_json::json!([{"path":scope,"recursive":true}]);
        value["required_evidence"] = serde_json::json!(["inspect:src/lib.rs"]);
    }
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn audit151_foreign_writer_conflict_is_redacted_without_blocking_disjoint_work() {
    let (_home, repo, state, store) = store_fixture().await;
    store
        .create_admitted_assignment(
            repo.path(),
            assignment_draft(repo.path(), "root-a", Some("src")),
            false,
        )
        .await
        .unwrap();
    let error = store
        .create_admitted_assignment(
            repo.path(),
            assignment_draft(repo.path(), "root-b", Some("src")),
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::AdmissionRejected {
            reason: AdmissionRejectionReason::ExternalWorkspaceWriter,
            reusable_assignment_id: None,
        }
    ));
    let independent = store
        .create_admitted_assignment(
            repo.path(),
            assignment_draft(repo.path(), "root-b", Some("other")),
            false,
        )
        .await
        .unwrap();
    assert_eq!(independent.integration_plan, IntegrationPlan::SingleWriter);
    store
        .create_admitted_assignment(
            repo.path(),
            assignment_draft(repo.path(), "reader", None),
            false,
        )
        .await
        .unwrap();
    store.close().await;
    state.close().await;
}

#[tokio::test]
async fn audit151_unrelated_root_commits_do_not_notify_a_subscribed_root() {
    let (_home, repo, state, store) = store_fixture().await;
    let waiter = store.durable_wake_poller.register("root-a");
    let mut receiver = waiter.wake_rx.clone();
    let mut connection = store.pool.acquire().await.unwrap();
    store
        .durable_wake_poller
        .subscriptions
        .refresh(&mut connection)
        .await
        .unwrap();
    receiver.borrow_and_update();
    store
        .create_assignment(repo.path(), assignment_draft(repo.path(), "root-b", None))
        .await
        .unwrap();
    store
        .durable_wake_poller
        .subscriptions
        .refresh(&mut connection)
        .await
        .unwrap();
    assert!(!receiver.has_changed().unwrap());
    store
        .create_assignment(repo.path(), assignment_draft(repo.path(), "root-a", None))
        .await
        .unwrap();
    store
        .durable_wake_poller
        .subscriptions
        .refresh(&mut connection)
        .await
        .unwrap();
    assert!(receiver.has_changed().unwrap());
    drop(connection);
    drop(waiter);
    store.close().await;
    state.close().await;
}

tokio::task_local! {
    pub(super) static CAPSULE_READ_PAUSE: Arc<TestSnapshotCapturePause>;
}

#[tokio::test]
async fn audit151_capsule_read_does_not_hold_the_database_writer() {
    let (_home, repo, state, store) = store_fixture().await;
    let directory = store.coordination_root.join("task_capsules");
    tokio::fs::create_dir_all(&directory).await.unwrap();
    tokio::fs::write(directory.join("blocked.json"), b"invalid retained capsule")
        .await
        .unwrap();
    let pause = Arc::new(TestSnapshotCapturePause::new());
    let reader_store = store.clone();
    let reader_pause = Arc::clone(&pause);
    let reader = tokio::spawn(async move {
        CAPSULE_READ_PAUSE
            .scope(reader_pause, reader_store.reconcile_task_capsules())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), pause.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let admission = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.create_assignment(
            repo.path(),
            assignment_draft(repo.path(), "other-root", None),
        ),
    )
    .await;
    pause.release.add_permits(1);
    reader.await.unwrap().unwrap();
    admission
        .expect("a pending capsule read must not exclude unrelated writes")
        .unwrap();
    store.close().await;
    state.close().await;
}

#[tokio::test]
async fn audit151_explorer_admission_does_not_reuse_different_constraints() {
    let (_home, repo, state, store) = store_fixture().await;
    let draft = assignment_draft(repo.path(), "root", None);
    let first = store
        .create_admitted_assignment(repo.path(), draft.clone(), false)
        .await
        .unwrap();
    assert_eq!(
        store
            .reusable_explorer_assignment(repo.path(), draft.clone())
            .await
            .unwrap(),
        None,
        "an active producer has no current observed-input binding"
    );
    let mut different = draft;
    different.stop_condition = "Stop after one file".into();
    assert_eq!(
        store
            .reusable_explorer_assignment(repo.path(), different.clone())
            .await
            .unwrap(),
        None
    );
    let second = store
        .create_admitted_assignment(repo.path(), different, false)
        .await
        .unwrap();
    assert_ne!(
        first.assignment.assignment_id,
        second.assignment.assignment_id
    );
    store.close().await;
    state.close().await;
}
