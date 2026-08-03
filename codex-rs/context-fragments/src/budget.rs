use crate::ContextualUserFragment;
use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::take_bytes_at_char_boundary;

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
        if text.len() > self.remaining_bytes {
            return false;
        }
        self.remaining_bytes -= text.len();
        true
    }

    /// Admit text, truncating the final admitted item within the remaining budget.
    pub fn take(&mut self, text: &str) -> Option<String> {
        if self.remaining_bytes == 0 {
            return None;
        }
        if text.len() <= self.remaining_bytes {
            self.remaining_bytes -= text.len();
            return Some(text.to_string());
        }

        let budget = std::mem::take(&mut self.remaining_bytes);
        if budget <= TRUNCATION_MARKER.len() {
            return Some(take_bytes_at_char_boundary(text, budget).to_string());
        }

        let text_budget = budget - TRUNCATION_MARKER.len();
        let prefix = take_bytes_at_char_boundary(text, text_budget.div_ceil(2));
        let suffix = take_suffix_bytes_at_char_boundary(text, text_budget / 2);
        Some(format!("{prefix}{TRUNCATION_MARKER}{suffix}"))
    }
}

fn take_suffix_bytes_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
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

    fn body(&self) -> String {
        self.text.clone()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
}
