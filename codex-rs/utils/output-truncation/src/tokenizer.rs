/// Count ordinary text with the model's o200k vocabulary, including strings
/// resembling special tokens as literal tool output.
pub fn model_token_count(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton().count_ordinary(text)
}

/// Keep the head and failure tail, counting the complete rendered packet.
pub fn truncate_model_text(text: &str, limit: usize) -> String {
    truncate_model_text_at_lines(text, limit, 0, text.lines().count())
}

pub fn truncate_model_text_at_lines(
    text: &str,
    limit: usize,
    line_offset: usize,
    total_lines: usize,
) -> String {
    truncate_model_text_at_lines_with_artifact(text, limit, line_offset, total_lines, None)
}

pub fn truncate_model_text_at_lines_with_artifact(
    text: &str,
    limit: usize,
    line_offset: usize,
    total_lines: usize,
    artifact_id: Option<&str>,
) -> String {
    truncate_model_text_at_lines_with_recovery(text, limit, line_offset, total_lines, artifact_id).0
}

/// Returns the actual omitted source lines alongside presentation, even when
/// the budget is too small to display a coordinate marker.
pub fn truncate_model_text_at_lines_with_recovery(
    text: &str,
    limit: usize,
    line_offset: usize,
    total_lines: usize,
    artifact_id: Option<&str>,
) -> (String, Option<(usize, usize)>) {
    truncate_model_text_at_lines_with_limits(
        text,
        limit,
        limit,
        line_offset,
        total_lines,
        artifact_id,
    )
}

/// Retain a whole packet up to `whole_packet_limit`, otherwise truncate to
/// `limit`. Reuse the same encoding for admission and truncation. Callers own
/// the hard cap; a zero truncation budget never admits a nonempty packet.
pub fn truncate_model_text_at_lines_with_limits(
    text: &str,
    limit: usize,
    whole_packet_limit: usize,
    line_offset: usize,
    total_lines: usize,
    artifact_id: Option<&str>,
) -> (String, Option<(usize, usize)>) {
    let all_lines = ||
        (!text.is_empty()).then(|| (line_offset + 1, line_offset + text.lines().count()));
    // Silent projections need neither the vocabulary nor a token vector for
    // output that will be discarded, including logs preceding a script error.
    if limit == 0 {
        return (String::new(), all_lines());
    }
    // Every ordinary token encodes at least one byte, so text this short fits
    // without encoding it or loading the vocabulary.
    let whole_packet_limit = whole_packet_limit.max(limit);
    if text.len() <= whole_packet_limit {
        return (text.to_string(), None);
    }
    let bpe = tiktoken_rs::o200k_base_singleton();
    let tokens = bpe.encode_ordinary(text);
    if tokens.len() <= whole_packet_limit {
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
                text,
                head_end,
                text.len() - (tail_bytes.len() - tail_start),
                line_offset,
                total_lines,
            );
            if let Some(artifact_id) = artifact_id {
                marker = marker.replace("]\n", &format!("; artifact_id={artifact_id}]\n"));
            }
            let files =
                omitted_file_counts(text, head_end, text.len() - (tail_bytes.len() - tail_start));
            // Tiny budgets still retain the line-coordinate recovery notice.
            if model_token_count(&files) <= limit / 3 {
                marker.push_str(&files);
            }
            // The cut can swallow the label that owns the retained tail, which
            // would then read as a continuation of the head's source. Never
            // let the label crowd out the evidence it describes.
            let tail_begin = text.len() - (tail_bytes.len() - tail_start);
            if !text[tail_begin..].starts_with(SOURCE_LABEL_PREFIX)
                && let Some(label) = omitted_source_label(text, head_end, tail_begin)
                && model_token_count(&marker) + model_token_count(label) < limit / 2
            {
                marker.push_str(label);
                marker.push('\n');
            }
        }
        let result = format!(
            "{}{}{}",
            std::str::from_utf8(&head_bytes[..head_end]).unwrap_or_default(),
            marker,
            std::str::from_utf8(&tail_bytes[tail_start..]).unwrap_or_default()
        );
        let result_tokens = model_token_count(&result);
        if result_tokens <= limit {
            let (first, last, _) = crate::omitted_line_span(
                text,
                head_end,
                text.len() - (tail_bytes.len() - tail_start),
            );
            return (result, Some((first + line_offset, last + line_offset)));
        }
        if retained == 0 {
            return (String::new(), all_lines());
        }
        // The complete packet already tells us how far it exceeds the budget.
        // A fixed 1% decrement needlessly re-tokenizes the packet and rescans
        // all omitted source lines, especially for small display budgets.
        // Recheck after each cut: boundary merges and the marker can change.
        // Close to a tiny packet, the partial-line marker can itself shrink.
        // Keep fine-grained fitting for the last 16 tokens on each side rather
        // than jumping over a fitting diagnostic tail to an empty packet.
        let excess = (result_tokens - limit).min(retained.saturating_sub(32));
        retained = retained.saturating_sub(excess.max(1).max(retained / 100));
    }
}

/// Mirrors the per-body label printed by the code-mode output projection.
const SOURCE_LABEL_PREFIX: &str = "[source {";

/// Last complete source label line that begins inside the omitted range.
fn omitted_source_label(text: &str, start: usize, end: usize) -> Option<&str> {
    let offset = text[start..end]
        .rmatch_indices(SOURCE_LABEL_PREFIX)
        .map(|(relative, _)| start + relative)
        .find(|&offset| offset == 0 || text.as_bytes()[offset - 1] == b'\n')?;
    let line_end = text[offset..]
        .find('\n')
        .map_or(text.len(), |length| offset + length);
    let line = text[offset..line_end].trim_end_matches('\r');
    line.ends_with("}]").then_some(line)
}

fn omitted_file_counts(text: &str, start: usize, end: usize) -> String {
    let first = text[..start].rfind('\n').map_or(0, |offset| offset + 1);
    let last = if text[..end].ends_with('\n') {
        end
    } else {
        text[end..]
            .find('\n')
            .map_or(text.len(), |offset| end + offset)
    };
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for line in text[first..last].lines() {
        let path = line
            .match_indices(':')
            .find_map(|(offset, _)| {
                let rest = &line[offset + 1..];
                let (number, _) = rest.split_once(':')?;
                // Timestamps and syslog prefixes also look like `prefix:digits:`.
                let candidate = &line[..offset];
                (!number.is_empty()
                    && number.bytes().all(|byte| byte.is_ascii_digit())
                    && (candidate.contains(['/', '\\'])
                        || !(candidate.contains(char::is_whitespace)
                            || candidate.starts_with(|ch: char| ch.is_ascii_digit()))))
                .then_some(candidate)
            })
            .or_else(|| {
                // Bare paths from rg --files/-l. Do not turn ordinary prose or
                // source lines into invented filenames.
                (!line.is_empty()
                    && !line.chars().any(char::is_whitespace)
                    && (line.contains(['/', '\\'])
                        || line.rsplit_once('.').is_some_and(|(base, ext)| {
                            !base.is_empty()
                                && !ext.is_empty()
                                && ext.chars().all(char::is_alphanumeric)
                        })))
                .then_some(line)
            });
        if let Some(path) = path.filter(|path| !path.is_empty() && !path.contains(['\u{1b}', '\t']))
        {
            *counts.entry(path).or_default() += 1;
        }
    }
    if counts.is_empty() {
        return String::new();
    }
    let mut notice = String::from("Omitted hits by file (includes partial boundary lines):\n");
    let mut shown = 0;
    for (path, count) in &counts {
        if shown == 8 {
            break;
        }
        if path.len() > 160 {
            continue;
        }
        notice.push_str(&format!("{path}: {count}\n"));
        shown += 1;
    }
    if shown < counts.len() {
        notice.push_str(&format!(
            "{} additional files not listed\n",
            counts.len() - shown
        ));
    }
    notice
}

