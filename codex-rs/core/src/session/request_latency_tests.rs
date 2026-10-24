//! Real turn -> loopback provider -> native tool -> continuation wall-clock probe.
//! Provider computation, Desktop rendering, and session startup are excluded.
use super::*;
use pretty_assertions::assert_eq;







#[test]
#[expect(
    clippy::print_stderr,
    reason = "this retained-history probe emits machine-readable timings and work counts"
)]
fn request_preparation_large_failure_history_wall_clock() -> Result<()> {
    run_turn_multi_thread_test_with_stack("request_preparation_large_failure_history", || async {
        crate::test_harness::require_network!();
        let payload = "retained diagnostic: α → β\n".repeat(1024);
        for sample in 0..3 {
            let server = responses::start_mock_server().await;
            let requests = responses::mount_sse_sequence(&server, vec![
                responses::sse(vec![
                    responses::ev_function_call("plan", "update_plan",
                        r#"{"plan":[{"step":"Return the fixture answer","status":"completed"}]}"#),
                    responses::ev_completed("tool-response"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "Verified retained failures."),
                    responses::ev_completed("final-response"),
                ]),
            ]).await;
            let home = tempfile::tempdir()?;
            let workspace = tempfile::tempdir()?;
            let cwd =
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
            let provider = non_openai_model_provider(&server);
            let (session, mut turn, events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"),
                    Vec::new(),
                    home.path(),
                    move |config| {
                        config.cwd = cwd;
                        config.model_provider = provider;
                        config.features.disable(Feature::CodeModeHost).unwrap();
                        config.features.disable(Feature::CodeMode).unwrap();
                        config.features.disable(Feature::Kd4Runtime).unwrap();
                        config.model_auto_compact_token_limit = Some(i64::MAX);
                    },
                )
                .await;
            // This probe measures retention and request preparation, not compaction.
            // The configured auto-compact limit is still capped at 90% of the
            // model window, so give the synthetic provider room for every byte.
            let model_info = &mut Arc::get_mut(&mut turn).unwrap().model_info;
            model_info.context_window = Some(2_000_000);
            model_info.max_context_window = Some(2_000_000);
            let mut history = Vec::new();
            for index in 0..32 {
                let call_id = format!("historical-failure-{index}");
                history.push(ResponseItem::FunctionCall {
                    id: None,
                    name: "read_file".into(),
                    namespace: None,
                    arguments: r#"{"path":"source.txt"}"#.into(),
                    call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                });
                let mut output = FunctionCallOutputPayload::from_text(payload.clone());
                output.success = Some(false);
                history.push(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            history.push(ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText {
                    text: "Return the fixture answer.".into(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            });
            session.record_conversation_items(&turn, &history).await?;
            // Pending-input recording belongs to an admitted task for this turn.
            session
                .spawn_task(
                    Arc::clone(&turn),
                    Vec::new(),
                    SignalCompletingTask {
                        finish: CancellationToken::new(),
                    },
                )
                .await;
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                run_turn(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    Arc::new(ExtensionData::new(turn.sub_id.clone())),
                    Vec::new(),
                    None,
                    &mut LogicalGenerationBudget::default(),
                    CancellationToken::new(),
                ),
            )
            .await??;
            let run_turn_ms = started.elapsed().as_secs_f64() * 1000.0;
            let sent = requests.requests();
            let mut errors = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let EventMsg::Error(error) = event.msg {
                    errors.push(error.message);
                }
            }
            assert_eq!(
                result.last_agent_message.as_deref(),
                Some("Verified retained failures."),
                "requests={}, errors={errors:?}, required_tool_terminal={:?}",
                sent.len(),
                result
                    .required_tool_terminal
                    .as_ref()
                    .map(|terminal| &terminal.message)
            );
            assert_eq!(sent.len(), 2);
            for request in &sent {
                let failures = request
                    .input()
                    .into_iter()
                    .filter(|item| {
                        item["type"] == "function_call_output"
                            && item["call_id"]
                                .as_str()
                                .is_some_and(|id| id.starts_with("historical-failure-"))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(failures.len(), 32);
                assert!(
                    failures
                        .iter()
                        .all(|item| item["output"].as_str() == Some(payload.as_str())),
                    "sampling must retain every historical failure byte in its original tool message"
                );
            }
            assert!(sent[1].input().iter().any(|item| {
                item["call_id"] == "plan"
                    && item["output"]
                        .as_str()
                        .is_some_and(|text| text.contains("Plan updated"))
            }));
            session.flush_tool_history_persistence().await?;
            let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
            eprintln!(
                "REQUEST_FAILURE_HISTORY {}",
                serde_json::json!({
                    "sample": sample, "wall_ms": wall_ms, "run_turn_ms": run_turn_ms, "history_bytes": payload.len() * 32,
                    "model_requests": sent.len(), "tool_calls": 1, "exact_failures": true,
                    "provider": "loopback-scripted", "required_persistence_included": true, "startup_included": false,
                    "scope": "run_turn through tool continuation, final answer and required tool-history persistence; excludes outer publication/delivery",
                })
            );
        }
        Ok(())
    })
}
