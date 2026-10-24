use super::*;

#[tokio::test]
async fn fitting_large_selection_needs_no_fragment_drain_and_small_limits_still_apply() {
    let home = tempfile::tempdir().unwrap();
    let text = "exact λ UTF8\r\n".repeat(20_000);
    let canonical = CanonicalToolResult::text(&text);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(home.path(), "large", &canonical).await;
    let snapshot = load_tool_output_snapshot(home.path(), "large", &artifact.artifact_id().unwrap()).await.unwrap();
    let selectors = vec![ToolOutputSelector::Bytes { start:0, end:text.len() as u64 }];
    let full = drain_recovery_snapshot_with_byte_limit(&snapshot, selectors.clone(), 229_376, 1024*1024, &CancellationToken::new()).await.unwrap();
    assert!(full.output.complete);
    assert_eq!(full.drained_continuation_pages, 0);
    assert_eq!(full.output.results[0].text.as_deref(), Some(text.as_str()));
    let limited = drain_recovery_snapshot_with_byte_limit(&snapshot, selectors, 10_000, 10_000, &CancellationToken::new()).await.unwrap();
    assert!(!limited.output.complete);
    assert!(limited.output.represented_source_bytes() <= 10_000);
    assert!(limited.continuation_stop.is_some());
}

#[tokio::test]
async fn reconstructed_overlapping_owners_do_not_advertise_redundant_budget_work() {
    let home = tempfile::tempdir().unwrap();
    let canonical = CanonicalToolResult::text("x".repeat(10_000));
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(home.path(), "overlap", &canonical).await;
    let snapshot = load_tool_output_snapshot(home.path(), "overlap", &artifact.artifact_id().unwrap()).await.unwrap();
    let result = drain_recovery_snapshot_with_byte_limit(&snapshot,
        vec![ToolOutputSelector::Bytes { start:0, end:10_000 }, ToolOutputSelector::Lines { start:1, end:1 }],
        50_000, 10_000, &CancellationToken::new()).await.unwrap();
    assert!(result.output.complete);
    assert!(result.continuation_stop.is_none());
    assert_eq!(result.output.delivered_ranges(), vec![(0,10_000)]);
}
