use super::*;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::UserMessageEvent;
use serde_json::json;

#[test]
fn live_and_seeded_rollback_remove_user_work_and_all_trailing_maintenance() {
    for seeded in [false, true] {
        let mut history = Vec::new();
        for id in ["user-1", "user-2", "compaction"] {
            history.push(RolloutItem::EventMsg(
                serde_json::from_value(json!({
                    "type":"task_started", "turn_id":id
                }))
                .unwrap(),
            ));
            let event = if id == "compaction" {
                serde_json::from_value(json!({"type":"context_compacted"})).unwrap()
            } else {
                EventMsg::UserMessage(UserMessageEvent {
                    message: id.into(),
                    ..Default::default()
                })
            };
            history.push(RolloutItem::EventMsg(event));
            history.push(RolloutItem::EventMsg(
                serde_json::from_value(json!({
                    "type":"task_complete", "turn_id":id
                }))
                .unwrap(),
            ));
        }
        let mut state = ThreadState::default();
        state.seed_turn_index_from_history(if seeded { &history } else { &[] });
        if !seeded {
            for item in &history {
                if let RolloutItem::EventMsg(event) = item {
                    let id = ThreadHistoryBuilder::scoped_event_turn_id(event).unwrap_or("");
                    state.track_current_turn_event_with_reconciled_wait_items(id, event);
                }
            }
        }
        let previous_generation = state.turn_index_history_generation();
        state.track_current_turn_event_with_reconciled_wait_items(
            "",
            &EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 }),
        );
        assert_ne!(state.turn_index_history_generation(), previous_generation);
        let page = state
            .indexed_turns_page(None, 10, SortDirection::Asc)
            .unwrap()
            .unwrap();
        assert_eq!(
            page.turns
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            ["user-1"]
        );
        assert!(
            state
                .indexed_turns_page(Some(("user-2", false)), 10, SortDirection::Asc)
                .is_err()
        );
    }
}

#[test]
fn live_raw_input_uses_envelope_ownership_without_rendering_or_resurrecting_items() {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::RawResponseItemEvent;
    let mut state = ThreadState::default();
    state.seed_turn_index_from_history(&[]);
    for (id, text) in [
        ("keep", "first request"),
        ("raw-user", "second request"),
        (
            "maintenance",
            "<environment_context>context</environment_context>",
        ),
    ] {
        let started = serde_json::from_value(json!({"type":"task_started", "turn_id":id})).unwrap();
        state.track_current_turn_event_with_reconciled_wait_items(id, &started);
        let raw = EventMsg::RawResponseItem(RawResponseItemEvent {
            item: ResponseItem::Message {
                id: None,
                role: "user".into(),
                content: vec![ContentItem::InputText { text: text.into() }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
        });
        state.track_current_turn_event_with_reconciled_wait_items(id, &raw);
        let complete =
            serde_json::from_value(json!({"type":"task_complete", "turn_id":id})).unwrap();
        state.track_current_turn_event_with_reconciled_wait_items(id, &complete);
    }
    state.track_current_turn_event_with_reconciled_wait_items(
        "",
        &EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 }),
    );
    let page = state
        .indexed_turns_page(None, 10, SortDirection::Asc)
        .unwrap()
        .unwrap();
    assert_eq!(
        page.turns
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
    assert!(page.turns[0].items.is_empty());
}
