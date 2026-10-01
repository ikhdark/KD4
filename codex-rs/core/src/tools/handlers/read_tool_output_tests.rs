#[tokio::test]
async fn explicit_nested_recovery_does_not_hide_a_model_handoff() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = std::sync::Arc::new(session);
    let turn = std::sync::Arc::new(turn);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        &turn.config.codex_home,
        &session.thread_id.to_string(),
        &CanonicalToolResult::text("retained evidence\n"),
    )
    .await;
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "artifact_id": artifact.artifact_id().expect("retained artifact"),
            "selectors": [{"kind": "lines", "start": 1, "end": 1}],
        })
        .to_string(),
    };
    for (index, source) in [
        ToolCallSource::Direct,
        ToolCallSource::CodeMode {
            cell_id: "later-cell".to_string(),
            parent_call_id: Some("later-exec".to_string()),
            runtime_tool_call_id: "recovery".to_string(),
            nested_deadline: None,
            cancellation_cause: None,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let result = ReadToolOutputHandler
            .handle(ToolInvocation {
                session: std::sync::Arc::clone(&session),
                step_context: crate::session::step_context::StepContext::for_test(
                    std::sync::Arc::clone(&turn),
                ),
                cancellation_token: Default::default(),
                tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                    crate::turn_diff_tracker::TurnDiffTracker::new(),
                )),
                call_id: format!("recover-{index}"),
                tool_name: ToolName::plain("read_tool_output"),
                source,
                payload: payload.clone(),
            })
            .await
            .expect("explicit recovery")
            .code_mode_result(&payload);
        assert_eq!(result["results"][0]["text"], "retained evidence\n");

        let mut pending = Some(crate::turn_timing::ContinuationCause::ToolResult);
        turn.turn_timing_state.begin_model_generation(
            &mut pending,
            &codex_protocol::protocol::SessionSource::Cli,
        );
    }
    let counters = turn
        .turn_timing_state
        .complete_snapshot()
        .protocol_timing()
        .counters;
    assert_eq!(counters.tool_output_recovery_call_count, 2);
    assert_eq!(counters.tool_output_in_cell_recovery_call_count, 0);
    assert_eq!(counters.attributable_recovery_generation_count, 2);
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn recovery_pages_use_one_validated_snapshot_without_reopening_the_artifact() {
    use crate::tools::command_output_artifact::create_canonical_output_artifact;

    let home = tempfile::tempdir().unwrap();
    let text = "recovery evidence\n".repeat(8_000);
    let canonical = CanonicalToolResult::text(&text);
    let artifact = create_canonical_output_artifact(home.path(), "snapshot", &canonical).await;
    let id = artifact.artifact_id().unwrap();
    let snapshot = load_tool_output_snapshot(home.path(), "snapshot", &id)
        .await
        .unwrap();
    let path = home
        .path()
        .join("tool-output/snapshot")
        .join(format!("{id}.log"));
    std::fs::remove_file(&path).unwrap();

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let stopped = drain_recovery_snapshot(
        &snapshot,
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: text.len() as u64,
        }],
        CODE_MODE_RECOVERY_TOKEN_CEILING,
        &cancelled,
    )
    .await
    .unwrap();
    assert_eq!(stopped.drained_continuation_pages, 0);
    assert!(!stopped.output.complete);
    assert_eq!(
        stopped.continuation_stop.unwrap().reason,
        ContinuationStopReason::Cancelled,
    );

    // Removing the backing file makes any hidden reopen fail. The production
    // continuation loop must still deliver authenticated pages from its
    // initial observation, and stop at the normal output ceiling.
    let recovered = drain_recovery_snapshot(
        &snapshot,
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: text.len() as u64,
        }],
        CODE_MODE_RECOVERY_TOKEN_CEILING,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(recovered.drained_continuation_pages > 0);
    assert!(!recovered.output.complete);
    assert_eq!(recovered.output.canonical_sha256, canonical.sha256);
    assert_eq!(
        recovered.continuation_stop.as_ref().unwrap().reason,
        ContinuationStopReason::Budget
    );
    let pages = recovered
        .output
        .results
        .iter()
        .filter(|part| part.text.is_some())
        .collect::<Vec<_>>();
    assert!(
        !pages.is_empty(),
        "recovery must return useful evidence, not just an overflow descriptor"
    );
    for page in pages {
        let range = page.canonical_range.unwrap();
        assert_eq!(
            page.text.as_deref().unwrap(),
            &text[range.start as usize..range.end as usize]
        );
    }

    // Reuse is scoped to this observation, never a cache that masks expiry
    // on the next invocation. Required disk validation still takes place.
    assert!(matches!(
        execute_recovery_transaction_with_continuations(
            home.path(),
            "snapshot",
            &id,
            vec![ToolOutputSelector::Lines { start: 1, end: 1 }],
            true,
            &CancellationToken::new(),
        )
        .await,
        Err(ReadToolOutputError::Expired),
    ));
}

#[tokio::test]
#[serial_test::serial(command_output_artifact)]
async fn new_recovery_transaction_revalidates_same_length_modified_bytes() {
    use crate::tools::command_output_artifact::create_canonical_output_artifact;

    let home = tempfile::tempdir().unwrap();
    let canonical = CanonicalToolResult::text("original evidence\n");
    let artifact = create_canonical_output_artifact(home.path(), "snapshot", &canonical).await;
    let id = artifact.artifact_id().unwrap();
    let selectors = vec![ToolOutputSelector::Lines { start: 1, end: 1 }];
    let first = execute_recovery_transaction_with_continuations(
        home.path(),
        "snapshot",
        &id,
        selectors.clone(),
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(first.output.complete);
    assert_eq!(
        first.output.results[0].text.as_deref(),
        Some("original evidence\n")
    );

    std::fs::write(
        home.path()
            .join("tool-output/snapshot")
            .join(format!("{id}.log")),
        "modified evidence\n",
    )
    .unwrap();
    let error = execute_recovery_transaction_with_continuations(
        home.path(),
        "snapshot",
        &id,
        selectors,
        true,
        &CancellationToken::new(),
    )
    .await
    .err()
    .expect("new calls must authenticate disk contents again");
    assert_eq!(
        error,
        ReadToolOutputError::Io("artifact SHA identity does not match metadata".to_string())
    );
}

use super::*;
use crate::tools::command_output_artifact::ByteSubdivisionPlan;

#[tokio::test]
async fn byte_budget_delivers_an_exact_prefix_and_resumable_remainder() {
    let temp = tempfile::tempdir().unwrap();
    let text = "λ source evidence\n".repeat(2000);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        temp.path(), "thread", &CanonicalToolResult::text(text.clone()),
    ).await;
    let snapshot = load_tool_output_snapshot(temp.path(), "thread", &artifact.artifact_id().unwrap().to_string())
        .await.unwrap();
    let result = drain_recovery_snapshot_with_byte_limit(
        &snapshot,
        vec![ToolOutputSelector::Bytes { start: 0, end: text.len() as u64 }],
        RECOVERY_AGGREGATE_TOKEN_CEILING, 4096, &CancellationToken::new(),
    ).await.unwrap();
    let delivered: u64 = result.output.delivered_ranges().iter().map(|(start, end)| end - start).sum();
    assert!(delivered > 0 && delivered <= 4096, "{delivered}");
    assert!(!result.output.complete);
    assert!(result.continuation_stop.as_ref().is_some_and(|stop| stop.resumable));
    for page in &result.output.results {
        if let (Some(range), Some(value)) = (page.canonical_range, page.text.as_ref()) {
            assert_eq!(value, &text[range.start as usize..range.end as usize]);
        }
    }
}

#[tokio::test]
async fn recovery_uses_actual_cell_budget_with_a_minimum_page() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = std::sync::Arc::new(session);
    let turn = std::sync::Arc::new(turn);
    let text = "exact source line with unique context\n".repeat(2_000);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        &turn.config.codex_home, &session.thread_id.to_string(),
        &CanonicalToolResult::text(text.clone())).await;
    session.register_tool_artifact_origin(
        artifact.artifact_id().unwrap(), "producer".into(), text.len() as u64,
        crate::tool_history::sha256(text.as_bytes())).await;
    let cell = codex_code_mode::CellId::new("bounded-recovery".into());
    session.services.code_mode_service.record_cell_parent_call_id(&cell, "outer");
    for budget in [0, 512, 4_000, 10_000] {
        session.services.code_mode_service.record_output_budget(&cell, Some(budget));
        let payload = ToolPayload::Function { arguments: serde_json::json!({
            "artifact_id": artifact.artifact_id().unwrap(),
            "selectors": [{"kind":"bytes", "start":0, "end":text.len()}]
        }).to_string() };
        let result = ReadToolOutputHandler.handle(ToolInvocation {
            session: std::sync::Arc::clone(&session),
            step_context: crate::session::step_context::StepContext::for_test(std::sync::Arc::clone(&turn)),
            cancellation_token: Default::default(),
            tracker: std::sync::Arc::new(tokio::sync::Mutex::new(crate::turn_diff_tracker::TurnDiffTracker::new())),
            call_id: "recover".into(), tool_name: ToolName::plain("read_tool_output"),
            source: ToolCallSource::CodeMode { cell_id: cell.to_string(), parent_call_id: Some("outer".into()),
                runtime_tool_call_id: "nested".into(), nested_deadline: None, cancellation_cause: None },
            payload: payload.clone(),
        }).await;
        let output = result.unwrap().code_mode_result(&payload);
        assert!(codex_utils_string::approx_token_count(&output.to_string()) <= budget.max(896) - 384 + 128);
        assert_eq!(output["complete"], false);
        let pages = output["results"].as_array().unwrap().iter()
            .filter(|page| page["text"].is_string()).collect::<Vec<_>>();
        assert!(!pages.is_empty());
        for page in pages {
            let start = page["canonical_range"]["start"].as_u64().unwrap() as usize;
            let end = page["canonical_range"]["end"].as_u64().unwrap() as usize;
            assert_eq!(page["text"].as_str().unwrap(), &text[start..end]);
        }
        assert!(output.get("continuation_stop").is_some());
        if budget >= 4_000 {
            assert!(!output["continuation_stop"]["page_selectors"].as_array().unwrap().is_empty());
        }
        let history = serde_json::to_value(session.clone_history().await.tool_history_state()).unwrap();
        assert_eq!(history["recovered_call_ids"], serde_json::json!(["producer", "recover"]));
    }
    session.services.code_mode_service.finish_cell_dispatch(&cell);
    assert_eq!(session.services.code_mode_service.output_budget(cell.as_str()), None);
}

#[tokio::test]
async fn recovery_handler_output_schema_covers_exact_search_and_rejected_selectors() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = std::sync::Arc::new(session);
    let turn = std::sync::Arc::new(turn);
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        &turn.config.codex_home,
        &session.thread_id.to_string(),
        &CanonicalToolResult::json(serde_json::json!({"present": "value"})),
    )
    .await;
    let artifact_id = artifact
        .artifact_id()
        .expect("retained artifact")
        .to_string();
    let spec = ReadToolOutputHandler.spec();
    let ToolSpec::Function(spec) = spec else {
        panic!("recovery uses a function spec");
    };
    let validator =
        jsonschema::validator_for(&spec.output_schema.as_ref().expect("output schema").to_value())
            .expect("valid output schema");
    for (selector, expected_status, expected_reason) in [
        (
            serde_json::json!({"kind": "json_pointer", "pointer": "invalid"}),
            "invalid",
            Some("invalid_selector"),
        ),
        (
            serde_json::json!({"kind": "json_pointer", "pointer": "/present~2"}),
            "invalid",
            Some("invalid_selector"),
        ),
        (
            serde_json::json!({"kind": "json_pointer", "pointer": "/present~"}),
            "invalid",
            Some("invalid_selector"),
        ),
        (
            serde_json::json!({"kind": "json_pointer", "pointer": "/missing"}),
            "not_found",
            Some("selector_not_found"),
        ),
        (
            serde_json::json!({"kind": "lines", "start": 0, "end": 0}),
            "invalid",
            Some("invalid_selector"),
        ),
        (
            serde_json::json!({"kind": "json_pointer", "pointer": "/present"}),
            "ok",
            None,
        ),
        (
            serde_json::json!({"kind": "search", "query": "present"}),
            "ok",
            None,
        ),
    ] {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({
                "artifact_id": artifact_id,
                "selectors": [selector],
            })
            .to_string(),
        };
        let result = ReadToolOutputHandler
            .handle(ToolInvocation {
                session: std::sync::Arc::clone(&session),
                step_context: crate::session::step_context::StepContext::for_test(
                    std::sync::Arc::clone(&turn),
                ),
                cancellation_token: Default::default(),
                tracker: std::sync::Arc::new(tokio::sync::Mutex::new(
                    crate::turn_diff_tracker::TurnDiffTracker::new(),
                )),
                call_id: "selector-recovery".to_string(),
                tool_name: ToolName::plain("read_tool_output"),
                source: ToolCallSource::Direct,
                payload: payload.clone(),
            })
            .await
            .expect("handler output")
            .code_mode_result(&payload);
        assert!(
            validator.is_valid(&result),
            "schema rejected handler result: {result}"
        );
        assert_eq!(
            result["results"][0]["status"], expected_status,
            "{selector}"
        );
        if let Some(reason) = expected_reason {
            assert_eq!(result["continuation_stop"]["reason"], reason);
            assert_eq!(result["continuation_stop"]["selector"], selector);
            assert_eq!(result["continuation_stop"]["resumable"], false);
            assert!(
                !result["continuation_stop"]["message"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(result["complete"], false);
            let mut malformed = result.clone();
            malformed["continuation_stop"]["reason"] = serde_json::json!("unknown");
            assert!(!validator.is_valid(&malformed));
        } else {
            assert_eq!(result["complete"], true);
            assert!(result.get("continuation_stop").is_none());
            if selector["kind"] == "search" {
                assert_eq!(result["results"][0]["value"]["total_matches"], 1);
                assert_eq!(
                    result["results"][0]["value"]["hydrated_ranges"][0]["text"],
                    r#"{"present":"value"}"#
                );
                let mut malformed = result.clone();
                malformed["results"][0]["value"]["hydrated_ranges"] =
                    serde_json::json!("missing ranges");
                assert!(!validator.is_valid(&malformed));
            } else {
                assert_eq!(result["results"][0]["value"], "value");
            }
        }
        let mut malformed = result;
        malformed["complete"] = serde_json::json!("true");
        assert!(!validator.is_valid(&malformed));
    }
}

use codex_tools::CanonicalByteRange;

fn selector_result(status: ToolOutputSelectorStatus) -> ToolOutputSelectorResult {
    ToolOutputSelectorResult {
        selector: ToolOutputSelector::Lines { start: 1, end: 1 },
        status,
        complete: status == ToolOutputSelectorStatus::Ok,
        exact_bytes: None,
        canonical_range: None,
        text: None,
        value: None,
        data_base64: None,
        subdivision_plan: None,
        child_selectors: Vec::new(),
        continuation: None,
        message: None,
    }
}

fn page_selector(start: u64) -> ToolOutputSelector {
    ToolOutputSelector::Bytes {
        start,
        end: start + 10,
    }
}

fn continuation_result(
    selector: ToolOutputSelector,
    continuation: Option<ToolOutputSelector>,
    text: &str,
) -> ToolOutputSelectorResult {
    ToolOutputSelectorResult {
        selector,
        status: ToolOutputSelectorStatus::Ok,
        complete: continuation.is_none(),
        exact_bytes: Some(text.len() as u64),
        canonical_range: None,
        text: Some(text.to_string()),
        value: None,
        data_base64: None,
        subdivision_plan: None,
        child_selectors: Vec::new(),
        continuation,
        message: None,
    }
}

fn recovery_output(results: Vec<ToolOutputSelectorResult>) -> ReadToolOutputResult {
    ReadToolOutputResult {
        artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 100,
        retained_bytes: 100,
        complete: results
            .iter()
            .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete),
        unavailable_ranges: Vec::new(),
        results,
    }
}

#[test]
fn many_page_incremental_recovery_matches_reconstruction_and_budget_rollback() {
    for mode in 0..4 {
        let text = "escaped \"\\\n\t 中😀 word_123 ".repeat(160);
        let bytes = match mode {
            2 => serde_json::to_vec(&serde_json::json!({"data": text})).unwrap(),
            3 => [text.as_bytes(), &[0xff, 0xfe]].concat(),
            _ => text.into_bytes(),
        };
        let pages = bytes
            .chunks(64)
            .enumerate()
            .map(|(index, bytes)| {
                let start = (index * 64) as u64;
                let end = start + bytes.len() as u64;
                let mut page = selector_result(ToolOutputSelectorStatus::Ok);
                page.selector = ToolOutputSelector::Bytes { start, end };
                page.canonical_range = Some(CanonicalByteRange::new(start, end));
                page.exact_bytes = Some(bytes.len() as u64);
                if let Ok(text) = std::str::from_utf8(bytes) {
                    page.text = Some(text.into());
                } else {
                    page.data_base64 =
                        Some(base64::engine::general_purpose::STANDARD.encode(bytes));
                }
                page
            })
            .collect::<Vec<_>>();
        assert!(pages.len() > 64);
        let mut owner = selector_result(if mode == 0 {
            ToolOutputSelectorStatus::Ok
        } else {
            ToolOutputSelectorStatus::SelectorTooLarge
        });
        owner.complete = false;
        owner.continuation = Some(pages[0].selector.clone());
        if mode != 0 {
            owner.selector = if mode == 2 {
                ToolOutputSelector::JsonPointer {
                    pointer: "/data".into(),
                }
            } else {
                ToolOutputSelector::Bytes {
                    start: 0,
                    end: bytes.len() as u64,
                }
            };
            owner.canonical_range = Some(CanonicalByteRange::new(0, bytes.len() as u64));
            owner.subdivision_plan =
                Some(crate::tools::command_output_artifact::ByteSubdivisionPlan {
                    range: owner.canonical_range.unwrap(),
                    chunk_bytes: 64,
                    chunk_count: pages.len() as u64,
                    selector_kind: "bytes".into(),
                });
        }
        let initial = recovery_output(vec![owner]);
        for ceiling in [usize::MAX, 3500, 3501] {
            let mut state = RecoveryContinuationState::new(initial.clone(), ceiling);
            let mut reference = RecoveryContinuationState::new(initial.clone(), ceiling);
            let mut checkpoints = Vec::new();
            for (index, fragment) in pages.iter().enumerate() {
                let step = reference.next_step();
                assert_eq!(state.next_step(), step);
                let ContinuationStep::Follow {
                    result_index,
                    selector,
                } = step
                else {
                    panic!("expected continuation {index}");
                };
                let mut page = fragment.clone();
                if mode == 0 && index + 1 < pages.len() {
                    page.complete = false;
                    page.continuation = Some(pages[index + 1].selector.clone());
                }
                let before = reference.output.clone();
                let predecessor = &mut reference.output.results[result_index];
                predecessor.continuation = next_owner_continuation(predecessor, &selector);
                if predecessor.status == ToolOutputSelectorStatus::Ok {
                    predecessor.complete = predecessor.continuation.is_none();
                }
                reference.output.results.push(page.clone());
                reference.output.complete = reference.output.results.iter().all(|result| {
                    result.status == ToolOutputSelectorStatus::Ok
                        && result.complete
                        && result.continuation.is_none()
                });
                let expected = reference.reconstructed_output();
                let fits = recovery_result_fits_token_ceiling(&expected, ceiling);
                let accepted = state.accept_page(
                    result_index,
                    &selector,
                    recovery_output(vec![page]),);
                assert_eq!(
                    accepted,
                    if fits {
                        Ok(())
                    } else {
                        Err(ContinuationStopReason::Budget)
                    }
                );
                if !fits {
                    reference.output = before;
                    reference
                        .record_stop(ContinuationStopReason::Budget, Some(selector.clone()));
                    state.record_stop(ContinuationStopReason::Budget, Some(selector));
                    break;
                }
                reference
                    .result_owners
                    .push(reference.result_owners[result_index]);
                checkpoints.push((before, selector));
                assert_eq!(
                    state.projected_size(None).tokens(),
                    recovery_size(&expected).tokens()
                );
                assert_eq!(state.reconstructed_output(), expected);
                // The cache must agree with the old per-selection serialization,
                // including the per-fragment rounding used for page admission.
                for owner in 0..state.initial_result_count {
                    let occupied = state
                        .output
                        .results
                        .iter()
                        .enumerate()
                        .map(|(index, result)| {
                            if state.result_owners[index] != owner {
                                recovery_size(result).tokens()
                            } else {
                                RecoveryResultCost::new(result).payload_tokens
                            }
                        })
                        .sum::<usize>();
                    assert_eq!(
                        state.page_token_ceiling(owner),
                        ceiling.saturating_sub(occupied)
                    );
                }
            }
            if ceiling == usize::MAX {
                assert_eq!(checkpoints.len(), pages.len());
                assert_eq!(state.next_step(), ContinuationStep::Complete);
            } else {
                assert!(checkpoints.len() > 8 && checkpoints.len() < pages.len());
                assert_eq!(
                    state.continuation_stop.as_ref().unwrap().reason,
                    ContinuationStopReason::Budget
                );
            }
            // Replay the old final-envelope rollback independently of the cache.
            let expected = loop {
                let reconstructed = reference.reconstructed_output();
                if recovery_envelope_fits(
                    &reconstructed,
                    reference.continuation_stop.as_ref(),
                    ceiling,
                ) {
                    break reconstructed;
                }
                let (previous, selector) = checkpoints
                    .pop()
                    .expect("fixture leaves room for stop metadata");
                reference.output = previous;
                reference.record_stop(ContinuationStopReason::Budget, Some(selector));
            };
            let actual = state.finish();
            assert_eq!(
                actual.drained_continuation_pages as usize,
                checkpoints.len()
            );
            assert_eq!(actual.continuation_stop, reference.continuation_stop);
            assert_eq!(
                serde_json::to_vec(&actual.output).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
            if ceiling == usize::MAX && mode != 0 {
                let result = &actual.output.results[0];
                let recovered = if let Some(text) = &result.text {
                    text.as_bytes().to_vec()
                } else if let Some(value) = &result.value {
                    serde_json::to_vec(value).unwrap()
                } else {
                    base64::engine::general_purpose::STANDARD
                        .decode(result.data_base64.as_ref().unwrap())
                        .unwrap()
                };
                assert_eq!(recovered, bytes);
                assert!(actual.output.complete);
                assert!(actual.continuation_stop.is_none());
            }
        }
    }
}

#[test]
fn search_page_limit_does_not_automatically_fetch_later_matches() {
    let selector = ToolOutputSelector::Search { case_insensitive: false,
        query: "needle".into(),
        start_byte: 0,
        max_results: 1,
        context_lines: 0,
    };
    let next = ToolOutputSelector::Search { case_insensitive: false,
        query: "needle".into(),
        start_byte: 100,
        max_results: 1,
        context_lines: 0,
    };
    let state = RecoveryContinuationState::new(
        recovery_output(vec![continuation_result(
            selector,
            Some(next.clone()),
            "one match",
        )]),
        usize::MAX,);
    assert_eq!(state.next_step(), ContinuationStep::Complete);
    let output = state.finish();
    assert_eq!(output.drained_continuation_pages, 0);
    assert_eq!(output.output.results.len(), 1);
    assert_eq!(output.output.results[0].continuation, Some(next));
    assert!(!output.output.complete);
}

#[test]
fn overlapping_json_selection_does_not_block_byte_reconstruction() {
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Bytes { start: 0, end: 4 };
    owner.canonical_range = Some(CanonicalByteRange::new(0, 4));
    let mut json = selector_result(ToolOutputSelectorStatus::Ok);
    json.selector = ToolOutputSelector::JsonPointer {
        pointer: "/flag".into(),
    };
    json.canonical_range = owner.canonical_range;
    json.value = Some(serde_json::json!(true));
    let mut first =
        continuation_result(ToolOutputSelector::Bytes { start: 0, end: 2 }, None, "tr");
    first.canonical_range = Some(CanonicalByteRange::new(0, 2));
    let mut last =
        continuation_result(ToolOutputSelector::Bytes { start: 2, end: 4 }, None, "ue");
    last.canonical_range = Some(CanonicalByteRange::new(2, 4));
    owner.continuation = Some(first.selector.clone());
    let mut state = RecoveryContinuationState::new(
        recovery_output(vec![owner.clone(), json.clone()]),
        usize::MAX,);
    state
        .accept_page(
            0,
            &first.selector.clone(),
            recovery_output(vec![first]),)
        .unwrap();
    state
        .accept_page(
            0,
            &last.selector.clone(),
            recovery_output(vec![last]),)
        .unwrap();
    assert_eq!(state.next_step(), ContinuationStep::Complete);
    let transaction = state.finish();
    assert!(transaction.output.complete);
    assert_eq!(transaction.output.results.len(), 2);
    assert_eq!(transaction.output.results[0].selector, owner.selector);
    assert_eq!(transaction.output.results[0].text.as_deref(), Some("true"));
    assert_eq!(transaction.output.results[1], json);
}

#[test]
fn overflow_reconstruction_requires_exact_coverage_and_decodes_original_json() {
    let bytes = br#"{"ok":true}"#;
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::JsonPointer {
        pointer: "/result".into(),
    };
    owner.canonical_range = Some(CanonicalByteRange { start: 10, end: 21 });
    let mut first = continuation_result(
        ToolOutputSelector::Bytes { start: 10, end: 15 },
        None,
        std::str::from_utf8(&bytes[..5]).unwrap(),
    );
    first.canonical_range = Some(CanonicalByteRange { start: 10, end: 15 });
    let mut last = continuation_result(
        ToolOutputSelector::Bytes { start: 15, end: 21 },
        None,
        std::str::from_utf8(&bytes[5..]).unwrap(),
    );
    last.canonical_range = Some(CanonicalByteRange { start: 15, end: 21 });
    assert!(reconstruct_selection(&owner, &[first.clone()]).is_none());
    let exact = reconstruct_selection(&owner, &[last.clone(), first.clone()]).unwrap();
    assert_eq!(exact.status, ToolOutputSelectorStatus::Ok);
    assert!(exact.complete);
    assert_eq!(exact.value, Some(serde_json::json!({"ok":true})));
    assert_eq!(
        reconstruct_selection(&owner, &[first.clone(), first.clone(), last.clone()])
            .unwrap()
            .value,
        exact.value
    );

    owner.continuation = Some(first.selector.clone());
    let mut state =
        RecoveryContinuationState::new(recovery_output(vec![owner.clone()]), usize::MAX);
    let first_selector = first.selector.clone();
    let last_selector = last.selector.clone();
    state
        .accept_page(0, &first_selector, recovery_output(vec![first]))
        .unwrap();
    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 0,
            selector: last_selector.clone(),
        }
    );
    state
        .accept_page(0, &last_selector, recovery_output(vec![last]))
        .unwrap();
    assert_eq!(state.next_step(), ContinuationStep::Complete);
    let transaction = state.finish();
    assert_eq!(transaction.drained_continuation_pages, 2);
    assert!(transaction.output.complete);
    assert_eq!(transaction.output.results.len(), 1);
    assert_eq!(transaction.output.results[0].selector, owner.selector);
    assert_eq!(
        transaction.output.results[0].value,
        Some(serde_json::json!({"ok":true}))
    );
}

#[test]
fn terminal_recovery_results_complete_after_the_validation_pass() {
    let mut ok = continuation_result(page_selector(0), None, "ok");
    ok.status = ToolOutputSelectorStatus::Ok;
    let mut oversized = continuation_result(page_selector(10), None, "oversized");
    oversized.status = ToolOutputSelectorStatus::SelectorTooLarge;
    let mut omitted = continuation_result(page_selector(20), None, "omitted");
    omitted.status = ToolOutputSelectorStatus::AggregateOmitted;
    let state = RecoveryContinuationState::new(
        recovery_output(vec![ok, oversized, omitted]),
        usize::MAX,);

    assert_eq!(state.next_step(), ContinuationStep::Complete);
}

#[test]
fn stop_envelope_rolls_back_pages_without_clipping_source() {
    let selector = page_selector(0);
    let next = page_selector(1);
    let mut owner = selector_result(ToolOutputSelectorStatus::Ok);
    owner.continuation = Some(selector.clone());
    owner.complete = false;
    let initial = recovery_output(vec![owner]);
    let mut state = RecoveryContinuationState::new(initial.clone(), usize::MAX);
    let page = continuation_result(
        selector.clone(),
        Some(next.clone()),
        &"exact source".repeat(100),
    );
    state
        .accept_page(0, &selector, recovery_output(vec![page]))
        .unwrap();
    let payload_tokens = codex_utils_string::approx_token_count(
        &serde_json::to_string(&state.reconstructed_output()).unwrap(),
    );
    state.token_ceiling = payload_tokens;
    state.record_stop(ContinuationStopReason::Budget, Some(next));
    assert!(!recovery_envelope_fits(
        &state.reconstructed_output(),
        state.continuation_stop.as_ref(),
        payload_tokens
    ));
    let result = state.finish();
    assert!(recovery_envelope_fits(
        &result.output,
        result.continuation_stop.as_ref(),
        payload_tokens
    ));
    assert_eq!(result.output, initial);
    assert_eq!(result.drained_continuation_pages, 0);
    let stop = result.continuation_stop.unwrap();
    assert_eq!(stop.reason, ContinuationStopReason::Budget);
    assert!(stop.resumable);
    assert_eq!(stop.selector, Some(selector));
}

#[test]
fn final_page_is_budgeted_as_reconstructed_selection() {
    let selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
    owner.canonical_range = Some(CanonicalByteRange { start: 0, end: 10 });
    owner.continuation = Some(selector.clone());
    let mut page = continuation_result(selector.clone(), None, "abcdefghij");
    page.canonical_range = owner.canonical_range;
    let exact = reconstruct_selection(&owner, &[page.clone()]).unwrap();
    let ceiling = codex_utils_string::approx_token_count(
        &serde_json::to_string(&recovery_output(vec![exact])).unwrap(),
    );
    assert!(!recovery_result_fits_token_ceiling(
        &recovery_output(vec![owner.clone(), page.clone()]),
        ceiling,
    ));
    let mut state =
        RecoveryContinuationState::new(recovery_output(vec![owner]), ceiling);
    assert_eq!(
        state.accept_page(0, &selector, recovery_output(vec![page])),
        Ok(())
    );
    let result = state.finish();
    assert!(result.output.complete);
    assert_eq!(result.output.results.len(), 1);
    assert_eq!(result.output.results[0].text.as_deref(), Some("abcdefghij"));
    assert!(result.continuation_stop.is_none());
}

#[test]
fn overlapping_owners_reuse_a_range_without_becoming_a_cycle() {
    let selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
    owner.canonical_range = Some(CanonicalByteRange { start: 0, end: 10 });
    owner.continuation = Some(selector.clone());
    let mut second = owner.clone();
    second.selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
    let mut page = continuation_result(selector.clone(), None, "abcdefghij");
    page.canonical_range = owner.canonical_range;
    let mut state =
        RecoveryContinuationState::new(recovery_output(vec![owner, second]), usize::MAX);
    state
        .accept_page(0, &selector, recovery_output(vec![page]))
        .unwrap();
    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 1,
            selector: selector.clone()
        }
    );
    let cached = state
        .cached_page(&selector)
        .expect("authenticated range is already available");
    state.accept_page(1, &selector, cached).unwrap();
    assert_eq!(state.next_step(), ContinuationStep::Complete);
    let result = state.finish();
    assert!(result.output.complete);
    assert_eq!(result.output.results.len(), 2);
    assert!(
        result
            .output
            .results
            .iter()
            .all(|r| r.text.as_deref() == Some("abcdefghij"))
    );
    assert!(result.continuation_stop.is_none());
}

#[test]
fn exact_continuation_pages_are_drained_in_selector_order() {
    let first_selector = page_selector(0);
    let second_selector = page_selector(10);
    let initial = recovery_output(vec![continuation_result(
        first_selector.clone(),
        Some(second_selector.clone()),
        "first page",
    )]);
    let page = recovery_output(vec![continuation_result(
        second_selector.clone(),
        None,
        "second page",
    )]);
    let retained_text = initial.results[0].text.as_ref().unwrap().as_ptr();
    let mut state = RecoveryContinuationState::new(initial, usize::MAX);

    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 0,
            selector: second_selector.clone(),
        }
    );
    assert_eq!(state.accept_page(0, &second_selector, page), Ok(()));
    assert_eq!(
        state.output.results[0].text.as_ref().unwrap().as_ptr(),
        retained_text,
        "accepting a page must retain previously recovered text without copying it",
    );
    assert_eq!(state.next_step(), ContinuationStep::Complete);

    let transaction = state.finish();
    assert_eq!(transaction.drained_continuation_pages, 1);
    assert!(transaction.output.complete);
    assert_eq!(
        transaction
            .output
            .results
            .iter()
            .map(|result| result.selector.clone())
            .collect::<Vec<_>>(),
        vec![first_selector, second_selector]
    );
    assert!(
        transaction
            .output
            .results
            .iter()
            .all(|result| result.continuation.is_none())
    );
}

#[test]
fn final_page_is_admitted_using_exact_reconstruction_and_shared_coverage() {
    let child = ToolOutputSelector::Bytes { start: 0, end: 12 };
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
    owner.canonical_range = Some(CanonicalByteRange::new(0, 12));
    owner.continuation = Some(child.clone());
    owner.message = Some("large descriptor metadata ".repeat(100));
    let mut page = continuation_result(child.clone(), None, "exact source");
    page.canonical_range = owner.canonical_range;
    let exact = reconstruct_selection(&owner, &[page.clone()]).unwrap();
    let expected = recovery_output(vec![exact.clone(), exact]);
    let ceiling =
        codex_utils_string::approx_token_count(&serde_json::to_string(&expected).unwrap()) + 10;
    let mut state = RecoveryContinuationState::new(
        recovery_output(vec![owner.clone(), owner]),
        ceiling,);
    assert_eq!(
        state.accept_page(0, &child, recovery_output(vec![page])),
        Ok(())
    );
    // A different owner may request the same authenticated range.
    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 1,
            selector: child.clone()
        }
    );
    let cached = state
        .cached_page(&child)
        .expect("reuse identity-checked bytes");
    assert_eq!(state.accept_page(1, &child, cached), Ok(()));
    let transaction = state.finish();
    assert!(transaction.output.complete);
    assert_eq!(transaction.output.results.len(), 2);
    assert!(
        transaction
            .output
            .results
            .iter()
            .all(|result| result.text.as_deref() == Some("exact source"))
    );
    assert!(recovery_result_fits_token_ceiling(
        &transaction.output,
        ceiling
    ));
}

#[test]
fn recovery_reuses_partially_overlapping_ranges_and_reserves_unrelated_output() {
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Bytes { start: 2, end: 8 };
    owner.canonical_range = Some(CanonicalByteRange::new(2, 8));
    let mut page = continuation_result(
        ToolOutputSelector::Bytes { start: 0, end: 10 },
        None,
        "0123456789",
    );
    page.canonical_range = Some(CanonicalByteRange::new(0, 10));
    assert_eq!(
        reconstruct_selection(&owner, &[page.clone()])
            .unwrap()
            .text
            .as_deref(),
        Some("234567")
    );
    let state = RecoveryContinuationState::new(recovery_output(vec![owner, page]), 1000);
    assert!(state.page_token_ceiling(0) < 1000);
    assert!(state.page_token_ceiling(0) > 0);
}

#[test]
fn continuation_budget_stop_preserves_first_unconsumed_selector() {
    let first_selector = page_selector(0);
    let second_selector = page_selector(10);
    let initial = recovery_output(vec![continuation_result(
        first_selector,
        Some(second_selector.clone()),
        "first page",
    )]);
    let initial_tokens = codex_utils_string::approx_token_count(
        &serde_json::to_string(&initial).expect("serialize initial page"),
    );
    let oversized_page = recovery_output(vec![continuation_result(
        second_selector.clone(),
        None,
        &"x".repeat(10_000),
    )]);
    let mut state = RecoveryContinuationState::new(initial.clone(), initial_tokens + 1);

    assert_eq!(
        state.accept_page(0, &second_selector, oversized_page),
        Err(ContinuationStopReason::Budget)
    );
    let transaction = state.finish();
    assert_eq!(transaction.output, initial);
    assert_eq!(transaction.drained_continuation_pages, 0);
    assert_eq!(
        transaction.output.results[0].continuation,
        Some(second_selector)
    );
}

#[test]
fn continuation_identity_drift_stops_without_mutating_the_aggregate() {
    let second_selector = page_selector(10);
    let initial = recovery_output(vec![continuation_result(
        page_selector(0),
        Some(second_selector.clone()),
        "first page",
    )]);
    let mut drifted_page = recovery_output(vec![continuation_result(
        second_selector.clone(),
        None,
        "second page",
    )]);
    drifted_page.canonical_sha256 = "different-revision".to_string();
    let mut state = RecoveryContinuationState::new(initial.clone(), usize::MAX);

    assert_eq!(
        state.accept_page(0, &second_selector, drifted_page),
        Err(ContinuationStopReason::IdentityDrift)
    );
    assert_eq!(state.finish().output, initial);
}

#[test]
fn aggregate_omission_may_retry_its_owner_advertised_selector_once() {
    let selector = ToolOutputSelector::Lines { start: 1, end: 10 };
    let mut omitted = selector_result(ToolOutputSelectorStatus::AggregateOmitted);
    omitted.selector = selector.clone();
    omitted.continuation = Some(selector.clone());
    let initial = recovery_output(vec![omitted]);
    let page = recovery_output(vec![continuation_result(
        selector.clone(),
        None,
        "exact retry",
    )]);
    let mut state = RecoveryContinuationState::new(initial, usize::MAX);

    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 0,
            selector: selector.clone(),
        }
    );
    assert_eq!(state.accept_page(0, &selector, page), Ok(()));
    assert_eq!(state.next_step(), ContinuationStep::Complete);
}

#[test]
fn repeated_owner_continuation_is_never_followed_twice() {
    let second_selector = page_selector(10);
    let initial = recovery_output(vec![continuation_result(
        page_selector(0),
        Some(second_selector.clone()),
        "first page",
    )]);
    let repeated_page = recovery_output(vec![continuation_result(
        second_selector.clone(),
        Some(second_selector.clone()),
        "second page",
    )]);
    let mut state = RecoveryContinuationState::new(initial, usize::MAX);

    assert_eq!(
        state.accept_page(0, &second_selector, repeated_page),
        Ok(())
    );
    assert_eq!(
        state.next_step(),
        ContinuationStep::Stop(ContinuationStopReason::RepeatedSelector)
    );
    let transaction = state.finish();
    assert_eq!(transaction.drained_continuation_pages, 1);
    assert_eq!(
        transaction.output.results[1].continuation,
        Some(second_selector)
    );
}

#[test]
fn selector_overflow_drains_the_owner_subdivision_plan_and_preserves_its_contract() {
    let parent_selector = ToolOutputSelector::Lines { start: 1, end: 100 };
    let child_selector = ToolOutputSelector::Bytes { start: 0, end: 10 };
    let second_child_selector = ToolOutputSelector::Bytes { start: 10, end: 20 };
    let mut overflow = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    overflow.selector = parent_selector;
    overflow.canonical_range = Some(CanonicalByteRange { start: 0, end: 20 });
    overflow.subdivision_plan = Some(ByteSubdivisionPlan {
        range: CanonicalByteRange { start: 0, end: 20 },
        chunk_bytes: 10,
        chunk_count: 2,
        selector_kind: "bytes".to_string(),
    });
    overflow.child_selectors = vec![child_selector.clone()];
    overflow.continuation = Some(child_selector.clone());
    let initial = recovery_output(vec![overflow]);
    let first_page = recovery_output(vec![continuation_result(
        child_selector.clone(),
        None,
        "exact child",
    )]);
    let second_page = recovery_output(vec![continuation_result(
        second_child_selector.clone(),
        None,
        "second exact child",
    )]);
    let mut state = RecoveryContinuationState::new(initial, usize::MAX);

    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 0,
            selector: child_selector.clone(),
        }
    );
    assert_eq!(
        state.accept_page(0, &child_selector, first_page),
        Ok(())
    );
    assert_eq!(
        state.next_step(),
        ContinuationStep::Follow {
            result_index: 0,
            selector: second_child_selector.clone(),
        }
    );
    assert_eq!(
        state.accept_page(0, &second_child_selector, second_page),
        Ok(())
    );
    assert_eq!(state.next_step(), ContinuationStep::Complete);
    let transaction = state.finish();
    assert!(!transaction.output.complete);
    assert_eq!(
        transaction.output.results[0].status,
        ToolOutputSelectorStatus::SelectorTooLarge
    );
    assert_eq!(
        transaction.output.results[0].child_selectors,
        vec![child_selector]
    );
    assert!(transaction.output.results[0].continuation.is_none());
    assert_eq!(transaction.drained_continuation_pages, 2);
    assert_eq!(
        transaction.output.results[2].selector,
        second_child_selector
    );
}

#[test]
fn typed_overflow_preserves_artifact_completeness() {
    let output = crate::tools::command_output_artifact::ReadToolOutputResult {
        artifact_id: "artifact".to_string(),
        canonical_sha256: "digest".to_string(),
        canonical_bytes: 1,
        retained_bytes: 1,
        complete: false,
        unavailable_ranges: Vec::new(),
        results: vec![
            selector_result(ToolOutputSelectorStatus::Ok),
            selector_result(ToolOutputSelectorStatus::SelectorTooLarge),
            selector_result(ToolOutputSelectorStatus::AggregateOmitted),
            selector_result(ToolOutputSelectorStatus::NotFound),
        ],
    };

    let delivered = serde_json::to_value(output).expect("recovery result");
    assert_eq!(delivered["retained_artifact_complete"], true);
    assert_eq!(delivered["delivered_selection_complete"], false);
}

#[test]
fn exact_code_mode_recovery_is_carried_by_owner_receipt() {
    let output = crate::tools::command_output_artifact::ReadToolOutputResult {
        artifact_id: "01900000-0000-7000-8000-000000000000".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 5,
        retained_bytes: 5,
        complete: true,
        unavailable_ranges: Vec::new(),
        results: vec![selector_result(ToolOutputSelectorStatus::Ok)],
    };

    let receipt =
        exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 2)
            .expect("exact nested recovery receipt");

    assert_eq!(receipt.state_revision, "canonical-revision");
    assert_eq!(receipt.action_bounds_hash, "selector-bounds");
    assert_eq!(receipt.suppressed_continuation_count, 2);
    assert!(
        exact_code_mode_recovery_receipt(false, &output, "selector-bounds".to_string(), 2,)
            .is_none()
    );
    assert!(
        exact_code_mode_recovery_receipt(true, &output, "selector-bounds".to_string(), 0,)
            .is_none()
    );
    let value = serde_json::to_value(output).expect("recovery output");
    let mut tool_output = ReadToolOutputToolOutput {
        inner: JsonToolOutput::new(value.clone()),
        exact_recovery: Some(receipt.clone()),
    };
    assert_eq!(
        tool_output.deterministic_continuation_receipts(),
        vec![receipt]
    );
    assert_eq!(
        tool_output.deterministic_continuation_content(),
        vec![value]
    );
    tool_output.exact_recovery = None;
    assert!(tool_output.deterministic_continuation_receipts().is_empty());
    assert!(tool_output.deterministic_continuation_content().is_empty());
}

#[test]
fn complete_artifact_recovery_reuses_the_producers_canonical_identity() {
    let mut result = selector_result(ToolOutputSelectorStatus::Ok);
    result.canonical_range = Some(codex_tools::CanonicalByteRange::new(0, 5));
    result.data_base64 = Some("aGVsbG8=".into());
    let output = ReadToolOutputResult {
        artifact_id: "artifact".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 5,
        retained_bytes: 5,
        complete: true,
        unavailable_ranges: Vec::new(),
        results: vec![result],
    };

    assert_eq!(
        output.delivered_evidence(),
        Some(serde_json::json!({"sha256":"canonical-revision","ranges":[[0,5]],"values":[]}))
    );
}

#[test]
fn recovered_text_identity_deduplicates_delivered_coverage() {
    let mut result = selector_result(ToolOutputSelectorStatus::Ok);
    result.text = Some("src/lib.rs:10:let stable = compute();".to_string());
    result.canonical_range = Some(CanonicalByteRange::new(0, 35));
    let output = ReadToolOutputResult {
        artifact_id: "artifact".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 40,
        retained_bytes: 40,
        complete: true,
        unavailable_ranges: Vec::new(),
        results: vec![result],
    };

    let evidence = output.delivered_evidence().expect("delivered text");
    assert_eq!(evidence["ranges"], serde_json::json!([[0,35]]));
    let mut repeated = output;
    repeated.results.push(repeated.results[0].clone());
    assert_eq!(
        repeated.delivered_evidence(),
        Some(evidence)
    );
}

#[test]
fn recovered_text_preserves_incomplete_selector_status() {
    let mut recovered = selector_result(ToolOutputSelectorStatus::Ok);
    recovered.text = Some("src/lib.rs:10:let stable = compute();".to_string());
    recovered.canonical_range = Some(CanonicalByteRange::new(0, 35));
    let complete = ReadToolOutputResult {
        artifact_id: "artifact".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 40,
        retained_bytes: 40,
        complete: true,
        unavailable_ranges: Vec::new(),
        results: vec![recovered.clone()],
    };
    let incomplete = ReadToolOutputResult {
        results: vec![
            recovered,
            selector_result(ToolOutputSelectorStatus::AggregateOmitted),
        ],
        ..complete.clone()
    };

    assert_eq!(
        incomplete.delivered_evidence(),
        complete.delivered_evidence()
    );
    // Failure status remains visible, but is not additional source coverage.
    let raw = serde_json::to_value(incomplete).unwrap();
    assert_eq!(raw["results"][1]["status"], "aggregate_omitted");
}

#[test]
fn disjoint_recovered_fragments_do_not_create_synthetic_semantic_facts() {
    let fragments = [
        "diff --git a/src/lib.rs b/src/lib.rs",
        "@@ -9,0 +10 @@\n+let stable = compute();",
    ];
    let mut results = Vec::new();
    for (index, fragment) in fragments.into_iter().enumerate() {
        let mut result = selector_result(ToolOutputSelectorStatus::Ok);
        result.text = Some(fragment.to_string());
        result.canonical_range = Some(CanonicalByteRange::new(
            index as u64 * 40, index as u64 * 40 + fragment.len() as u64,
        ));
        results.push(result);
    }
    let output = ReadToolOutputResult {
        artifact_id: "artifact".to_string(),
        canonical_sha256: "canonical-revision".to_string(),
        canonical_bytes: 80,
        retained_bytes: 80,
        complete: true,
        unavailable_ranges: Vec::new(),
        results,
    };
    let evidence = output.delivered_evidence().unwrap();
    assert_eq!(evidence["ranges"], serde_json::json!([
        [0,fragments[0].len()], [40,40 + fragments[1].len()]
    ]));
}

#[test]
fn recovered_fragment_identity_uses_delivered_ranges_not_selector_spelling() {
    let mut first = selector_result(ToolOutputSelectorStatus::Ok);
    first.selector = ToolOutputSelector::Lines { start: 1, end: 1 };
    first.text = Some("same recovered fact".to_string());
    first.canonical_range = Some(CanonicalByteRange::new(0, 19));
    let mut second = first.clone();
    second.selector = ToolOutputSelector::Lines { start: 2, end: 2 };
    second.canonical_range = Some(CanonicalByteRange::new(20, 39));
    let first = recovery_output(vec![first]);
    let mut second = recovery_output(vec![second]);
    assert_ne!(first.delivered_evidence(), second.delivered_evidence());
    second.results[0].canonical_range = first.results[0].canonical_range;
    assert_eq!(first.delivered_evidence(), second.delivered_evidence());
}

#[test]
fn continuation_stop_is_typed_and_preserves_the_unconsumed_selector() {
    let selector = page_selector(10);
    let initial = recovery_output(vec![continuation_result(
        page_selector(0),
        Some(selector.clone()),
        "first page",
    )]);
    let mut state = RecoveryContinuationState::new(initial, usize::MAX);
    state.record_stop(ContinuationStopReason::Budget, Some(selector.clone()));

    let stop = state
        .finish()
        .continuation_stop
        .expect("typed stop receipt");
    assert_eq!(stop.reason, ContinuationStopReason::Budget);
    assert_eq!(stop.selector, Some(selector));
    assert!(stop.resumable);
    assert_eq!(
        stop.message.as_deref(),
        Some(
            "Recovery reached its output budget. Continue with the unconsumed selector in a new call."
        )
    );
    assert_eq!(
        serde_json::to_value(stop).expect("serialize stop")["reason"],
        "budget"
    );
}

#[test]
fn resumable_byte_stop_preserves_the_remaining_owner_suffix() {
    let child = ToolOutputSelector::Bytes { start: 7, end: 11 };
    let mut owner = selector_result(ToolOutputSelectorStatus::SelectorTooLarge);
    owner.selector = ToolOutputSelector::Bytes { start: 3, end: 31 };
    owner.canonical_range = Some(CanonicalByteRange::new(3, 31));
    owner.continuation = Some(child.clone());
    owner.child_selectors = vec![child.clone()];
    assert_eq!(
        next_owner_continuation(&owner, &child),
        Some(ToolOutputSelector::Bytes { start: 11, end: 31 }),
        "an absent uniform subdivision plan cannot erase the remaining suffix"
    );
    for reason in [
        ContinuationStopReason::Budget,
        ContinuationStopReason::Cancelled,
    ] {
        let mut state = RecoveryContinuationState::new(
            recovery_output(vec![owner.clone()]),
            usize::MAX,);
        state.record_stop(reason, Some(child.clone()));
        let stop = state.finish().continuation_stop.unwrap();
        assert!(stop.resumable);
        assert_eq!(
            stop.selector,
            Some(ToolOutputSelector::Bytes { start: 7, end: 31 })
        );
    }
    let mut state =
        RecoveryContinuationState::new(recovery_output(vec![owner]), usize::MAX);
    state.record_page_read_error(&ReadToolOutputError::StillWriting, child);
    assert_eq!(
        state.finish().continuation_stop.unwrap().selector,
        Some(ToolOutputSelector::Bytes { start: 7, end: 31 })
    );
}

#[test]
fn continuation_page_error_preserves_retryability_and_cause() {
    let selector = page_selector(10);
    let cases = [
        (ReadToolOutputError::InvalidArtifactId, false),
        (
            ReadToolOutputError::InvalidRange("invalid continuation range".to_string()),
            false,
        ),
        (ReadToolOutputError::Expired, false),
        (ReadToolOutputError::StillWriting, true),
        (
            ReadToolOutputError::Io("artifact storage unavailable".to_string()),
            false,
        ),
    ];

    for (error, resumable) in cases {
        let expected_message = error.for_model();
        let mut state = RecoveryContinuationState::new(
            recovery_output(vec![selector_result(ToolOutputSelectorStatus::Ok)]),
            usize::MAX,);
        state.record_page_read_error(&error, selector.clone());
        let stop = state
            .finish()
            .continuation_stop
            .expect("page error stop receipt");
        assert_eq!(stop.reason, ContinuationStopReason::PageReadError);
        assert_eq!(stop.selector, Some(selector.clone()));
        assert_eq!(stop.resumable, resumable);
        assert_eq!(stop.message.as_deref(), Some(expected_message.as_str()));
    }
}

#[test]
fn default_range_is_exactly_two_hundred_lines() {
    let args = ReadToolOutputArgs {
        artifact_id: uuid::Uuid::now_v7().to_string(),
        selectors: None,
        start_line: Some(17),
        end_line: None,
        ranges: None,
        max_bytes: None,
    };
    assert_eq!(resolved_line_range(&args).unwrap(), (17, 216));
}

#[test]
fn legacy_single_range_uses_the_canonical_line_invariants() {
    for (start_line, end_line) in [(0, Some(1)), (3, Some(2))] {
        let args = ReadToolOutputArgs {
            artifact_id: uuid::Uuid::now_v7().to_string(),
            selectors: None,
            start_line: Some(start_line),
            end_line,
            ranges: None,
            max_bytes: None,
        };
        assert!(resolved_selectors(&args).is_err());
    }

    let args = ReadToolOutputArgs {
        artifact_id: uuid::Uuid::now_v7().to_string(),
        selectors: None,
        start_line: Some(3),
        end_line: Some(3),
        ranges: None,
        max_bytes: None,
    };
    assert_eq!(
        resolved_selectors(&args).unwrap(),
        vec![ToolOutputSelector::Lines { start: 3, end: 3 }]
    );
}

#[test]
fn max_bytes_is_legacy_validated_but_not_a_clipping_contract() {
    assert_eq!(resolved_max_bytes(None).unwrap(), 16_384);
    assert_eq!(resolved_max_bytes(Some(1)).unwrap(), 1);
    assert_eq!(resolved_max_bytes(Some(16_384)).unwrap(), 16_384);
    assert!(resolved_max_bytes(Some(0)).is_err());
    for large in [16_385, usize::MAX] {
        assert_eq!(resolved_max_bytes(Some(large)).unwrap(), 16_384);
    }
}

#[test]
fn read_tool_output_schema_matches_runtime_bounds() {
    let tool = serde_json::to_value(create_read_tool_output_tool())
        .expect("serialize read_tool_output tool");
    let validator = jsonschema::validator_for(&tool["parameters"])
        .expect("compile read_tool_output schema");
    let line_selector = serde_json::json!({
        "kind": "lines",
        "start": 1,
        "end": 1,
    });
    let selector_args = |count: usize| {
        serde_json::json!({
            "artifact_id": "artifact",
            "selectors": vec![line_selector.clone(); count],
        })
    };
    let range_args = |count: usize| {
        serde_json::json!({
            "artifact_id": "artifact",
            "ranges": (1..=count)
                .map(|line| serde_json::json!({
                    "start_line": line,
                    "end_line": line,
                }))
                .collect::<Vec<_>>(),
        })
    };
    let runtime_accepts = |value: &Value| {
        parse_read_tool_output_args(&value.to_string())
            .ok()
            .is_some_and(|args| {
                resolved_max_bytes(args.max_bytes).is_ok() && resolved_selectors(&args).is_ok()
            })
    };
    let cases = [
        (selector_args(1), true, true),
        (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS), true, true),
        (selector_args(0), false, false),
        (selector_args(READ_TOOL_OUTPUT_MAX_SELECTORS + 1), false, false),
        (range_args(1), false, true),
        (range_args(64), false, true),
        (range_args(0), false, false),
        (range_args(65), false, false),
    ];

    for (arguments, schema_expected, runtime_expected) in cases {
        assert_eq!(
            validator.is_valid(&arguments),
            schema_expected,
            "schema verdict for {arguments}"
        );
        assert_eq!(
            runtime_accepts(&arguments),
            runtime_expected,
            "runtime verdict for {arguments}"
        );
    }

    // Old callers may still supply max_bytes, but new calls must use the
    // selector bounds advertised by the schema instead of a clipping knob.
    for (max_bytes, runtime_expected) in [
        (0, false),
        (1, true),
        (READ_TOOL_OUTPUT_MAX_BYTES, true),
        (READ_TOOL_OUTPUT_MAX_BYTES + 1, false),
    ] {
        let mut arguments = selector_args(1);
        arguments["max_bytes"] = serde_json::json!(max_bytes);
        assert!(
            !validator.is_valid(&arguments),
            "legacy field is not advertised"
        );
        assert_eq!(
            runtime_accepts(&arguments),
            runtime_expected,
            "legacy runtime verdict for {arguments}"
        );
    }
}

#[test]
fn invalid_recovery_selectors_defer_shape_to_advertised_schema() {
    let error = parse_read_tool_output_args(
        r#"{"artifact_id":"artifact","selector":{"kind":"line","start":1,"end":2}}"#,
    )
    .expect_err("singular selector and line kind must be rejected");
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("parse failures must be returned to the model");
    };

    assert!(message.contains("advertised read_tool_output schema"));
    assert!(!message.contains(r#""selectors""#));
    assert!(!message.contains(r#"{"artifact_id""#));
}

#[test]
fn three_exact_ranges_become_one_bounded_owner_batch() {
    let args = ReadToolOutputArgs {
        artifact_id: uuid::Uuid::now_v7().to_string(),
        selectors: None,
        start_line: None,
        end_line: None,
        ranges: Some(vec![
            ReadToolOutputRangeArgs {
                start_line: 2,
                end_line: 4,
            },
            ReadToolOutputRangeArgs {
                start_line: 11,
                end_line: 13,
            },
            ReadToolOutputRangeArgs {
                start_line: 21,
                end_line: 25,
            },
        ]),
        max_bytes: None,
    };

    assert_eq!(
        resolved_selectors(&args).unwrap(),
        vec![
            ToolOutputSelector::Lines { start: 2, end: 4 },
            ToolOutputSelector::Lines { start: 11, end: 13 },
            ToolOutputSelector::Lines { start: 21, end: 25 },
        ]
    );
}

#[test]
fn legacy_ranges_are_sorted_merged_and_capped_at_sixteen() {
    let mut args = ReadToolOutputArgs {
        artifact_id: uuid::Uuid::now_v7().to_string(),
        selectors: None,
        start_line: None,
        end_line: None,
        ranges: Some(vec![
            ReadToolOutputRangeArgs {
                start_line: 10,
                end_line: 12,
            },
            ReadToolOutputRangeArgs {
                start_line: 2,
                end_line: 4,
            },
            ReadToolOutputRangeArgs {
                start_line: 4,
                end_line: 6,
            },
            ReadToolOutputRangeArgs {
                start_line: 7,
                end_line: 9,
            },
        ]),
        max_bytes: None,
    };

    assert_eq!(
        resolved_selectors(&args).unwrap(),
        vec![ToolOutputSelector::Lines { start: 2, end: 12 }]
    );

    args.ranges = Some(
        (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES)
            .map(|line| ReadToolOutputRangeArgs {
                start_line: line * 2,
                end_line: line * 2,
            })
            .collect(),
    );
    assert_eq!(
        resolved_selectors(&args).unwrap().len(),
        READ_TOOL_OUTPUT_MAX_LEGACY_RANGES
    );

    args.ranges = Some(
        (1..=READ_TOOL_OUTPUT_MAX_LEGACY_RANGES + 1)
            .map(|line| ReadToolOutputRangeArgs {
                start_line: line * 2,
                end_line: line * 2,
            })
            .collect(),
    );
    assert!(resolved_selectors(&args).is_err());

    args.ranges = Some(vec![
        ReadToolOutputRangeArgs {
            start_line: 1,
            end_line: 1_000,
        },
        ReadToolOutputRangeArgs {
            start_line: 2_000,
            end_line: 3_000,
        },
    ]);
    assert!(resolved_selectors(&args).is_err());
}
