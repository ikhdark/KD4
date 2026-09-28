use super::*;
use crate::tool_history::ToolHistoryCandidate;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::read_complete_canonical_snapshot;
use codex_login::CodexAuth;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::TokenUsage;
use codex_tools::CanonicalToolResult;
use core_test_support::responses;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: role.into(),
        content: vec![ContentItem::InputText { text: text.into() }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn call(id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "exec".into(),
        namespace: None,
        arguments: "{}".into(),
        call_id: id.into(),
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn capture_prefill(sess: &Session, turn: &TurnContext) {
    prepare_sampling_prompt_for_client(
        sess.clone_history().await,
        turn,
        sess.services.git_workspace.as_ref(),
    )
    .await;
    sess.state
        .lock()
        .await
        .ensure_auto_compact_window_server_prefill_from_usage(&TokenUsage {
            input_tokens: 1000,
            total_tokens: 1000,
            ..Default::default()
        });
}

async fn pressure_case(body_scope: bool, kind: &str) {
    let server = responses::start_mock_server().await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "Preserve CHECKPOINT_CONSTRAINT.\n",
    )
    .unwrap();
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = "Checkpoint pressure local test".into();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider.request_max_retries = Some(0);
    provider.stream_max_retries = Some(u64::from(kind == "retry_grows"));
    let scope = if body_scope {
        AutoCompactTokenLimitScope::BodyAfterPrefix
    } else {
        AutoCompactTokenLimitScope::Total
    };
    let (mut sess, turn, rx) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            CodexAuth::from_api_key("test-key"),
            vec![],
            home.path(),
            |config| {
                config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                    workspace.path(),
                )
                .unwrap();
                config.model_provider = provider;
                config.model_context_window = Some(1_000_000);
                config.model_auto_compact_token_limit = Some(100_000);
                config.model_auto_compact_token_limit_scope = scope;
                for feature in [
                    Feature::TokenBudget,
                    Feature::CodeModeHost,
                    Feature::CodeMode,
                    Feature::Kd4Runtime,
                    Feature::EnableRequestCompression,
                ] {
                    config.features.disable(feature).unwrap();
                }
                config.base_instructions = Some(
                    "Follow CHECKPOINT_CONSTRAINT; retain failures and recovery handles.".into(),
                );
                config.completed_tool_history_projection = true;
            },
        )
        .await;
    let rollout = if kind == "fits" && !body_scope {
        Some(
            crate::session::tests::attach_thread_persistence(
                Arc::get_mut(&mut sess).expect("fixture session is not yet shared"),
            )
            .await,
        )
    } else {
        None
    };
    if kind == "unknown_prefill" {
        sess.state
            .lock()
            .await
            .ensure_auto_compact_window_server_prefill_from_usage(&TokenUsage {
                input_tokens: 1000,
                total_tokens: 1000,
                ..Default::default()
            });
    } else if kind != "prefill" {
        capture_prefill(&sess, &turn).await;
    }
    let source = "source evidence line\n".repeat(6000);
    let canonical = CanonicalToolResult::text(source.clone());
    let artifact =
        create_canonical_output_artifact(home.path(), &sess.thread_id.to_string(), &canonical)
            .await;
    assert!(artifact.complete);
    let artifact_id = artifact.artifact_id().unwrap();
    sess.register_non_workspace_code_mode_call("retire-me".into())
        .await;
    sess.register_tool_history_candidate(ToolHistoryCandidate {
        call_id: "retire-me".into(),
        tool_identity: "functions.exec".into(),
        semantic_class: "tool_output".into(),
        successful: true,
        source_dependencies: BTreeSet::new(),
        source_dependencies_current: true,
        artifact_id: artifact_id.clone(),
        artifact_bytes: canonical.exact_bytes,
        artifact_sha256: canonical.sha256.clone(),
        original_output_sha256: canonical.sha256,
        original_tokens: canonical.approximate_tokens,
        preserved_non_text_tokens: Some(0),
        bounded_model_output: source.clone(),
        complete: true,
        projection_eligible: true,
        proof_identity: None,
        supersession_identity: None,
        consumed_by_generation: Some(ModelGenerationId {
            turn_id: "seed".into(),
            ordinal: 1,
        }),
        derived: Default::default(),
    })
    .await;
    sess.record_conversation_items(&turn, &[call("retire-me")])
        .await
        .unwrap();
    sess.record_conversation_items(
        &turn,
        &[
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "retire-me".into(),
                output: FunctionCallOutputPayload::from_text(source.clone()),
                internal_chat_message_metadata_passthrough: None,
            },
            message("assistant", "Source read; UNRESOLVED_FAILURE remains."),
        ],
    )
    .await
    .unwrap();
    if kind == "prefill" {
        capture_prefill(&sess, &turn).await;
    }
    sess.register_non_workspace_code_mode_call("failed-proof".into())
        .await;
    sess.record_conversation_items(&turn, &[call("failed-proof")])
        .await
        .unwrap();
    let mut failed =
        FunctionCallOutputPayload::from_text("UNRESOLVED_FAILURE: assertion mismatch".into());
    failed.success = Some(false);
    sess.record_conversation_items(
        &turn,
        &[ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "failed-proof".into(),
            output: failed,
            internal_chat_message_metadata_passthrough: None,
        }],
    )
    .await
    .unwrap();
    let args = json!({"summary":"evidence consumed","completed_call_ids":[if kind=="rejected" {"unknown"} else {"retire-me"}],
        "active_work":"Preserve UNRESOLVED_FAILURE and CHECKPOINT_CONSTRAINT","retained_evidence":[]});
    let mut first = vec![responses::ev_function_call(
        "checkpoint",
        "context_checkpoint",
        &args.to_string(),
    )];
    if kind == "oversized" {
        first.push(responses::ev_assistant_message(
            "large",
            &"mandatory unselected evidence ".repeat(20000),
        ));
    }
    first.push(responses::ev_completed_with_tokens(
        "checkpoint-response",
        102000,
    ));
    let compacts = matches!(kind, "rejected" | "oversized" | "retry_grows")
        || (body_scope && matches!(kind, "prefill" | "unknown_prefill"));
    let mut sequence = vec![responses::sse(first)];
    if kind == "retry_grows" {
        // Accepted output followed by EOF rebuilds the retry prompt. Its new
        // pressure must be checked before another sampling request is sent.
        sequence.push(responses::sse(vec![responses::ev_assistant_message(
            "retry-growth",
            &"mandatory unselected evidence ".repeat(20000),
        )]));
    }
    if compacts {
        sequence.push(responses::sse(vec![responses::ev_assistant_message("compact",
            "## Goal\nfinish task\n\n## Current state\ncheckpoint processed\n\n## Completed work\nsource read\n\n## Unresolved work\nUNRESOLVED_FAILURE\n\n## Evidence\nretained artifacts\n\n## Next action\nfinish request"),responses::ev_completed_with_tokens("compact",100)]));
    }
    sequence.push(responses::sse(vec![
        responses::ev_assistant_message("final", "CHECKPOINT_COMPLETE"),
        responses::ev_completed_with_tokens("final", 100),
    ]));
    let requests = responses::mount_sse_sequence(&server, sequence).await;
    let result = tokio::time::timeout(
        Duration::from_secs(40),
        run_turn(
            Arc::clone(&sess),
            Arc::clone(&turn),
            Arc::new(codex_extension_api::ExtensionData::new("checkpoint-turn")),
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text:
                        "Checkpoint consumed evidence and finish; preserve CHECKPOINT_CONSTRAINT."
                            .into(),
                    text_elements: vec![],
                }],
                client_id: None,
            }],
            None,
            &mut LogicalGenerationBudget::default(),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("bounded checkpoint turn")
    .unwrap();
    let mut errors = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let EventMsg::Error(error) = event.msg {
            errors.push(error.message);
        }
    }
    let captured = requests.requests();
    assert!(
        errors.is_empty(),
        "{kind} body_scope={body_scope} requests={} {errors:?}",
        captured.len()
    );
    assert_eq!(
        result.last_agent_message.as_deref(),
        Some("CHECKPOINT_COMPLETE")
    );
    assert_eq!(
        captured.len(),
        (if compacts { 3 } else { 2 }) + usize::from(kind == "retry_grows")
    );
    assert_eq!(
        captured
            .iter()
            .filter(|request| request
                .body_contains_text(crate::compact::COMPACTION_BASE_INSTRUCTIONS.trim()))
            .count(),
        usize::from(compacts)
    );
    if !compacts {
        let request = captured.last().unwrap();
        let pin: serde_json::Value =
            serde_json::from_str(&request.function_call_output_text("retire-me").unwrap()).unwrap();
        assert_eq!(pin["artifact_id"], artifact_id);
        assert!(request.body_contains_text("CHECKPOINT_CONSTRAINT"));
        assert!(request.body_contains_text("UNRESOLVED_FAILURE"));
        assert_eq!(
            request.function_call_output_text("failed-proof").as_deref(),
            Some("UNRESOLVED_FAILURE: assertion mismatch")
        );
        let accepted: serde_json::Value =
            serde_json::from_str(&request.function_call_output_text("checkpoint").unwrap())
                .unwrap();
        assert_eq!(accepted["changed"], true);
    }
    assert_eq!(
        read_complete_canonical_snapshot(
            home.path(),
            &sess.thread_id.to_string(),
            &artifact_id,
            source.len()
        )
        .await
        .unwrap(),
        source.as_bytes()
    );
    sess.services.code_mode_service.shutdown().await.unwrap();
    if let Some(path) = rollout {
        sess.flush_rollout().await.unwrap();
        let codex_protocol::protocol::InitialHistory::Resumed(restored) =
            crate::rollout::recorder::RolloutRecorder::get_rollout_history(&path)
                .await
                .unwrap()
        else {
            panic!("persisted checkpoint history")
        };
        let items = restored
            .history
            .iter()
            .filter_map(|item| match item {
                codex_protocol::protocol::RolloutItem::ResponseItem(item) => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item,
                    ResponseItem::FunctionCall { name, .. } if name == "context_checkpoint"
                ))
                .count(),
            1
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item,
                    ResponseItem::FunctionCall { call_id, .. } if call_id == "retire-me"
                ))
                .count(),
            1,
            "checkpointing must not re-execute the producer"
        );
        assert!(
            items.iter().any(|item| matches!(item,
                ResponseItem::FunctionCallOutput { call_id, output, .. }
                    if call_id == "retire-me" && output.text_content() == Some(source.as_str())
            )),
            "canonical output survives persistence"
        );
        assert!(items.iter().any(|item| {
            serde_json::to_string(item)
                .unwrap()
                .contains("<completed_phase_checkpoint>")
        }));
    }
}

#[test]
fn checkpoint_pressure_uses_prepared_request_and_keeps_required_compaction() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    for body in [false, true] {
                        for kind in [
                            "fits",
                            "rejected",
                            "oversized",
                            "prefill",
                            "unknown_prefill",
                            "retry_grows",
                        ] {
                            pressure_case(body, kind).await;
                        }
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "forces preflight to wait so cancellation is tested while blocked"
)]
async fn checkpoint_preflight_preserves_usage_and_honors_cancellation_and_explicit_request() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).model_auto_compact_token_limit_scope =
        AutoCompactTokenLimitScope::BodyAfterPrefix;
    let prompt = Prompt::default();
    let cancellation = CancellationToken::new();
    let before = session.token_usage_info().await;
    let guard = session.state.lock().await;
    let mut check = Box::pin(checkpoint_preflight_requires_compaction(
        &session,
        &turn,
        &prompt,
        &cancellation,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), check.as_mut())
            .await
            .is_err()
    );
    cancellation.cancel();
    drop(guard);
    assert!(matches!(check.await, Err(CodexErr::TurnAborted)));
    assert_eq!(session.token_usage_info().await, before);

    Arc::make_mut(&mut turn.config)
        .features
        .enable(Feature::TokenBudget)
        .unwrap();
    session.request_new_context_window().await;
    assert!(
        checkpoint_preflight_requires_compaction(
            &session,
            &turn,
            &prompt,
            &CancellationToken::new()
        )
        .await
        .unwrap()
    );
    assert_eq!(session.token_usage_info().await, before);
}
