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
    if max_tokens == 0 {
        let total_tokens = u64::try_from(total_token_count).unwrap_or(u64::MAX);
        return (String::new(), Some(total_tokens));
    }
    if total_token_count <= max_tokens {
        return (s.to_string(), None);
    }

    let proportional_bytes = if total_token_count == 0 {
        0
    } else {
        s.len()
            .saturating_mul(max_tokens)
            .checked_div(total_token_count)
            .unwrap_or(0)
    };
    let mut content_bytes = proportional_bytes.min(approx_bytes_for_tokens(max_tokens));
    let total_tokens = u64::try_from(total_token_count).unwrap_or(u64::MAX);
    loop {
        let truncated = truncate_with_byte_estimate(s, content_bytes, /*use_tokens*/ true);
        if approx_token_count(&truncated) <= max_tokens {
            return (truncated, Some(total_tokens));
        }
        if content_bytes == 0 {
            return (String::new(), Some(total_tokens));
        }
        // Geometric reduction bounds retries even when the retained ends are
        // much denser than the middle. Verify each candidate: the estimate is
        // not necessarily monotonic as UTF-8 boundaries and marker digits change.
        content_bytes -= content_bytes.div_ceil(4);
    }
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

pub fn approx_token_count(text: &str) -> usize {
    TokenCountEstimate::new(text).tokens()
}

/// Cached components of the approximate token count. Keeping both components
/// avoids rounding each fragment before combining whitespace-separated text.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokenCountEstimate {
    bytes: usize,
    lexical: usize,
}

impl TokenCountEstimate {
    pub fn new(text: &str) -> Self {
        Self {
            bytes: text.len(),
            lexical: lexical_token_count(text, usize::MAX),
        }
    }

    pub fn tokens(self) -> usize {
        token_byte_estimate(self.bytes).max(self.lexical)
    }

    /// Add independently delimited fragments (for example, complete JSON values).
    /// The caller must ensure no word spans the join; delimiters count separately.
    pub fn add_delimited(self, next: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(next.bytes),
            lexical: self.lexical.saturating_add(next.lexical),
        }
    }

    /// Remove a previously added, independently delimited fragment.
    pub fn subtract_delimited(self, previous: Self) -> Self {
        Self {
            bytes: self.bytes - previous.bytes,
            lexical: self.lexical - previous.lexical,
        }
    }

    /// Combine fragments with a nonempty separator consisting only of whitespace.
    /// The separator terminates words, so their lexical counts are additive.
    pub fn then(self, next: Self, whitespace_bytes: usize) -> Self {
        assert!(
            whitespace_bytes > 0,
            "a separator must terminate the preceding word"
        );
        Self {
            bytes: self
                .bytes
                .saturating_add(whitespace_bytes)
                .saturating_add(next.bytes),
            lexical: self.lexical.saturating_add(next.lexical),
        }
    }
}

/// Compare against the same estimate as `approx_token_count`, stopping as soon
/// as either its byte lower bound or its lexical lower bound exceeds the limit.
pub fn approx_token_count_exceeds(text: &str, limit: usize) -> bool {
    token_byte_estimate(text.len()) > limit || lexical_token_count(text, limit) > limit
}

fn token_byte_estimate(bytes: usize) -> usize {
    bytes.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

fn lexical_token_count(text: &str, limit: usize) -> usize {
    let mut lexical_estimate = 0usize;
    let mut word_bytes = 0usize;
    let mut offset = 0;
    while let Some(&byte) = text.as_bytes().get(offset) {
        let (word, whitespace, width) = if byte.is_ascii() {
            (
                byte.is_ascii_alphanumeric() || byte == b'_',
                matches!(byte, b' ' | b'\t'..=b'\r'),
                1,
            )
        } else {
            // `offset` always advances by a complete UTF-8 character.
            let Some(ch) = text[offset..].chars().next() else {
                break;
            };
            (ch.is_alphanumeric(), ch.is_whitespace(), ch.len_utf8())
        };
        offset += width;
        if word {
            word_bytes = word_bytes.saturating_add(width);
            continue;
        }
        lexical_estimate = lexical_estimate.saturating_add(token_byte_estimate(word_bytes));
        word_bytes = 0;
        if !whitespace {
            lexical_estimate = lexical_estimate.saturating_add(1);
        }
        if lexical_estimate > limit {
            return lexical_estimate;
        }
    }
    lexical_estimate.saturating_add(token_byte_estimate(word_bytes))
}

pub fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

pub fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let bytes_u64 = bytes as u64;
    bytes_u64.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1))
        / (APPROX_BYTES_PER_TOKEN as u64)
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
