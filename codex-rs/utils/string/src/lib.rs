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
    format!("{:x}", Sha1::digest(bytes.as_ref()))
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
    if trimmed.is_empty() || trimmed.chars().all(|ch| !ch.is_ascii_alphanumeric()) {
        return "unspecified".to_string();
    }
    if trimmed.len() <= MAX_LEN {
        trimmed.to_string()
    } else {
        trimmed[..MAX_LEN].to_string()
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

#[cfg(test)]
mod tests {
    #[test]
    fn newline_normalization_preserves_plain_storage_and_mixed_endings() {
        use std::borrow::Cow;
        assert!(matches!(
            super::normalize_newlines("plain\ntext"),
            Cow::Borrowed(_)
        ));
        let owned = "plain text".to_string();
        let ptr = owned.as_ptr();
        assert_eq!(super::normalize_newlines(owned).as_ptr(), ptr);
        assert_eq!(
            super::normalize_newlines("é\r\n\r中\n\r\r\n"),
            "é\n\n中\n\n\n"
        );
    }

    #[test]
    fn escaping_preserves_unicode_and_literal_entities() {
        assert_eq!(
            super::xml_text("é&<>&amp;\"\n").to_string(),
            "é&amp;&lt;&gt;&amp;amp;\"\n"
        );
        assert_eq!(
            super::xml_attribute("a\"\r\n\t&<中>").to_string(),
            "a&quot;&#13;&#10;&#9;&amp;&lt;中&gt;"
        );
        assert_eq!(
            super::json_pointer_segment("中~/~0").to_string(),
            "中~0~1~00"
        );
    }

    #[test]
    fn cached_estimates_match_whitespace_joined_text_without_rounding_parts() {
        use super::TokenCountEstimate;
        let parts = ["a", "bc", "!?", "中😀", "  ", "long_word123", ""];
        for left in parts {
            for right in parts {
                for separator in ["\n", "\n\n", " \t"] {
                    assert_eq!(
                        TokenCountEstimate::new(left)
                            .then(TokenCountEstimate::new(right), separator.len())
                            .tokens(),
                        super::approx_token_count(&format!("{left}{separator}{right}")),
                    );
                }
            }
        }
        assert_eq!(
            TokenCountEstimate::new("a")
                .then(TokenCountEstimate::new("b"), 1)
                .tokens(),
            2
        );
    }

    use super::normalize_markdown_hash_location_suffix;
    use super::sanitize_metric_tag_value;
    use super::sha1_hex;
    use pretty_assertions::assert_eq;

    #[test]
    fn sanitize_metric_tag_value_trims_and_fills_unspecified() {
        let msg = "///";
        assert_eq!(sanitize_metric_tag_value(msg), "unspecified");
    }

    #[test]
    fn sanitize_metric_tag_value_replaces_invalid_chars() {
        let msg = "bad value!";
        assert_eq!(sanitize_metric_tag_value(msg), "bad_value");
    }

    #[test]
    fn sha1_hex_uses_lowercase_full_width_encoding() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn normalize_markdown_hash_location_suffix_converts_single_location() {
        assert_eq!(
            normalize_markdown_hash_location_suffix("#L74C3"),
            Some(":74:3".to_string())
        );
    }

    #[test]
    fn normalize_markdown_hash_location_suffix_converts_ranges() {
        assert_eq!(
            normalize_markdown_hash_location_suffix("#L74C3-L76C9"),
            Some(":74:3-76:9".to_string())
        );
    }

    #[test]
    fn normalize_markdown_hash_location_suffix_rejects_malformed_components() {
        for suffix in [
            "#L", "#Labc", "#L2C", "#LC3", "#L２", "#L2C٣", "#L1-L", "#L1C2C3",
        ] {
            assert_eq!(
                normalize_markdown_hash_location_suffix(suffix),
                None,
                "{suffix}"
            );
        }
    }
}
