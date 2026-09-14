use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) fn capitalize_first(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => {
            let mut capitalized = first.to_uppercase().collect::<String>();
            capitalized.push_str(chars.as_str());
            capitalized
        }
        None => String::new(),
    }
}

/// Truncate a tool result to fit within the given height and width. If the text is valid JSON, we format it in a compact way before truncating.
#[expect(
    clippy::expect_used,
    reason = "Rows start nonempty and are never removed; a positive-width row contains a grapheme"
)]
pub(crate) fn format_and_truncate_tool_result(
    text: &str,
    max_lines: usize,
    line_width: usize,
) -> String {
    if max_lines == 0 || line_width == 0 {
        return String::new();
    }
    let formatted = format_json_compact(text);
    let text = formatted.as_deref().unwrap_or(text);
    let mut lines = vec![String::new()];
    let mut width = 0;
    let mut truncated = false;
    for grapheme in text.graphemes(true) {
        let newline = matches!(grapheme, "\n" | "\r\n" | "\r");
        let cells = UnicodeWidthStr::width(grapheme);
        if cells > line_width {
            truncated = true;
            break;
        }
        if newline || width + cells > line_width {
            if lines.len() == max_lines {
                truncated = true;
                break;
            }
            // Prefer a word boundary, but split long tokens only between graphemes.
            let last = lines.last_mut().expect("preview has a row");
            let carry = if !newline && !grapheme.chars().all(char::is_whitespace) {
                last.grapheme_indices(true)
                    .rev()
                    .find_map(|(index, grapheme)| (grapheme == " ").then_some(index))
                    .filter(|boundary| {
                        UnicodeWidthStr::width(&last[boundary + 1..]) + cells <= line_width
                    })
                    .map(|boundary| {
                        let carry = last[boundary + 1..].to_string();
                        last.truncate(boundary);
                        carry
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            width = UnicodeWidthStr::width(carry.as_str());
            lines.push(carry);
            if newline {
                continue;
            }
        }
        lines
            .last_mut()
            .expect("preview has a row")
            .push_str(grapheme);
        width += cells;
    }
    if truncated {
        let last = lines.last_mut().expect("preview has a row");
        while UnicodeWidthStr::width(last.as_str()) >= line_width {
            let (index, _) = last
                .grapheme_indices(true)
                .next_back()
                .expect("nonempty row");
            last.truncate(index);
        }
        last.push('…');
    }
    lines.join("\n")
}

/// Format JSON text in a compact single-line format with spaces for better Ratatui wrapping.
/// Ex: `{"a":"b",c:["d","e"]}` -> `{"a": "b", "c": ["d", "e"]}`
/// Returns the formatted JSON string if the input is valid JSON, otherwise returns None.
/// This is a little complicated, but it's necessary because Ratatui's wrapping is *very* limited
/// and can only do line breaks at whitespace. If we use the default serde_json format, we get lines
/// without spaces that Ratatui can't wrap nicely. If we use the serde_json pretty format as-is,
/// it's much too sparse and uses too many terminal rows.
/// Relevant issue: https://github.com/ratatui/ratatui/issues/293
pub(crate) fn format_json_compact(text: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let json_compact = json.to_string();

    // Add structural whitespace without expanding the whole payload into pretty JSON.
    let mut result = String::new();
    let mut in_string = false;
    let mut escape_next = false;

    // Iterate over the characters in the JSON string, adding spaces after : and , but only when not in a string
    for ch in json_compact.chars() {
        match ch {
            '"' if !escape_next => {
                in_string = !in_string;
                result.push(ch);
            }
            '\\' if in_string => {
                escape_next = !escape_next;
                result.push(ch);
            }
            ':' | ',' if !in_string => {
                result.push(ch);
                result.push(' ');
            }
            _ => {
                if escape_next && in_string {
                    escape_next = false;
                }
                result.push(ch);
            }
        }
    }

    Some(result)
}

/// Truncate `text` to `max_graphemes` graphemes. Using graphemes to avoid accidentally truncating in the middle of a multi-codepoint character.
pub(crate) fn truncate_text(text: &str, max_graphemes: usize) -> String {
    let mut graphemes = text.grapheme_indices(true);

    // Check if there's a grapheme at position max_graphemes (meaning there are more than max_graphemes total)
    if let Some((byte_index, _)) = graphemes.nth(max_graphemes) {
        // There are more than max_graphemes, so we need to truncate
        if max_graphemes >= 3 {
            // Truncate to max_graphemes - 3 and add "..." to stay within limit
            let mut truncate_graphemes = text.grapheme_indices(true);
            if let Some((truncate_byte_index, _)) = truncate_graphemes.nth(max_graphemes - 3) {
                let truncated = &text[..truncate_byte_index];
                format!("{truncated}...")
            } else {
                text.to_string()
            }
        } else {
            // max_graphemes < 3, so just return first max_graphemes without "..."
            let truncated = &text[..byte_index];
            truncated.to_string()
        }
    } else {
        // There are max_graphemes or fewer graphemes, return original text
        text.to_string()
    }
}

/// Truncate a path-like string to the given display width, keeping leading and trailing segments
/// where possible and inserting a single Unicode ellipsis between them. If an individual segment
/// cannot fit, it is front-truncated with an ellipsis.
pub(crate) fn center_truncate_path(path: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_string();
    }

    let sep = std::path::MAIN_SEPARATOR;
    let prefix_len = path.len() - path.trim_start_matches(sep).len();
    let prefix = &path[..prefix_len];
    let has_trailing_sep = path.ends_with(sep);
    let mut raw_segments: Vec<&str> = path[prefix_len..].split(sep).collect();
    if has_trailing_sep
        && !raw_segments.is_empty()
        && raw_segments.last().is_some_and(|last| last.is_empty())
    {
        raw_segments.pop();
    }

    if raw_segments.is_empty() {
        if !prefix.is_empty() && UnicodeWidthStr::width(prefix) <= max_width {
            return prefix.to_string();
        }
        return "…".to_string();
    }

    struct Segment<'a> {
        original: &'a str,
        text: String,
        truncatable: bool,
        is_suffix: bool,
    }

    let assemble = |segments: &[Segment<'_>]| -> String {
        let mut result = prefix.to_string();
        for (index, segment) in segments.iter().enumerate() {
            if index > 0 {
                result.push(sep);
            }
            result.push_str(segment.text.as_str());
        }
        result
    };

    let front_truncate = |original: &str, allowed_width: usize| -> String {
        if allowed_width == 0 {
            return String::new();
        }
        if UnicodeWidthStr::width(original) <= allowed_width {
            return original.to_string();
        }
        if allowed_width == 1 {
            return "…".to_string();
        }

        let mut start = original.len();
        let mut used_width = 1; // reserve space for leading ellipsis
        for (index, grapheme) in original.grapheme_indices(true).rev() {
            let ch_width = UnicodeWidthStr::width(grapheme);
            if used_width + ch_width > allowed_width {
                break;
            }
            used_width += ch_width;
            start = index;
        }
        let mut truncated = String::from("…");
        truncated.push_str(&original[start..]);
        truncated
    };

    let segment_count = raw_segments.len();
    // Keep the server and share together when shortening a UNC path.
    let preferred_suffix = if prefix_len == 2 * sep.len_utf8() {
        1
    } else {
        2
    };
    let desired_suffix = if segment_count > 1 {
        std::cmp::min(preferred_suffix, segment_count - 1)
    } else {
        0
    };
    let combos = [true, false].into_iter().flat_map(|prioritized| {
        (1..=segment_count).rev().flat_map(move |left| {
            let min_right = usize::from(left != segment_count);
            (min_right..=segment_count - left)
                .rev()
                .filter(move |right| (*right >= desired_suffix) == prioritized)
                .map(move |right| (left, right))
        })
    });
    // Prefix sums reject candidates that cannot fit even after permitted segment
    // truncation, before allocating any candidate strings.
    let mut min_widths = vec![0usize];
    for segment in &raw_segments {
        let width = UnicodeWidthStr::width(*segment);
        let minimum = if width > max_width || segment_count <= 2 {
            width.min(1)
        } else {
            width
        };
        min_widths.push(min_widths.last().copied().unwrap_or(0) + minimum);
    }

    let fit_segments =
        |segments: &mut Vec<Segment<'_>>, allow_front_truncate: bool| -> Option<String> {
            loop {
                let candidate = assemble(segments);
                let width = UnicodeWidthStr::width(candidate.as_str());
                if width <= max_width {
                    return Some(candidate);
                }

                if !allow_front_truncate {
                    return None;
                }

                let mut indices: Vec<usize> = Vec::new();
                for (idx, seg) in segments.iter().enumerate().rev() {
                    if seg.truncatable && seg.is_suffix {
                        indices.push(idx);
                    }
                }
                for (idx, seg) in segments.iter().enumerate().rev() {
                    if seg.truncatable && !seg.is_suffix {
                        indices.push(idx);
                    }
                }

                if indices.is_empty() {
                    return None;
                }

                let mut changed = false;
                for idx in indices {
                    let original_width = UnicodeWidthStr::width(segments[idx].original);
                    if original_width <= max_width && segment_count > 2 {
                        continue;
                    }
                    let seg_width = UnicodeWidthStr::width(segments[idx].text.as_str());
                    let other_width = width.saturating_sub(seg_width);
                    let allowed_width = max_width.saturating_sub(other_width).max(1);
                    let new_text = front_truncate(segments[idx].original, allowed_width);
                    if new_text != segments[idx].text {
                        segments[idx].text = new_text;
                        changed = true;
                        break;
                    }
                }

                if !changed {
                    return None;
                }
            }
        };

    for (left_count, right_count) in combos {
        let need_ellipsis = left_count + right_count < segment_count;
        let min_width =
            UnicodeWidthStr::width(prefix) + min_widths[left_count] + min_widths[segment_count]
                - min_widths[segment_count - right_count]
                + left_count
                + right_count
                - 1
                + 2 * usize::from(need_ellipsis);
        if min_width > max_width {
            continue;
        }
        let mut segments: Vec<Segment<'_>> = raw_segments[..left_count]
            .iter()
            .map(|seg| Segment {
                original: seg,
                text: (*seg).to_string(),
                truncatable: true,
                is_suffix: false,
            })
            .collect();

        if need_ellipsis {
            segments.push(Segment {
                original: "…",
                text: "…".to_string(),
                truncatable: false,
                is_suffix: false,
            });
        }

        if right_count > 0 {
            segments.extend(
                raw_segments[segment_count - right_count..]
                    .iter()
                    .map(|seg| Segment {
                        original: seg,
                        text: (*seg).to_string(),
                        truncatable: true,
                        is_suffix: true,
                    }),
            );
        }

        let allow_front_truncate = need_ellipsis || segment_count <= 2;
        if let Some(candidate) = fit_segments(&mut segments, allow_front_truncate) {
            return candidate;
        }
    }

    front_truncate(path, max_width)
}

/// Join a list of strings with proper English punctuation.
/// Examples:
/// - [] -> ""
/// - ["apple"] -> "apple"
/// - ["apple", "banana"] -> "apple and banana"
/// - ["apple", "banana", "cherry"] -> "apple, banana and cherry"
pub(crate) fn proper_join<T: AsRef<str>>(items: &[T]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].as_ref().to_string(),
        2 => format!("{} and {}", items[0].as_ref(), items[1].as_ref()),
        _ => {
            let last = items[items.len() - 1].as_ref();
            let mut result = String::new();

            for (i, item) in items.iter().take(items.len() - 1).enumerate() {
                if i > 0 {
                    result.push_str(", ");
                }
                result.push_str(item.as_ref());
            }

            format!("{result} and {last}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn tool_preview_obeys_rows_cells_and_grapheme_boundaries() {
        assert_eq!(format_and_truncate_tool_result("a\nb\nc", 2, 80), "a\nb…");
        assert_eq!(format_and_truncate_tool_result("界界界", 2, 2), "界\n…");
        assert_eq!(
            format_and_truncate_tool_result("Ae\u{301}BC", 2, 2),
            "Ae\u{301}\nBC"
        );
        assert_eq!(format_and_truncate_tool_result("👩‍💻x", 1, 1), "…");
        assert_eq!(
            format_and_truncate_tool_result("a \u{301}bcX", 2, 4),
            "a \u{301}bc\nX"
        );
        assert_eq!(format_and_truncate_tool_result("abc", 0, usize::MAX), "");
        assert_eq!(format_and_truncate_tool_result("abc", usize::MAX, 0), "");
        assert_eq!(
            format_and_truncate_tool_result("abc", usize::MAX, usize::MAX),
            "abc"
        );
    }

    #[test]
    fn front_truncation_keeps_combining_marks_with_their_base() {
        assert_eq!(center_truncate_path("Ae\u{301}BC", 3), "…BC");
        assert_eq!(center_truncate_path("X👩‍💻BC", 4), "…BC");
    }

    #[test]
    fn center_truncation_preserves_unc_prefix() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = format!("{sep}{sep}server{sep}share{sep}folder{sep}file.txt");
        assert_eq!(
            center_truncate_path(&path, 29),
            format!("{sep}{sep}server{sep}share{sep}…{sep}file.txt")
        );
    }

    #[test]
    fn compact_json_preserves_escaped_quotes_and_structural_characters_in_strings() {
        let input = r#"{"text":"a, b: \\\"quoted\\\"", "slash":"\\\\"}"#;
        let output = format_json_compact(input).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).unwrap(),
            serde_json::from_str::<serde_json::Value>(input).unwrap()
        );
        assert!(output.contains("\"text\": \"a, b:"));
        assert!(output.contains(", \""));
    }

    #[test]
    fn test_truncate_text() {
        let text = "Hello, world!";
        let truncated = truncate_text(text, /*max_graphemes*/ 8);
        assert_eq!(truncated, "Hello...");
    }

    #[test]
    fn test_truncate_empty_string() {
        let text = "";
        let truncated = truncate_text(text, /*max_graphemes*/ 5);
        assert_eq!(truncated, "");
    }

    #[test]
    fn test_truncate_max_graphemes_zero() {
        let text = "Hello";
        let truncated = truncate_text(text, /*max_graphemes*/ 0);
        assert_eq!(truncated, "");
    }

    #[test]
    fn test_truncate_max_graphemes_one() {
        let text = "Hello";
        let truncated = truncate_text(text, /*max_graphemes*/ 1);
        assert_eq!(truncated, "H");
    }

    #[test]
    fn test_truncate_max_graphemes_two() {
        let text = "Hello";
        let truncated = truncate_text(text, /*max_graphemes*/ 2);
        assert_eq!(truncated, "He");
    }

    #[test]
    fn test_truncate_max_graphemes_three_boundary() {
        let text = "Hello";
        let truncated = truncate_text(text, /*max_graphemes*/ 3);
        assert_eq!(truncated, "...");
    }

    #[test]
    fn test_truncate_text_shorter_than_limit() {
        let text = "Hi";
        let truncated = truncate_text(text, /*max_graphemes*/ 10);
        assert_eq!(truncated, "Hi");
    }

    #[test]
    fn test_truncate_text_exact_length() {
        let text = "Hello";
        let truncated = truncate_text(text, /*max_graphemes*/ 5);
        assert_eq!(truncated, "Hello");
    }

    #[test]
    fn test_truncate_emoji() {
        let text = "👋🌍🚀✨💫";
        let truncated = truncate_text(text, /*max_graphemes*/ 3);
        assert_eq!(truncated, "...");

        let truncated_longer = truncate_text(text, /*max_graphemes*/ 4);
        assert_eq!(truncated_longer, "👋...");
    }

    #[test]
    fn test_truncate_unicode_combining_characters() {
        let text = "é́ñ̃"; // Characters with combining marks
        let truncated = truncate_text(text, /*max_graphemes*/ 2);
        assert_eq!(truncated, "é́ñ̃");
    }

    #[test]
    fn test_truncate_very_long_text() {
        let text = "a".repeat(1000);
        let truncated = truncate_text(&text, /*max_graphemes*/ 10);
        assert_eq!(truncated, "aaaaaaa...");
        assert_eq!(truncated.len(), 10); // 7 'a's + 3 dots
    }

    #[test]
    fn test_format_json_compact_simple_object() {
        let json = r#"{ "name": "John", "age": 30 }"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(result, r#"{"name": "John", "age": 30}"#);
    }

    #[test]
    fn test_format_json_compact_nested_object() {
        let json = r#"{ "user": { "name": "John", "details": { "age": 30, "city": "NYC" } } }"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(
            result,
            r#"{"user": {"name": "John", "details": {"age": 30, "city": "NYC"}}}"#
        );
    }

    #[test]
    fn test_center_truncate_doesnt_truncate_short_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = format!("{sep}Users{sep}codex{sep}Public");
        let truncated = center_truncate_path(&path, /*max_width*/ 40);

        assert_eq!(truncated, path);
    }

    #[test]
    fn test_center_truncate_truncates_long_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = format!("~{sep}hello{sep}the{sep}fox{sep}is{sep}very{sep}fast");
        let truncated = center_truncate_path(&path, /*max_width*/ 24);

        assert_eq!(
            truncated,
            format!("~{sep}hello{sep}the{sep}…{sep}very{sep}fast")
        );
    }

    #[test]
    fn test_center_truncate_truncates_long_windows_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = format!(
            "C:{sep}Users{sep}codex{sep}Projects{sep}super{sep}long{sep}windows{sep}path{sep}file.txt"
        );
        let truncated = center_truncate_path(&path, /*max_width*/ 36);

        let expected = format!("C:{sep}Users{sep}codex{sep}…{sep}path{sep}file.txt");

        assert_eq!(truncated, expected);
    }

    #[test]
    fn test_center_truncate_handles_long_segment() {
        let sep = std::path::MAIN_SEPARATOR;
        let path = format!("~{sep}supercalifragilisticexpialidocious");
        let truncated = center_truncate_path(&path, /*max_width*/ 18);

        assert_eq!(truncated, format!("~{sep}…cexpialidocious"));
    }

    #[test]
    fn test_format_json_compact_array() {
        let json = r#"[ 1, 2, { "key": "value" }, "string" ]"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(result, r#"[1, 2, {"key": "value"}, "string"]"#);
    }

    #[test]
    fn test_format_json_compact_already_compact() {
        let json = r#"{"compact":true}"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(result, r#"{"compact": true}"#);
    }

    #[test]
    fn test_format_json_compact_with_whitespace() {
        let json = r#"
        {
            "name": "John",
            "hobbies": [
                "reading",
                "coding"
            ]
        }
        "#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(
            result,
            r#"{"name": "John", "hobbies": ["reading", "coding"]}"#
        );
    }

    #[test]
    fn test_format_json_compact_invalid_json() {
        let invalid_json = r#"{"invalid": json syntax}"#;
        let result = format_json_compact(invalid_json);
        assert!(result.is_none());
    }

    #[test]
    fn test_format_json_compact_empty_object() {
        let json = r#"{}"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_format_json_compact_empty_array() {
        let json = r#"[]"#;
        let result = format_json_compact(json).unwrap();
        assert_eq!(result, "[]");
    }

    #[test]
    fn test_format_json_compact_primitive_values() {
        assert_eq!(format_json_compact("42").unwrap(), "42");
        assert_eq!(format_json_compact("true").unwrap(), "true");
        assert_eq!(format_json_compact("false").unwrap(), "false");
        assert_eq!(format_json_compact("null").unwrap(), "null");
        assert_eq!(format_json_compact(r#""string""#).unwrap(), r#""string""#);
    }

    #[test]
    fn test_proper_join() {
        let empty: Vec<String> = vec![];
        assert_eq!(proper_join(&empty), "");
        assert_eq!(proper_join(&["apple"]), "apple");
        assert_eq!(proper_join(&["apple", "banana"]), "apple and banana");
        assert_eq!(
            proper_join(&["apple", "banana", "cherry"]),
            "apple, banana and cherry"
        );
        assert_eq!(
            proper_join(&["apple", "banana", "cherry", "date"]),
            "apple, banana, cherry and date"
        );
    }
}
