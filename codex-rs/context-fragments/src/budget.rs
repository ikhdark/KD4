use crate::ContextualUserFragment;
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
        if text.len() > self.remaining_bytes {
            return false;
        }
        self.remaining_bytes -= text.len();
        true
    }

    /// Admit text, truncating the final admitted item within the remaining budget.
    pub fn take(&mut self, text: &str) -> Option<String> {
        self.take_up_to(text, self.remaining_bytes)
    }

    /// Admit text up to an item-specific byte cap while preserving the unused
    /// aggregate budget for later fragments.
    pub fn take_up_to(&mut self, text: &str, max_bytes: usize) -> Option<String> {
        let budget = self.remaining_bytes.min(max_bytes);
        if budget == 0 {
            return None;
        }
        if text.len() <= budget {
            self.remaining_bytes -= text.len();
            return Some(text.to_string());
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
        self.remaining_bytes = self.remaining_bytes.saturating_sub(admitted.len());
        Some(admitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_cap_preserves_aggregate_budget_for_later_fragments() {
        let mut budget = ModelContextBudget::new(10);
        let initial_bytes = budget.remaining_bytes();

        let first = budget
            .take_up_to(&"x".repeat(100), 12)
            .expect("capped fragment");
        assert!(first.len() <= 12);
        assert_eq!(budget.remaining_bytes(), initial_bytes - first.len());
        assert_eq!(budget.take("later"), Some("later".to_string()));
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

    fn body(&self) -> String {
        self.text.clone()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
}
