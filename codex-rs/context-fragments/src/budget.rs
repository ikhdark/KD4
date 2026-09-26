use crate::ContextualUserFragment;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_utils_string::approx_bytes_for_tokens;

/// Maximum approximate token budget for one aggregate model-context contribution.
pub const MAX_MODEL_CONTEXT_TOKENS: usize = 10_000;

const TRUNCATION_MARKER: &str = "\n[... context truncated ...]\n";

/// A shared hard budget for a collection of model-visible context fragments.
#[derive(Debug, Clone)]
pub struct ModelContextBudget {
    remaining_bytes: usize,
}

impl Default for ModelContextBudget {
    fn default() -> Self {
        Self::new(MAX_MODEL_CONTEXT_TOKENS)
    }
}

impl ModelContextBudget {
    pub fn new(max_tokens: usize) -> Self {
        Self {
            remaining_bytes: approx_bytes_for_tokens(max_tokens),
        }
    }

    pub fn remaining_bytes(&self) -> usize {
        self.remaining_bytes
    }

    /// Admit a whole item. Use this for structured fragments that must not be split.
    pub fn try_take(&mut self, text: &str) -> bool {
        self.try_take_bytes(text.len())
    }

    /// Charge an already-measured model-visible cost, including message envelopes.
    pub fn try_take_bytes(&mut self, bytes: usize) -> bool {
        if bytes > self.remaining_bytes {
            return false;
        }
        self.remaining_bytes -= bytes;
        true
    }

    /// Admit text, truncating the final admitted item within the remaining budget.
    pub fn take<'a>(&mut self, text: &'a str) -> Option<std::borrow::Cow<'a, str>> {
        self.take_up_to(text, self.remaining_bytes)
    }

    /// Admit text up to an item-specific byte cap while preserving the unused
    /// aggregate budget for later fragments.
    pub fn take_up_to<'a>(
        &mut self,
        text: &'a str,
        max_bytes: usize,
    ) -> Option<std::borrow::Cow<'a, str>> {
        let budget = self.remaining_bytes.min(max_bytes);
        if budget == 0 {
            return None;
        }
        if text.len() <= budget {
            self.remaining_bytes -= text.len();
            return Some(std::borrow::Cow::Borrowed(text));
        }

        let admitted = if budget <= TRUNCATION_MARKER.len() {
            text[..text.floor_char_boundary(budget)].to_string()
        } else {
            let text_budget = budget - TRUNCATION_MARKER.len();
            let prefix = &text[..text.floor_char_boundary(text_budget.div_ceil(2))];
            let suffix_start = text.ceil_char_boundary(text.len().saturating_sub(text_budget / 2));
            let suffix = &text[suffix_start..];
            format!("{prefix}{TRUNCATION_MARKER}{suffix}")
        };
        if admitted.is_empty() {
            return None;
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(admitted.len());
        Some(std::borrow::Cow::Owned(admitted))
    }

    /// Admit a rendered fragment, truncating only its body so its start and end
    /// markers survive.
    ///
    /// Truncating rendered text can cut a marker, and a fragment missing either
    /// marker no longer matches its registration, so a user-role fragment would
    /// be treated as user input. When the markers leave no room for body text,
    /// the fragment is omitted without charging the budget.
    pub fn take_fragment<F>(&mut self, fragment: &F) -> Option<String>
    where
        F: ContextualUserFragment + ?Sized,
    {
        let (start_marker, end_marker) = fragment.markers();
        let markers_len = start_marker.len().saturating_add(end_marker.len());
        let body = fragment.body();
        let body = if markers_len.saturating_add(body.len()) <= self.remaining_bytes {
            body
        } else {
            let mut body_budget = Self {
                remaining_bytes: self.remaining_bytes.checked_sub(markers_len)?,
            };
            std::borrow::Cow::Owned(body_budget.take(&body)?.into_owned())
        };
        let rendered = format!("{start_marker}{body}{end_marker}");
        self.remaining_bytes -= rendered.len();
        Some(rendered)
    }
}

/// An already-rendered fragment used after aggregate budget enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedContextFragment {
    role: &'static str,
    text: String,
}

impl RenderedContextFragment {
    pub fn new(role: &'static str, text: String) -> Self {
        Self { role, text }
    }
}

impl ContextualUserFragment for RenderedContextFragment {
    fn role(&self) -> &'static str {
        self.role
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.text)
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn into(self) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: self.role.to_string(),
            content: vec![ContentItem::InputText { text: self.text }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn into_boxed_response_item(self: Box<Self>) -> ResponseItem {
        ContextualUserFragment::into(*self)
    }

    fn into_response_input_item(self) -> ResponseInputItem {
        ResponseInputItem::Message {
            role: self.role.to_string(),
            content: vec![ContentItem::InputText { text: self.text }],
            phase: None,
        }
    }
}
