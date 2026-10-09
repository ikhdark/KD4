//! Complete native turns against a scripted provider, not live-model speed claims.
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn completion_audit_reset_preserves_ready_delivery() -> Result<()> {
    run_turn_multi_thread_test_with_stack("completion_audit_reset_preserves_ready_delivery", || async {
        core_test_support::require_network!();
        for recovery_tools in [true, false] {
            for candidate in [false, true] {
                let server = responses::start_mock_server().await;
                let answer = "Verified answer: α → β.";
                let prefix = if candidate { "// @exec: {\"deliver\":true}\n" } else { "" };
                let code = format!("{prefix}await tools.new_context({{}}); text({});", serde_json::to_string(answer)?);
                let mut sequence = vec![responses::sse(vec![
                    responses::ev_custom_tool_call("ready", "exec", &code),
                    responses::ev_completed("ready"),
                ])];
                // The reset tool is registered only with recovery tools. An
                // unsupported reset must refuse delivery and reach the fallback.
                let delivered = candidate && recovery_tools;
                if !delivered {
                    sequence.push(responses::sse(vec![
                        responses::ev_assistant_message("fallback", answer),
                        responses::ev_completed("fallback"),
                    ]));
                }
                let requests = responses::mount_sse_sequence(&server, sequence).await;
                let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
                if recovery_tools {
                    extensions.tool_contributor(Arc::new(crate::session::tests::TokenBudgetRecoveryTools));
                }
                let provider = non_openai_model_provider(&server);
                let test = test_codex()
                    .with_extensions(Arc::new(extensions.build()))
                    .with_config(move |config| {
                        config.model_provider = provider;
                        config.features.enable(Feature::TokenBudget).unwrap();
                        config.features.enable(Feature::CodeMode).unwrap();
                        config.features.enable(Feature::Kd4Runtime).unwrap();
                        config.features.disable(Feature::CodeModeHost).unwrap();
                        config.model_auto_compact_token_limit = Some(i64::MAX);
                    }).build(&server).await?;
                let start = std::time::Instant::now();
                let completed = test.submit_turn_and_capture_completion("Return the verified answer and reset the context.").await?;
                let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
                let count = requests.requests().len();
                assert!(completed.error.is_none(), "{completed:?}");
                assert_eq!(completed.last_agent_message.as_deref(), Some(answer));
                eprintln!("COMPLETION_LATENCY {}", serde_json::json!({
                    "scenario":"reset-ready-delivery", "recovery_tools":recovery_tools,
                    "candidate":candidate, "wall_ms":wall_ms, "model_requests":count,
                    "exact_answer":true, "provider":"scripted",
                }));
                assert_eq!(count, 1 + usize::from(!delivered));
                assert_eq!(completed.surfaced_result.is_some(), delivered);
                test.codex.flush_rollout().await?;
                let (items, _, errors) = crate::RolloutRecorder::load_rollout_items(
                    &test.codex.rollout_path().expect("rollout path"),
                ).await?;
                assert_eq!(errors, 0);
                assert_eq!(items.iter().any(|item| matches!(item,
                    codex_protocol::protocol::RolloutItem::Compacted(_))), recovery_tools,
                    "registered resets must commit; rejected resets must not discard history");
                assert!(items.iter().any(|item| matches!(item,
                    codex_protocol::protocol::RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. })
                        if role == "assistant" && content.iter().any(|part| matches!(part,
                            ContentItem::OutputText { text } if text == answer)))), "answer must survive reopening");
            }
        }
        Ok(())
    })
}

#[test]
fn completion_audit_emergency_report_compacts_only_without_headroom() -> Result<()> {
    run_turn_multi_thread_test_with_stack("completion_audit_emergency_report_headroom", || async {
        core_test_support::require_network!();
        for (usage, must_compact) in [(100_000, false), (199_999, true)] {
            let server = responses::start_mock_server().await;
            let limit = MAX_REGULAR_LOGICAL_GENERATIONS as usize;
            let mut sequence = (0..limit).map(|index| {
                let mut done = responses::ev_completed_with_tokens(&format!("regular-{index}"),
                    if index + 1 == limit { usage } else { 100 });
                done["response"]["end_turn"] = serde_json::json!(false);
                // Usage must follow a model item; an empty completion leaves
                // the user boundary newer than the last sampled history item.
                responses::sse(vec![
                    responses::ev_assistant_message(&format!("progress-{index}"), "Still working."),
                    done,
                ])
            }).collect::<Vec<_>>();
            if must_compact {
                sequence.push(responses::sse(vec![
                    responses::ev_assistant_message("summary", &complete_compaction_summary("report unfinished work")),
                    responses::ev_completed_with_tokens("summary", 100),
                ]));
            }
            sequence.push(responses::sse(vec![
                responses::ev_assistant_message("answer", "Work is suspended; implementation remains unfinished."),
                responses::ev_completed_with_tokens("answer", 100),
            ]));
            let requests = responses::mount_sse_sequence(&server, sequence).await;
            let provider = non_openai_model_provider(&server);
            let test = test_codex().with_config(move |config| {
                config.model_provider = provider;
                config.features.disable(Feature::Kd4Runtime).unwrap();
                config.features.disable(Feature::CodeModeHost).unwrap();
                config.model_context_window = Some(200_000);
                config.model_auto_compact_token_limit = Some(90_000);
            }).build(&server).await?;
            let start = std::time::Instant::now();
            let completed = test.submit_turn_and_capture_completion("Continue the unfinished task.").await?;
            let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
            let sent = requests.requests();
            eprintln!("COMPLETION_LATENCY {}", serde_json::json!({
                "scenario":"emergency-report", "must_compact":must_compact,
                "wall_ms":wall_ms, "model_requests":sent.len(), "provider":"scripted",
            }));
            assert_eq!(sent.len(), limit + 1 + usize::from(must_compact));
            assert_eq!(completed.last_agent_message.as_deref(), Some("Work is suspended; implementation remains unfinished."));
            assert!(completed.error.is_some(), "suspension must not be reported as task success");
            assert_eq!(sent.last().unwrap().body_json()["tool_choice"], "none");
            assert_eq!(sent[limit].body_contains_text(codex_prompts::SUMMARIZATION_PROMPT), must_compact);
        }
        Ok(())
    })
}

#[test]
fn completion_audit_final_answer_reserve_boundary() {
    assert!(defer_compaction_for_final_answer(true, 100_000, Some(108_192)));
    assert!(!defer_compaction_for_final_answer(true, 100_001, Some(108_192)));
    assert!(!defer_compaction_for_final_answer(false, 100_000, Some(200_000)));
    assert!(!defer_compaction_for_final_answer(true, 100_000, None));
}
