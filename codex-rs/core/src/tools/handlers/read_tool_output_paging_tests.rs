use super::*;

#[tokio::test]
async fn search_source_byte_budget_preserves_coordinates_and_recoverable_context() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = std::sync::Arc::new(session);
    let turn = std::sync::Arc::new(turn);
    let text = format!("λ🦀{} needle\nneedle next\n", "x".repeat(20_000));
    let first_match = text.find("needle").unwrap() as u64;
    let first_line_end = text.find('\n').unwrap() as u64 + 1;
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        &turn.config.codex_home,
        &session.thread_id.to_string(),
        &CanonicalToolResult::text(&text),
    )
    .await;
    let artifact_id = artifact.artifact_id().unwrap();
    let invoke = |source, selectors: Vec<ToolOutputSelector>, max_bytes| ToolInvocation {
        session: std::sync::Arc::clone(&session),
        step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(
            &turn,
        )),
        cancellation_token: Default::default(),
        tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
        call_id: "search-byte-budget".into(),
        tool_name: ToolName::plain("read_tool_output"),
        source,
        payload: ToolPayload::Function {
            arguments: serde_json::json!({
                "artifact_id": artifact_id, "selectors": selectors, "max_bytes": max_bytes,
            })
            .to_string(),
        },
    };
    let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else {
        unreachable!()
    };
    let schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
    for script in [false, true] {
        for budget in [1usize, READ_TOOL_OUTPUT_MAX_BYTES] {
            let source = if script {
                ToolCallSource::CodeMode {
                    cell_id: "search-byte-budget".into(),
                    parent_call_id: None,
                    runtime_tool_call_id: "search".into(),
                    nested_deadline: None,
                    cancellation_cause: None,
                }
            } else {
                ToolCallSource::Direct
            };
            let search = ToolOutputSelector::Search {
                query: "needle".into(),
                enclosing: false,
                case_insensitive: false,
                start_byte: 0,
                max_results: 1,
                context_lines: 0,
            };
            let call = invoke(source.clone(), vec![search], budget);
            let result = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
            let output = result.code_mode_result(&call.payload);
            schema.validate(&output).unwrap();
            assert_eq!(output["selection_status"], "partial");
            let selected = &output["results"][0];
            assert_eq!(selected["status"], "ok");
            assert_eq!(selected["complete"], false);
            assert_eq!(selected["value"]["total_matches"], 2);
            assert_eq!(selected["value"]["matches_returned"], 1);
            assert_eq!(selected["value"]["matches"][0]["start_byte"], first_match);
            assert_eq!(selected["value"]["matches"][0]["end_byte"], first_match + 6);
            assert_eq!(selected["value"]["hydrated_ranges"], serde_json::json!([]));
            assert_eq!(selected["continuation"]["start_byte"], first_match + 6);
            assert!(
                output.get("continuation_stop").is_none(),
                "do not advertise a non-progressing retry"
            );
            assert_eq!(
                selected["child_selectors"],
                serde_json::json!([{"kind":"lines", "start":1, "end":1}])
            );
            // The advertised context can progress with the same cap, even if
            // one byte splits the first UTF-8 code point. No source reread.
            let children = serde_json::from_value(selected["child_selectors"].clone()).unwrap();
            let call = invoke(source, children, budget);
            let context = ReadToolOutputHandler
                .handle(call.clone())
                .await
                .unwrap()
                .code_mode_result(&call.payload);
            schema.validate(&context).unwrap();
            let context: ReadToolOutputResult = serde_json::from_value(context).unwrap();
            let ranges = context.delivered_ranges();
            assert_eq!(ranges, vec![(0, budget as u64)]);
            let mut bytes = Vec::new();
            for fragment in context.results {
                if fragment.status != ToolOutputSelectorStatus::Ok {
                    continue;
                }
                if let Some(text) = fragment.text {
                    bytes.extend_from_slice(text.as_bytes());
                }
                if let Some(encoded) = fragment.data_base64 {
                    bytes.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap(),
                    );
                }
            }
            assert_eq!(bytes, text.as_bytes()[..budget]);
            assert!(budget as u64 <= first_line_end);
        }
    }
}

#[test]
fn search_source_byte_budget_does_not_leave_dangling_shared_hydration() {
    let text = format!("{} needle alpha\n", "x".repeat(300));
    let selectors = ["alpha", "needle", "zz_absent"].map(|query| ToolOutputSelector::Search {
        query: query.into(),
        enclosing: false,
        case_insensitive: false,
        start_byte: 0,
        max_results: 20,
        context_lines: 0,
    });
    let canonical = CanonicalToolResult::text(&text);
    let mut output = crate::tools::command_output_artifact::select_producer_snapshot(
        &canonical,
        "artifact",
        selectors.into(),
        9_000,
    )
    .unwrap();
    assert_eq!(
        output.results[1].value.as_ref().unwrap()["hydrated_ranges"][0]["shared"],
        true
    );
    // Enrichment shares the same hydration contract; dropping source context
    // cannot leave an earlier complete-enclosing claim intact.
    output.results[0].value.as_mut().unwrap()["enclosing_complete"] = Value::Bool(true);
    bound_recovery_source_bytes(&mut output, 1);
    for result in &output.results[..2] {
        assert_eq!(result.status, ToolOutputSelectorStatus::Ok);
        assert!(!result.complete);
        assert_eq!(result.value.as_ref().unwrap()["matches_returned"], 1);
        assert_eq!(
            result.value.as_ref().unwrap()["hydrated_ranges"],
            serde_json::json!([])
        );
        assert_eq!(
            result.child_selectors,
            vec![ToolOutputSelector::Lines { start: 1, end: 1 }]
        );
    }
    assert_eq!(
        output.results[0].value.as_ref().unwrap()["enclosing_complete"],
        false
    );
    assert!(
        output.results[2].complete,
        "a negative search needs no hydration budget"
    );
    assert_eq!(
        output.results[2].value.as_ref().unwrap()["total_matches"],
        0
    );
    assert!(output.delivered_ranges().is_empty());
    assert!(!output.complete);
}

#[tokio::test]
async fn small_source_byte_budgets_make_exact_progress_across_utf8_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let text = "aλ🦀z".repeat(12);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(),
        "small-bytes",
        &CanonicalToolResult::text(&text),
    )
    .await;
    let snapshot =
        load_tool_output_snapshot(home.path(), "small-bytes", &artifact.artifact_id().unwrap())
            .await
            .unwrap();
    for budget in 1..=64 {
        let mut cursor = 0;
        let mut recovered = Vec::new();
        while cursor < text.len() as u64 {
            let page = drain_recovery_snapshot_with_byte_limit(
                &snapshot,
                vec![ToolOutputSelector::Bytes {
                    start: cursor,
                    end: text.len() as u64,
                }],
                8_000,
                budget,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            let ranges = page.output.delivered_ranges();
            assert_eq!(ranges.len(), 1, "budget {budget}");
            assert_eq!(ranges[0].0, cursor);
            assert!(ranges[0].1 > cursor && ranges[0].1 - cursor <= budget as u64);
            for result in &page.output.results {
                if result.status != ToolOutputSelectorStatus::Ok {
                    continue;
                }
                if let Some(text) = &result.text {
                    recovered.extend_from_slice(text.as_bytes());
                }
                if let Some(encoded) = &result.data_base64 {
                    recovered.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap(),
                    );
                }
            }
            cursor = ranges[0].1;
            if cursor < text.len() as u64 {
                let stop = page.continuation_stop.as_ref().unwrap();
                assert_eq!(stop.reason, ContinuationStopReason::Budget);
                assert!(recovery_call_succeeded(&page.output, Some(stop)));
                assert_eq!(
                    stop.selector,
                    Some(ToolOutputSelector::Bytes {
                        start: cursor,
                        end: text.len() as u64
                    })
                );
            } else {
                assert!(page.output.complete);
            }
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
        home.path(),
        "script-cap",
        &CanonicalToolResult::text(&text),
    )
    .await;
    let snapshot =
        load_tool_output_snapshot(home.path(), "script-cap", &artifact.artifact_id().unwrap())
            .await
            .unwrap();
    let result = drain_recovery_snapshot_with_byte_limit(
        &snapshot,
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: text.len() as u64,
        }],
        READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES / 4,
        READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let envelope = recovery_envelope(&result.output, result.continuation_stop.as_ref()).unwrap();
    assert!(serde_json::to_vec(&envelope).unwrap().len() <= READ_TOOL_OUTPUT_SCRIPT_MAX_BYTES);
    assert!(!result.output.complete);
    let mut end = 0;
    for page in &result.output.results {
        // Pages whose bounds split a UTF-8 sequence are delivered as base64;
        // both encodings carry exact canonical bytes.
        let bytes = match (&page.text, &page.data_base64) {
            (Some(value), _) => value.as_bytes().to_vec(),
            (None, Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            (None, None) => continue,
        };
        let range = page.canonical_range.unwrap();
        assert_eq!(range.start, end, "{:?} {:?}", page.selector, page.status);
        assert_eq!(
            bytes.as_slice(),
            &text.as_bytes()[range.start as usize..range.end as usize]
        );
        end = range.end;
    }
    assert!(end > READ_TOOL_OUTPUT_MAX_BYTES as u64);
    let stop = result.continuation_stop.unwrap();
    assert!(stop.resumable);
    assert_eq!(
        stop.selector,
        Some(ToolOutputSelector::Bytes {
            start: end,
            end: text.len() as u64
        })
    );
}

#[tokio::test]
async fn nearly_complete_recovery_uses_margin_before_reserving_page_hints() {
    let home = tempfile::tempdir().unwrap();
    let text = "source evidence\n".repeat(1_500);
    let canonical = CanonicalToolResult::text(&text);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(),
        "margin",
        &canonical,
    )
    .await;
    let id = artifact.artifact_id().unwrap();
    let snapshot = load_tool_output_snapshot(home.path(), "margin", &id)
        .await
        .unwrap();
    let selectors = vec![ToolOutputSelector::Bytes {
        start: 0,
        end: text.len() as u64,
    }];
    let exact = snapshot.select(selectors.clone(), 10_000).await.unwrap();
    assert!(exact.complete);
    let budget = recovery_size(&exact).tokens() - 64;
    let recovered =
        drain_recovery_snapshot(&snapshot, selectors, budget, &CancellationToken::new())
            .await
            .unwrap();
    assert!(recovered.output.complete);
    assert!(recovered.continuation_stop.is_none());
    assert_eq!(
        recovered.output.results[0].text.as_deref(),
        Some(text.as_str())
    );
    assert!(recovery_envelope_fits(
        &recovered.output,
        None,
        budget + 128
    ));
}

#[test]
fn unavailable_suffix_does_not_block_a_fully_retained_selection() {
    let canonical = CanonicalToolResult::text("retained evidence\n");
    let mut output = crate::tools::command_output_artifact::select_producer_snapshot(
        &canonical,
        "artifact",
        vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
        1000,
    )
    .unwrap();
    output
        .unavailable_ranges
        .push(codex_tools::CanonicalByteRange::new(100, 200));
    let state = RecoveryContinuationState::new(output, 1000);
    assert_eq!(state.next_step(), ContinuationStep::Complete);
}

#[test]
fn adjacent_partial_recovery_pages_share_one_payload_envelope() {
    let canonical = CanonicalToolResult::text("first\nsecond\n");
    let mut pages = Vec::new();
    for (start, end) in [(0, 6), (6, 13)] {
        let output = crate::tools::command_output_artifact::select_producer_snapshot(
            &canonical,
            "artifact",
            vec![ToolOutputSelector::Bytes { start, end }],
            1000,
        )
        .unwrap();
        pages.extend(output.results);
    }
    merge_adjacent_recovery_pages(&mut pages);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].text.as_deref(), Some("first\nsecond\n"));
    assert_eq!(
        pages[0].selector,
        ToolOutputSelector::Bytes { start: 0, end: 13 }
    );
}
