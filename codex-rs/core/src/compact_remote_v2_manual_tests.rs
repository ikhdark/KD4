use super::*;
use crate::test_harness::responses;
use codex_protocol::models::ContentItem;

#[test_case::test_case(false, false; "provider overflow recovers")]
#[test_case::test_case(true, false; "preflight overflow recovers")]
#[test_case::test_case(false, true; "fallback overflow preserves history")]
#[tokio::test]
async fn manual_compaction_context_overflow(
    preflight: bool,
    fallback_fails: bool,
) -> anyhow::Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    let home = tempfile::tempdir()?;
    let (session, mut turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"),
            Vec::new(),
            home.path(),
            |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
                config.model_provider.stream_max_retries = Some(0);
            },
        )
        .await;
    let turn_mut = Arc::get_mut(&mut turn).unwrap();
    turn_mut.model_info.context_window = Some(32_000);
    turn_mut.model_info.effective_context_window_percent = 100;
    let request = if preflight {
        "Keep exact unresolved constraints. ".repeat(20_000)
    } else {
        "Keep exact unresolved constraints.".into()
    };
    let input: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user",
        "content": [{"type": "input_text", "text": request}],
    }))?;
    session
        .record_conversation_items(&turn, &[input.clone()])
        .await?;
    let before = session.clone_history().await;
    let window = session.current_window_id().await;
    let overflow = responses::sse_failed("overflow", "context_length_exceeded", "too long");
    let mut replies = Vec::new();
    if !preflight {
        replies.push(overflow.clone());
    }
    replies.push(if fallback_fails {
        overflow
    } else {
        responses::sse(vec![
            responses::ev_assistant_message(
                "summary",
                &serde_json::json!({
                    "Goal": "Keep exact unresolved constraints.",
                    "Current state": "Compacting the thread.",
                    "Completed work": "No task work completed.",
                    "Unresolved work": "Preserve the original constraints.",
                    "Evidence": "The original user request remains authoritative.",
                    "Next action": "Continue the task.",
                })
                .to_string(),
            ),
            responses::ev_completed("compacted"),
        ])
    });
    let log = responses::mount_sse_sequence(&server, replies).await;
    let result = run_remote_compact_task(
        Arc::clone(&session),
        Arc::clone(&turn),
        &CancellationToken::new(),
    )
    .await;
    if fallback_fails {
        assert!(
            matches!(result, Err(CodexErr::ContextWindowExceeded)),
            "{result:?}"
        );
        assert_eq!(session.current_window_id().await, window);
        assert_eq!(
            session.clone_history().await.raw_items(),
            before.raw_items()
        );
    } else {
        result?;
        assert_ne!(session.current_window_id().await, window);
        let history = session.clone_history().await;
        if preflight {
            assert!(
                serde_json::to_string(history.raw_items())?
                    .contains("local_compaction_text_recovery")
            );
        } else {
            assert!(history.raw_items().contains(&input));
        }
        assert!(history.raw_items().iter().any(|item| matches!(
            item,
            ResponseItem::Message { content, .. }
                if content.iter().any(|part| matches!(part,
                    ContentItem::InputText { text } if text.contains("## Unresolved work")
                ))
        )));
    }
    let requests = log.requests();
    assert_eq!(requests.len(), if preflight { 1 } else { 2 });
    let local_request = requests.last().unwrap().body_json();
    assert!(
        !local_request["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "compaction_trigger")
    );
    let mut starts = 0;
    let mut errors = Vec::new();
    let mut compaction_starts = Vec::new();
    let mut compaction_completions = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::TurnStarted(_) => starts += 1,
            EventMsg::Error(error) => errors.push(error),
            EventMsg::ItemStarted(event) => {
                if let TurnItem::ContextCompaction(item) = event.item {
                    compaction_starts.push(item.id);
                }
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::ContextCompaction(item) = event.item {
                    compaction_completions.push(item.id);
                }
            }
            _ => {}
        }
    }
    assert_eq!(starts, 1, "format fallback stays within the manual turn");
    assert_eq!(errors.len(), usize::from(fallback_fails));
    assert_eq!(compaction_starts.len(), 2);
    assert_eq!(compaction_starts, compaction_completions);
    Ok(())
}

#[tokio::test]
async fn manual_compaction_cancelled_during_local_fallback_keeps_history() -> anyhow::Result<()> {
    use crate::test_harness::streaming_sse::StreamingSseChunk;
    use crate::test_harness::streaming_sse::start_streaming_sse_server;

    crate::test_harness::require_network!();
    let (gate, gated) = tokio::sync::oneshot::channel();
    let (server, _completions) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: responses::sse_failed("overflow", "context_length_exceeded", "too long"),
        }],
        vec![StreamingSseChunk {
            gate: Some(gated),
            body: String::new(),
        }],
    ])
    .await;
    let home = tempfile::tempdir()?;
    let (session, turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"),
            Vec::new(),
            home.path(),
            |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
                config.model_provider.stream_max_retries = Some(0);
            },
        )
        .await;
    let input: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "Preserve this constraint."}],
    }))?;
    session.record_conversation_items(&turn, &[input]).await?;
    let before = session.clone_history().await;
    let window = session.current_window_id().await;
    let cancellation = CancellationToken::new();
    let mut compact = Box::pin(run_remote_compact_task(
        Arc::clone(&session),
        Arc::clone(&turn),
        &cancellation,
    ));
    let reached_fallback = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = server.wait_for_request_count(2) => Ok(()),
            result = &mut compact => Err(anyhow::anyhow!("fallback ended before cancellation: {result:?}")),
        }
    })
    .await;
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), &mut compact).await;
    drop(compact);
    drop(gate);
    let request_count = server.requests().await.len();
    server.shutdown().await;
    reached_fallback??;
    assert!(matches!(result?, Err(CodexErr::TurnAborted)));
    assert_eq!(
        request_count, 2,
        "cancellation must not dispatch another request"
    );
    assert_eq!(session.current_window_id().await, window);
    assert_eq!(
        session.clone_history().await.raw_items(),
        before.raw_items()
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event.msg, EventMsg::Error(_)));
    }
    Ok(())
}

#[tokio::test]
async fn manual_compaction_auth_failure_does_not_fallback() -> anyhow::Result<()> {
    crate::test_harness::require_network!();
    let server = responses::start_mock_server().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"code": "invalid_api_key", "message": "unauthorized fixture"},
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir()?;
    let (session, turn, events) =
        crate::session::tests::make_session_and_context_with_auth_config_home_and_rx(
            codex_login::CodexAuth::from_api_key("test"),
            Vec::new(),
            home.path(),
            |config| {
                config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
                config.model_provider.supports_websockets = false;
            },
        )
        .await;
    let before = session.clone_history().await;
    let window = session.current_window_id().await;
    let result = run_remote_compact_task(session.clone(), turn, &CancellationToken::new()).await;
    assert!(result.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(session.current_window_id().await, window);
    assert_eq!(
        session.clone_history().await.raw_items(),
        before.raw_items()
    );
    let mut errors = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event.msg, EventMsg::Error(_)) {
            errors += 1;
        }
    }
    assert_eq!(errors, 1);
    Ok(())
}
