use super::*;

#[tokio::test]
async fn small_source_byte_budgets_make_exact_progress_across_utf8_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let text = "aλ🦀z".repeat(12);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(), "small-bytes", &CanonicalToolResult::text(&text),
    ).await;
    let snapshot = load_tool_output_snapshot(home.path(), "small-bytes", &artifact.artifact_id().unwrap()).await.unwrap();
    for budget in 1..=64 {
        let mut cursor = 0;
        let mut recovered = Vec::new();
        while cursor < text.len() as u64 {
            let page = drain_recovery_snapshot_with_byte_limit(
                &snapshot, vec![ToolOutputSelector::Bytes { start: cursor, end: text.len() as u64 }],
                8_000, budget, &CancellationToken::new(),
            ).await.unwrap();
            let ranges = page.output.delivered_ranges();
            assert_eq!(ranges.len(), 1, "budget {budget}");
            assert_eq!(ranges[0].0, cursor);
            assert!(ranges[0].1 > cursor && ranges[0].1 - cursor <= budget as u64);
            for result in &page.output.results {
                if result.status != ToolOutputSelectorStatus::Ok { continue; }
                if let Some(text) = &result.text { recovered.extend_from_slice(text.as_bytes()); }
                if let Some(encoded) = &result.data_base64 {
                    recovered.extend(base64::engine::general_purpose::STANDARD.decode(encoded).unwrap());
                }
            }
            cursor = ranges[0].1;
            if cursor < text.len() as u64 {
                let stop = page.continuation_stop.as_ref().unwrap();
                assert_eq!(stop.reason, ContinuationStopReason::Budget);
                assert!(recovery_call_succeeded(&page.output, Some(stop)));
                assert_eq!(stop.selector, Some(ToolOutputSelector::Bytes { start: cursor, end: text.len() as u64 }));
            } else { assert!(page.output.complete); }
        }
        assert_eq!(recovered, text.as_bytes(), "budget {budget}");
    }
}

#[tokio::test]
async fn script_recovery_payload_cap_preserves_exact_prefix_and_remainder() {
    let home = tempfile::tempdir().unwrap();
    // Escaping must count toward the serialized cap, not only source bytes.
    let text = "\"\\\tλ\r\n".repeat(180_000);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(), "script-cap", &CanonicalToolResult::text(&text),
    ).await;
    let snapshot = load_tool_output_snapshot(home.path(), "script-cap", &artifact.artifact_id().unwrap()).await.unwrap();
    let result = drain_recovery_snapshot_with_byte_limit(
        &snapshot, vec![ToolOutputSelector::Bytes { start: 0, end: text.len() as u64 }],
        READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES / 4, READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
        &CancellationToken::new(),
    ).await.unwrap();
    let envelope = recovery_envelope(&result.output, result.continuation_stop.as_ref()).unwrap();
    assert!(serde_json::to_vec(&envelope).unwrap().len() <= READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
    assert!(!result.output.complete);
    let mut end = 0;
    for page in &result.output.results {
        if let Some(value) = &page.text {
            let range = page.canonical_range.unwrap();
            assert_eq!(range.start, end);
            assert_eq!(value, &text[range.start as usize..range.end as usize]);
            end = range.end;
        }
    }
    assert!(end > READ_TOOL_OUTPUT_MAX_BYTES as u64);
    let stop = result.continuation_stop.unwrap();
    assert!(stop.resumable);
    assert_eq!(stop.selector, Some(ToolOutputSelector::Bytes { start: end, end: text.len() as u64 }));
}

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
