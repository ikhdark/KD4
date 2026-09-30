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
    // Silent projections need neither the vocabulary nor a token vector for
    // output that will be discarded, including logs preceding a script error.
    if limit == 0 {
        return String::new();
    }
    // Every ordinary token encodes at least one byte, so text this short fits
    // without encoding it or loading the vocabulary.
    if text.len() <= limit {
        return text.to_string();
    }
    let bpe = tiktoken_rs::o200k_base_singleton();
    let tokens = bpe.encode_ordinary(text);
    if tokens.len() <= limit {
        return text.to_string();
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
        }
        let result = format!(
            "{}{}{}",
            std::str::from_utf8(&head_bytes[..head_end]).unwrap_or_default(),
            marker,
            std::str::from_utf8(&tail_bytes[tail_start..]).unwrap_or_default()
        );
        if model_token_count(&result) <= limit {
            return result;
        }
        if retained == 0 {
            return String::new();
        }
        retained = retained.saturating_sub(1.max(retained / 100));
    }
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
    fn unicode_and_tiny_budgets_are_valid_and_bounded() {
        for text in ["🙂漢字".repeat(1000), "<|endoftext|>".repeat(1000)] {
            for limit in [0, 1, 10, 100, 1000] {
                assert!(model_token_count(&truncate_model_text(&text, limit)) <= limit);
            }
        }
        assert_eq!(truncate_model_text("hello world", 2), "hello world");
    }
}
