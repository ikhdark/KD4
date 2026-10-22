use super::*;
use crate::config::RolloutConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use serde_json::json;

fn header(thread_id: ThreadId) -> serde_json::Value {
    json!({"timestamp":"2026-08-24T00:00:00Z", "format_version":1,
    "type":"session_meta", "payload":{
        "session_id":thread_id, "id":thread_id, "timestamp":"2026-08-24T00:00:00Z",
        "cwd":".", "originator":"compatibility-fixture", "cli_version":"fixture"
    }})
}

#[tokio::test]
async fn unsupported_history_rejects_resume_before_repair_or_decompression() -> std::io::Result<()>
{
    for compressed in [false, true] {
        for (version, payload) in [
            (2, json!({"type":"shutdown_complete"})),
            (1, json!({"type":"future_event"})),
            (
                1,
                json!({"type":"collab_agent_interaction_end", "call_id":"call",
                "sender_thread_id":ThreadId::new(), "receiver_thread_id":ThreadId::new(),
                "prompt":"inspect", "status":"future_status"}),
            ),
        ] {
            let home = tempfile::tempdir()?;
            let id = ThreadId::new();
            // No final newline: even repairing that boundary is a source mutation.
            let source = format!(
                "{}\n{}",
                header(id),
                json!({
                    "timestamp":"2026-08-24T00:00:01Z", "format_version":version,
                    "type":"event_msg", "payload":payload
                })
            );
            let plain = home.path().join("rollout.jsonl");
            let path = if compressed {
                home.path().join("rollout.jsonl.zst")
            } else {
                plain.clone()
            };
            let bytes = if compressed {
                zstd::stream::encode_all(source.as_bytes(), 0)?
            } else {
                source.into_bytes()
            };
            std::fs::write(&path, &bytes)?;
            let config = RolloutConfig {
                codex_home: home.path().into(),
                sqlite_home: home.path().into(),
                cwd: home.path().into(),
                model_provider_id: "fixture".into(),
            };
            let error = match RolloutRecorder::resume_and_load(&config, path.clone(), id).await {
                Err(error) => error,
                Ok((recorder, _)) => {
                    recorder.shutdown().await?;
                    panic!("unsupported writer admitted");
                }
            };
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert!(error.to_string().contains("unsupported rollout"));
            assert_eq!(std::fs::read(&path)?, bytes);
            if compressed {
                assert!(!plain.exists());
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn supported_physical_damage_still_retains_later_records() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\nnot JSON\n{}\n",
            header(ThreadId::new()),
            json!({
                "timestamp":"2026-08-24T00:00:01Z", "format_version":1,
                "type":"event_msg", "payload":{"type":"shutdown_complete"}
            })
        ),
    )?;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(&path).await?;
    assert_eq!(errors, 1);
    assert!(
        items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ShutdownComplete)))
    );
    Ok(())
}

#[tokio::test]
async fn legacy_writer_retains_rich_inputs_and_unfinished_tool_lifecycles() -> std::io::Result<()> {
    let home = tempfile::tempdir()?;
    let id = ThreadId::new();
    let raw_items = vec![
        json!({"type":"UserMessage", "id":"user", "content":[
            {"type":"text", "text":"α"}, {"type":"local_path", "path":"notes", "content":"captured"},
            {"type":"mention", "name":"app", "path":"app://id"}
        ]}),
        json!({"type":"DynamicToolCall", "id":"dynamic", "tool":"lookup", "arguments":{"opaque":null}, "status":"in_progress"}),
        json!({"type":"CollabAgentToolCall", "id":"collab", "tool":"send_input", "status":"failed", "sender_thread_id":id}),
        json!({"type":"ImageView", "id":"image", "path":"file:///fixture.png"}),
    ];
    let mut expected = Vec::new();
    for value in raw_items {
        let item: codex_protocol::items::TurnItem = serde_json::from_value(value).unwrap();
        if matches!(
            item,
            codex_protocol::items::TurnItem::DynamicToolCall(_)
                | codex_protocol::items::TurnItem::CollabAgentToolCall(_)
                | codex_protocol::items::TurnItem::ImageView(_)
        ) {
            let mut started = item.clone();
            if let codex_protocol::items::TurnItem::CollabAgentToolCall(call) = &mut started {
                call.status = codex_protocol::items::CollabAgentToolCallStatus::InProgress;
            }
            expected.push(RolloutItem::EventMsg(EventMsg::ItemStarted(
                ItemStartedEvent {
                    thread_id: id,
                    turn_id: "turn".into(),
                    started_at_ms: 0,
                    item: started,
                },
            )));
        }
        if matches!(&item, codex_protocol::items::TurnItem::DynamicToolCall(call)
            if call.status == codex_protocol::items::DynamicToolCallStatus::InProgress)
        {
            continue;
        }
        expected.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(
            ItemCompletedEvent {
                thread_id: id,
                turn_id: "turn".into(),
                completed_at_ms: 0,
                item,
            },
        )));
    }
    let retained = crate::policy::persisted_rollout_items(&expected, ThreadHistoryMode::Legacy);
    assert_eq!(
        serde_json::to_value(&retained).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    let config = RolloutConfig {
        codex_home: home.path().into(),
        sqlite_home: home.path().into(),
        cwd: home.path().into(),
        model_provider_id: "fixture".into(),
    };
    let recorder = RolloutRecorder::new(
        &config,
        RolloutRecorderParams::new(
            id,
            None,
            None,
            SessionSource::Exec,
            None,
            "compatibility-fixture".into(),
            codex_protocol::models::BaseInstructions::default(),
            vec![],
        ),
    )
    .await?;
    recorder.record_canonical_items(&retained).await?;
    recorder.persist().await?;
    recorder.shutdown().await?;
    let (items, _, errors) = RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
    assert_eq!(errors, 0);
    let events: Vec<_> = items
        .into_iter()
        .filter(|item| matches!(item, RolloutItem::EventMsg(_)))
        .collect();
    assert_eq!(
        serde_json::to_value(events).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    Ok(())
}
