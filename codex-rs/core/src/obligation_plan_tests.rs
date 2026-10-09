use super::*;
use serde_json::json;

async fn apply(store: &PlanStore, mut args: serde_json::Value) -> PlanStoreUpdate {
    if let Some(snapshot) = store.execution_snapshot().await {
        args["expected_revision"] = snapshot.revision.into();
    }
    store.update_tool(serde_json::from_value(args).unwrap()).await.unwrap()
}



#[tokio::test]
async fn obligation_explicit_resolution_closes_completed_split_without_renaming_back() {
    let store = PlanStore::default();
    let initial = apply(&store, json!({"plan":[{"step":"Check A and B","status":"pending"}]})).await;
    let id = initial.lineage.step_id("Check A and B");
    let current = apply(&store, json!({"plan":[
        {"step":"Check A","status":"completed","continues":[id]},
        {"step":"Check B","status":"completed","continues":[id]}],"resolve":[id]})).await;
    // Separate children retain their own scope as well as the umbrella.
    assert!(current.lineage.obligation_summary(&current.current).unresolved.is_empty());
    store.restore_with_lineage(Some(current.current), Some(current.lineage)).await;
    assert!(store.execution_snapshot().await.unwrap().obligations.unresolved.is_empty());
}

#[tokio::test]
async fn obligation_orphan_resolution_requires_completed_descendants_atomically() {
    let store = PlanStore::default();
    let initial = apply(&store, json!({"plan":[{"step":"Verify compatibility","status":"pending"}]})).await;
    let id = initial.lineage.step_id("Verify compatibility");
    let dropped = json!({"plan":[],"superseded":[{"step_id":id,"reason":"Deferred, not completed"}]});
    let mut bypass = dropped.clone();
    bypass["resolve"] = json!([id]);
    bypass["expected_revision"] = json!(store.execution_snapshot().await.unwrap().revision);
    let before = store.snapshot_with_lineage().await;
    assert!(store.update_tool(serde_json::from_value(bypass).unwrap()).await.unwrap_err().contains("descendant"));
    assert_eq!(store.snapshot_with_lineage().await, before);

    let orphaned = apply(&store, dropped).await;
    store.restore_with_lineage(Some(orphaned.current), Some(orphaned.lineage)).await;
    for plan in [json!([]), json!([{"step":"Unrelated work","status":"completed"}])] {
        let before = store.snapshot_with_lineage().await;
        let args = json!({"plan":plan,"resolve":[id],
            "expected_revision":store.execution_snapshot().await.unwrap().revision});
        assert!(store.update_tool(serde_json::from_value(args).unwrap()).await.unwrap_err().contains("descendant"));
        assert_eq!(store.snapshot_with_lineage().await, before);
        assert_eq!(store.execution_snapshot().await.unwrap().obligations.unresolved, vec![id.clone()]);
    }
    let recovered = apply(&store, json!({"plan":[
        {"step":"Compatibility verified","status":"completed","continues":[id]}],"resolve":[id]})).await;
    assert!(recovered.lineage.obligation_summary(&recovered.current).unresolved.is_empty());
}

#[tokio::test]
async fn obligation_changed_scope_rejects_stale_stable_id_completion_atomically() {
    let store = PlanStore::default();
    let initial = apply(&store, json!({"plan":[{"step":"Run unit tests","status":"pending"}]})).await;
    let id = initial.lineage.step_id("Run unit tests");
    apply(&store, json!({"plan":[{"step":"Run integration tests","status":"pending","continues":[id]}]})).await;
    let before = store.execution_snapshot().await;
    assert!(store.update_tool(serde_json::from_value(json!({"set":[{"step_id":id,"status":"completed"}]})).unwrap()).await.is_err());
    assert_eq!(store.execution_snapshot().await, before);
}



#[tokio::test]
async fn obligation_split_retirement_reason_survives_explanation_replacement_and_resume() {
    let store = PlanStore::default();
    let initial = apply(&store, json!({"plan":[{"step":"Check A and B","status":"pending"}]})).await;
    let id = initial.lineage.step_id("Check A and B");
    let split = apply(&store, json!({"plan":[
        {"step":"Check A","status":"pending","continues":[id]},
        {"step":"Check B","status":"pending","continues":[id]}]})).await;
    let b = split.lineage.step_id("Check B");
    let kept = apply(&store, json!({"plan":[{"step":"Check A","status":"pending"}],
        "superseded":[{"step_id":b,"reason":"B is deferred, not completed"}]})).await;
    let a = kept.lineage.step_id("Check A");
    let latest = apply(&store, json!({"set":[{"step_id":a,"status":"in_progress"}],"explanation":"Continue A"})).await;
    store.restore_with_lineage(Some(latest.current), Some(latest.lineage)).await;
    let (plan, lineage) = store.snapshot_with_lineage().await.unwrap();
    assert!(lineage.active_for_plan(&plan).requirements.values().any(|requirement|
        requirement.text == "Check B" && requirement.superseded_reason.as_deref() == Some("B is deferred, not completed")));
}
