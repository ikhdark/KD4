use super::*;
use serde_json::json;

async fn apply(store: &PlanStore, mut args: serde_json::Value) -> PlanStoreUpdate {
    if let Some(snapshot) = store.execution_snapshot().await {
        args["expected_revision"] = snapshot.revision.into();
    }
    store
        .update_tool(serde_json::from_value(args).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn checklist_rename_rejects_stale_completion_atomically() {
    let store = PlanStore::default();
    let initial = apply(
        &store,
        json!({"plan":[{"step":"Run unit tests","status":"pending"}]}),
    )
    .await;
    let id = initial.lineage.step_id("Run unit tests");
    apply(
        &store,
        json!({"plan":[{"step":"Run integration tests","status":"pending","continues":[id]}]}),
    )
    .await;
    let before = store.execution_snapshot().await;
    assert!(
        store
            .update_tool(
                serde_json::from_value(json!({"set":[{"step_id":id,"status":"completed"}]}))
                    .unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(store.execution_snapshot().await, before);
}
