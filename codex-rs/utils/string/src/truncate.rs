//! Utilities for truncating large chunks of output while preserving a prefix
//! and suffix on UTF-8 boundaries.

const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Retain at most `max_bytes` bytes of source content, plus a character-count marker.
pub fn truncate_middle_chars(s: &str, max_bytes: usize) -> String {
    truncate_with_byte_estimate(s, max_bytes, /*use_tokens*/ false)
}

/// Truncate the middle of a UTF-8 string to at most `max_tokens` approximate
/// tokens, preserving the beginning and the end. Returns the possibly
/// truncated string and `Some(original_token_count)` if truncation occurred;
/// otherwise returns the original string and `None`.
pub fn truncate_middle_with_token_budget(s: &str, max_tokens: usize) -> (String, Option<u64>) {
    if s.is_empty() {
        return (String::new(), None);
    }

    let total_token_count = approx_token_count(s);
    let total_tokens = u64::try_from(total_token_count).unwrap_or(u64::MAX);
    if max_tokens == 0 {
        return (String::new(), Some(total_tokens));
    }
    if total_token_count <= max_tokens {
        return (s.to_string(), None);
    }

    // The marker's cost does not scale with retained content, so reserve it
    // before sizing the retained ends from the input's average density. A count
    // derived from the whole input bounds the marker actually emitted.
    let marker_tokens = approx_token_count(&format_truncation_marker(
        /*use_tokens*/ true,
        approx_tokens_from_byte_count(s.len()),
    ));
    let content_tokens = max_tokens.saturating_sub(marker_tokens);
    // Scale by the retained ends' observed density, which can differ from the
    // input average in either direction.
    let rescale = |bytes: usize, tokens: usize| {
        bytes.saturating_mul(content_tokens) / tokens.saturating_sub(marker_tokens).max(1)
    };
    let mut content_bytes = (s.len().saturating_mul(content_tokens) / total_token_count)
        .min(approx_bytes_for_tokens(content_tokens));
    let mut underfilled: Option<(usize, String)> = None;
    for attempt in 0usize.. {
        let truncated = truncate_with_byte_estimate(s, content_bytes, /*use_tokens*/ true);
        let tokens = approx_token_count(&truncated);
        if tokens <= max_tokens {
            // Ends sparser than the average leave budget unused. Retry once from
            // the rescaled size, keeping this fit in case the retry retains less.
            let grown = rescale(content_bytes, tokens).min(s.len() - 1);
            if attempt == 0 && grown > content_bytes {
                underfilled = Some((content_bytes, truncated));
                content_bytes = grown;
                continue;
            }
            return match underfilled {
                Some((fit_bytes, fit)) if fit_bytes >= content_bytes => (fit, Some(total_tokens)),
                _ => (truncated, Some(total_tokens)),
            };
        }
        if content_bytes == 0 {
            break;
        }
        // Verify each candidate: the estimate is not necessarily monotonic as
        // UTF-8 boundaries and marker digits change, so after a few precise
        // attempts guarantee geometric progress.
        let minimum_step = if attempt < 3 {
            1
        } else {
            content_bytes.div_ceil(8)
        };
        content_bytes = rescale(content_bytes, tokens).min(content_bytes - minimum_step);
    }
    let fit = underfilled.map_or_else(String::new, |(_, fit)| fit);
    (fit, Some(total_tokens))
}

fn truncate_with_byte_estimate(s: &str, max_bytes: usize, use_tokens: bool) -> String {
    if s.is_empty() {
        return String::new();
    }

    if max_bytes == 0 {
        return format_truncation_marker(
            use_tokens,
            removed_units(
                use_tokens,
                s.len(),
                if use_tokens { 0 } else { s.chars().count() },
            ),
        );
    }

    if s.len() <= max_bytes {
        return s.to_string();
    }

    let total_bytes = s.len();
    let (left_budget, right_budget) = split_budget(max_bytes);
    let (left, right) = split_boundaries(s, left_budget, right_budget);
    let removed_chars = if use_tokens {
        0
    } else {
        s[left.len()..s.len() - right.len()].chars().count()
    };
    let marker = format_truncation_marker(
        use_tokens,
        removed_units(
            use_tokens,
            total_bytes - left.len() - right.len(),
            removed_chars,
        ),
    );

    assemble_truncated_output(left, right, &marker)
}

/// Estimate tokens as UTF-8 bytes divided by four, rounded up.
/// This heuristic may overestimate or underestimate tokenizer-specific counts.
pub fn approx_token_count(text: &str) -> usize {
    TokenCountEstimate::new(text).tokens()
}

/// Cached byte count, rounded only after fragments have been combined.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokenCountEstimate {
    bytes: usize,
}

impl TokenCountEstimate {
    pub fn new(text: &str) -> Self {
        Self {
            bytes: text.len(),
        }
    }

    pub fn tokens(self) -> usize {
        token_byte_estimate(self.bytes)
    }

    /// Add independently delimited fragments (for example, complete JSON values).
    /// Include delimiter bytes separately; rounding happens in `tokens`.
    pub fn add_delimited(self, next: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(next.bytes),
        }
    }

    /// Remove a previously added, independently delimited fragment.
    pub fn subtract_delimited(self, previous: Self) -> Self {
        Self {
            bytes: self.bytes - previous.bytes,
        }
    }

    /// Combine fragments with a nonempty separator consisting only of whitespace.
    pub fn then(self, next: Self, whitespace_bytes: usize) -> Self {
        assert!(
            whitespace_bytes > 0,
            "a whitespace separator must be nonempty"
        );
        Self {
            bytes: self
                .bytes
                .saturating_add(whitespace_bytes)
                .saturating_add(next.bytes),
        }
    }
}

/// Compare against the same byte-based estimate as `approx_token_count`.
pub fn approx_token_count_exceeds(text: &str, limit: usize) -> bool {
    token_byte_estimate(text.len()) > limit
}

fn token_byte_estimate(bytes: usize) -> usize {
    bytes.div_ceil(APPROX_BYTES_PER_TOKEN)
}

pub fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

pub fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    token_byte_estimate(bytes) as u64
}

fn split_boundaries(s: &str, beginning_bytes: usize, end_bytes: usize) -> (&str, &str) {
    let len = s.len();
    let prefix_end = s.floor_char_boundary(beginning_bytes.min(len));
    let suffix_start = s
        .ceil_char_boundary(len.saturating_sub(end_bytes))
        .max(prefix_end);
    (&s[..prefix_end], &s[suffix_start..])
}

#[cfg(test)]
fn split_string(s: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    let (before, after) = split_boundaries(s, beginning_bytes, end_bytes);
    (
        s[before.len()..s.len() - after.len()].chars().count(),
        before,
        after,
    )
}

fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

fn format_truncation_marker(use_tokens: bool, removed_count: u64) -> String {
    if use_tokens {
        format!("…{removed_count} tokens truncated…")
    } else {
        format!("…{removed_count} chars truncated…")
    }
}

fn removed_units(use_tokens: bool, removed_bytes: usize, removed_chars: usize) -> u64 {
    if use_tokens {
        approx_tokens_from_byte_count(removed_bytes)
    } else {
        u64::try_from(removed_chars).unwrap_or(u64::MAX)
    }
}

fn assemble_truncated_output(prefix: &str, suffix: &str, marker: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + marker.len() + suffix.len() + 1);
    out.push_str(prefix);
    out.push_str(marker);
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests;
