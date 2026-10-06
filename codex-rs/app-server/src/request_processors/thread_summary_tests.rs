use super::*;

use anyhow::Result;
use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn started_notification_preserves_response_history_without_copying_it() -> Result<()> {
    use codex_app_server_protocol::TurnItemsView;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;

    for include_history in [false, true] {
        let cwd = test_path_buf("/tmp").abs();
        let summary = ConversationSummary {
            conversation_id: ThreadId::from_string("3f941c35-29b3-493b-b0a4-e25800d9aeb0")?,
            timestamp: None,
            updated_at: None,
            path: PathBuf::new(),
            preview: "preview".to_string(),
            model_provider: "test-provider".to_string(),
            cwd: cwd.to_path_buf(),
            cli_version: "test".to_string(),
            source: codex_protocol::protocol::SessionSource::VSCode,
            git_info: None,
        };
        let mut thread = summary_to_thread(summary, &cwd);
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
fn extract_conversation_summary_prefers_plain_user_messages() -> Result<()> {
    let conversation_id = ThreadId::from_string("3f941c35-29b3-493b-b0a4-e25800d9aeb0")?;
    let timestamp = Some("2025-09-05T16:53:11.850Z".to_string());
    let path = PathBuf::from("rollout.jsonl");

    let head = vec![
        json!({
            "session_id": conversation_id.to_string(),
            "id": conversation_id.to_string(),
            "timestamp": timestamp,
            "cwd": "/",
            "originator": "codex",
            "cli_version": "0.0.0",
            "model_provider": "test-provider"
        }),
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

    let items = head[1..]
        .iter()
        .map(|item| serde_json::from_value(item.clone()).map(RolloutItem::ResponseItem))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(
        super::super::thread_processor::preview_from_rollout_items(&items),
        "Count to 5"
    );

    let session_meta = serde_json::from_value::<SessionMeta>(head[0].clone())?;

    let summary = extract_conversation_summary(
        path.clone(),
        &head,
        &session_meta,
        /*git*/ None,
        "test-provider",
        timestamp.clone(),
    )
    .expect("summary");

    let expected = ConversationSummary {
        conversation_id,
        timestamp: timestamp.clone(),
        updated_at: timestamp,
        path,
        preview: "Count to 5".to_string(),
        model_provider: "test-provider".to_string(),
        cwd: PathBuf::from("/"),
        cli_version: "0.0.0".to_string(),
        source: codex_protocol::protocol::SessionSource::VSCode,
        git_info: None,
    };

    assert_eq!(summary, expected);
    Ok(())
}
