use codex_utils_string::truncate_middle_with_token_budget;

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
    let value = truncate_middle_with_token_budget(value, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS).0;
    format!(
        "<{tag} source=\"{}\" kind=\"{kind}\">\n{}\n</{tag}>",
        escape_attr_value(key),
        escape_text(&value)
    )
}

fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(ch),
        }
    }
    escaped
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
