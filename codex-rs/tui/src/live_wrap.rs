use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A single visual row produced by RowBuilder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    /// True if this row ends with an explicit line break (as opposed to a hard wrap).
    pub explicit_break: bool,
}

impl Row {
    pub fn width(&self) -> usize {
        self.text.width()
    }
}

/// Incrementally wraps input text at grapheme boundaries. A single grapheme
/// wider than `width` occupies its own row so no input is lost.
///
/// Step 1: plain-text only. ANSI-carry and styled spans will be added later.
pub struct RowBuilder {
    target_width: usize,
    /// Buffer for the current logical line (until a '\n' is seen).
    current_line: String,
    /// Output rows built so far for the current logical line and previous ones.
    rows: Vec<Row>,
}

impl RowBuilder {
    pub fn new(target_width: usize) -> Self {
        Self {
            target_width: target_width.max(1),
            current_line: String::new(),
            rows: Vec::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.target_width
    }

    pub fn set_width(&mut self, width: usize) {
        let width = width.max(1);
        if self.target_width == width {
            return;
        }
        self.target_width = width;
        // Rewrap everything we have (simple approach for Step 1).
        let mut all = String::new();
        for row in self.rows.drain(..) {
            all.push_str(&row.text);
            if row.explicit_break {
                all.push('\n');
            }
        }
        all.push_str(&self.current_line);
        self.current_line.clear();
        self.push_fragment(&all);
    }

    /// Push an input fragment. May contain newlines.
    pub fn push_fragment(&mut self, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        let mut start = 0usize;
        for (i, ch) in fragment.char_indices() {
            if ch == '\n' {
                // Flush anything pending before the newline.
                if start < i {
                    self.current_line.push_str(&fragment[start..i]);
                }
                self.flush_current_line(/*explicit_break*/ true);
                start = i + ch.len_utf8();
            }
        }
        if start < fragment.len() {
            self.current_line.push_str(&fragment[start..]);
            self.wrap_current_line();
        }
    }

    /// Mark the end of the current logical line (equivalent to pushing a '\n').
    pub fn end_line(&mut self) {
        self.flush_current_line(/*explicit_break*/ true);
    }

    /// Return a snapshot of produced rows (non-draining).
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Rows suitable for display, including the current partial line if any.
    pub fn display_rows(&self) -> Vec<Row> {
        let mut out = self.rows.clone();
        if !self.current_line.is_empty() {
            out.push(Row {
                text: self.current_line.clone(),
                explicit_break: false,
            });
        }
        out
    }

    /// Drain the oldest rows that exceed `max_keep` display rows (including the
    /// current partial line, if any). Returns the drained rows in order.
    pub fn drain_commit_ready(&mut self, max_keep: usize) -> Vec<Row> {
        let display_count = self.rows.len() + if self.current_line.is_empty() { 0 } else { 1 };
        if display_count <= max_keep {
            return Vec::new();
        }
        let to_commit = display_count - max_keep;
        let commit_count = to_commit.min(self.rows.len());
        self.rows.drain(..commit_count).collect()
    }

    fn flush_current_line(&mut self, explicit_break: bool) {
        // Wrap any remaining content in the current line and then finalize with explicit_break.
        self.wrap_current_line();
        // If the current line ended exactly on a width boundary and is non-empty, represent
        // the explicit break as an empty explicit row so that fragmentation invariance holds.
        if explicit_break {
            if self.current_line.is_empty() {
                // We ended on a boundary previously; add an empty explicit row.
                self.rows.push(Row {
                    text: String::new(),
                    explicit_break: true,
                });
            } else {
                // There is leftover content that did not wrap yet; push it now with the explicit flag.
                let mut s = String::new();
                std::mem::swap(&mut s, &mut self.current_line);
                self.rows.push(Row {
                    text: s,
                    explicit_break: true,
                });
            }
        }
        // Reset current line buffer for next logical line.
        self.current_line.clear();
    }

    fn wrap_current_line(&mut self) {
        let mut start = 0;
        let mut width = 0usize;
        for (idx, grapheme) in self.current_line.grapheme_indices(true) {
            let next_width = grapheme.width();
            if idx > start && width.saturating_add(next_width) > self.target_width {
                self.rows.push(Row {
                    text: self.current_line[start..idx].to_string(),
                    explicit_break: false,
                });
                start = idx;
                width = 0;
            }
            width += next_width;
        }
        // Keep the last row, including its final grapheme, available for the
        // next fragment (which may extend it with an accent or emoji joiner).
        if start > 0 {
            self.current_line.drain(..start);
        }
    }
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

    #[test]
    fn rows_do_not_exceed_width_ascii() {
        let mut rb = RowBuilder::new(/*target_width*/ 10);
        rb.push_fragment("hello whirl this is a test");
        let rows = rb.rows().to_vec();
        assert_eq!(
            rows,
            vec![
                Row {
                    text: "hello whir".to_string(),
                    explicit_break: false
                },
                Row {
                    text: "l this is ".to_string(),
                    explicit_break: false
                }
            ]
        );
    }

    #[test]
    fn rows_do_not_exceed_width_emoji_cjk() {
        // 😀 is width 2; 你/好 are width 2.
        let mut rb = RowBuilder::new(/*target_width*/ 6);
        rb.push_fragment("😀😀 你好");
        let rows = rb.rows().to_vec();
        // At width 6, we expect the first row to fit exactly two emojis and a space
        // (2 + 2 + 1 = 5) plus one more column for the first CJK char (2 would overflow),
        // so only the two emojis and the space fit; the rest remains buffered.
        assert_eq!(
            rows,
            vec![Row {
                text: "😀😀 ".to_string(),
                explicit_break: false
            }]
        );
    }

    #[test]
    fn fragmentation_invariance_long_token() {
        let s = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"; // 26 chars
        let mut rb_all = RowBuilder::new(/*target_width*/ 7);
        rb_all.push_fragment(s);
        let all_rows = rb_all.rows().to_vec();

        let mut rb_chunks = RowBuilder::new(/*target_width*/ 7);
        for i in (0..s.len()).step_by(3) {
            let end = (i + 3).min(s.len());
            rb_chunks.push_fragment(&s[i..end]);
        }
        let chunk_rows = rb_chunks.rows().to_vec();

        assert_eq!(
            all_rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["ABCDEFG", "HIJKLMN", "OPQRSTU"]
        );
        assert_eq!(all_rows, chunk_rows);
    }

    #[test]
    fn newline_splits_rows() {
        let mut rb = RowBuilder::new(/*target_width*/ 10);
        rb.push_fragment("hello\nworld");
        let rows = rb.display_rows();
        assert_eq!(
            rows,
            vec![
                Row {
                    text: "hello".into(),
                    explicit_break: true
                },
                Row {
                    text: "world".into(),
                    explicit_break: false
                }
            ]
        );
    }

    #[test]
    fn rewrap_on_width_change() {
        let mut rb = RowBuilder::new(/*target_width*/ 10);
        rb.push_fragment("abcdefghijK");
        assert_eq!(rb.rows()[0].text, "abcdefghij");
        rb.set_width(/*width*/ 5);
        assert_eq!(
            rb.display_rows()
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["abcde", "fghij", "K"]
        );
    }
    #[test]
    fn fragmented_graphemes_and_oversized_glyphs_keep_all_text() {
        let mut builder = RowBuilder::new(2);
        for fragment in ["👩", "‍", "💻", "e", "\u{301}", "x", "\n"] {
            builder.push_fragment(fragment);
        }
        assert_eq!(
            builder
                .display_rows()
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["👩‍💻", "e\u{301}x"]
        );
        let mut builder = RowBuilder::new(1);
        builder.push_fragment("你a\n");
        assert_eq!(
            builder
                .display_rows()
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["你", "a"]
        );
        assert_eq!(
            take_prefix_by_width("e\u{301}x", 1),
            ("e\u{301}".to_string(), "x", 1)
        );
        assert_eq!(take_prefix_by_width("👩‍💻x", 2), ("👩‍💻".to_string(), "x", 2));
    }
}
