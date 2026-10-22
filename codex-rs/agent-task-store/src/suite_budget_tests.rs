use crate::LocalAgentTaskStore;
use codex_state::StateRuntime;

#[tokio::test]
async fn root_children_resume_and_explicit_grants_share_exact_allowances() {
    let home = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(home.path().to_path_buf(), "test".into())
        .await
        .unwrap();
    let store = LocalAgentTaskStore::initialize(&runtime).await.unwrap();
    store
        .initialize_validation_task("root", None, true)
        .await
        .unwrap();
    store
        .initialize_validation_task("legacy-child", Some("root"), false)
        .await
        .unwrap();
    store
        .initialize_validation_task("typed-child", Some("root"), false)
        .await
        .unwrap();
    let scope = store.validation_task_scope("root").await.unwrap().unwrap();
    assert_eq!(
        store.validation_task_scope("legacy-child").await.unwrap(),
        Some(scope.clone())
    );
    assert_eq!(
        store.validation_task_scope("typed-child").await.unwrap(),
        Some(scope.clone())
    );
    let (parent, child) = tokio::join!(
        store.reserve_validation_suite(&scope.0, &scope.1, "parent", "parent.json", None),
        store.reserve_validation_suite(&scope.0, &scope.1, "child", "child.json", None),
    );
    assert_ne!(
        parent.unwrap(),
        child.unwrap(),
        "one atomic reservation across the whole topology"
    );
    store.close().await;
    let resumed = LocalAgentTaskStore::initialize(&runtime).await.unwrap();
    resumed
        .initialize_validation_task("root", None, false)
        .await
        .unwrap();
    assert!(
        !resumed
            .reserve_validation_suite(&scope.0, &scope.1, "resumed", "resume.json", None)
            .await
            .unwrap()
    );
    resumed
        .accept_validation_task_grant("root", "task", "grant-1")
        .await
        .unwrap();
    assert!(
        resumed
            .reserve_validation_suite("task", "grant-1", "first", "first.json", None)
            .await
            .unwrap()
    );
    resumed
        .accept_validation_task_grant("root", "task", "grant-1")
        .await
        .unwrap();
    assert!(
        !resumed
            .reserve_validation_suite("task", "grant-1", "repeat", "repeat.json", None)
            .await
            .unwrap()
    );
    resumed
        .accept_validation_task_grant("root", "task", "grant-2")
        .await
        .unwrap();
    resumed
        .inherit_validation_task_scope("delayed-child", "task", "grant-1")
        .await
        .unwrap();
    assert_eq!(
        resumed
            .validation_task_scope("delayed-child")
            .await
            .unwrap(),
        Some(("task".into(), "grant-1".into()))
    );
    assert!(
        resumed
            .reserve_validation_suite("task", "grant-2", "additional", "additional.json", None)
            .await
            .unwrap()
    );
    assert_eq!(
        resumed.validation_task_scope("legacy-child").await.unwrap(),
        Some(scope)
    );
    resumed
        .initialize_validation_task("unrelated", None, true)
        .await
        .unwrap();
    let independent = resumed
        .validation_task_scope("unrelated")
        .await
        .unwrap()
        .unwrap();
    assert!(
        resumed
            .reserve_validation_suite(
                &independent.0,
                &independent.1,
                "independent",
                "independent.json",
                None
            )
            .await
            .unwrap()
    );
    resumed
        .initialize_validation_task("old-missing-state", None, false)
        .await
        .unwrap();
    let unknown = resumed
        .validation_task_scope("old-missing-state")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !resumed
            .reserve_validation_suite(&unknown.0, &unknown.1, "unknown", "unknown.json", None)
            .await
            .unwrap()
    );
    resumed.close().await;
    runtime.close().await;
}

#[tokio::test]
async fn owned_nonexecution_refund_is_compare_and_swap_not_a_new_grant() {
    let home = tempfile::tempdir().unwrap();
    let runtime = StateRuntime::init(home.path().to_path_buf(), "test".into())
        .await
        .unwrap();
    let store = LocalAgentTaskStore::initialize(&runtime).await.unwrap();
    store
        .accept_validation_task_grant("root", "task", "grant")
        .await
        .unwrap();
    assert!(
        store
            .reserve_validation_suite("task", "grant", "busy", "busy.json", None)
            .await
            .unwrap()
    );
    assert!(
        !store
            .reserve_validation_suite("task", "grant", "wrong", "wrong.json", Some("foreign"))
            .await
            .unwrap()
    );
    assert!(
        store
            .reserve_validation_suite("task", "grant", "retry", "retry.json", Some("busy"))
            .await
            .unwrap()
    );
    assert!(
        store
            .start_validation_suite("task", "grant", "retry")
            .await
            .unwrap()
    );
    assert!(
        !store
            .reserve_validation_suite("task", "grant", "duplicate", "duplicate.json", Some("busy"))
            .await
            .unwrap()
    );
    store.close().await;
    runtime.close().await;
}
