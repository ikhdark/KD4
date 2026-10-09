//! Complete native turns against a scripted provider, not live-model speed claims.
use super::*;
use pretty_assertions::assert_eq;

#[test]
#[expect(clippy::print_stderr, reason = "this scripted latency audit emits machine-readable measurements")]
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
#[expect(clippy::print_stderr, reason = "this scripted latency audit emits machine-readable measurements")]
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

#[test]
#[expect(clippy::print_stderr, reason = "this cancellation audit emits machine-readable latency measurements")]
fn latency_audit_cancel_stalled_prompt_capture() -> Result<()> {
    run_turn_multi_thread_test_with_stack("latency_audit_cancel_stalled_prompt_capture", || async {
        core_test_support::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let provider = non_openai_model_provider(&server);
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test key"), Vec::new(), home.path(),
                move |config| config.model_provider = provider,
            ).await;
        let evidence = ResponseItem::FunctionCallOutput {
            id: None, call_id: "workspace-read".into(),
            output: FunctionCallOutputPayload::from_text("completed source evidence".into()),
            internal_chat_message_metadata_passthrough: None,
        };
        session.record_conversation_items(&turn, &[
            ResponseItem::FunctionCall {
                id: None, name: "read_file".into(), namespace: None,
                arguments: r#"{"path":"source.rs"}"#.into(), call_id: "workspace-read".into(),
                internal_chat_message_metadata_passthrough: None,
            }, evidence,
        ]).await?;
        let stored_evidence = session.clone_history().await.raw_items().iter()
            .find(|item| matches!(item, ResponseItem::FunctionCallOutput { call_id, .. }
                if call_id == "workspace-read"))
            .expect("recorded source evidence").clone();
        let ResponseItem::FunctionCallOutput { output, .. } = &stored_evidence else {
            unreachable!("selected function output");
        };
        assert_eq!(output.text_content(), Some("completed source evidence"));
        let pause = session.services.git_workspace.pause_next_workspace_evidence_capture();
        let cancellation = CancellationToken::new();
        let mut running = tokio::spawn({
            let session = Arc::clone(&session);
            let turn = Arc::clone(&turn);
            let cancellation = cancellation.clone();
            async move {
                let mut budget = LogicalGenerationBudget::default();
                run_turn(session, Arc::clone(&turn), Arc::new(ExtensionData::new(turn.sub_id.clone())),
                    Vec::new(), None, &mut budget, cancellation).await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started()).await?;
        // Drive planning into prompt preparation while the real capture is held.
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut running).await.is_err());
        let started = std::time::Instant::now();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_millis(500), &mut running).await;
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        pause.release(); // Also release the fixture if the regression returns late.
        if result.is_err() { running.abort(); let _ = running.await; }
        assert!(matches!(result, Ok(Ok(Err(CodexErr::TurnAborted)))), "{result:?}");
        assert!(session.clone_history().await.raw_items().contains(&stored_evidence));
        assert!(server.received_requests().await.unwrap().is_empty());
        eprintln!("LATENCY_AUDIT {}", serde_json::json!({
            "scenario":"cancel-stalled-prompt", "wall_ms":wall_ms,
            "model_requests":0, "completed_evidence_preserved":true,
        }));
        Ok(())
    })
}

#[test]
#[expect(clippy::print_stderr, reason = "this retention audit emits machine-readable failure and recovery timings")]
fn latency_audit_remote_retention_preflight_and_recovery() -> Result<()> {
    run_turn_multi_thread_test_with_stack("latency_audit_remote_retention_preflight", || async {
        core_test_support::require_network!();
        for directory_failure in [false, true] {
            let server = responses::start_mock_server().await;
            let home = tempfile::tempdir()?;
            let provider = non_openai_model_provider(&server);
            let (session, turn, _events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"), Vec::new(), home.path(),
                    move |config| config.model_provider = provider,
                ).await;
            let mut input = Vec::new();
            if directory_failure {
                for index in 0..40 {
                    let call_id = format!("retained-{index}");
                    let text = format!("completed evidence {index}");
                    let canonical = codex_tools::CanonicalToolResult::text(text.clone());
                    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
                        home.path(), &session.thread_id().to_string(), &canonical,
                    ).await;
                    assert!(artifact.complete, "{artifact:?}");
                    let candidate = crate::tool_history::ToolHistoryCandidate {
                        call_id: call_id.clone(), tool_identity: "functions.exec".into(),
                        semantic_class: "tool_output".into(), successful: true,
                        source_dependencies: Default::default(), source_dependencies_current: true,
                        artifact_id: artifact.artifact_id().unwrap(), artifact_bytes: canonical.exact_bytes,
                        artifact_sha256: canonical.sha256.clone(), original_output_sha256: canonical.sha256,
                        original_tokens: 24_000, preserved_non_text_tokens: Some(0),
                        bounded_model_output: text.clone(), complete: true, projection_eligible: true,
                        proof_identity: None, supersession_identity: None, consumed_by_generation: None,
                        derived: Default::default(),
                    };
                    session.register_tool_history_candidate(candidate).await;
                    input.push(ResponseItem::FunctionCall {
                        id: None, name: "functions.exec".into(), namespace: None,
                        arguments: "{}".into(), call_id: call_id.clone(),
                        internal_chat_message_metadata_passthrough: None,
                    });
                    input.push(ResponseItem::FunctionCallOutput {
                        id: None, call_id, output: FunctionCallOutputPayload::from_text(text),
                        internal_chat_message_metadata_passthrough: None,
                    });
                }
            } else {
                input.push(ResponseItem::Message {
                    id: None, role: "user".into(), phase: None,
                    content: vec![ContentItem::InputText { text: "exact constraint ".repeat(32_000) }],
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            session.record_conversation_items(&turn, &input).await?;
            let original = session.clone_history().await;
            let window = session.current_window_id().await;
            let blocked = home.path().join("tool-output").join(session.thread_id().to_string());
            let backup = blocked.with_extension("retained-fixture");
            assert!(blocked.starts_with(home.path()) && backup.starts_with(home.path()));
            assert!(!backup.exists());
            let had_artifacts = blocked.exists();
            if had_artifacts { std::fs::rename(&blocked, &backup)?; }
            std::fs::create_dir_all(blocked.parent().unwrap())?;
            std::fs::write(&blocked, "blocked retention")?;
            let body = responses::sse(vec![serde_json::json!({
                "type":"response.output_item.done",
                "item":{"type":"compaction", "encrypted_content":"checkpoint"},
            }), responses::ev_completed("compacted")]);
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream").set_body_string(body)
                    .set_delay(Duration::from_millis(250)))
                .mount(&server).await;
            let started = std::time::Instant::now();
            let result = crate::compact_remote_v2::run_remote_compact_task(
                Arc::clone(&session), Arc::clone(&turn), &CancellationToken::new(),
            ).await;
            let failure_ms = started.elapsed().as_secs_f64() * 1000.0;
            let error = result.expect_err("retention failure cannot install a checkpoint").to_string();
            assert!(error.contains(if directory_failure { "artifact_directory" } else { "exact unresolved text" }), "{error}");
            assert!(server.received_requests().await.unwrap().is_empty(), "known retention failure must precede provider dispatch");
            assert_eq!(session.current_window_id().await, window);
            assert_eq!(session.clone_history().await.raw_items(), original.raw_items());
            // Remove only the fixture's exact file; retry from the retained history.
            assert_eq!(std::fs::read_to_string(&blocked)?, "blocked retention");
            std::fs::remove_file(&blocked)?;
            if had_artifacts { std::fs::rename(&backup, &blocked)?; }
            let started = std::time::Instant::now();
            crate::compact_remote_v2::run_remote_compact_task(
                Arc::clone(&session), Arc::clone(&turn), &CancellationToken::new(),
            ).await?;
            let recovery_ms = started.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert_ne!(session.current_window_id().await, window);
            assert!(session.clone_history().await.raw_items().iter().any(|item|
                matches!(item, ResponseItem::Compaction { encrypted_content, .. } if encrypted_content == "checkpoint")));
            eprintln!("LATENCY_AUDIT {}", serde_json::json!({
                "scenario":"remote-retention-preflight", "directory_failure":directory_failure,
                "failure_wall_ms":failure_ms, "failure_model_requests":0,
                "recovery_wall_ms":recovery_ms, "recovery_model_requests":1,
                "provider_delay_ms":250, "original_history_preserved":true,
            }));
        }
        Ok(())
    })
}
