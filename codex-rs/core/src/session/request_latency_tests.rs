//! Real turn -> loopback provider -> native tool -> continuation wall-clock probe.
//! Provider computation, Desktop rendering, and session startup are excluded.
use super::*;
use pretty_assertions::assert_eq;

#[test]
#[expect(clippy::print_stderr, reason = "this lifecycle probe emits machine-readable timings and work counts")]
fn completed_poll_lifecycle_wall_clock() -> Result<()> {
    run_turn_multi_thread_test_with_stack("completed_poll_lifecycle", || async {
        core_test_support::require_network!();
        for sample in 0..3 {
            let server = responses::start_mock_server().await;
            let home = tempfile::tempdir()?;
            let workspace = tempfile::tempdir()?;
            let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
            let provider = non_openai_model_provider(&server);
            let (session, turn, _events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"), Vec::new(), home.path(),
                    move |config| {
                        config.cwd = cwd;
                        config.model_provider = provider;
                        config.features.enable(Feature::UnifiedExec).unwrap();
                        config.features.disable(Feature::CodeModeHost).unwrap();
                        config.features.disable(Feature::CodeMode).unwrap();
                        config.features.disable(Feature::Kd4Runtime).unwrap();
                        config.model_auto_compact_token_limit = Some(i64::MAX);
                    },
                ).await;
            let ledger = &session.services.command_execution;
            ledger.track_running_process(
                7,
                crate::tools::command_execution::CommandAttemptKey::new(
                    "exec_command", codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    turn.config.cwd.to_string_lossy().into_owned(), &["completed-fixture".into()],
                ),
                crate::tools::command_output_artifact::RawOutputArtifact::unavailable("fixture output retention error"),
            ).await.unwrap();
            let incarnation = ledger.running_process(7).await.unwrap().incarnation;
            assert!(ledger.mark_running_process_completed(7, 0).await.accepted());
            let expected = ledger.completed_process_result(7, incarnation).await.unwrap();
            let arguments = serde_json::json!({"session_id":7,"incarnation":incarnation,"chars":""}).to_string();
            let requests = responses::mount_sse_sequence(&server, vec![
                responses::sse(vec![
                    responses::ev_function_call("poll", "write_stdin", &arguments),
                    responses::ev_completed("tool-response"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "Verified completed process."),
                    responses::ev_completed("final-response"),
                ]),
            ]).await;
            session.record_conversation_items(&turn, &[ResponseItem::Message {
                id: None, role: "user".into(),
                content: vec![ContentItem::InputText { text: "Poll the completed process and report.".into() }],
                phase: None, internal_chat_message_metadata_passthrough: None,
            }]).await?;
            // Reproduce the observed multi-second storage delay, not provider
            // latency. The timer starts only if a capture is actually attempted.
            let pause = session.services.git_workspace.pause_next_workspace_evidence_capture();
            let slow_storage = AbortOnDropHandle::new(tokio::spawn(async move {
                pause.wait_until_started().await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                pause.release();
            }));
            let captures_before = session.services.git_workspace.workspace_evidence_capture_count();
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(30), run_turn(
                Arc::clone(&session), Arc::clone(&turn),
                Arc::new(ExtensionData::new(turn.sub_id.clone())), Vec::new(), None,
                &mut LogicalGenerationBudget::default(), CancellationToken::new(),
            )).await??;
            let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
            drop(slow_storage);
            assert_eq!(result.last_agent_message.as_deref(), Some("Verified completed process."));
            assert!(result.required_tool_terminal.is_none());
            let sent = requests.requests();
            assert_eq!(sent.len(), 2, "exactly one tool continuation, no retry");
            let history = session.clone_history().await;
            let output = history.raw_items().iter().find_map(|item| match item {
                ResponseItem::FunctionCallOutput { call_id, output, .. } if call_id == "poll" => output.text_content(),
                _ => None,
            }).expect("canonical completed-process receipt");
            assert_eq!(serde_json::from_str::<serde_json::Value>(output)?, expected);
            let continuation_input = sent[1].input();
            let continuation = continuation_input.iter().find(|item|
                item["type"] == "function_call_output" && item["call_id"] == "poll"
            ).expect("poll receipt must reach continuation");
            assert_eq!(continuation["output"].as_str(), Some(output));
            assert_eq!(serde_json::from_str::<serde_json::Value>(
                continuation["output"].as_str().unwrap())?, expected);
            // Sampling keeps authenticated bytes in their original tool message;
            // workspace applicability belongs to a separate developer sidecar.
            let notices = continuation_input.iter()
                .filter(|item| item["role"] == "developer")
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter_map(|part| part["text"].as_str())
                .filter(|text| text.starts_with("<workspace_evidence_invalidation>\n"))
                .flat_map(str::lines)
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .flat_map(crate::tool_history::expand_workspace_notices)
                .filter(|notice| notice["call_id"] == "poll")
                .collect::<Vec<_>>();
            assert_eq!(notices.len(), 1, "exactly one applicability notice for the polled receipt");
            let notice = &notices[0];
            assert_eq!(notice["historical_authenticity"], "authenticated");
            assert_eq!(notice["valid_for_current_workspace"], false);
            session.flush_tool_history_persistence().await?;
            let timing = turn.turn_timing_state.complete_snapshot().protocol_timing();
            let captures = session.services.git_workspace.workspace_evidence_capture_count() - captures_before;
            assert_eq!(captures, 0, "unscoped poll evidence must not move a useless capture to continuation");
            eprintln!("COMPLETED_POLL_LIFECYCLE {}", serde_json::json!({
                "sample":sample, "wall_ms":wall_ms,
                "workspace_captures":captures,
                "model_requests":sent.len(), "tool_calls":timing.tool_calls,
                "input_bytes":sent.iter().map(|request| serde_json::to_vec(&request.input()).unwrap().len()).sum::<usize>(),
                "exact_receipt":true, "exact_answer":true,
                "provider":"loopback-scripted", "injected_capture_delay_ms":3000,
                "scope":"run_turn through authenticated poll, continuation and final answer",
            }));
        }
        Ok(())
    })
}

#[test]
#[expect(clippy::print_stderr, reason = "this lifecycle probe emits machine-readable timings and work counts")]
fn request_lifecycle_wall_clock() -> Result<()> {
    run_turn_multi_thread_test_with_stack("request_lifecycle_wall_clock", || async {
        core_test_support::require_network!();
        for (workspace_evidence, path_watches) in [(false, false), (true, false), (true, true)] {
            for sources_changed in [false, true] {
                for sample in 0..3 {
                    let server = responses::start_mock_server().await;
                    let requests = responses::mount_sse_sequence(&server, vec![
                        responses::sse(vec![
                            responses::ev_function_call("plan", "update_plan",
                                r#"{"plan":[{"step":"Return the fixture answer","status":"completed"}]}"#),
                            responses::ev_completed("tool-response"),
                        ]),
                        responses::sse(vec![
                            responses::ev_assistant_message("answer", "Verified fixture answer: α → β."),
                            responses::ev_completed("final-response"),
                        ]),
                    ]).await;
                    let home = tempfile::tempdir()?;
                    let workspace = tempfile::tempdir()?;
                    assert!(
                        std::process::Command::new("git")
                            .args(["init", "--quiet"])
                            .current_dir(workspace.path())
                            .status()?
                            .success()
                    );
                    fs::write(workspace.path().join("source.txt"), "fixture evidence")?;
                    let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                        workspace.path(),
                    )?;
                    let provider = non_openai_model_provider(&server);
                    let (session, turn, _events) =
                        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                            CodexAuth::from_api_key("test key"), Vec::new(), home.path(),
                            move |config| {
                                config.cwd = cwd;
                                config.model_provider = provider;
                                config.features.disable(Feature::CodeModeHost).unwrap();
                                config.features.disable(Feature::CodeMode).unwrap();
                                config.features.disable(Feature::Kd4Runtime).unwrap();
                                config.model_auto_compact_token_limit = Some(i64::MAX);
                            },
                        ).await;
                    let cancellation = CancellationToken::new();
                    let step = session.capture_step_context(Arc::clone(&turn)).await?;
                    let router = built_tools(&session, &step, &[], &cancellation).await?;
                    let sources = router.tool_search_sources_for_instructions(
                        turn.developer_instructions.as_deref(),
                    );
                    record_context_notice_if_changed(
                        &session,
                        &turn,
                        "tool_search_sources",
                        if sources_changed {
                            "obsolete fixture source"
                        } else {
                            &sources
                        },
                    )
                    .await?;
                    if workspace_evidence {
                        session
                            .record_conversation_items(
                                &turn,
                                &[
                                    ResponseItem::FunctionCall {
                                        id: None,
                                        name: "read_file".into(),
                                        namespace: None,
                                        arguments: r#"{"path":"source.txt"}"#.into(),
                                        call_id: "earlier-read".into(),
                                        internal_chat_message_metadata_passthrough: None,
                                    },
                                    ResponseItem::FunctionCallOutput {
                                        id: None,
                                        call_id: "earlier-read".into(),
                                        output: FunctionCallOutputPayload::from_text(
                                            "fixture evidence".into(),
                                        ),
                                        internal_chat_message_metadata_passthrough: None,
                                    },
                                ],
                            )
                            .await?;
                    }
                    let _blocked_capture = if path_watches {
                        let cache = &session.services.git_workspace;
                        let source = workspace.path().join("source.txt");
                        let watch = cache.begin_source_path_change_observation(workspace.path(), &source, false)
                            .await.expect("fixture dependency watch");
                        let history = session.clone_history().await;
                        let output = history.raw_items().iter().find(|item| matches!(item,
                            ResponseItem::FunctionCallOutput { call_id, .. } if call_id == "earlier-read"
                        )).expect("recorded read output");
                        let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
                            cache.workspace_root_identity(workspace.path()).await,
                            output,
                            std::collections::BTreeSet::from([
                                crate::tool_history::SourceDependencyV1::new(&source, false),
                            ]),
                        ).unwrap().with_source_path_observations(vec![watch]);
                        session.register_workspace_evidence(observation, ()).await;
                        // Keep a real shared capture blocked throughout the turn.
                        // A request that unnecessarily joins it cannot complete.
                        let pause = cache.pause_next_workspace_evidence_capture();
                        let cache = Arc::clone(cache);
                        let root = workspace.path().to_path_buf();
                        let capture = AbortOnDropHandle::new(tokio::spawn(async move {
                            cache.workspace_evidence_identity_with_attribution(&root).await
                        }));
                        tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started()).await?;
                        Some(capture)
                    } else {
                        None
                    };
                    session
                        .record_conversation_items(
                            &turn,
                            &[ResponseItem::Message {
                                id: None,
                                role: "user".into(),
                                content: vec![ContentItem::InputText {
                                    text: "Return the fixture answer.".into(),
                                }],
                                phase: None,
                                internal_chat_message_metadata_passthrough: None,
                            }],
                        )
                        .await?;
                    let captures_before = session
                        .services
                        .git_workspace
                        .workspace_evidence_capture_count();
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
                            cancellation,
                        ),
                    )
                    .await??;
                    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
                    let captures = session
                        .services
                        .git_workspace
                        .workspace_evidence_capture_count()
                        - captures_before;
                    let sent = requests.requests();
                    assert_eq!(
                        result.last_agent_message.as_deref(),
                        Some("Verified fixture answer: α → β.")
                    );
                    assert!(result.required_tool_terminal.is_none());
                    assert_eq!(
                        sent.len(),
                        2,
                        "one tool request and one answer request; no retries"
                    );
                    if path_watches {
                        assert!(sent.iter().all(|request| request.input().iter().any(|item|
                            item["type"] == "function_call_output"
                                && item["call_id"] == "earlier-read"
                                && item["output"].as_str() == Some("fixture evidence")
                        )), "root-only preparation must retain currently watched evidence");
                    }
                    assert!(
                        sent[1]
                            .input()
                            .iter()
                            .any(|item| item["type"] == "function_call_output"
                                && item["call_id"] == "plan"
                                && item["output"]
                                    .as_str()
                                    .is_some_and(|output| output.contains("Plan updated"))),
                        "successful real tool result must reach the continuation"
                    );
                    let expected_notice =
                        format!("<tool_search_sources>\n{sources}\n</tool_search_sources>");
                    if sources_changed || !sources.is_empty() {
                        assert!(
                            sent[0].body_contains_text(&expected_notice),
                            "updated exposure must reach the first request"
                        );
                    }
                    session.flush_tool_history_persistence().await?;
                    eprintln!(
                        "REQUEST_LIFECYCLE {}",
                        serde_json::json!({
                            "workspace_evidence": workspace_evidence, "path_watches": path_watches, "sources_changed": sources_changed,
                            "sample": sample, "wall_ms": wall_ms, "workspace_captures": captures,
                            "model_requests": sent.len(), "tool_calls": 1, "exact_answer": true,
                            "provider": "loopback-scripted", "scope": "run_turn through tool continuation and final answer",
                        })
                    );
                    assert_eq!(
                        captures,
                        if workspace_evidence && !path_watches { 2 } else { 0 },
                        "digest evidence needs one capture per request; fully watched evidence needs none"
                    );
                }
            }
        }
        Ok(())
    })
}

#[test]
fn request_preparation_cancellation_retires_blocked_capture() -> Result<()> {
    run_turn_multi_thread_test_with_stack("request_preparation_cancellation", || async {
        core_test_support::require_network!();
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(workspace.path())
                .status()?
                .success()
        );
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
        let provider = non_openai_model_provider(&server);
        let (session, turn, _events) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                CodexAuth::from_api_key("test key"),
                Vec::new(),
                home.path(),
                move |config| {
                    config.cwd = cwd;
                    config.model_provider = provider;
                    config.features.disable(Feature::CodeModeHost).unwrap();
                },
            )
            .await;
        session
            .record_conversation_items(
                &turn,
                &[
                    ResponseItem::FunctionCall {
                        id: None,
                        name: "read_file".into(),
                        namespace: None,
                        arguments: r#"{"path":"source.txt"}"#.into(),
                        call_id: "blocked-read".into(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                    ResponseItem::FunctionCallOutput {
                        id: None,
                        call_id: "blocked-read".into(),
                        output: FunctionCallOutputPayload::from_text("historical bytes".into()),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ],
            )
            .await?;
        let pause = session
            .services
            .git_workspace
            .pause_next_workspace_evidence_capture();
        let cancellation = CancellationToken::new();
        let cancel_turn = cancellation.clone();
        let running = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut budget = LogicalGenerationBudget::default();
            run_turn(
                session,
                Arc::clone(&turn),
                Arc::new(ExtensionData::new(turn.sub_id.clone())),
                Vec::new(),
                None,
                &mut budget,
                cancel_turn,
            )
            .await
        }));
        tokio::time::timeout(Duration::from_secs(10), pause.wait_until_started()).await?;
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), running).await??;
        assert!(
            matches!(result, Err(CodexErr::TurnAborted)),
            "blocked storage must not hold interruption"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "cancelled preparation must not dispatch"
        );
        // Intentionally never release the capture: turn ownership must cancel it.
        Ok(())
    })
}

#[test]
#[expect(clippy::print_stderr, reason = "this retained-history probe emits machine-readable timings and work counts")]
fn request_preparation_large_failure_history_wall_clock() -> Result<()> {
    run_turn_multi_thread_test_with_stack("request_preparation_large_failure_history", || async {
        core_test_support::require_network!();
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
            let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
            let provider = non_openai_model_provider(&server);
            let (session, mut turn, events) =
                crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                    CodexAuth::from_api_key("test key"), Vec::new(), home.path(),
                    move |config| {
                        config.cwd = cwd;
                        config.model_provider = provider;
                        config.features.disable(Feature::CodeModeHost).unwrap();
                        config.features.disable(Feature::CodeMode).unwrap();
                        config.features.disable(Feature::Kd4Runtime).unwrap();
                        config.model_auto_compact_token_limit = Some(i64::MAX);
                    },
                ).await;
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
                    id: None, name: "read_file".into(), namespace: None,
                    arguments: r#"{"path":"source.txt"}"#.into(), call_id: call_id.clone(),
                    internal_chat_message_metadata_passthrough: None,
                });
                let mut output = FunctionCallOutputPayload::from_text(payload.clone());
                output.success = Some(false);
                history.push(ResponseItem::FunctionCallOutput {
                    id: None, call_id, output,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            history.push(ResponseItem::Message {
                id: None, role: "user".into(),
                content: vec![ContentItem::InputText { text: "Return the fixture answer.".into() }],
                phase: None, internal_chat_message_metadata_passthrough: None,
            });
            session.record_conversation_items(&turn, &history).await?;
            let started = std::time::Instant::now();
            let result = tokio::time::timeout(Duration::from_secs(30), run_turn(
                Arc::clone(&session), Arc::clone(&turn),
                Arc::new(ExtensionData::new(turn.sub_id.clone())), Vec::new(), None,
                &mut LogicalGenerationBudget::default(), CancellationToken::new(),
            )).await??;
            let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
            let sent = requests.requests();
            let mut errors = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let EventMsg::Error(error) = event.msg {
                    errors.push(error.message);
                }
            }
            assert_eq!(result.last_agent_message.as_deref(), Some("Verified retained failures."),
                "requests={}, errors={errors:?}, required_tool_terminal={:?}",
                sent.len(), result.required_tool_terminal.as_ref().map(|terminal| &terminal.message));
            assert_eq!(sent.len(), 2);
            for request in &sent {
                let failures = request.input().into_iter().filter(|item|
                    item["type"] == "function_call_output"
                        && item["call_id"].as_str().is_some_and(|id| id.starts_with("historical-failure-"))
                ).collect::<Vec<_>>();
                assert_eq!(failures.len(), 32);
                assert!(failures.iter().all(|item| item["output"].as_str() == Some(payload.as_str())),
                    "sampling must retain every historical failure byte in its original tool message");
            }
            assert!(sent[1].input().iter().any(|item|
                item["call_id"] == "plan" && item["output"].as_str().is_some_and(|text| text.contains("Plan updated"))
            ));
            session.flush_tool_history_persistence().await?;
            eprintln!("REQUEST_FAILURE_HISTORY {}", serde_json::json!({
                "sample": sample, "wall_ms": wall_ms, "history_bytes": payload.len() * 32,
                "model_requests": sent.len(), "tool_calls": 1, "exact_failures": true,
                "provider": "loopback-scripted", "scope": "run_turn through tool continuation and final answer",
            }));
        }
        Ok(())
    })
}
