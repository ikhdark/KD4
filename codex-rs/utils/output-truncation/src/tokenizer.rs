/// Count ordinary text with the model's o200k vocabulary, including strings
/// resembling special tokens as literal tool output.
pub fn model_token_count(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton().count_ordinary(text)
}

/// Keep the head and failure tail, counting the complete rendered packet.
pub fn truncate_model_text(text: &str, limit: usize) -> String {
    truncate_model_text_at_lines(text, limit, 0, text.lines().count())
}

pub fn truncate_model_text_at_lines(text: &str, limit: usize, line_offset: usize, total_lines: usize) -> String {
    truncate_model_text_at_lines_with_artifact(text, limit, line_offset, total_lines, None)
}

pub fn truncate_model_text_at_lines_with_artifact(
    text: &str, limit: usize, line_offset: usize, total_lines: usize, artifact_id: Option<&str>,
) -> String {
    truncate_model_text_at_lines_with_recovery(text, limit, line_offset, total_lines, artifact_id).0
}

/// Returns the actual omitted source lines alongside presentation, even when
/// the budget is too small to display a coordinate marker.
pub fn truncate_model_text_at_lines_with_recovery(
    text: &str, limit: usize, line_offset: usize, total_lines: usize, artifact_id: Option<&str>,
) -> (String, Option<(usize, usize)>) {
    let all_lines = (!text.is_empty()).then(|| (line_offset + 1, line_offset + text.lines().count()));
    // Silent projections need neither the vocabulary nor a token vector for
    // output that will be discarded, including logs preceding a script error.
    if limit == 0 {
        return (String::new(), all_lines);
    }
    // Every ordinary token encodes at least one byte, so text this short fits
    // without encoding it or loading the vocabulary.
    if text.len() <= limit {
        return (text.to_string(), None);
    }
    let bpe = tiktoken_rs::o200k_base_singleton();
    let tokens = bpe.encode_ordinary(text);
    if tokens.len() <= limit {
        return (text.to_string(), None);
    }
    let mut marker = format!("\nWarning: truncated output ({} tokens)\n", tokens.len());
    if model_token_count(&marker) + 2 > limit {
        marker = "…".to_string();
    }
    let mut retained = limit.saturating_sub(model_token_count(&marker) + 2);
    loop {
        let head = retained / 2;
        let tail = retained - head;
        // Token boundaries may split a UTF-8 character. Drop only the partial
        // character at each cut rather than emitting replacement bytes.
        let head_bytes = bpe.decode_bytes(&tokens[..head]).unwrap_or_default();
        let tail_bytes = bpe
            .decode_bytes(&tokens[tokens.len() - tail..])
            .unwrap_or_default();
        let head_end = std::str::from_utf8(&head_bytes)
            .err()
            .map_or(head_bytes.len(), |error| error.valid_up_to());
        let mut tail_start = 0;
        while tail_start < tail_bytes.len()
            && std::str::from_utf8(&tail_bytes[tail_start..]).is_err()
        {
            tail_start += 1;
        }
        if marker != "…" {
            marker = crate::omitted_line_marker_at_lines(
                text, head_end, text.len() - (tail_bytes.len() - tail_start),
                line_offset, total_lines,
            );
            if let Some(artifact_id) = artifact_id {
                marker = marker.replace("]\n", &format!("; artifact_id={artifact_id}]\n"));
            }
            let files = omitted_file_counts(text, head_end, text.len() - (tail_bytes.len() - tail_start));
            // Tiny budgets still retain the line-coordinate recovery notice.
            if model_token_count(&files) <= limit / 3 {
                marker.push_str(&files);
            }
        }
        let result = format!(
            "{}{}{}",
            std::str::from_utf8(&head_bytes[..head_end]).unwrap_or_default(),
            marker,
            std::str::from_utf8(&tail_bytes[tail_start..]).unwrap_or_default()
        );
        if model_token_count(&result) <= limit {
            let (first, last, _) = crate::omitted_line_span(
                text, head_end, text.len() - (tail_bytes.len() - tail_start),
            );
            return (result, Some((first + line_offset, last + line_offset)));
        }
        if retained == 0 {
            return (String::new(), all_lines);
        }
        retained = retained.saturating_sub(1.max(retained / 100));
    }
}

fn omitted_file_counts(text: &str, start: usize, end: usize) -> String {
    let first = text[..start].rfind('\n').map_or(0, |offset| offset + 1);
    let last = if text[..end].ends_with('\n') { end } else {
        text[end..].find('\n').map_or(text.len(), |offset| end + offset)
    };
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for line in text[first..last].lines() {
        let path = line.match_indices(':').find_map(|(offset, _)| {
            let rest = &line[offset + 1..];
            let (number, _) = rest.split_once(':')?;
            (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
                .then_some(&line[..offset])
        }).or_else(|| {
            // Bare paths from rg --files/-l. Do not turn ordinary prose or
            // source lines into invented filenames.
            (!line.is_empty() && !line.chars().any(char::is_whitespace)
                && (line.contains(['/', '\\']) || line.rsplit_once('.').is_some_and(|(base, ext)|
                    !base.is_empty() && !ext.is_empty() && ext.chars().all(char::is_alphanumeric))))
                .then_some(line)
        });
        if let Some(path) = path.filter(|path| !path.is_empty() && !path.contains(['\u{1b}', '\t'])) {
            *counts.entry(path).or_default() += 1;
        }
    }
    if counts.is_empty() { return String::new(); }
    let mut notice = String::from("Omitted hits by file (includes partial boundary lines):\n");
    let mut shown = 0;
    for (path, count) in &counts {
        if shown == 8 { break; }
        if path.len() > 160 { continue; }
        notice.push_str(&format!("{path}: {count}\n"));
        shown += 1;
    }
    if shown < counts.len() {
        notice.push_str(&format!("{} additional files not listed\n", counts.len() - shown));
    }
    notice
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_packet_uses_tokenizer_capacity_and_preserves_failure_tail() {
        let source = "    let result = read_source_file(path).await?;\n".repeat(2000)
            + "assertion failed: expected 7, got 3\n";
        let output = truncate_model_text(&source, 10_000);
        assert!(model_token_count(&output) <= 10_000);
        assert!(model_token_count(&output) > 9_800);
        assert!(output.len() > 30_000);
        assert!(output.ends_with("assertion failed: expected 7, got 3\n"));
        assert!(output.contains("[omitted lines "));
    }

    #[test]
    fn slice_truncation_preserves_original_line_coordinates() {
        let source = (201..=1200)
            .map(|line| format!("source line {line:04}\r\n"))
            .collect::<String>();
        let output = truncate_model_text_at_lines(&source, 100, 200, 1500);
        assert!(model_token_count(&output) <= 100);
        let (head, rest) = output.split_once("\n[omitted lines ").unwrap();
        let (marker, tail) = rest.split_once("]\n").unwrap();
        assert!(source.starts_with(head));
        assert!(source.ends_with(tail));
        let (span, total) = marker.split_once(" of ").unwrap();
        assert_eq!(total.split(';').next(), Some("1500"));
        let first = 201 + head.bytes().filter(|byte| *byte == b'\n').count();
        let last = 200 + source[..source.len() - tail.len()].lines().count();
        assert_eq!(span, format!("{first}-{last}"));
    }

    #[test]
    fn artifact_qualified_marker_keeps_the_budget_and_source_coordinates() {
        let source = "source line\n".repeat(2000);
        let artifact = "01a115cd-c4a8-7403-b194-68d9ebaec306";
        let text = truncate_model_text_at_lines_with_artifact(&source, 120, 0, 2000, Some(artifact));
        assert!(text.contains(&format!("artifact_id={artifact}]")));
        assert!(text.contains("[omitted lines "));
        assert!(model_token_count(&text) <= 120);
    }

    #[test]
    fn unicode_and_tiny_budgets_are_valid_and_bounded() {
        for text in ["🙂漢字".repeat(1000), "<|endoftext|>".repeat(1000)] {
            for limit in [0, 1, 10, 100, 1000] {
                assert!(model_token_count(&truncate_model_text(&text, limit)) <= limit);
            }
        }
        assert_eq!(truncate_model_text("hello world", 2), "hello world");
    }

    #[test]
    fn omitted_hits_name_files_and_preserve_the_token_ceiling() {
        let source = "src/first.rs:12:needle\nC:\\repo\\second.rs:3:needle\n".repeat(100);
        let notice = omitted_file_counts(&source, 0, source.len());
        assert!(notice.contains("src/first.rs: 100\n"));
        assert!(notice.contains("C:\\repo\\second.rs: 100\n"));
        let output = truncate_model_text(&source, 300);
        assert!(output.contains("Omitted hits by file"));
        assert!(model_token_count(&output) <= 300);
        assert!(omitted_file_counts("ordinary prose\n", 0, 15).is_empty());
        assert!(omitted_file_counts("src/日本語.rs\n", 0, "src/日本語.rs\n".len()).contains("src/日本語.rs: 1"));
        let many = (0..30).map(|i| format!("src/file_{i}.rs\n")).collect::<String>();
        assert!(omitted_file_counts(&many, 0, many.len()).contains("22 additional files"));
    }
}
