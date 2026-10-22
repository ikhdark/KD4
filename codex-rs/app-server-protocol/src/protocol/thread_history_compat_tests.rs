use super::*;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::HasLegacyEvent;
use serde_json::json;

fn event(value: serde_json::Value) -> EventMsg {
    serde_json::from_value(value).unwrap()
}

fn start(builder: &mut ThreadHistoryBuilder, id: &str) {
    builder.handle_event(&event(json!({"type":"task_started", "turn_id":id})));
}

fn end(builder: &mut ThreadHistoryBuilder, id: &str) {
    builder.handle_event(&event(json!({"type":"task_complete", "turn_id":id})));
}

fn completion(item: TurnItem, turn_id: &str) -> EventMsg {
    EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id: codex_protocol::ThreadId::default(),
        turn_id: turn_id.into(),
        completed_at_ms: 0,
        item,
    })
}

#[test]
fn canonical_collaboration_outcomes_survive_actual_legacy_fanout() {
    use codex_protocol::items::CollabAgentTool as Tool;
    use codex_protocol::items::CollabAgentToolCallStatus as Status;
    for tool in [
        Tool::SpawnAgent,
        Tool::SendInput,
        Tool::ResumeAgent,
        Tool::CloseAgent,
        Tool::Wait,
    ] {
        for (status, receiver) in [
            (
                Status::Failed,
                AgentStatus::Completed(Some("old answer".into())),
            ),
            (
                Status::Completed,
                AgentStatus::Errored("old failure".into()),
            ),
        ] {
            let child = codex_protocol::ThreadId::new();
            let item = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: "call".into(),
                tool,
                status,
                sender_thread_id: codex_protocol::ThreadId::new(),
                receiver_thread_ids: vec![child],
                receiver_agents: vec![],
                prompt: Some("new request".into()),
                model: None,
                reasoning_effort: None,
                agents_states: [(child, receiver)].into_iter().collect(),
            });
            let expected = ThreadItem::from(item.clone());
            let completed = completion(item, "turn");
            let mut builder = ThreadHistoryBuilder::new();
            start(&mut builder, "turn");
            builder.handle_event(&completed);
            for alias in completed.as_legacy_events(false) {
                let changes = builder.handle_event_with_changes(&alias);
                assert!(
                    changes.is_empty(),
                    "weaker echo republished a terminal operation"
                );
            }
            end(&mut builder, "turn");
            assert_eq!(builder.finish()[0].items, vec![expected]);
        }
    }
}

#[test]
fn canonical_rich_user_input_keeps_identity_order_and_only_suppresses_its_echo() {
    use codex_protocol::user_input::UserInput;
    let item = TurnItem::UserMessage(UserMessageItem {
        id: "canonical-user".into(),
        client_id: Some("client".into()),
        content: vec![
            UserInput::Text {
                text: "inspect α".into(),
                text_elements: vec![],
            },
            UserInput::LocalPath {
                path: "notes.txt".into(),
                content: "captured".into(),
            },
            UserInput::Skill {
                name: "review".into(),
                path: "SKILL.md".into(),
            },
            UserInput::Mention {
                name: "app".into(),
                path: "app://id".into(),
            },
        ],
    });
    let expected = ThreadItem::from(item.clone());
    let completed = completion(item, "turn");
    let aliases = completed.as_legacy_events(false);
    let mut builder = ThreadHistoryBuilder::new();
    start(&mut builder, "turn");
    builder.handle_event(&completed);
    for alias in &aliases {
        assert!(builder.handle_event_with_changes(alias).is_empty());
    }
    // Equal content later is another user message, not the immediate mirror.
    builder.handle_event(&event(json!({"type":"agent_message", "message":"answer"})));
    for alias in &aliases {
        builder.handle_event(alias);
    }
    let turns = builder.finish();
    assert_eq!(turns[0].items[0], expected);
    assert_eq!(turns[0].items.len(), 3);
}

#[test]
fn rollback_counts_user_work_not_standalone_compaction_and_reports_the_same_suffix() {
    let mut builder = ThreadHistoryBuilder::new();
    for id in ["user-1", "user-2", "maintenance-a", "maintenance-b"] {
        start(&mut builder, id);
        if id.starts_with("user") {
            builder.handle_event(&event(json!({"type":"user_message", "message":id})));
        } else {
            builder.handle_event(&event(json!({"type":"context_compacted"})));
        }
        end(&mut builder, id);
    }
    let changes =
        builder.handle_event_with_changes(&EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        }));
    assert_eq!(
        changes.removed_turn_ids,
        ["user-2", "maintenance-a", "maintenance-b"]
    );
    assert_eq!(
        builder
            .finish()
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        ["user-1"]
    );
}

#[test]
fn unanswered_implicit_legacy_input_is_not_success_but_a_real_answer_is() {
    for answer in [None, Some(""), Some("finished")] {
        let mut builder = ThreadHistoryBuilder::new();
        builder.handle_event(&event(json!({"type":"user_message", "message":"continue"})));
        if let Some(answer) = answer {
            builder.handle_event(&event(json!({"type":"agent_message", "message":answer})));
        }
        let expected = if answer == Some("finished") {
            TurnStatus::Completed
        } else {
            TurnStatus::InProgress
        };
        assert_eq!(builder.finish()[0].status, expected);
    }
}

#[test]
fn imported_history_keeps_a_stable_status_without_fabricated_completion() {
    let mut builder = ThreadHistoryBuilder::new();
    start(&mut builder, "imported");
    builder.handle_event(&event(json!({"type":"user_message", "message":"question"})));
    builder.handle_event(&event(json!({"type":"agent_message", "message":"answer"})));
    builder.handle_event(&event(json!({
        "type":"imported_turn_closed", "turn_id":"imported", "completed_at":10, "duration_ms":20
    })));
    let turn = builder.finish().remove(0);
    // Stable clients, including the native Desktop, decode a closed status set.
    assert_eq!(
        serde_json::to_value(&turn.status).unwrap(),
        json!("completed")
    );
    assert_eq!(
        (turn.error, turn.completed_at, turn.duration_ms),
        (None, None, None)
    );
    assert!(turn.timing.is_none() && turn.surfaced_result.is_none());
    // The value earlier builds emitted still decodes.
    assert_eq!(
        serde_json::from_value::<TurnStatus>(json!("imported")).unwrap(),
        TurnStatus::Completed
    );
}

#[test]
fn v0_spawn_metadata_remains_unavailable_in_history() {
    let line: codex_protocol::protocol::RolloutLine = serde_json::from_value(json!({
        "timestamp":"2026-03-01T00:00:00Z", "type":"event_msg", "payload": {
            "type":"collab_agent_spawn_end", "call_id":"old-spawn",
            "sender_thread_id":codex_protocol::ThreadId::new(), "prompt":"inspect", "status":"not_found"
        }
    })).unwrap();
    let turns = build_turns_from_rollout_items(&[line.item]);
    assert!(matches!(
        &turns[0].items[0],
        ThreadItem::CollabAgentToolCall {
            model: None,
            reasoning_effort: None,
            ..
        }
    ));
}

#[test]
fn raw_user_provenance_counts_but_injected_environment_context_does_not() {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    let mut builder = ThreadHistoryBuilder::new();
    for (id, text) in [
        ("keep", "first request"),
        ("raw-user", "second request"),
        (
            "maintenance",
            "<environment_context>synthetic context</environment_context>",
        ),
    ] {
        start(&mut builder, id);
        builder.handle_rollout_item(&RolloutItem::ResponseItem(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }));
        end(&mut builder, id);
    }
    builder.handle_event(&EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 1,
    }));
    assert_eq!(
        builder
            .finish()
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
}

#[test]
fn legacy_assistant_encoded_mail_turn_is_rolled_back_alone() {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    // Mail as writers before typed delivery persisted it: an assistant message
    // whose only text is the serialized communication, without turn attribution.
    let mail = json!({
        "author": "/root/worker",
        "recipient": "/root",
        "other_recipients": [],
        "content": "worker finished",
        "trigger_turn": true,
    })
    .to_string();
    let assistant = |text: &str| ResponseItem::Message {
        id: None,
        role: "assistant".into(),
        content: vec![ContentItem::OutputText { text: text.into() }],
        phase: Some(codex_protocol::models::MessagePhase::Commentary),
        internal_chat_message_metadata_passthrough: None,
    };
    let turns_after_rollback = |reply: ResponseItem| {
        let mut builder = ThreadHistoryBuilder::new();
        start(&mut builder, "user-turn");
        builder.handle_rollout_item(&RolloutItem::ResponseItem(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText {
                text: "request".into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }));
        end(&mut builder, "user-turn");
        start(&mut builder, "mail-turn");
        builder.handle_rollout_item(&RolloutItem::ResponseItem(reply));
        end(&mut builder, "mail-turn");
        builder.handle_rollout_item(&RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
            ThreadRolledBackEvent { num_turns: 1 },
        )));
        builder
            .finish()
            .into_iter()
            .map(|turn| turn.id)
            .collect::<Vec<_>>()
    };

    // The recorded rollback removed the mail turn, not the user turn before it.
    assert_eq!(
        turns_after_rollback(assistant(mail.as_str())),
        ["user-turn"]
    );

    // A writer that stamps turn attribution never encodes mail as assistant
    // JSON, so there the same text is ordinary assistant output.
    let mut attributed = assistant(mail.as_str());
    attributed.set_turn_id_if_missing("mail-turn");
    assert_eq!(
        turns_after_rollback(attributed),
        turns_after_rollback(assistant("plain assistant text"))
    );
}
