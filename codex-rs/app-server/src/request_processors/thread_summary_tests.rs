use super::*;

use anyhow::Result;
use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn started_notification_omits_turns_and_leaves_response_history_in_place() -> Result<()> {
    use codex_app_server_protocol::TurnItemsView;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;

    for include_history in [false, true] {
        let cwd = test_path_buf("/tmp").abs();
        let thread_id = ThreadId::from_string("3f941c35-29b3-493b-b0a4-e25800d9aeb0")?;
        let mut thread = Thread {
            project_id: None,
            id: thread_id.to_string(),
            extra: None,
            session_id: thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: "preview".to_string(),
            ephemeral: false,
            history_mode: Default::default(),
            model_provider: "test-provider".to_string(),
            created_at: 0,
            updated_at: 0,
            recency_at: None,
            status: ThreadStatus::NotLoaded,
            path: None,
            cwd,
            cli_version: "test".to_string(),
            source: codex_app_server_protocol::SessionSource::VsCode,
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
            turns: Vec::new(),
        };
        if include_history {
            thread.turns.push(Turn {
                id: "turn-1".to_string(),
                items: vec![ThreadItem::AgentMessage {
                    id: "item-1".to_string(),
                    text: "x".repeat(1024 * 1024),
                    phase: None,
                }],
                items_view: TurnItemsView::Full,
                status: TurnStatus::Completed,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            });
        }
        let expected_response = thread.clone();
        let turns_pointer = thread.turns.as_ptr();
        let notification = thread_started_notification(&mut thread);

        assert_eq!(thread, expected_response);
        assert_eq!(thread.turns.as_ptr(), turns_pointer);
        let mut expected_notification = expected_response;
        expected_notification.turns.clear();
        assert_eq!(notification.thread, expected_notification);
    }
    Ok(())
}

#[test]
fn rollout_preview_prefers_plain_user_messages() -> Result<()> {
    let head = [
        json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "# AGENTS.md instructions for project\n\n<INSTRUCTIONS>\n<AGENTS.md contents>\n</INSTRUCTIONS>".to_string(),
            }],
        }),
        json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("<prior context> {USER_MESSAGE_BEGIN}Count to 5"),
            }],
        }),
    ];

    let items = head
        .iter()
        .map(|item| serde_json::from_value(item.clone()).map(RolloutItem::ResponseItem))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(
        super::super::thread_processor::preview_from_rollout_items(&items),
        "Count to 5"
    );

    Ok(())
}
