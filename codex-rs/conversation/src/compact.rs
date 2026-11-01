//! Recognition of persisted compaction summaries; scheduling stays in core.
use codex_protocol::models::{ContentItem, ResponseItem};
use codex_prompts::SUMMARY_PREFIX;
pub const COMPACTION_SUMMARY_ITEM_ID_PREFIX: &str = "msg_compaction_summary_";

pub fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

pub fn compaction_summary_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message {
        id: Some(id),
        role,
        content,
        ..
    } = item
    else {
        return None;
    };
    if role != "user" || !id.as_str().starts_with(COMPACTION_SUMMARY_ITEM_ID_PREFIX) {
        return None;
    }
    content.iter().find_map(|content| match content {
        ContentItem::InputText { text } if is_summary_message(text) => Some(text.as_str()),
        _ => None,
    })
}

pub fn is_compaction_summary_item(item: &ResponseItem) -> bool {
    compaction_summary_text(item).is_some()
}
