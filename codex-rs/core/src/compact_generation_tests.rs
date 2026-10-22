use super::*;

#[tokio::test]
async fn completed_summary_repair_starts_a_generation_without_resetting_retry_budget() {
    use crate::test_harness::responses;
    use codex_protocol::protocol::TurnTimingAttemptKind;
    use codex_protocol::protocol::TurnTimingGenerationPurpose;

    crate::test_harness::require_network!();
    // 0 = valid completed checkpoint, 1 = invalid completed checkpoint,
    // 2 = retryable stream failure. Expected counts follow semantic completion,
    // independently of the implementation's timing transitions.
    for (name, outcomes, succeeds, generations, retries) in [
        ("valid", vec![0], true, 1, 0),
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
                    responses::sse(vec![
                        responses::ev_assistant_message(
                            &id,
                            if *outcome == 0 { valid } else { "Done." },
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
        let requests = log.requests();
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
