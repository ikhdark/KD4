use super::*;

#[tokio::test]
async fn deferred_handle_is_queryable_without_a_live_runtime_packet() {
    let home = tempfile::tempdir().unwrap();
    let id = ToolOutputArtifactId::new();
    let owner = reserve_canonical_retention(home.path(), "closed-cell-thread", id);
    assert!(matches!(load_tool_output_snapshot(home.path(), "closed-cell-thread", &id.to_string()).await,
        Err(ReadToolOutputError::StillWriting)));
    let canonical = CanonicalToolResult::json(serde_json::json!({"changes": "x".repeat(2_048)}));
    let artifact = create_canonical_output_artifact_with_id(home.path(), "closed-cell-thread", &canonical, id).await;
    assert!(artifact.complete);
    owner.settle(&artifact);
    assert!(load_tool_output_snapshot(home.path(), "closed-cell-thread", &id.to_string()).await.is_ok());
    assert!(matches!(load_tool_output_snapshot(home.path(), "another-thread", &id.to_string()).await,
        Err(ReadToolOutputError::Expired)));
}

#[tokio::test]
async fn late_retention_failure_settles_to_explicit_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let id = ToolOutputArtifactId::new();
    let owner = reserve_canonical_retention(home.path(), "thread", id);
    // The advertised handle survives loss of the accepting runtime owner.
    drop(owner);
    let result = load_tool_output_snapshot(home.path(), "thread", &id.to_string()).await;
    assert!(matches!(result, Err(ReadToolOutputError::Io(message))
        if message.contains("before settlement") && message.contains("do not replay effects")));
}
