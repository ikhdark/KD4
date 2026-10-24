use super::*;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus as CoreDynamicToolCallStatus;
use codex_protocol::items::TurnItem as CoreTurnItem;
use codex_protocol::protocol::AgentMessageEvent;

fn user(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
        message: text.into(),
        ..Default::default()
    }))
}

fn agent(text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: text.into(), phase: None,
    }))
}

fn project_full(mut turns: Vec<Turn>, view: TurnItemsView) -> Vec<Turn> {
    for turn in &mut turns {
        turn.items = match view {
            TurnItemsView::Full => std::mem::take(&mut turn.items),
            TurnItemsView::NotLoaded => Vec::new(),
            TurnItemsView::Summary => {
                let user = turn.items.iter().find(|item| matches!(item, ThreadItem::UserMessage { .. }));
                let agent = turn.items.iter().rev().find(|item| matches!(item, ThreadItem::AgentMessage { .. }));
                user.into_iter().chain(agent.filter(|agent| user.is_none_or(|user| user.id() != agent.id())))
                    .cloned().collect()
            }
        };
        turn.items_view = view;
    }
    turns
}

#[test]
fn projected_replay_matches_full_replay_across_implicit_boundaries_and_rollback() {
    let history = vec![
        user("first"), agent("first response"),
        RolloutItem::EventMsg(EventMsg::AgentReasoning(AgentReasoningEvent { text: "reasoning".repeat(4096) })),
        RolloutItem::EventMsg(EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text: "raw".repeat(4096) })),
        user("removed"), agent("removed response"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 })),
        user("retained"), agent("retained response"),
    ];
    for end in 0..=history.len() {
        for view in [TurnItemsView::Full, TurnItemsView::Summary, TurnItemsView::NotLoaded] {
            assert_eq!(
                build_turns_from_rollout_items_with_items_view(&history[..end], view),
                project_full(build_turns_from_rollout_items(&history[..end]), view)
            );
        }
    }
}

#[test]
fn projected_modern_tool_replay_preserves_metadata_without_materializing_large_payloads() {
    let tool = CoreTurnItem::DynamicToolCall(DynamicToolCallItem {
        id: "tool".into(), namespace: None, tool: "large-tool".into(),
        arguments: serde_json::Value::String("omitted-payload".repeat(100_000)),
        status: CoreDynamicToolCallStatus::Completed,
        content_items: None, success: Some(true), error: None, duration: None,
    });
    let history = vec![
        RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_id: "turn".into(), trace_id: None, started_at: Some(12),
            model_context_window: None, collaboration_mode_kind: Default::default(),
        })),
        user("question"),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: codex_protocol::ThreadId::new(), turn_id: "turn".into(),
            item: tool.clone(), completed_at_ms: 13_000,
        })),
        agent("answer"),
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: "turn".into(), last_agent_message: None, error: None,
            completed_at: Some(15), duration_ms: Some(3000),
            time_to_first_token_ms: None, timing: None, surfaced_result: None,
        })),
    ];
    let mut structural = ThreadHistoryBuilder::new();
    structural.omit_tool_payloads = true;
    assert!(matches!(structural.materialize_item(&tool), ThreadItem::Plan { text, .. } if text.is_empty()));
    for item in &history { structural.handle_rollout_item(item); }
    let structural_bytes = serde_json::to_vec(&structural.finish()).unwrap().len();
    let full = build_turns_from_rollout_items(&history);
    assert!(serde_json::to_vec(&full).unwrap().len() > 1_000_000);
    assert!(structural_bytes < 4096, "tool payload must never enter replay state");
    for view in [TurnItemsView::Full, TurnItemsView::Summary, TurnItemsView::NotLoaded] {
        assert_eq!(
            build_turns_from_rollout_items_with_items_view(&history, view),
            project_full(full.clone(), view)
        );
    }
}

#[test]
fn projected_replay_preserves_canonical_message_mirrors_and_late_turn_owners() {
    use codex_protocol::items::AgentMessageContent;
    use codex_protocol::items::AgentMessageItem;
    use codex_protocol::items::CollabAgentTool as CoreCollabTool;
    use codex_protocol::items::CollabAgentToolCallItem;
    use codex_protocol::items::CollabAgentToolCallStatus as CoreCollabStatus;
    let thread_id = codex_protocol::ThreadId::new();
    let start = |id: &str| RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: id.into(), trace_id: None, started_at: Some(10),
        model_context_window: None, collaboration_mode_kind: Default::default(),
    }));
    let collab = |status| CoreTurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: "wait".into(), tool: CoreCollabTool::Wait, status,
        sender_thread_id: thread_id, receiver_thread_ids: Vec::new(), receiver_agents: Vec::new(),
        prompt: Some("omitted prompt".repeat(10_000)), model: None, reasoning_effort: None,
        agents_states: HashMap::new(),
    });
    let completed = |id: &str, item| RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id, turn_id: id.into(), item, completed_at_ms: 10_000,
    }));
    let history = vec![
        start("old"), user("old question"),
        completed("old", collab(CoreCollabStatus::InProgress)),
        completed("old", CoreTurnItem::AgentMessage(AgentMessageItem {
            id: "canonical-answer".into(),
            content: vec![AgentMessageContent::Text { text: "answer".into() }], phase: None,
        })),
        agent("answer"),
        start("current"), user("current question"),
        completed("old", collab(CoreCollabStatus::Failed)),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent { num_turns: 1 })),
        user("after rollback"), agent("final answer"),
    ];
    for end in 0..=history.len() {
        for view in [TurnItemsView::Full, TurnItemsView::Summary, TurnItemsView::NotLoaded] {
            assert_eq!(
                build_turns_from_rollout_items_with_items_view(&history[..end], view),
                project_full(build_turns_from_rollout_items(&history[..end]), view)
            );
        }
    }
}
