mod json;
mod truncate;

use sha1::Digest;
use sha1::Sha1;

pub use json::to_ascii_json_string;
pub use truncate::approx_bytes_for_tokens;
pub use truncate::approx_token_count;
pub use truncate::approx_tokens_from_byte_count;
pub use truncate::truncate_middle_chars;
pub use truncate::truncate_middle_with_token_budget;

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
    match point.split_once('C') {
        Some((line, column)) => Some((line, Some(column))),
        None => Some((point, None)),
    }
}

#[cfg(test)]
#[allow(warnings, clippy::all)]
mod tests {
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
}
