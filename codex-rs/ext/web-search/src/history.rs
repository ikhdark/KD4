use codex_api::SearchInput;
use codex_conversation::event_mapping::is_contextual_user_message;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
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
    // Non-input user items parsed as hook prompts consist only of contextual
    // fragments too. Classify without constructing the UI turn item.
    matches!(item, ResponseItem::Message { role, content, .. }
        if role == USER_ROLE
            && content.iter().any(|item| matches!(item, ContentItem::InputText { .. }))
            && !is_contextual_user_message(item))
}

fn bounded_message(item: &ResponseItem, budget: usize) -> Option<ResponseItem> {
    match item {
        ResponseItem::Message {
            role,
            content,
            phase,
            internal_chat_message_metadata_passthrough,
            ..
        } if role == ASSISTANT_ROLE || is_visible_user_text(item) => bounded_text_message(
            role,
            phase.as_ref(),
            internal_chat_message_metadata_passthrough,
            content.iter().filter_map(|part| match part {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    Some(text.as_str())
                }
                _ => None,
            }),
            budget,
        ),
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
            let parts = ["Agent message from ", author.as_str(), ":\n"]
                .into_iter()
                .chain(
                    content
                        .iter()
                        .filter_map(|part| match part {
                            AgentMessageInputContent::InputText { text } => {
                                Some([text.as_str(), "\n"])
                            }
                            AgentMessageInputContent::EncryptedContent { .. } => None,
                        })
                        .flatten(),
                );
            bounded_text_message(
                ASSISTANT_ROLE,
                None,
                internal_chat_message_metadata_passthrough,
                parts,
                budget,
            )
        }
        _ => None,
    }
}

fn bounded_text_message<'a>(
    role: &str,
    phase: Option<&MessagePhase>,
    metadata: &Option<InternalChatMessageMetadataPassthrough>,
    parts: impl Iterator<Item = &'a str>,
    budget: usize,
) -> Option<ResponseItem> {
    if metadata
        .as_ref()
        .and_then(|metadata| metadata.turn_id.as_ref())
        .is_some_and(|turn_id| turn_id.len() > budget)
        || serde_json::to_vec(metadata).ok()?.len() > budget
    {
        return None;
    }
    let make_message = |content| ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content,
        phase: phase.cloned(),
        internal_chat_message_metadata_passthrough: metadata.clone(),
    };
    let make_part = |text| {
        if role == USER_ROLE {
            ContentItem::InputText { text }
        } else {
            ContentItem::OutputText { text }
        }
    };

    // Reserve one byte for the separator accounted for by recent_input.
    let envelope = serde_json::to_vec(&make_message(Vec::new())).ok()?.len();
    let content_budget = budget.checked_sub(envelope + 1)?;
    let empty_part_size = serde_json::to_vec(&make_part(String::new())).ok()?.len();
    let max_parts = (content_budget + 1) / (empty_part_size + 1);
    let marker_size = serde_json::to_vec(OMITTED).ok()?.len() - 2;

    // Store only the prefix that could fit, even if the input has arbitrarily
    // many empty or filtered parts. Count raw text once to preserve the old
    // cutoffs, but no candidate should have to walk the rest of the input.
    let mut prefix = Vec::new();
    let mut total_text_bytes = 0usize;
    for part in parts {
        if prefix.len() < max_parts {
            prefix.push(part);
        }
        total_text_bytes = total_text_bytes.saturating_add(part.len()).min(budget + 1);
        if total_text_bytes > budget {
            break;
        }
    }

    let fit = |limit: usize| {
        let mut remaining = limit;
        let mut available = if limit < total_text_bytes {
            content_budget.checked_sub(marker_size)?
        } else {
            content_budget
        };
        let mut slices = Vec::new();
        for part in &prefix {
            if remaining == 0 {
                break;
            }
            let overhead = empty_part_size + usize::from(!slices.is_empty());
            available = available.checked_sub(overhead)?;
            let end = part.floor_char_boundary(remaining.min(part.len()));
            let text = &part[..end];
            // The empty part already includes the string's surrounding quotes.
            // Check the raw lower bound before sizing the escaped JSON text.
            if end > available {
                return None;
            }
            let escaped = serde_json::to_vec(text).ok()?.len() - 2;
            available = available.checked_sub(escaped)?;
            remaining = if end < part.len() { 0 } else { remaining - end };
            slices.push(text);
        }
        // If the cutoff wasn't reached, a part beyond the stored prefix would
        // be needed; its overhead alone makes the candidate too large.
        (remaining == 0).then_some((slices, limit < total_text_bytes))
    };

    let complete = (total_text_bytes <= budget)
        .then(|| fit(total_text_bytes))
        .flatten();
    let (slices, omitted) = complete.or_else(|| {
        let mut low = 0;
        let mut high = budget.min(total_text_bytes.saturating_sub(1));
        let mut best = None;
        while low < high {
            let mid = low.midpoint(high) + 1;
            if let Some(candidate) = fit(mid) {
                low = mid;
                best = Some(candidate);
            } else {
                high = mid - 1;
            }
        }
        best
    })?;
    let last = slices.len().saturating_sub(1);
    let content = slices
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let mut text = part.to_string();
            if omitted && index == last {
                text.push_str(OMITTED);
            }
            make_part(text)
        })
        .collect();
    Some(make_message(content))
}
