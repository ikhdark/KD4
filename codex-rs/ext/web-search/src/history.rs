use codex_api::SearchInput;
use codex_core::parse_turn_item;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

const CONTEXT_BYTES: usize = 16_000;
const ASSISTANT_BYTES: usize = 4_000;
const OMITTED: &str = "\n[search context omitted]";
const ASSISTANT_ROLE: &str = "assistant";
const USER_ROLE: &str = "user";

/// Builds the conversation tail for standalone web search.
///
/// Select newest text first, then emit it chronologically. The serialized
/// projection is bounded across roles; executable commands remain independent.
pub(crate) fn recent_input(items: &[ResponseItem]) -> Option<SearchInput> {
    let mut users = items
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, item)| is_visible_user_text(item).then_some(index));
    let latest = users.next()?;
    let earliest = users.next().unwrap_or(latest);
    let mut messages = Vec::new();
    let mut remaining = CONTEXT_BYTES - 2;
    let mut assistant_remaining = ASSISTANT_BYTES;
    for item in items[earliest..=latest].iter().rev() {
        let assistant = matches!(item, ResponseItem::AgentMessage { .. })
            || matches!(item, ResponseItem::Message { role, .. } if role == ASSISTANT_ROLE);
        let allowance = if assistant {
            remaining.min(assistant_remaining)
        } else {
            remaining
        };
        if allowance <= OMITTED.len() {
            continue;
        }
        if let Some(message) = bounded_message(item, allowance) {
            let size = serde_json::to_vec(&message).ok()?.len() + 1;
            remaining -= size;
            if assistant {
                assistant_remaining = assistant_remaining.saturating_sub(size);
            }
            messages.push(message);
        }
    }
    messages.reverse();
    (!messages.is_empty()).then_some(SearchInput::Items(messages))
}

fn is_visible_user_text(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, content, .. }
        if role == USER_ROLE
            && content.iter().any(|item| matches!(item, ContentItem::InputText { .. }))
            && matches!(parse_turn_item(item), Some(TurnItem::UserMessage(_))))
}

fn bounded_message(item: &ResponseItem, budget: usize) -> Option<ResponseItem> {
    let (role, phase, metadata, parts) = match item {
        ResponseItem::Message {
            role,
            content,
            phase,
            internal_chat_message_metadata_passthrough,
            ..
        } if role == ASSISTANT_ROLE || is_visible_user_text(item) => {
            let parts = content
                .iter()
                .filter_map(|part| match part {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            (
                role.as_str(),
                phase.clone(),
                internal_chat_message_metadata_passthrough,
                parts,
            )
        }
        ResponseItem::AgentMessage {
            author,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } => {
            if content
                .iter()
                .any(|part| matches!(part, AgentMessageInputContent::EncryptedContent { .. }))
            {
                return None;
            }
            let mut parts = vec!["Agent message from ", author.as_str(), ":\n"];
            for part in content {
                if let AgentMessageInputContent::InputText { text } = part {
                    parts.push(text);
                    parts.push("\n");
                }
            }
            (
                ASSISTANT_ROLE,
                None,
                internal_chat_message_metadata_passthrough,
                parts,
            )
        }
        _ => return None,
    };
    if serde_json::to_vec(metadata).ok()?.len() > budget {
        return None;
    }
    let total_text_bytes = parts.iter().map(|part| part.len()).sum::<usize>();
    let make = |limit: usize| {
        let mut remaining = limit;
        let mut content = Vec::new();
        for part in &parts {
            if remaining == 0 {
                break;
            }
            let end = part.floor_char_boundary(remaining.min(part.len()));
            let text = part[..end].to_string();
            remaining -= end;
            if end < part.len() {
                remaining = 0;
            }
            content.push(if role == USER_ROLE {
                ContentItem::InputText { text }
            } else {
                ContentItem::OutputText { text }
            });
        }
        if limit < total_text_bytes
            && let Some(ContentItem::InputText { text } | ContentItem::OutputText { text }) =
                content.last_mut()
        {
            text.push_str(OMITTED);
        }
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content,
            phase: phase.clone(),
            internal_chat_message_metadata_passthrough: metadata.clone(),
        }
    };
    if total_text_bytes <= budget {
        let complete = make(total_text_bytes);
        if serde_json::to_vec(&complete).ok()?.len() < budget {
            return Some(complete);
        }
    }
    let mut low = 0;
    let mut high = budget.min(total_text_bytes.saturating_sub(1));
    let mut best = None;
    while low < high {
        let mid = low.midpoint(high) + 1;
        let message = make(mid);
        if serde_json::to_vec(&message).ok()?.len() < budget {
            low = mid;
            best = Some(message);
        } else {
            high = mid - 1;
        }
    }
    best
}
