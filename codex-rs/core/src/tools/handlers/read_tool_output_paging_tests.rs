use super::*;

#[tokio::test]
async fn nearly_complete_recovery_uses_margin_before_reserving_page_hints() {
    let home = tempfile::tempdir().unwrap();
    let text = "source evidence\n".repeat(1_500);
    let canonical = CanonicalToolResult::text(&text);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(), "margin", &canonical,
    ).await;
    let id = artifact.artifact_id().unwrap();
    let snapshot = load_tool_output_snapshot(home.path(), "margin", &id).await.unwrap();
    let selectors = vec![ToolOutputSelector::Bytes { start: 0, end: text.len() as u64 }];
    let exact = snapshot.select(selectors.clone(), 10_000).await.unwrap();
    assert!(exact.complete);
    let budget = recovery_size(&exact).tokens() - 64;
    let recovered = drain_recovery_snapshot(
        &snapshot, selectors, budget, &CancellationToken::new(),
    ).await.unwrap();
    assert!(recovered.output.complete);
    assert!(recovered.continuation_stop.is_none());
    assert_eq!(recovered.output.results[0].text.as_deref(), Some(text.as_str()));
    assert!(recovery_envelope_fits(&recovered.output, None, budget + 128));
}

#[test]
fn unavailable_suffix_does_not_block_a_fully_retained_selection() {
    let canonical = CanonicalToolResult::text("retained evidence\n");
    let mut output = crate::tools::command_output_artifact::select_producer_snapshot(
        &canonical, "artifact", vec![ToolOutputSelector::Lines {start: 1, end: 1}], 1000,
    ).unwrap();
    output.unavailable_ranges.push(codex_tools::CanonicalByteRange::new(100, 200));
    let state = RecoveryContinuationState::new(output, 1000);
    assert_eq!(state.next_step(), ContinuationStep::Complete);
}

#[test]
fn adjacent_partial_recovery_pages_share_one_payload_envelope() {
    let canonical = CanonicalToolResult::text("first\nsecond\n");
    let mut pages = Vec::new();
    for (start, end) in [(0, 6), (6, 13)] {
        let output = crate::tools::command_output_artifact::select_producer_snapshot(
            &canonical, "artifact", vec![ToolOutputSelector::Bytes {start, end}], 1000,
        ).unwrap();
        pages.extend(output.results);
    }
    merge_adjacent_recovery_pages(&mut pages);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].text.as_deref(), Some("first\nsecond\n"));
    assert_eq!(pages[0].selector, ToolOutputSelector::Bytes {start: 0, end: 13});
}
