use super::*;
use crate::test_harness::responses;
use codex_protocol::models::ContentItem;

fn images(count: usize) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".into(),
        content: (0..count)
            .map(|index| ContentItem::InputImage {
                image_url: format!("https://example.invalid/{index}.png"),
                detail: None,
            })
            .collect(),
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn stream(events: Vec<ResponseEvent>) -> ResponseStream {
    let (tx, rx_event) = codex_api::response_event_channel::response_event_channel(events.len());
    for event in events {
        tx.try_send(Ok(event)).unwrap();
    }
    drop(tx);
    ResponseStream {
        rx_event,
        attempt_identity: None,
        consumer_dropped: CancellationToken::new(),
    }
}

#[tokio::test]
async fn audit146_remote_rate_limits_reach_session_notifications() {
    for send_limits in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let (session, turn, rx) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |_| {},
            )
            .await;
        let snapshot: codex_protocol::protocol::RateLimitSnapshot = serde_json::from_value(
            serde_json::json!({"limit_id":"audit146", "limit_name":"Audit 146"}),
        )
        .unwrap();
        let mut events = Vec::new();
        if send_limits {
            events.push(ResponseEvent::RateLimits(snapshot.clone()));
        }
        events.extend([
            ResponseEvent::OutputItemDone(ResponseItem::Compaction {
                id: None,
                encrypted_content: "opaque".into(),
                internal_chat_message_metadata_passthrough: None,
            }),
            ResponseEvent::Completed {
                response_id: "done".into(),
                token_usage: None,
                end_turn: None,
            },
        ]);
        collect_compaction_output(
            stream(events),
            None,
            Some((&session, &turn)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let mut observed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EventMsg::TokenCount(count) = event.msg {
                if count
                    .rate_limits
                    .as_ref()
                    .is_some_and(|limits| limits.limit_id.as_deref() == Some("audit146"))
                {
                    observed.push(count.rate_limits.unwrap());
                }
            }
        }
        assert_eq!(observed, if send_limits { vec![snapshot] } else { vec![] });
    }
}

#[tokio::test]
async fn audit146_remote_image_only_omissions_have_exact_recovery() {
    use crate::tools::command_output_artifact::read_exact_tool_output_artifact;
    for count in [8, 9] {
        let home = tempfile::tempdir().unwrap();
        let (session, turn, _) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |_| {},
            )
            .await;
        let input = vec![images(count)];
        let retained = prepare_v2_retained_input_source(&session, &input)
            .await
            .unwrap();
        assert_eq!(retained.retained_images, 8);
        assert_eq!(retained.omitted_images, count - 8);
        assert!(!retained.omitted_text);
        let sidecar = retained
            .items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::Message { content, .. } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                ContentItem::InputText { text } => {
                    serde_json::from_str::<serde_json::Value>(text).ok()
                }
                _ => None,
            })
            .find(|value| value["kind"] == "local_compaction_text_recovery");
        if count == 8 {
            assert!(sidecar.is_none());
            continue;
        }
        let sidecar = sidecar.expect("image omission must have exact source recovery");
        let bytes = read_exact_tool_output_artifact(
            &turn.config.codex_home,
            &session.thread_id().to_string(),
            sidecar["artifact_id"].as_str().unwrap(),
        )
        .await
        .unwrap();
        let archived: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(archived["items"], serde_json::to_value(&input).unwrap());
    }
}

#[tokio::test]
async fn audit146_missing_image_recovery_storage_keeps_original_history() {
    let home = tempfile::tempdir().unwrap();
    let (session, turn, _) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"),
            Vec::new(),
            home.path(),
            |_| {},
        )
        .await;
    session.replace_history(vec![images(9)], None).await;
    let original = session.clone_history().await;
    std::fs::write(turn.config.codex_home.join("tool-output"), "blocked").unwrap();
    assert!(
        prepare_v2_retained_input_source(&session, original.raw_items())
            .await
            .is_err()
    );
    assert_eq!(
        session.clone_history().await.raw_items(),
        original.raw_items()
    );
}

#[tokio::test]
async fn audit146_remote_lifecycle_measures_only_installed_checkpoints_and_prepares_once()
-> anyhow::Result<()> {
    crate::test_harness::require_network!();
    for success in [false, true] {
        let server = responses::start_mock_server().await;
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())?;
        let (session, turn, rx) =
            crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
                codex_login::CodexAuth::from_api_key("test"),
                Vec::new(),
                home.path(),
                |config| {
                    config.cwd = cwd;
                    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                    config.model_provider.supports_websockets = false;
                },
            )
            .await;
        let mut input = vec![images(9)];
        if let ResponseItem::Message { content, .. } = &mut input[0] {
            content.push(ContentItem::InputText {
                text: "Preserve this constraint. ".repeat(3_000),
            });
        }
        session.replace_history(input, None).await;
        let original = session.clone_history().await;
        let mut provider_events = Vec::new();
        if success {
            provider_events.push(serde_json::json!({"type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":"opaque"}}));
        }
        provider_events.push(responses::ev_completed("compacted"));
        let log = responses::mount_sse_once(&server, responses::sse(provider_events)).await;
        let (result, preparations) = RETENTION_PREPARATIONS
            .scope(std::cell::Cell::new(0), async {
                let result = tokio::time::timeout(
                    Duration::from_secs(15),
                    run_remote_compact_task(
                        Arc::clone(&session),
                        Arc::clone(&turn),
                        &CancellationToken::new(),
                    ),
                )
                .await;
                (result, RETENTION_PREPARATIONS.with(std::cell::Cell::get))
            })
            .await;
        assert_eq!(result?.is_ok(), success);
        assert_eq!(
            preparations, 1,
            "one immutable input allocation/archive preparation per request"
        );
        assert_eq!(log.requests().len(), 1);
        let mut completed = Vec::new();
        let mut legacy_successes = 0;
        while let Ok(event) = rx.try_recv() {
            match event.msg {
                EventMsg::ItemCompleted(event) => {
                    if let TurnItem::ContextCompaction(item) = event.item {
                        completed.push(item);
                    }
                }
                EventMsg::ContextCompacted(_) => legacy_successes += 1,
                _ => {}
            }
        }
        assert_eq!(
            completed.len(),
            1,
            "failed attempts must still close their native item"
        );
        assert_eq!(legacy_successes, usize::from(success));
        let item = &completed[0];
        if success {
            assert!(item.tokens_before.is_some());
            assert!(item.tokens_after.is_some());
            assert_eq!(item.retained_image_count, Some(8));
            assert_eq!(item.omitted_image_count, Some(1));
            assert_eq!(item.omitted_text, Some(true));
        } else {
            assert!(item.tokens_after.is_none());
            assert_eq!(
                session.clone_history().await.raw_items(),
                original.raw_items()
            );
        }
        session.flush_tool_history_persistence().await?;
    }
    Ok(())
}
