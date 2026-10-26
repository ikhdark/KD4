mod json;
mod truncate;

use sha1::Digest;
use sha1::Sha1;

pub use json::to_ascii_json_string;
pub use truncate::TokenCountEstimate;
pub use truncate::approx_bytes_for_tokens;
pub use truncate::approx_token_count;
pub use truncate::approx_token_count_exceeds;
pub use truncate::approx_tokens_from_byte_count;
pub use truncate::truncate_middle_chars;
pub use truncate::truncate_middle_chars_from_parts;
pub use truncate::truncate_middle_with_token_budget;

/// Normalize CRLF and bare CR, retaining the input allocation when unchanged.
pub fn normalize_newlines<'a>(
    text: impl Into<std::borrow::Cow<'a, str>>,
) -> std::borrow::Cow<'a, str> {
    let text = text.into();
    let Some(first) = text.find('\r') else {
        return text;
    };
    let mut normalized = String::with_capacity(text.len());
    normalized.push_str(&text[..first]);
    let mut chars = text[first..].chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            normalized.push('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            normalized.push(ch);
        }
    }
    std::borrow::Cow::Owned(normalized)
}

/// Format XML text directly into its enclosing output without an escaped copy.
pub fn xml_text(text: &str) -> impl std::fmt::Display + '_ {
    EscapedText {
        text,
        kind: EscapeKind::XmlText,
    }
}

/// Format a double-quoted XML attribute, including whitespace character references.
pub fn xml_attribute(text: &str) -> impl std::fmt::Display + '_ {
    EscapedText {
        text,
        kind: EscapeKind::XmlAttribute,
    }
}

/// Format one RFC 6901 JSON pointer segment without an intermediate string.
pub fn json_pointer_segment(text: &str) -> impl std::fmt::Display + '_ {
    EscapedText {
        text,
        kind: EscapeKind::JsonPointer,
    }
}

enum EscapeKind {
    XmlText,
    XmlAttribute,
    JsonPointer,
}
struct EscapedText<'a> {
    text: &'a str,
    kind: EscapeKind,
}

impl std::fmt::Display for EscapedText<'_> {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut start = 0;
        for (index, byte) in self.text.bytes().enumerate() {
            let replacement = match (&self.kind, byte) {
                (EscapeKind::XmlText | EscapeKind::XmlAttribute, b'&') => "&amp;",
                (EscapeKind::XmlText | EscapeKind::XmlAttribute, b'<') => "&lt;",
                (EscapeKind::XmlText | EscapeKind::XmlAttribute, b'>') => "&gt;",
                (EscapeKind::XmlAttribute, b'"') => "&quot;",
                (EscapeKind::XmlAttribute, b'\r') => "&#13;",
                (EscapeKind::XmlAttribute, b'\n') => "&#10;",
                (EscapeKind::XmlAttribute, b'\t') => "&#9;",
                (EscapeKind::JsonPointer, b'~') => "~0",
                (EscapeKind::JsonPointer, b'/') => "~1",
                _ => continue,
            };
            output.write_str(&self.text[start..index])?;
            output.write_str(replacement)?;
            start = index + 1;
        }
        output.write_str(&self.text[start..])
    }
}

/// Return the lowercase SHA-1 digest for `bytes`.
pub fn sha1_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha1::digest(bytes.as_ref()))
}

/// Sanitize a tag value to comply with metric tag validation rules:
/// only ASCII alphanumeric, '.', '_', '-', and '/' are allowed.
pub fn sanitize_metric_tag_value(value: &str) -> String {
    const MAX_LEN: usize = 256;
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    let trimmed = trimmed[..trimmed.len().min(MAX_LEN)].trim_matches('_');
    if !trimmed.bytes().any(|byte| byte.is_ascii_alphanumeric()) {
        "unspecified".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Convert a markdown-style `#L..` location suffix into a terminal-friendly
/// `:line[:column][-line[:column]]` suffix.
pub fn normalize_markdown_hash_location_suffix(suffix: &str) -> Option<String> {
    let fragment = suffix.strip_prefix('#')?;
    let (start, end) = match fragment.split_once('-') {
        Some((start, end)) => (start, Some(end)),
        None => (fragment, None),
    };
    let (start_line, start_column) = parse_markdown_hash_location_point(start)?;
    let mut normalized = String::from(":");
    normalized.push_str(start_line);
    if let Some(column) = start_column {
        normalized.push(':');
        normalized.push_str(column);
    }
    if let Some(end) = end {
        let (end_line, end_column) = parse_markdown_hash_location_point(end)?;
        normalized.push('-');
        normalized.push_str(end_line);
        if let Some(column) = end_column {
            normalized.push(':');
            normalized.push_str(column);
        }
    }
    Some(normalized)
}

fn parse_markdown_hash_location_point(point: &str) -> Option<(&str, Option<&str>)> {
    let point = point.strip_prefix('L')?;
    let (line, column) = match point.split_once('C') {
        Some((line, column)) => (line, Some(column)),
        None => (point, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(line) && column.is_none_or(digits)).then_some((line, column))
}

