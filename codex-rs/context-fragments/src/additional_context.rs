use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_tokens_from_byte_count;

use crate::ContextualUserFragment;

const MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS: usize = 1_000;
const APPLICATION_CONTEXT_KIND: &str = "application";
const APPLICATION_CONTEXT_TAG: &str = "application_context";
const EXTERNAL_CONTEXT_KIND: &str = "untrusted";
const EXTERNAL_CONTEXT_TAG: &str = "external_context";
const LEGACY_ADDITIONAL_CONTEXT_END_MARKER_SUFFIX: &str = ">";
const LEGACY_ADDITIONAL_CONTEXT_START_MARKER_PREFIX: &str = "<external_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalContextUserFragment {
    key: String,
    value: String,
}

impl AdditionalContextUserFragment {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

impl ContextualUserFragment for AdditionalContextUserFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("", "")
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn matches_text(text: &str) -> bool {
        let trimmed = text.trim();
        matches_explicit_context(trimmed, EXTERNAL_CONTEXT_TAG, EXTERNAL_CONTEXT_KIND)
            || matches_legacy_external_context(trimmed)
    }

    fn body(&self) -> String {
        additional_context_body(
            EXTERNAL_CONTEXT_TAG,
            EXTERNAL_CONTEXT_KIND,
            &self.key,
            &self.value,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalContextDeveloperFragment {
    key: String,
    value: String,
}

impl AdditionalContextDeveloperFragment {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

impl ContextualUserFragment for AdditionalContextDeveloperFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn matches_text(text: &str) -> bool {
        matches_explicit_context(
            text.trim(),
            APPLICATION_CONTEXT_TAG,
            APPLICATION_CONTEXT_KIND,
        )
    }

    fn body(&self) -> String {
        additional_context_body(
            APPLICATION_CONTEXT_TAG,
            APPLICATION_CONTEXT_KIND,
            &self.key,
            &self.value,
        )
    }
}

fn matches_explicit_context(trimmed: &str, tag: &str, kind: &str) -> bool {
    let opening_prefix = format!("<{tag}");
    let Some(opening_rest) = trimmed.strip_prefix(&opening_prefix) else {
        return false;
    };
    if !opening_rest
        .chars()
        .next()
        .is_some_and(|ch| ch == ' ' || ch == '>')
    {
        return false;
    }

    let closing_tag = format!("</{tag}>");
    if !trimmed.ends_with(&closing_tag) {
        return false;
    }

    let Some((opening_tag, _)) = trimmed.split_once('>') else {
        return false;
    };
    opening_tag.contains(" source=\"") && opening_tag.contains(&format!(" kind=\"{kind}\""))
}

fn matches_legacy_external_context(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(LEGACY_ADDITIONAL_CONTEXT_START_MARKER_PREFIX) else {
        return false;
    };
    let Some((key, value_and_close)) = rest.split_once(LEGACY_ADDITIONAL_CONTEXT_END_MARKER_SUFFIX)
    else {
        return false;
    };

    value_and_close.ends_with(&format!("</external_{key}>"))
}

fn additional_context_body(tag: &str, kind: &str, key: &str, value: &str) -> String {
    format!(
        "<{tag} source=\"{}\" kind=\"{kind}\">\n{}\n</{tag}>",
        escape_attr_value(key),
        escape_text_with_token_budget(value)
    )
}

fn escape_text_with_token_budget(value: &str) -> String {
    let max_bytes = approx_bytes_for_tokens(MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS);
    let escaped_bytes = value.chars().fold(0usize, |total, ch| {
        total.saturating_add(escaped_text_char_len(ch))
    });
    if escaped_bytes <= max_bytes {
        let mut escaped = String::with_capacity(escaped_bytes);
        push_escaped_text(&mut escaped, value);
        return escaped;
    }

    let mut omitted_tokens = approx_tokens_from_byte_count(escaped_bytes.saturating_sub(max_bytes));
    for _ in 0..4 {
        let marker = format!("…{omitted_tokens} tokens truncated…");
        let bounds = escaped_text_bounds(value, max_bytes.saturating_sub(marker.len()));
        let next_omitted_tokens =
            approx_tokens_from_byte_count(escaped_bytes.saturating_sub(bounds.2));
        if next_omitted_tokens == omitted_tokens {
            break;
        }
        omitted_tokens = next_omitted_tokens;
    }

    let marker = format!("…{omitted_tokens} tokens truncated…");
    let bounds = escaped_text_bounds(value, max_bytes.saturating_sub(marker.len()));
    let (prefix_end, suffix_start, kept_escaped_bytes) = bounds;
    let mut escaped = String::with_capacity(
        kept_escaped_bytes
            .saturating_add(marker.len())
            .min(max_bytes),
    );
    push_escaped_text(&mut escaped, &value[..prefix_end]);
    escaped.push_str(&marker);
    push_escaped_text(&mut escaped, &value[suffix_start..]);
    debug_assert!(escaped.len() <= max_bytes);
    escaped
}

fn escaped_text_bounds(value: &str, content_budget: usize) -> (usize, usize, usize) {
    let prefix_budget = content_budget / 2;
    let suffix_budget = content_budget - prefix_budget;
    let mut prefix_end = 0;
    let mut prefix_bytes = 0usize;
    for (idx, ch) in value.char_indices() {
        let char_len = escaped_text_char_len(ch);
        if prefix_bytes.saturating_add(char_len) > prefix_budget {
            break;
        }
        prefix_bytes += char_len;
        prefix_end = idx + ch.len_utf8();
    }

    let mut suffix_start = value.len();
    let mut suffix_bytes = 0usize;
    for (idx, ch) in value.char_indices().rev() {
        if idx < prefix_end {
            break;
        }
        let char_len = escaped_text_char_len(ch);
        if suffix_bytes.saturating_add(char_len) > suffix_budget {
            break;
        }
        suffix_bytes += char_len;
        suffix_start = idx;
    }

    (
        prefix_end,
        suffix_start,
        prefix_bytes.saturating_add(suffix_bytes),
    )
}

fn escaped_text_char_len(ch: char) -> usize {
    match ch {
        '&' => "&amp;".len(),
        '<' => "&lt;".len(),
        '>' => "&gt;".len(),
        _ => ch.len_utf8(),
    }
}

fn push_escaped_text(escaped: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
}

fn escape_attr_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}
