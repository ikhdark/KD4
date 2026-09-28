use super::*;

#[test]
fn turn_interrupt_rejects_retained_interrupted_snapshot() {
    let error = validate_turn_interrupt_target(
        Some(("turn-1", &TurnStatus::Interrupted)),
        /*is_running*/ true,
        "turn-1",
    )
    .expect_err("an interrupted presentation snapshot must not be treated as active");

    assert_eq!(error.message, "no active turn to interrupt");
}

#[test]
fn turn_interrupt_allows_running_startup_race_without_snapshot() {
    validate_turn_interrupt_target(None, /*is_running*/ true, "turn-1")
        .expect("core may report running before TurnStarted is projected");
}

#[test]
fn turn_start_rejects_an_active_turn_and_directs_the_client_to_steer() {
    let error = validate_turn_start_target(Some("turn-active"), /*is_running*/ true)
        .expect_err("turn/start must not submit input to an active turn");

    assert!(error.message.contains("turn/steer"));
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "reason": "activeTurnInProgress",
            "turnId": "turn-active",
        }))
    );

    let error = validate_turn_start_target(None, /*is_running*/ true)
        .expect_err("turn/start must reject the core-running projection race");
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "reason": "activeTurnInProgress",
        }))
    );
}

#[test]
fn in_flight_task_coalescing_fingerprint_preserves_text_and_normalizes_identity_fields() {
    let params = |thread_id: &str, text: &str, run_independently| TurnStartParams {
        thread_id: thread_id.to_string(),
        client_user_message_id: Some(format!("client-{thread_id}")),
        run_independently,
        input: vec![V2UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    };
    let first = params("thread-1", "fix   the\n bug", None);
    let mut same_task = params("thread-1", "fix   the\n bug", Some(true));
    same_task.client_user_message_id = Some("another-client-message".to_string());
    let whitespace_changed_task = params("thread-1", "fix the bug", Some(true));

    let first_fingerprint = normalized_task_fingerprint(&first, "C:/repo", "model=o3");
    assert!(first_fingerprint.is_some());
    assert_ne!(
        first_fingerprint,
        normalized_task_fingerprint(
            &params("thread-2", "fix   the\n bug", None),
            "C:/repo",
            "model=o3"
        )
    );
    assert_eq!(
        first_fingerprint,
        normalized_task_fingerprint(&same_task, "C:/repo", "model=o3")
    );
    assert_ne!(
        first_fingerprint,
        normalized_task_fingerprint(&whitespace_changed_task, "C:/repo", "model=o3")
    );
    assert_ne!(
        first_fingerprint,
        normalized_task_fingerprint(&same_task, "C:/other", "model=o3")
    );
    assert_ne!(
        first_fingerprint,
        normalized_task_fingerprint(&same_task, "C:/repo", "model=gpt-5")
    );
}

#[test]
fn task_workspace_identity_uses_one_snapshot_projection_for_defaults_and_overrides() {
    let workspace = tempfile::TempDir::new().expect("workspace");
    let fallback_cwd = AbsolutePathBuf::from_absolute_path(workspace.path().join("repo"))
        .expect("absolute fallback cwd");
    let fallback_roots = vec![fallback_cwd.clone()];
    let params = TurnStartParams::default();

    assert_eq!(
        task_workspace_identity(&params, &fallback_cwd, &fallback_roots),
        format!("cwd={fallback_cwd:?};roots={fallback_roots:?}")
    );

    let override_cwd = AbsolutePathBuf::from_absolute_path(workspace.path().join("other"))
        .expect("absolute override cwd");
    let override_roots = vec![override_cwd.clone()];
    let params = TurnStartParams {
        cwd: Some(override_cwd.to_path_buf()),
        runtime_workspace_roots: Some(override_roots.clone()),
        ..Default::default()
    };

    assert_eq!(
        task_workspace_identity(&params, &fallback_cwd, &fallback_roots),
        format!("cwd={override_cwd:?};roots={override_roots:?}")
    );
}

#[test]
fn in_flight_task_coalescing_returns_reuse_coordinates() {
    let thread_id = codex_protocol::ThreadId::new();
    let existing = crate::thread_state::InFlightTaskReference {
        thread_id,
        turn_id: "turn-existing".to_string(),
    };

    let error = identical_task_in_flight_error(&existing);

    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "reason": "identicalTaskInFlight",
            "threadId": thread_id.to_string(),
            "turnId": "turn-existing",
            "runIndependentlyOverride": "runIndependently",
        }))
    );
}

#[test]
fn in_flight_task_capacity_error_is_retryable() {
    let error = in_flight_task_capacity_error();

    assert_eq!(error.code, codex_app_server_protocol::OVERLOADED_ERROR_CODE);
    assert_eq!(
        serde_json::from_value::<codex_app_server_protocol::OverloadErrorData>(
            error.data.expect("overload error data")
        )
        .expect("valid overload error data"),
        codex_app_server_protocol::OverloadErrorData {
            reason: codex_app_server_protocol::OverloadReason::InFlightTaskCapacity,
            retryable: true,
        }
    );
}

#[tokio::test]
async fn missing_error_path_rejected_task_does_not_apply_connection_updates() {
    let manager = ThreadStateManager::new();
    let existing_thread_id = ThreadId::new();
    let fingerprint = "same-task".to_string();
    manager
        .claim_turn_start(Some(&fingerprint), existing_thread_id, "turn-existing")
        .await;
    let updates_applied = std::cell::Cell::new(false);

    let error = claim_turn_start_before_connection_updates(
        &manager,
        Some(&fingerprint),
        ThreadId::new(),
        "turn-rejected",
        || async {
            updates_applied.set(true);
            Ok(())
        },
    )
    .await
    .expect_err("duplicate task must be rejected");

    assert!(!updates_applied.get());
    assert_eq!(
        error
            .data
            .and_then(|data| data["reason"].as_str().map(str::to_owned)),
        Some("identicalTaskInFlight".to_string())
    );
}

#[tokio::test]
async fn reserved_turn_start_on_one_thread_is_rejected_before_connection_updates() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let first = claim_turn_start_before_connection_updates(
        &manager,
        Some("first-task"),
        thread_id,
        "turn-first",
        || async {
            admitted_tx.send(()).expect("signal admitted request");
            release_rx
                .await
                .expect("second request must release the first");
            Ok(())
        },
    );
    let updates_applied = std::cell::Cell::new(false);

    let second = async {
        admitted_rx
            .await
            .expect("first request reached connection update");
        let result = claim_turn_start_before_connection_updates(
            &manager,
            Some("second-task"),
            thread_id,
            "turn-second",
            || async {
                updates_applied.set(true);
                Ok(())
            },
        )
        .await;
        release_tx
            .send(())
            .expect("release pending connection update");
        result
    };
    let (first, second) = tokio::join!(first, second);
    first.expect("first turn should reserve the thread");
    let error = second.expect_err("a second turn/start must not become steering input");

    assert!(!updates_applied.get());
    assert_eq!(
        error.data,
        Some(serde_json::json!({
            "reason": "activeTurnInProgress",
            "turnId": "turn-first",
        }))
    );
}

#[tokio::test]
async fn missing_error_path_failed_connection_update_releases_task_claim() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let fingerprint = "retryable-task".to_string();

    claim_turn_start_before_connection_updates(
        &manager,
        Some(&fingerprint),
        thread_id,
        "turn-failed",
        || async { Err(internal_error("injected connection update failure")) },
    )
    .await
    .expect_err("connection update should fail");

    assert_eq!(
        manager
            .claim_turn_start(Some(&fingerprint), thread_id, "turn-retry")
            .await,
        crate::thread_state::TurnStartClaim::Claimed
    );
}

#[tokio::test]
async fn rejected_core_start_releases_task_claim_for_exact_retry() {
    let manager = ThreadStateManager::new();
    let thread_id = ThreadId::new();
    let fingerprint = "retryable-core-rejection";
    assert_eq!(
        manager
            .claim_turn_start(Some(fingerprint), thread_id, "turn-rejected")
            .await,
        crate::thread_state::TurnStartClaim::Claimed
    );

    let error = release_turn_start_after_submission_error(
        &manager,
        thread_id,
        "turn-rejected",
        CodexErr::InvalidRequest("a turn is already active".to_string()),
    )
    .await;

    assert_eq!(error.code, -32600);
    assert_eq!(error.message, "a turn is already active");
    assert_eq!(
        manager
            .claim_turn_start(Some(fingerprint), thread_id, "turn-retry")
            .await,
        crate::thread_state::TurnStartClaim::Claimed
    );
}

fn additional_context_entry(value: impl Into<String>) -> AdditionalContextEntry {
    AdditionalContextEntry {
        value: value.into(),
        kind: AdditionalContextKind::Untrusted,
    }
}

#[test]
fn map_additional_context_rejects_oversized_source_identifier() {
    let source = "s".repeat(MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES + 1);
    let additional_context = IndexMap::from([(source, additional_context_entry("value"))]);

    let error = map_additional_context(Some(additional_context))
        .expect_err("oversized additional-context source should be rejected");

    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!(
            "additionalContext source identifiers may contain at most {MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES} bytes (longest was {} bytes)",
            MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES + 1
        )
    );
}

#[test]
fn map_additional_context_rejects_too_many_entries() {
    let additional_context = (0..=MAX_ADDITIONAL_CONTEXT_ENTRIES)
        .map(|index| (format!("source-{index}"), additional_context_entry("value")))
        .collect();

    let error = map_additional_context(Some(additional_context))
        .expect_err("excess additional-context entries should be rejected");

    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!(
            "additionalContext may contain at most {MAX_ADDITIONAL_CONTEXT_ENTRIES} entries (received {})",
            MAX_ADDITIONAL_CONTEXT_ENTRIES + 1
        )
    );
}

#[test]
fn map_additional_context_rejects_aggregate_rendered_size() {
    // Escaping exceeds the render budget while the raw bytes remain admissible.
    let value = "&".repeat(MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES / 5 + 1);
    let entry_count = MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES
        / (MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES
            + ESTIMATED_ADDITIONAL_CONTEXT_WRAPPER_BYTES)
        + 1;
    assert!(entry_count <= MAX_ADDITIONAL_CONTEXT_ENTRIES);
    let additional_context = (0..entry_count)
        .map(|index| {
            (
                format!("source-{index}"),
                additional_context_entry(value.clone()),
            )
        })
        .collect();

    let error = map_additional_context(Some(additional_context))
        .expect_err("aggregate additional-context size should be rejected");

    assert_eq!(error.code, -32600);
    assert!(
        error.message.starts_with(&format!(
            "additionalContext may render to at most {MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES} bytes"
        )),
        "unexpected error: {}",
        error.message
    );
}

#[test]
fn additional_context_raw_budget_counts_all_utf8_bytes_and_keys() {
    let limit = MAX_ADDITIONAL_CONTEXT_AGGREGATE_RAW_BYTES;
    for value in ["v".repeat(limit), "é".repeat(limit / 2)] {
        let error = map_additional_context(Some(IndexMap::from([(
            "s".to_string(),
            additional_context_entry(value),
        )])))
        .expect_err("the full original value must be counted");
        assert_eq!(
            error.data,
            Some(serde_json::json!({
                "reason": "additionalContextTooLarge", "maxBytes": limit, "actualBytes": limit + 1,
            }))
        );
    }
    let value = "v".repeat(limit - 1);
    let mapped = map_additional_context(Some(IndexMap::from([(
        "s".to_string(),
        additional_context_entry(value.clone()),
    )])))
    .expect("exact raw boundary is allowed");
    assert_eq!(mapped["s"].value, value);
    let error = map_additional_context(Some(IndexMap::from([
        (
            "a".to_string(),
            additional_context_entry("x".repeat(limit / 2)),
        ),
        (
            "b".to_string(),
            additional_context_entry("x".repeat(limit / 2)),
        ),
    ])))
    .expect_err("aggregate budget includes both keys and values");
    assert_eq!(error.data.unwrap()["actualBytes"], limit + 2);
}

#[test]
fn steering_errors_preserve_recovery_details() {
    for (input, expected) in [
        (
            SteerInputError::NoActiveTurn(Vec::new()),
            serde_json::json!({"reason":"noActiveTurn"}),
        ),
        (
            SteerInputError::EmptyInput,
            serde_json::json!({"reason":"emptyInput"}),
        ),
        (
            SteerInputError::ExpectedTurnMismatch {
                expected: "A".into(),
                actual: "B".into(),
            },
            serde_json::json!({"reason":"expectedTurnMismatch","expectedTurnId":"A","actualTurnId":"B"}),
        ),
        (
            SteerInputError::PendingInputLimitExceeded {
                max_items: 32,
                max_bytes: 4096,
            },
            serde_json::json!({"reason":"pendingInputLimitExceeded","maxItems":32,"maxBytes":4096}),
        ),
    ] {
        let (error, _) = steer_input_error(input);
        assert_eq!(error.code, -32600);
        assert_eq!(error.data, Some(expected));
    }
    for kind in [
        codex_protocol::protocol::NonSteerableTurnKind::Review,
        codex_protocol::protocol::NonSteerableTurnKind::Compact,
    ] {
        let (error, _) =
            steer_input_error(SteerInputError::ActiveTurnNotSteerable { turn_kind: kind });
        let data: TurnError = serde_json::from_value(error.data.unwrap()).unwrap();
        assert_eq!(
            data.codex_error_info,
            Some(CodexErrorInfo::ActiveTurnNotSteerable {
                turn_kind: kind.into()
            })
        );
    }
}

#[test]
fn interrupt_acknowledgements_are_targeted_and_claimed_once() {
    let mut state = crate::thread_state::ThreadState::default();
    let request = ConnectionRequestId {
        connection_id: crate::outgoing_message::ConnectionId(1),
        request_id: RequestId::Integer(1),
    };
    let duplicate = ConnectionRequestId {
        connection_id: request.connection_id,
        request_id: RequestId::Integer(2),
    };
    let _first = state.reserve_interrupt(request.clone(), "A".to_string());
    let _second = state.reserve_interrupt(duplicate.clone(), "A".to_string());
    assert!(state.take_pending_interrupts("B").is_empty());
    assert!(state.finish_interrupt(&request));
    assert!(!state.finish_interrupt(&request));
    assert_eq!(state.take_pending_interrupts("A"), vec![duplicate.clone()]);
    assert!(!state.finish_interrupt(&duplicate));
}

#[test]
fn map_additional_context_preserves_client_order() {
    let additional_context = IndexMap::from([
        ("dependency".to_string(), additional_context_entry("first")),
        ("consumer".to_string(), additional_context_entry("second")),
    ]);

    let mapped = map_additional_context(Some(additional_context)).expect("context should map");

    assert_eq!(
        mapped.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["dependency", "consumer"]
    );
}

#[test]
fn additional_context_estimate_caps_escaped_utf8_values() {
    for (value, expected_value_bytes) in [
        ("<&é>".to_string(), 15),
        ("&".repeat(4_000), 16_384),
        ("é".repeat(10_000), 16_384),
    ] {
        let entry = additional_context_entry(value.clone());
        assert_eq!(
            estimated_additional_context_rendered_bytes("source", &entry),
            6 + expected_value_bytes + ESTIMATED_ADDITIONAL_CONTEXT_WRAPPER_BYTES
        );
        let mapped = map_additional_context(Some(IndexMap::from([("source".to_string(), entry)])))
            .expect("one capped value is within the aggregate budget");
        assert_eq!(mapped["source"].value, value);
    }
}

#[test]
fn bug_classifier_accepts_exact_multibyte_evidence_offsets() {
    let raw = "Crash in caf\u{e9}";
    let output = r#"{
        "summary":"A crash is reported.",
        "severity":null,
        "failureMechanism":{"value":"Crash","evidence":{"startByte":0,"endByte":5,"text":"Crash"}},
        "affectedComponents":[{"value":"café","evidence":{"startByte":9,"endByte":14,"text":"café"}}],
        "statedCause":null,
        "requiredRepair":null
    }"#;

    let result = parse_bug_classification(output, raw).expect("valid UTF-8 byte ranges");

    assert_eq!(result.failure_mechanism.as_deref(), Some("Crash"));
    assert_eq!(result.affected_components_json, r#"["café"]"#);
}

#[tokio::test]
async fn bug_classifier_stream_accepts_reasoning_and_rejects_extra_answers_or_tools() {
    let answer = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: serde_json::json!({
                "summary": "A crash was reported.",
                "severity": null,
                "failureMechanism": null,
                "affectedComponents": [],
                "statedCause": null,
                "requiredRepair": null
            })
            .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let reasoning: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "reasoning", "id": "reasoning-1", "summary": []
    }))
    .expect("reasoning output item");
    let completed = || ResponseEvent::Completed {
        response_id: "classification-1".to_string(),
        token_usage: None,
        end_turn: Some(true),
    };
    let shutdown = CancellationToken::new();
    let mut stream = futures::stream::iter([
        Ok(ResponseEvent::OutputItemDone(reasoning.clone())),
        Ok(ResponseEvent::OutputItemDone(answer.clone())),
        Ok(completed()),
    ]);
    let result = consume_bug_classification_stream(&mut stream, &shutdown, "Crash")
        .await
        .expect("one assistant answer is valid even when reasoning is emitted");
    assert_eq!(result.summary, "A crash was reported.");

    let tool: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "function_call", "call_id": "call-1", "name": "exec", "arguments": "{}"
    }))
    .expect("tool output item");
    for extra in [answer.clone(), tool] {
        let mut stream = futures::stream::iter([
            Ok(ResponseEvent::OutputItemDone(answer.clone())),
            Ok(ResponseEvent::OutputItemDone(extra)),
            Ok(completed()),
        ]);
        assert!(matches!(
            consume_bug_classification_stream(&mut stream, &shutdown, "Crash").await,
            Err(BugClassificationFailure::MalformedOutput)
        ));
    }
    for items in [vec![reasoning], vec![answer]] {
        let mut stream = futures::stream::iter(
            items
                .into_iter()
                .map(|item| Ok(ResponseEvent::OutputItemDone(item))),
        );
        assert!(matches!(
            consume_bug_classification_stream(&mut stream, &shutdown, "Crash").await,
            Err(BugClassificationFailure::MalformedOutput)
        ));
    }
}

#[test]
fn bug_classifier_rejects_non_boundary_and_unsupported_facts() {
    let raw = "café";
    let non_boundary = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":{"value":"é","evidence":{"startByte":4,"endByte":5,"text":"é"}},
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;
    let unsupported = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":{"value":"invented","evidence":{"startByte":0,"endByte":3,"text":"caf"}},
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;

    assert!(matches!(
        parse_bug_classification(non_boundary, raw),
        Err(BugClassificationFailure::Grounding)
    ));
    assert!(matches!(
        parse_bug_classification(unsupported, raw),
        Err(BugClassificationFailure::Grounding)
    ));
}

#[test]
fn bug_classifier_requires_exact_schema_and_normalizes_cited_severity() {
    let raw = "HIGH failure";
    let valid = r#"{
        "summary":"A high-severity failure is reported.",
        "severity":{"value":"high","evidence":{"startByte":0,"endByte":4,"text":"HIGH"}},
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null
    }"#;
    let missing_key = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null
    }"#;
    let unknown_key = r#"{
        "summary":"A report.",
        "severity":null,
        "failureMechanism":null,
        "affectedComponents":[],
        "statedCause":null,
        "requiredRepair":null,
        "extra":"not allowed"
    }"#;

    let result = parse_bug_classification(valid, raw).expect("cited severity should normalize");
    assert_eq!(result.severity.as_deref(), Some("high"));
    assert!(matches!(
        parse_bug_classification(missing_key, raw),
        Err(BugClassificationFailure::Schema)
    ));
    assert!(matches!(
        parse_bug_classification(unknown_key, raw),
        Err(BugClassificationFailure::Schema)
    ));
}
