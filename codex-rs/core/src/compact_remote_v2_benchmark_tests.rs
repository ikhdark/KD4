// Production pin-retention regression plus the remaining opt-in storage-preflight benchmark.

fn benchmark_pin_block(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message { content, .. } = item else {
        return None;
    };
    content.iter().find_map(|part| {
        let ContentItem::InputText { text } = part else {
            return None;
        };
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        (value["kind"] == "tool_history_artifact_pins").then_some(text.as_str())
    })
}

#[tokio::test]
async fn repeated_remote_compaction_keeps_one_pin_block() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let mut artifacts = Vec::new();
    for index in 0..24 {
        let canonical = codex_tools::CanonicalToolResult::json(
            serde_json::json!({"proof": format!("exact-proof-{index}")}),
        );
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            &canonical,
        )
        .await;
        assert!(artifact.complete);
        let id = artifact.artifact_id().unwrap();
        session
            .register_tool_artifact_origin(
                id.clone(),
                format!("proof-{index}"),
                canonical.exact_bytes,
                canonical.sha256.clone(),
            )
            .await;
        artifacts.push((id, canonical.bytes.clone()));
    }
    let user_text = format!(
        "Keep USER_CONSTRAINT; original evidence handles: {}",
        serde_json::to_string(&artifacts.iter().map(|(id, _)| id).collect::<Vec<_>>()).unwrap()
    );
    session
        .record_conversation_items(&turn, &[message("user", &user_text, None)])
        .await
        .unwrap();
    let mut first_pins = None;
    for _ in 0..6 {
        let input = session.clone_history().await.raw_items().to_vec();
        let (retained, _) = prepare_v2_retained_input(&session, &input).await.unwrap();
        let opaque = ResponseItem::Compaction {
            id: None,
            encrypted_content: "fixed-checkpoint".to_string(),
            internal_chat_message_metadata_passthrough: None,
        };
        let (replacement, baseline, digests) = process_compacted_history_with_retained_input(
            &session,
            &turn,
            vec![opaque],
            retained,
            &InitialContextInjection::DoNotInject,
        )
        .await;
        session
            .replace_compacted_history(
                &turn,
                replacement.clone(),
                None,
                baseline,
                digests,
                persisted_v2_compacted_item(replacement),
            )
            .await
            .unwrap();
        let installed = session.clone_history().await;
        let pins = installed
            .raw_items()
            .iter()
            .filter_map(benchmark_pin_block)
            .collect::<Vec<_>>();
        assert_eq!(pins.len(), 1);
        assert!(
            installed
                .raw_items()
                .iter()
                .any(crate::compact_remote::is_remote_compaction_artifact_pins)
        );
        assert!(installed.raw_items().iter().any(|item| matches!(item,
            ResponseItem::Message { role, content, .. } if role == "user"
                && content == &vec![ContentItem::InputText { text: user_text.clone() }])));
        for (id, _) in &artifacts {
            assert!(pins[0].contains(id));
        }
        if let Some(first) = &first_pins {
            assert_eq!(pins[0], first);
        } else {
            first_pins = Some(pins[0].to_string());
        }
        assert!(approx_token_count(pins[0]) <= 2_000);

        let persisted = persisted_v2_compacted_item(installed.raw_items().to_vec());
        let replayed: CompactedItem =
            serde_json::from_str(&serde_json::to_string(&persisted).unwrap()).unwrap();
        let (replayed_retained, _) =
            prepare_v2_retained_input(&session, &replayed.replacement_history.unwrap())
                .await
                .unwrap();
        assert!(
            !replayed_retained
                .iter()
                .any(crate::compact_remote::is_remote_compaction_artifact_pins)
        );
    }
    for (id, expected) in artifacts {
        let recovered = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
            &turn.config.codex_home,
            &session.thread_id.to_string(),
            &id,
            1024 * 1024,
        )
        .await
        .unwrap();
        assert_eq!(recovered, expected);
    }
}

fn benchmark_configure_loopback(session: &mut Session, turn: &mut TurnContext, uri: &str) {
    let mut config = (*turn.config).clone();
    config.model_provider.base_url = Some(format!("{uri}/v1"));
    config.model_provider.supports_websockets = false;
    config.model_provider.request_max_retries = Some(0);
    config.model_provider.stream_max_retries = Some(0);
    let config = Arc::new(config);
    turn.provider = create_model_provider(config.model_provider.clone(), turn.auth_manager.clone());
    turn.config = Arc::clone(&config);
    session.services.model_client = ModelClient::new(
        Some(Arc::clone(&session.services.auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        session.thread_id,
        config.model_provider.clone(),
        turn.session_source.clone(),
        turn.originator.clone(),
        config.model_verbosity,
        false,
        false,
        None,
        false,
        None,
        config.http_client_factory(),
    );
}

async fn benchmark_early_remote_checkpoint(
    session: &Arc<Session>,
    step: &Arc<StepContext>,
    metadata: CompactionTurnMetadata,
) -> CodexResult<()> {
    let attempt = run_remote_compact_v2_attempt(
        session,
        step,
        None,
        &CompactionTraceContext::disabled(),
        metadata,
        &mut CompactionAnalyticsDetails::default(),
        &CancellationToken::new(),
    )
    .await?;
    let (replacement, baseline, digests) = process_compacted_history_with_retained_input(
        session,
        &step.turn,
        vec![attempt.compaction_output],
        attempt.retained_input,
        &InitialContextInjection::DoNotInject,
    )
    .await;
    session
        .replace_compacted_history(
            &step.turn,
            replacement.clone(),
            None,
            baseline,
            digests,
            persisted_v2_compacted_item(replacement),
        )
        .await?;
    session.recompute_token_usage(&step.turn).await;
    Ok(())
}

#[tokio::test]
#[ignore = "targeted compaction baseline/candidate benchmark"]
#[expect(clippy::print_stdout, reason = "emits the opt-in benchmark report")]
async fn compaction_benchmark_remote_storage_preflight() {
    for sample in 0..3 {
        for blocked in [false, true] {
            for early in [false, true] {
                let server = responses::start_mock_server().await;
                let body = responses::sse(vec![
                    serde_json::json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"benchmark-checkpoint"}}),
                    responses::ev_completed("benchmark-response"),
                ]);
                wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path("/v1/responses"))
                    .respond_with(
                        wiremock::ResponseTemplate::new(200)
                            .insert_header("content-type", "text/event-stream")
                            .set_body_string(body),
                    )
                    .expect(u64::from(!blocked))
                    .mount(&server)
                    .await;
                let (mut session, mut turn) =
                    crate::session::tests::make_session_and_context().await;
                benchmark_configure_loopback(&mut session, &mut turn, &server.uri());
                let exact = format!(
                    "{}EXACT_CONSTRAINT{}",
                    "constraint ".repeat(6000),
                    " end".repeat(6000)
                );
                session
                    .record_conversation_items(&turn, &[message("user", &exact, None)])
                    .await
                    .unwrap();
                let original = session.clone_history().await.raw_items().to_vec();
                if blocked {
                    std::fs::create_dir_all(&turn.config.codex_home).unwrap();
                    std::fs::write(turn.config.codex_home.join("tool-output"), "blocked").unwrap();
                }
                let session = Arc::new(session);
                let turn = Arc::new(turn);
                let step = session
                    .capture_step_context(Arc::clone(&turn))
                    .await
                    .unwrap();
                let window = session.current_window_id().await;
                let metadata = CompactionTurnMetadata::new(
                    CompactionTrigger::Manual,
                    CompactionReason::UserRequested,
                    CompactionImplementation::ResponsesCompactionV2,
                    CompactionPhase::StandaloneTurn,
                );
                let started = std::time::Instant::now();
                let result = if early {
                    benchmark_early_remote_checkpoint(&session, &step, metadata).await
                } else {
                    run_remote_compact_task_inner_impl(
                        &session,
                        &step,
                        None,
                        None,
                        InitialContextInjection::DoNotInject,
                        metadata,
                        &mut CompactionAnalyticsDetails::default(),
                        &CancellationToken::new(),
                    )
                    .await
                };
                let elapsed_us = started.elapsed().as_micros();
                let request_count = server.received_requests().await.unwrap().len();
                assert_eq!(request_count, usize::from(!blocked));
                let installed = session.clone_history().await.raw_items().to_vec();
                if blocked {
                    assert!(
                        matches!(result, Err(CodexErr::Fatal(ref text)) if text.contains("could not preserve exact unresolved text"))
                    );
                    assert_eq!(installed, original);
                    assert_eq!(session.current_window_id().await, window);
                } else {
                    result.unwrap();
                    assert_ne!(session.current_window_id().await, window);
                    assert_eq!(installed.iter().filter(|item| matches!(item, ResponseItem::Compaction { encrypted_content, .. } if encrypted_content == "benchmark-checkpoint")).count(), 1);
                    let receipt = installed
                        .iter()
                        .find_map(|item| {
                            let ResponseItem::Message { content, .. } = item else {
                                return None;
                            };
                            content.iter().find_map(|part| {
                                let ContentItem::InputText { text } = part else {
                                    return None;
                                };
                                let value: serde_json::Value = serde_json::from_str(text).ok()?;
                                (value["kind"] == "local_compaction_text_recovery").then_some(value)
                            })
                        })
                        .expect("installed recovery locator");
                    let bytes =
                        crate::tools::command_output_artifact::read_complete_canonical_snapshot(
                            &turn.config.codex_home,
                            &session.thread_id.to_string(),
                            receipt["artifact_id"].as_str().unwrap(),
                            1024 * 1024,
                        )
                        .await
                        .unwrap();
                    let recovered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(recovered["items"][0]["content"][0]["text"], exact);
                }
                println!(
                    "COMPACTION_BENCH {}",
                    serde_json::json!({
                        "case": "remote_storage_preflight", "sample": sample, "blocked": blocked,
                        "early": early, "model_requests": request_count, "elapsed_us": elapsed_us,
                        "history_preserved_on_failure": blocked, "exact_recovery_on_success": !blocked
                    })
                );
            }
        }
    }
}
