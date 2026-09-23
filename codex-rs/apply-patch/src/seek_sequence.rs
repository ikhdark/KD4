/// Attempt to find the sequence of `pattern` lines within `lines` beginning at or after `start`.
/// Returns the first starting index or `None` if not found. Matches use
/// decreasing strictness: exact match, then ignoring trailing whitespace, then ignoring leading
/// and trailing whitespace, then normalizing Unicode punctuation and whitespace.
/// When `eof` is true, the match must end at end-of-file
/// and must still begin at or after `start`.
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

    type NormalizeLine = fn(&str) -> &str;
    let tiers: [(&str, NormalizeLine); 4] = [
        ("exact", |line| line),
        ("trailing-whitespace", str::trim_end),
        ("whitespace", str::trim),
        ("Unicode-normalized", str::trim),
    ];
    for (strictness, prepare) in tiers {
        // Borrow trimmed views once per tier instead of trimming each line pair
        // again for every overlapping candidate. Keep the exact pass allocation-free.
        let prepared = matches!(strictness, "trailing-whitespace" | "whitespace").then(|| {
            (
                lines[search_start..]
                    .iter()
                    .map(|line| prepare(line))
                    .collect::<Vec<_>>(),
                pattern.iter().map(|line| prepare(line)).collect::<Vec<_>>(),
            )
        });
        let normalized_pattern = (strictness == "Unicode-normalized").then(|| {
            pattern
                .iter()
                .map(|line| normalise(line).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        });
        for index in search_start..=last_start {
            if lines[index..index + pattern.len()]
                .iter()
                .zip(pattern)
                .enumerate()
                .all(|(offset, (line, pattern))| match &normalized_pattern {
                    Some(normalized) => normalise(line).eq(normalized[offset].iter().copied()),
                    None => match &prepared {
                        Some((lines, pattern)) => {
                            lines[index - search_start + offset] == pattern[offset]
                        }
                        None => line == pattern,
                    },
                })
            {
                return Ok(Some(index));
            }
        }
    }

    Ok(None)
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Ambiguous {strictness} match at lines {first_line} and {second_line}; add more context to select one location"
)]
pub(crate) struct AmbiguousMatch {
    pub(crate) first_line: usize,
    pub(crate) second_line: usize,
    strictness: &'static str,
}

#[cfg(test)]
mod tests {
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
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false).unwrap(),
            Some(1)
        );
    }

    #[test]
    fn test_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        // Pattern omits trailing whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn test_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        // Pattern omits any additional whitespace.
        let pattern = to_vec(&["foo", "bar"]);
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn test_pattern_longer_than_input_returns_none() {
        let lines = to_vec(&["just one line"]);
        let pattern = to_vec(&["too", "many", "lines"]);
        // Should not panic – must return None when pattern cannot possibly fit.
        assert_eq!(
            seek_sequence(&lines, &pattern, /*start*/ 0, /*eof*/ false).unwrap(),
            None
        );
    }

    #[test]
    fn test_eof_match_respects_start() {
        let lines = to_vec(&["a", "b", "c"]);
        let pattern = to_vec(&["c"]);
        assert_eq!(seek_sequence(&lines, &pattern, 2, true).unwrap(), Some(2));
        assert_eq!(seek_sequence(&lines, &pattern, 3, true).unwrap(), None);
        assert_eq!(
            seek_sequence(&lines, &to_vec(&["b"]), 0, true).unwrap(),
            None
        );
    }

    #[test]
    fn test_exact_match_precedes_unicode_fallback() {
        let lines = to_vec(&["‘early’—value", "'early'-value"]);
        let pattern = to_vec(&["'early'-value"]);
        assert_eq!(seek_sequence(&lines, &pattern, 0, false).unwrap(), Some(1));
        assert_eq!(
            seek_sequence(&lines[..1], &pattern, 0, false).unwrap(),
            Some(0)
        );
    }
}
