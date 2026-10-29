/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Returns the unique starting index or `None` if not found, rejecting multiple
/// matches at the strongest matching tier. Matches use
/// decreasing strictness: exact match, then ignoring trailing whitespace, then ignoring leading
/// and trailing whitespace, then normalizing Unicode punctuation and whitespace.
/// When `eof` is true, the match must end at end-of-file
/// and must still begin at or after `start`.
/// When `anchored` is true, `start` follows an earlier match in the same file
/// hunk, so the first match at or after it is the intended location, as with
/// sequentially applied diff hunks.
///
/// Special cases handled defensively:
///  • Empty `pattern` → matches at `start` (or EOF), unless `start` is past EOF
///  • `pattern.len() > lines.len()` → returns `None` (cannot match, avoids
///    out‑of‑bounds panic that occurred pre‑2025‑04‑12)
pub(crate) fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
    anchored: bool,
) -> Result<Option<usize>, AmbiguousMatch> {
    if pattern.is_empty() {
        return Ok((start <= lines.len()).then_some(if eof { lines.len() } else { start }));
    }

    // When the pattern is longer than the available input there is no possible
    // match. Early‑return to avoid the out‑of‑bounds slice that would occur in
    // the search loops below (previously caused a panic when
    // `pattern.len() > lines.len()`).
    let Some(last_start) = lines.len().checked_sub(pattern.len()) else {
        return Ok(None);
    };
    if start > last_start {
        return Ok(None);
    }
    let search_start = if eof { last_start } else { start };

    for strictness in TIERS {
        let limit = if anchored { 1 } else { 2 };
        match tier_matches(lines, pattern, search_start, last_start, strictness, limit).as_slice() {
            [] => {}
            [only] => return Ok(Some(*only)),
            [first, second, ..] => {
                return Err(AmbiguousMatch {
                    first_line: first + 1,
                    second_line: second + 1,
                    strictness,
                });
            }
        }
    }

    Ok(None)
}

/// Zero-based starts of up to `limit` matches, in file order, at the tier that
/// `seek_sequence` reported as ambiguous for the same arguments.
pub(crate) fn ambiguous_candidates(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
    ambiguity: &AmbiguousMatch,
    limit: usize,
) -> Vec<usize> {
    let Some(last_start) = lines.len().checked_sub(pattern.len()) else {
        return Vec::new();
    };
    if pattern.is_empty() || start > last_start {
        return Vec::new();
    }
    let search_start = if eof { last_start } else { start };
    tier_matches(
        lines,
        pattern,
        search_start,
        last_start,
        ambiguity.strictness,
        limit,
    )
}

/// Zero-based starts of up to `limit` matches anywhere in `lines` at the most
/// permissive tier. Every stricter tier matches a subset of these starts, so a
/// single start leaves no tier or search start able to select another site.
pub(crate) fn candidate_starts_at_any_tier(
    lines: &[String],
    pattern: &[String],
    limit: usize,
) -> Vec<usize> {
    match lines.len().checked_sub(pattern.len()) {
        Some(last_start) if !pattern.is_empty() => {
            tier_matches(lines, pattern, 0, last_start, TIERS[TIERS.len() - 1], limit)
        }
        _ => Vec::new(),
    }
}

/// Matching tiers in decreasing strictness: exact match, then ignoring trailing
/// whitespace, then ignoring leading and trailing whitespace, then normalizing
/// Unicode punctuation and whitespace.
const TIERS: [&str; 4] = [
    "exact",
    "trailing-whitespace",
    "whitespace",
    "Unicode-normalized",
];

// ------------------------------------------------------------------
// Final, most permissive pass – attempt to match after *normalising*
// common Unicode punctuation to their ASCII equivalents so that diffs
// authored with plain ASCII characters can still be applied to source
// files that contain typographic dashes / quotes, etc.
// ------------------------------------------------------------------
fn normalise(s: &str) -> impl Iterator<Item = char> + '_ {
    s.trim().chars().map(|c| match c {
        // Various dash / hyphen code-points → ASCII '-'
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // Fancy single quotes → '\''
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // Fancy double quotes → '"'
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // Non-breaking space and other odd spaces → normal space
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    })
}

/// Zero-based starts in `search_start..=last_start` where `pattern` matches at
/// one tier, stopping after `limit` matches.
fn tier_matches(
    lines: &[String],
    pattern: &[String],
    search_start: usize,
    last_start: usize,
    strictness: &str,
    limit: usize,
) -> Vec<usize> {
    let mut found = Vec::new();
    let mut record = |index| {
        found.push(index);
        found.len() >= limit.max(1)
    };
    match strictness {
        "trailing-whitespace" | "whitespace" => {
            // Borrow trimmed views once per tier instead of trimming each line
            // pair again for every overlapping candidate.
            let prepare: fn(&str) -> &str = if strictness == "whitespace" {
                str::trim
            } else {
                str::trim_end
            };
            let prepared_lines = lines[search_start..]
                .iter()
                .map(|line| prepare(line))
                .collect::<Vec<_>>();
            let prepared_pattern = pattern.iter().map(|line| prepare(line)).collect::<Vec<_>>();
            for index in search_start..=last_start {
                let offset = index - search_start;
                if prepared_lines[offset..offset + pattern.len()] == prepared_pattern[..]
                    && record(index)
                {
                    break;
                }
            }
        }
        "Unicode-normalized" if !pattern.is_empty() => {
            let normalized_pattern = pattern
                .iter()
                .map(|line| normalise(line).collect::<String>())
                .collect::<Vec<_>>();
            // Overlapping candidate windows share one left-to-right pass
            // (Knuth-Morris-Pratt over normalized lines), so each source line
            // is compared about once instead of once per window. Comparisons
            // stay lazy and stop at the first differing character.
            let mut fallback = vec![0; normalized_pattern.len()];
            let mut length = 0;
            for index in 1..normalized_pattern.len() {
                while length > 0 && normalized_pattern[index] != normalized_pattern[length] {
                    length = fallback[length - 1];
                }
                if normalized_pattern[index] == normalized_pattern[length] {
                    length += 1;
                }
                fallback[index] = length;
            }
            let mut matched = 0;
            for (offset, line) in lines[search_start..last_start + pattern.len()]
                .iter()
                .enumerate()
            {
                loop {
                    if normalise(line).eq(normalized_pattern[matched].chars()) {
                        matched += 1;
                        break;
                    }
                    if matched == 0 {
                        break;
                    }
                    matched = fallback[matched - 1];
                }
                if matched == pattern.len() {
                    if record(search_start + offset + 1 - pattern.len()) {
                        break;
                    }
                    matched = fallback[matched - 1];
                }
            }
        }
        // Keep the exact pass allocation-free.
        _ => {
            for index in search_start..=last_start {
                if lines[index..index + pattern.len()] == *pattern && record(index) {
                    break;
                }
            }
        }
    }
    found
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Ambiguous {strictness} match at lines {first_line} and {second_line}; add more context to select one location"
)]
pub(crate) struct AmbiguousMatch {
    pub(crate) first_line: usize,
    pub(crate) second_line: usize,
    pub(crate) strictness: &'static str,
}
