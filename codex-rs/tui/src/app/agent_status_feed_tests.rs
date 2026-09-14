use super::*;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_utils_absolute_path::AbsolutePathBuf;

#[test]
fn agent_status_uses_bounded_buffered_activity() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::CommandExecution {
                id: "command-1".to_string(),
                command: "cargo test -p codex-tui".to_string(),
                cwd: AbsolutePathBuf::try_from("/workspace")
                    .expect("absolute path")
                    .into(),
                process_id: None,
                parent_call_id: None,
                parent_cell_id: None,
                runtime_tool_call_id: None,
                execution_id: None,
                source: CommandExecutionSource::Agent,
                status: CommandExecutionStatus::Completed,
                command_actions: Vec::new(),
                aggregated_output: Some("unbounded output\n".repeat(10_000)),
                exit_code: Some(0),
                duration_ms: Some(42),
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "message-1".to_string(),
                text: "Finished checking the focused TUI tests.".to_string(),
                phase: None,
                memory_citation: None,
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 2,
        },
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /agent
    Sub-agents running

      • `/root/reviewer`
        $ cargo test -p codex-tui
        Finished checking the focused TUI tests.
    "###);
    assert!(!rendered.contains("unbounded output"));
}

#[test]
fn agent_status_uses_reasoning_summaries_only() {
    let mut store = ThreadEventStore::new(/*capacity*/ 8);
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::Reasoning {
                id: "reasoning-with-summary".to_string(),
                summary: vec!["safe summary".to_string()],
                content: vec!["hidden raw reasoning".to_string()],
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 1,
        },
    ));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::Reasoning {
                id: "reasoning-without-summary".to_string(),
                summary: Vec::new(),
                content: vec!["raw-only reasoning".to_string()],
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 2,
        },
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    let cell = AgentStatusHistoryCell::new(vec![preview]);
    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(rendered, @r###"
    /agent
    Sub-agents running

      • `/root/reviewer`
        safe summary
    "###);
    assert!(!rendered.contains("hidden raw reasoning"));
    assert!(!rendered.contains("raw-only reasoning"));
}

#[test]
fn agent_status_bounds_items_unicode_and_lines_and_keeps_latest_item() {
    use codex_app_server_protocol::ItemStartedNotification;

    let mut store = ThreadEventStore::new(/*capacity*/ 16);
    for index in 0..8 {
        store.push_notification(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                item: ThreadItem::AgentMessage {
                    id: format!("message-{index}"),
                    text: format!("activity {index}"),
                    phase: None,
                    memory_citation: None,
                },
                thread_id: "thread-child".to_string(),
                turn_id: "turn-1".to_string(),
                completed_at_ms: index,
            },
        ));
    }
    store.push_notification(ServerNotification::ItemStarted(ItemStartedNotification {
        item: ThreadItem::AgentMessage {
            id: "latest".to_string(),
            text: "obsolete draft".to_string(),
            phase: None,
            memory_citation: None,
        },
        thread_id: "thread-child".to_string(),
        turn_id: "turn-1".to_string(),
        started_at_ms: 9,
    }));
    store.push_notification(ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            item: ThreadItem::AgentMessage {
                id: "latest".to_string(),
                text: "e\u{301}".repeat(300),
                phase: None,
                memory_citation: None,
            },
            thread_id: "thread-child".to_string(),
            turn_id: "turn-1".to_string(),
            completed_at_ms: 10,
        },
    ));

    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    assert_eq!(
        preview.activity,
        vec![
            "activity 3".to_string(),
            "activity 4".to_string(),
            "activity 5".to_string(),
            "activity 6".to_string(),
            "activity 7".to_string(),
            format!("{}...", "e\u{301}".repeat(237)),
        ]
    );
    let rendered = AgentStatusHistoryCell::new(vec![preview]).display_lines(20);
    assert_eq!(
        rendered.len(),
        7,
        "heading, agent title, and three preview lines"
    );
    assert!(
        rendered
            .last()
            .expect("last line")
            .to_string()
            .ends_with("...")
    );
    assert!(rendered[4..].iter().all(|line| line.width() <= 20));
}

#[test]
fn agent_status_does_not_claim_started_file_changes_are_complete() {
    use codex_app_server_protocol::ItemStartedNotification;
    use codex_app_server_protocol::PatchApplyStatus;

    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.push_notification(ServerNotification::ItemStarted(ItemStartedNotification {
        item: ThreadItem::FileChange {
            id: "patch".to_string(),
            changes: Vec::new(),
            status: PatchApplyStatus::InProgress,
        },
        thread_id: "thread-child".to_string(),
        turn_id: "turn-1".to_string(),
        started_at_ms: 0,
    }));
    let preview = AgentStatusThreadPreview::from_store("/root/reviewer".to_string(), &store);
    assert_eq!(preview.activity, vec!["File changes: 0"]);
}
