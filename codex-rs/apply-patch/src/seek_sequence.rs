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
        "Unicode-normalized" => {
            let normalized_pattern = pattern
                .iter()
                .map(|line| normalise(line).collect::<Vec<_>>())
                .collect::<Vec<_>>();
            for index in search_start..=last_start {
                if lines[index..index + pattern.len()]
                    .iter()
                    .zip(&normalized_pattern)
                    .all(|(line, normalized)| normalise(line).eq(normalized.iter().copied()))
                    && record(index)
                {
                    break;
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

#[cfg(test)]
mod tests {
    use super::AmbiguousMatch;
    use super::ambiguous_candidates;
    use super::seek_sequence;
    use std::string::ToString;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        let pattern = to_vec(&["bar", "baz"]);
        assert_eq!(
            seek_sequence(
                &lines, &pattern, /*start*/ 0, /*eof*/ false, /*anchored*/ false
            )
            .unwrap(),
            Some(1)
        );
    }

    #[test]
    fn test_duplicate_matches_report_candidates_at_each_tier() {
        for (candidates, pattern, strictness) in [
            (["old", "old"], "old", "exact"),
            (["old  ", "old\t"], "old", "trailing-whitespace"),
            (["  old", "    old"], "old", "whitespace"),
            (["‘old’", "‘old’"], "'old'", "Unicode-normalized"),
        ] {
            let lines = to_vec(&["prefix", candidates[0], "gap", candidates[1]]);
            assert_eq!(
                seek_sequence(&lines, &to_vec(&[pattern]), 0, false, false),
                Err(AmbiguousMatch {
                    first_line: 2,
                    second_line: 4,
                    strictness,
                })
            );
            // After an earlier match anchors the search, the nearest
            // candidate is the intended one, at the same tier.
            assert_eq!(
                seek_sequence(&lines, &to_vec(&[pattern]), 1, false, true),
                Ok(Some(1))
            );
            assert_eq!(
                seek_sequence(&lines, &to_vec(&[pattern]), 2, false, true),
                Ok(Some(3))
            );
        }
    }

    #[test]
    fn ambiguous_candidates_list_every_match_at_the_reported_tier() {
        // Three whitespace-tier matches and one exact match: the exact match
        // wins, so only the whitespace tier is ambiguous when it is absent.
        let lines = to_vec(&["  old", "x", " old", "y", "\told", "old"]);
        let pattern = to_vec(&["old"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false, false), Ok(Some(5)));
        let ambiguity = seek_sequence(&lines[..5], &pattern, 0, false, false).unwrap_err();
        assert_eq!(
            ambiguity,
            AmbiguousMatch {
                first_line: 1,
                second_line: 3,
                strictness: "whitespace",
            }
        );
        assert_eq!(
            ambiguous_candidates(&lines[..5], &pattern, 0, false, &ambiguity, 8),
            vec![0, 2, 4]
        );
        assert_eq!(
            ambiguous_candidates(&lines[..5], &pattern, 1, false, &ambiguity, 8),
            vec![2, 4]
        );
        assert_eq!(
            ambiguous_candidates(&lines[..5], &pattern, 0, false, &ambiguity, 2),
            vec![0, 2]
        );
    }

    #[test]
    fn test_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        // Pattern omits trailing whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(
                &lines, &pattern, /*start*/ 0, /*eof*/ false, /*anchored*/ false
            )
            .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn test_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        // Pattern omits any additional whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(
                &lines, &pattern, /*start*/ 0, /*eof*/ false, /*anchored*/ false
            )
            .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn test_pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic – must return None when pattern cannot possibly fit.
        assert_eq!(
            seek_sequence(
                &lines, &pattern, /*start*/ 0, /*eof*/ false, /*anchored*/ false
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn test_eof_match_respects_start() {
        let lines = to_vec(&["a", "b", "c"]);
        let pattern = to_vec(&["c"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, 2, true, false).unwrap(),
            Some(2)
        );
        assert_eq!(
            seek_sequence(&lines, &pattern, 3, true, false).unwrap(),
            None
        );
        assert_eq!(
            seek_sequence(&lines, &to_vec(&["b"]), 0, true, false).unwrap(),
            None
        );
    }

    #[test]
    fn test_exact_match_precedes_unicode_fallback() {
        let lines = to_vec(&["‘early’—value", "'early'-value"]);
        let pattern = to_vec(&["'early'-value"]);
        for anchored in [false, true] {
            assert_eq!(
                seek_sequence(&lines, &pattern, 0, false, anchored).unwrap(),
                Some(1)
            );
        }
        assert_eq!(
            seek_sequence(&lines[..1], &pattern, 0, false, false).unwrap(),
            Some(0)
        );
    }
}
