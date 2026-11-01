use codex_protocol::models::ResponseItem;

pub fn raw_assistant_output_text_from_item(item: &ResponseItem) -> Option<String> {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let combined = content
            .iter()
            .filter_map(|ci| match ci {
                codex_protocol::models::ContentItem::OutputText { text }
                | codex_protocol::models::ContentItem::Refusal { refusal: text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        return Some(combined);
    }
    None
}
