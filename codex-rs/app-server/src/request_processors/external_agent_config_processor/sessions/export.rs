use super::ConversationMessage;
use super::ImportedExternalAgentSession;
use super::MessageRole;
use super::records::read_session_import;
use super::summarize_for_label;
use super::title::IMPORTED_SESSION_FALLBACK_TITLE;
use super::title::SessionTitleCandidates;
use super::title::fallback_title_from_user_message;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_utils_output_truncation::approx_tokens_from_byte_count_i64;
use std::io;
use std::path::Path;

const EXTERNAL_SESSION_IMPORTED_MARKER: &str = "<EXTERNAL SESSION IMPORTED>";

pub(crate) fn load_session_for_import_with_content_sha256(
    path: &Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> io::Result<Option<(ImportedExternalAgentSession, String)>> {
    let parsed = read_session_import(path, cancellation)?;
    let Some(cwd) = parsed.cwd else {
        return Ok(None);
    };
    let messages = parsed.messages;
    let first_user_message_text = messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.text.as_str());
    let first_user_message = first_user_message_text.map(summarize_for_label);
    let fallback_title = messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .find_map(|message| fallback_title_from_user_message(&message.text))
        .or_else(|| first_user_message_text.map(|_| IMPORTED_SESSION_FALLBACK_TITLE.to_string()));
    let title = SessionTitleCandidates {
        custom_title: parsed.custom_title,
        ai_title: parsed.ai_title,
        fallback_title,
    }
    .select();
    let rollout_items = rollout_items_from_messages(messages);
    if rollout_items.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        ImportedExternalAgentSession {
            cwd,
            title,
            first_user_message,
            rollout_items,
        },
        parsed.content_sha256,
    )))
}

fn rollout_items_from_messages(messages: Vec<ConversationMessage>) -> Vec<RolloutItem> {
    let mut items = Vec::new();
    let mut current_turn = None;
    let mut response_item_bytes = 0i64;
    let mut last_model_visible_tokens = 0i64;
    let mut user_turn_count = 0usize;
    let completed_at = messages.last().and_then(|message| message.timestamp);

    for message in messages {
        match message.role {
            MessageRole::User => {
                if let Some(turn_id) = current_turn.take() {
                    items.push(turn_complete_item(turn_id, /*completed_at*/ None));
                }
                user_turn_count += 1;
                let turn_id = format!("external-import-turn-{user_turn_count}");
                items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                    TurnStartedEvent {
                        turn_id: turn_id.clone(),
                        trace_id: None,
                        started_at: message.timestamp,
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                    },
                )));
                items.push(RolloutItem::EventMsg(EventMsg::UserMessage(
                    UserMessageEvent {
                        message: message.text.clone(),
                        ..Default::default()
                    },
                )));
                response_item_bytes =
                    response_item_bytes.saturating_add(message_byte_count(&message));
                items.push(RolloutItem::ResponseItem(response_item(message)));
                current_turn = Some(turn_id);
            }
            MessageRole::Assistant => {
                if current_turn.is_none() {
                    continue;
                }
                response_item_bytes =
                    response_item_bytes.saturating_add(message_byte_count(&message));
                last_model_visible_tokens = approx_tokens_from_byte_count_i64(response_item_bytes);
                items.push(RolloutItem::EventMsg(EventMsg::AgentMessage(
                    AgentMessageEvent {
                        message: message.text.clone(),
                        phase: None,
                    },
                )));
                items.push(RolloutItem::ResponseItem(response_item(message)));
            }
        }
    }

    if let Some(turn_id) = current_turn {
        items.push(external_session_imported_marker_item());
        items.push(token_count_item(last_model_visible_tokens));
        items.push(turn_complete_item(turn_id, completed_at));
    }

    items
}

fn external_session_imported_marker_item() -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
        message: EXTERNAL_SESSION_IMPORTED_MARKER.to_string(),
        phase: None,
    }))
}

fn response_item(message: ConversationMessage) -> ResponseItem {
    let content = match message.role {
        MessageRole::Assistant => ContentItem::OutputText { text: message.text },
        MessageRole::User => ContentItem::InputText { text: message.text },
    };
    ResponseItem::Message {
        id: None,
        role: match message.role {
            MessageRole::Assistant => "assistant".to_string(),
            MessageRole::User => "user".to_string(),
        },
        content: vec![content],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn message_byte_count(message: &ConversationMessage) -> i64 {
    i64::try_from(message.text.len()).unwrap_or(i64::MAX)
}

fn token_count_item(last_model_visible_tokens: i64) -> RolloutItem {
    // Imported text has no provider usage receipt. Keep its local context
    // estimate separate from measured consumption in the current protocol.
    let usage = TokenUsage {
        total_tokens: 0,
        ..TokenUsage::default()
    };
    RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
        info: Some(TokenUsageInfo {
            estimated_context_tokens: Some(last_model_visible_tokens),
            total_token_usage: usage.clone(),
            last_token_usage: usage,
            model_context_window: None,
        }),
        rate_limits: None,
    }))
}

fn turn_complete_item(turn_id: String, completed_at: Option<i64>) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        surfaced_result: None,
        turn_id,
        last_agent_message: None,
        error: None,
        completed_at,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    }))
}

