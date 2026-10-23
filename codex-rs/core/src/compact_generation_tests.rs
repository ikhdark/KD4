use super::*;

#[test]
fn structured_checkpoint_preserves_sections_and_rejects_empty_or_extra_bodies() {
    let object = serde_json::json!({
        "Goal": "Preserve the user's no-validation constraint.",
        "Current state": "Implementation remains unverified.",
        "Completed work": "Source edits only.",
        "Unresolved work": "Finish the remaining fixes.",
        "Evidence": "Exact original source is retained.",
        "Next action": "Continue authorized source work without running checks.",
    });
    let expected = "## Goal\nPreserve the user's no-validation constraint.\n\n## Current state\nImplementation remains unverified.\n\n## Completed work\nSource edits only.\n\n## Unresolved work\nFinish the remaining fixes.\n\n## Evidence\nExact original source is retained.\n\n## Next action\nContinue authorized source work without running checks.";
    let rendered = render_structured_compaction_summary(&object.to_string()).unwrap();
    assert_eq!(rendered, expected);
    assert!(validated_compaction_summary(None, &rendered, true, false).is_ok());
    assert!(generated_summary_recovery_canonical(None, &object.to_string(), &rendered).is_some());
    let mut empty = object.clone();
    empty["Unresolved work"] = serde_json::json!("  ");
    assert!(render_structured_compaction_summary(&empty.to_string()).is_err());
    let mut extra = object;
    extra["Unknown"] = serde_json::json!("must not be silently dropped");
    assert!(render_structured_compaction_summary(&extra.to_string()).is_err());
    assert_eq!(render_structured_compaction_summary(expected).unwrap(), expected);
    let schema = local_compaction_output_schema(true, false, false).unwrap();
    assert_eq!(schema["required"].as_array().unwrap().len(), 6);
    assert_eq!(schema["additionalProperties"], false);
    for (openai, custom, rebase) in [(false, false, false), (true, true, false), (true, false, true)] {
        assert!(local_compaction_output_schema(openai, custom, rebase).is_none());
    }
}

#[tokio::test]
async fn completed_summary_repair_starts_a_generation_without_resetting_retry_budget() {
    use crate::test_harness::responses;
    use codex_protocol::protocol::TurnTimingAttemptKind;
    use codex_protocol::protocol::TurnTimingGenerationPurpose;

    crate::test_harness::require_network!();
    // 0 = valid completed checkpoint, 1 = invalid completed checkpoint,
    // 2 = retryable stream failure, 3 = valid structured checkpoint.
    // Expected counts follow semantic completion,
    // independently of the implementation's timing transitions.
    for (name, outcomes, succeeds, generations, retries) in [
        ("valid", vec![0], true, 1, 0),
        ("structured", vec![3], true, 1, 0),
        ("transport", vec![2, 0], true, 1, 1),
        ("repair", vec![1, 0], true, 2, 0),
        ("mixed", vec![2, 1, 0], true, 2, 1),
        ("invalid", vec![1, 1], false, 2, 0),
        ("budget", vec![2, 1, 2], false, 2, 1),
    ] {
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir().unwrap();
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                    config.model_provider.stream_max_retries = Some(1);
                    config.model_reasoning_summary = Some(codex_protocol::config_types::ReasoningSummary::Detailed);
                    config.compact_prompt = None;
                },
            )
            .await;
        let user = ResponseItem::from(ResponseInputItem::from(vec![UserInput::Text {
            text: "preserve this task".into(),
            text_elements: Vec::new(),
        }]));
        session
            .record_conversation_items(&turn, &[user])
            .await
            .unwrap();
        let before = session.clone_history().await.raw_items().to_vec();
        let valid = "## Goal\nPreserve this task\n\n## Current state\nInvestigation pending\n\n## Completed work\nNo implementation yet\n\n## Unresolved work\nFinish the task\n\n## Evidence\nUser request retained\n\n## Next action\nContinue investigation";
        let bodies = outcomes
            .iter()
            .enumerate()
            .map(|(index, outcome)| {
                let id = format!("{name}-{index}");
                if *outcome == 2 {
                    responses::sse_failed(&id, "server_error", "temporary failure")
                } else {
                    let structured = serde_json::json!({
                        "Goal": "Preserve this task", "Current state": "Investigation pending",
                        "Completed work": "No implementation yet", "Unresolved work": "Finish the task",
                        "Evidence": "User request retained", "Next action": "Continue investigation",
                    }).to_string();
                    responses::sse(vec![
                        responses::ev_assistant_message(
                            &id,
                            if *outcome == 0 { valid } else if *outcome == 3 { &structured } else { "Done." },
                        ),
                        responses::ev_completed_with_tokens(&id, 100),
                    ])
                }
            })
            .collect();
        let log = responses::mount_sse_sequence(&server, bodies).await;
        turn.turn_timing_state.mark_turn_started();
        let result = run_compact_task_inner_impl(
            Arc::clone(&session),
            Arc::clone(&turn),
            None,
            Some(&None),
            Vec::new(),
            InitialContextInjection::DoNotInject,
            CompactionTurnMetadata::new(
                CompactionTrigger::Manual,
                CompactionReason::UserRequested,
                CompactionImplementation::Responses,
                CompactionPhase::StandaloneTurn,
            ),
            &mut CompactionAnalyticsDetails::default(),
            &mut TurnItem::ContextCompaction(ContextCompactionItem::new()),
            false,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.is_ok(), succeeds, "{name}: {result:?}");
        if name == "structured" {
            assert_eq!(result.as_ref().unwrap(), valid);
        }
        let requests = log.requests();
        assert_eq!(turn.reasoning_summary, codex_protocol::config_types::ReasoningSummary::Detailed);
        for request in &requests {
            let body = request.body_json();
            assert!(body["reasoning"].get("summary").is_none(), "internal compaction must not request an unused reasoning summary");
            assert_eq!(body["reasoning"]["effort"], serde_json::json!(turn.request_reasoning_effort()));
            assert_eq!(body["model"], turn.model_info.slug);
            assert_eq!(body["text"]["format"]["type"], "json_schema");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(body["text"]["format"]["schema"]["required"].as_array().unwrap().len(), 6);
            // Every attempt, repairs included, keeps the turn's cached prompt
            // prefix: its base instructions and tool schemas, with calls
            // disabled. The summarizer contract travels in the compaction input.
            // Responses Lite carries both prefixes as leading input items.
            assert_eq!(body["tool_choice"], "none", "{name}: compaction must not call tools");
            let base = &turn.base_instructions.text;
            assert!(
                body["instructions"].as_str() == Some(base.as_str())
                    || request.message_input_texts("developer").contains(base),
                "{name}: compaction must send the turn's base instructions"
            );
            let advertised =
                |tools: &serde_json::Value| tools.as_array().is_some_and(|tools| !tools.is_empty());
            assert!(
                advertised(&body["tools"])
                    || body["input"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["type"] == "additional_tools" && advertised(&item["tools"])
                        })
                    }),
                "{name}: compaction must advertise the turn's tool schemas"
            );
            assert!(
                request
                    .message_input_texts("user")
                    .iter()
                    .any(|text| text == COMPACTION_BASE_INSTRUCTIONS.trim()),
                "{name}: the summarizer contract belongs in the compaction input"
            );
        }
        assert_eq!(
            requests.len(),
            outcomes.len(),
            "{name}: bounded request sequence"
        );
        let timing = turn.turn_timing_state.complete_snapshot().protocol_timing();
        assert_eq!(
            timing.counters.logical_generation_count, generations,
            "{name}"
        );
        assert_eq!(
            timing.counters.model_request_count as usize,
            outcomes.len(),
            "{name}"
        );
        assert_eq!(timing.counters.model_retry_count, retries, "{name}");
        let mut expected_generation = 0;
        for (index, row) in timing.model_requests.iter().enumerate() {
            let repair = index > 0 && outcomes[index - 1] == 1;
            if repair {
                expected_generation += 1;
            }
            assert_eq!(row.generation_index, expected_generation, "{name}/{index}");
            assert_eq!(
                row.attempt_kind,
                if index == 0 || repair {
                    TurnTimingAttemptKind::Primary
                } else {
                    TurnTimingAttemptKind::Retry
                },
                "{name}/{index}"
            );
            assert_eq!(
                row.generation_purpose,
                Some(if expected_generation == 0 {
                    TurnTimingGenerationPurpose::CompactionRecovery
                } else {
                    TurnTimingGenerationPurpose::Repair
                }),
                "{name}/{index}"
            );
            let corrective = requests[index]
                .body_json()
                .to_string()
                .contains("The previous compaction handoff was rejected");
            assert_eq!(
                corrective,
                expected_generation > 0,
                "{name}/{index}: request semantics"
            );
        }
        let completed = outcomes.iter().filter(|outcome| **outcome != 2).count() as i64;
        assert_eq!(
            session.total_token_usage().await.unwrap().total_tokens,
            completed * 100,
            "{name}"
        );
        if !succeeds {
            assert_eq!(
                session.clone_history().await.raw_items(),
                before,
                "{name}: original history"
            );
        }
    }
}
