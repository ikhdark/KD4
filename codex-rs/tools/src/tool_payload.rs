use std::borrow::Cow;

use codex_protocol::models::SearchToolCallParams;

/// Canonical payload shapes accepted by model-visible tool runtimes.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolPayload {
    Function { arguments: String },
    ToolSearch { arguments: SearchToolCallParams },
    Custom { input: String },
}

impl ToolPayload {
    pub fn log_payload(&self) -> Cow<'_, str> {
        match self {
            ToolPayload::Function { arguments } => Cow::Borrowed(arguments),
            ToolPayload::ToolSearch { arguments } => Cow::Borrowed(&arguments.query),
            ToolPayload::Custom { input } => Cow::Borrowed(input),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_search_log_payload_borrows_query() {
        let payload = ToolPayload::ToolSearch {
            arguments: SearchToolCallParams {
                query: "find the owner".to_string(),
                limit: None,
            },
        };

        assert!(matches!(
            payload.log_payload(),
            Cow::Borrowed("find the owner")
        ));
    }
}
