use super::*;

#[test]
fn projected_history_pages_match_full_reconstruction_with_active_merge_and_cursors() {
    use codex_protocol::items::DynamicToolCallItem;
    use codex_protocol::items::DynamicToolCallStatus;
    use codex_protocol::items::TurnItem;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::ItemCompletedEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::UserMessageEvent;
    let mut history = Vec::new();
    for id in ["first", "second", "third"] {
        history.push(RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: id.into(), trace_id: None, started_at: Some(10),
            model_context_window: None, collaboration_mode_kind: Default::default(),
        })));
        history.push(RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: format!("question {id}"), ..Default::default()
        })));
        history.push(RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: ThreadId::new(), turn_id: id.into(), completed_at_ms: 11_000,
            item: TurnItem::DynamicToolCall(DynamicToolCallItem {
                id: format!("tool-{id}"), namespace: None, tool: "tool".into(),
                arguments: serde_json::Value::String("large omitted payload".repeat(4096)),
                status: DynamicToolCallStatus::Completed, content_items: None,
                success: Some(true), error: None, duration: None,
            }),
        })));
        history.push(RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
            message: format!("answer {id}"), phase: None,
        })));
    }
    let mut active = reconstruct_thread_turns_for_turns_list(&history, ThreadStatus::Idle, false, None)
        .first().cloned().unwrap();
    active.status = TurnStatus::InProgress;
    for active_turn in [None, Some(active)] {
        for live in [false, true] {
            for direction in [SortDirection::Asc, SortDirection::Desc] {
                for limit in [Some(0), Some(1), Some(2), Some(20)] {
                    for view in [TurnItemsView::Full, TurnItemsView::Summary, TurnItemsView::NotLoaded] {
                        let full = reconstruct_thread_turns_for_turns_list(
                            &history, ThreadStatus::Idle, live, active_turn.clone(),
                        );
                        let mut expected = paginate_thread_turns(full, None, limit, direction).unwrap();
                        apply_thread_turns_items_view(&mut expected.turns, view);
                        let actual = build_thread_resume_initial_turns_page_from_history(
                            &history, ThreadStatus::Idle, live, active_turn.clone(),
                            &ThreadResumeInitialTurnsPageParams {
                                limit, sort_direction: Some(direction), items_view: Some(view),
                            },
                        ).unwrap();
                        assert_eq!(actual.data, expected.turns);
                        assert_eq!(actual.next_cursor, expected.next_cursor);
                        assert_eq!(actual.backwards_cursor, expected.backwards_cursor);
                    }
                }
            }
        }
    }
}
