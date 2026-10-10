use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Byte ranges of terminal-wrapped rows in one logical line. Empty lines occupy
/// one row; an oversized grapheme stays intact. No hard breaks are inserted.
pub(crate) fn terminal_row_ranges(
    text: &str,
    target_width: usize,
) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let target_width = target_width.max(1);
    let mut graphemes = text.grapheme_indices(true).peekable();
    let mut next_start = Some(0);
    std::iter::from_fn(move || {
        let start = next_start.take()?;
        let mut width = 0usize;
        while let Some(&(idx, grapheme)) = graphemes.peek() {
            let next_width = grapheme.width();
            if idx > start && width.saturating_add(next_width) > target_width {
                next_start = Some(idx);
                return Some(start..idx);
            }
            width += next_width;
            graphemes.next();
        }
        Some(start..text.len())
    })
}

/// Take a prefix of `text` whose visible width is at most `max_cols`.
/// Returns (prefix, suffix, prefix_width).
pub fn take_prefix_by_width(text: &str, max_cols: usize) -> (String, &str, usize) {
    if max_cols == 0 || text.is_empty() {
        return (String::new(), text, 0);
    }
    let mut cols = 0usize;
    let mut end_idx = 0usize;
    for (i, grapheme) in text.grapheme_indices(true) {
        let ch_width = grapheme.width();
        if cols.saturating_add(ch_width) > max_cols {
            break;
        }
        cols += ch_width;
        end_idx = i + grapheme.len();
    }
    let prefix = text[..end_idx].to_string();
    let suffix = &text[end_idx..];
    (prefix, suffix, cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn rows(text: &str, width: usize) -> Vec<&str> {
        terminal_row_ranges(text, width)
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn terminal_rows_break_at_the_width_without_losing_text() {
        assert_eq!(
            rows("hello whirl this is a test", 10),
            vec!["hello whir", "l this is ", "a test"]
        );
        assert_eq!(
            rows("ABCDEFGHIJKLMNOPQRSTUVWXYZ", 7),
            vec!["ABCDEFG", "HIJKLMN", "OPQRSTU", "VWXYZ"]
        );
    }

    #[test]
    fn terminal_rows_keep_wide_glyphs_and_graphemes_intact() {
        // 😀, 你 and 好 are two columns wide, so only "😀😀 " fits in six columns.
        assert_eq!(rows("😀😀 你好", 6), vec!["😀😀 ", "你好"]);
        assert_eq!(rows("👩‍💻e\u{301}x", 2), vec!["👩‍💻", "e\u{301}x"]);
        // A glyph wider than the row still gets a row of its own.
        assert_eq!(rows("你a", 1), vec!["你", "a"]);
    }

    #[test]
    fn terminal_rows_give_an_empty_line_one_row_and_clamp_zero_width() {
        assert_eq!(terminal_row_ranges("", 5).collect::<Vec<_>>(), vec![0..0]);
        assert_eq!(rows("ab", 0), vec!["a", "b"]);
    }

    #[test]
    fn take_prefix_by_width_stops_at_grapheme_boundaries() {
        assert_eq!(
            take_prefix_by_width("e\u{301}x", 1),
            ("e\u{301}".to_string(), "x", 1)
        );
        assert_eq!(take_prefix_by_width("👩‍💻x", 2), ("👩‍💻".to_string(), "x", 2));
    }
}
