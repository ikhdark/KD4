//! Historical decision receipts are metadata, never new user input.
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) struct AcceptedUserDecision;

impl AcceptedUserDecision {
    pub(super) fn is_snapshot(item: &ResponseItem) -> bool {
        matches!(item, ResponseItem::Message { role, content, .. } if role == "developer"
            && matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text.starts_with("<codex_accepted_user_decision>")
                    && text.ends_with("</codex_accepted_user_decision>")))
    }
}
