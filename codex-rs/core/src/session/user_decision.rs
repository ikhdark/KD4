//! Native decision provenance is shared with plan/history projection. Legacy
//! text-only receipts remain metadata, never authenticated user input.
pub(super) use codex_conversation::user_decision::AcceptedUserDecision;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) fn is_legacy_snapshot(item: &ResponseItem) -> bool {
    !AcceptedUserDecision::is_native_snapshot(item) &&
        matches!(item, ResponseItem::Message { role, content, .. } if role == "developer"
            && matches!(content.as_slice(), [ContentItem::InputText { text }]
                if text.starts_with("<codex_accepted_user_decision>")
                    && text.ends_with("</codex_accepted_user_decision>")))
}
