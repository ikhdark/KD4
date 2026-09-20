use ratatui::text::Line;
use ratatui::text::Span;

/// Clone a borrowed ratatui `Line` into an owned `'static` line.
pub fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                style: s.style,
                content: std::borrow::Cow::Owned(s.content.to_string()),
            })
            .collect(),
    }
}

/// Consume a line, retaining owned span strings and copying only borrowed content.
pub fn line_into_static(line: Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .into_iter()
            .map(|span| Span {
                style: span.style,
                content: std::borrow::Cow::Owned(span.content.into_owned()),
            })
            .collect(),
    }
}

/// Append lines by moving their owned content into `out`.
pub fn push_owned_lines(src: Vec<Line<'_>>, out: &mut Vec<Line<'static>>) {
    out.extend(src.into_iter().map(line_into_static));
}

/// Consider a line blank if it has no spans or only spans whose contents are
/// empty or consist solely of spaces (no tabs/newlines).
#[cfg(test)]
pub fn is_blank_line_spaces_only(line: &Line<'_>) -> bool {
    if line.spans.is_empty() {
        return true;
    }
    line.spans
        .iter()
        .all(|s| s.content.is_empty() || s.content.chars().all(|c| c == ' '))
}

/// Prefix each line with `initial_prefix` for the first line and
/// `subsequent_prefix` for following lines. Returns a new Vec of owned lines.
pub fn prefix_lines(
    lines: Vec<Line<'static>>,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut spans = Vec::with_capacity(l.spans.len() + 1);
            spans.push(if i == 0 {
                initial_prefix.clone()
            } else {
                subsequent_prefix.clone()
            });
            spans.extend(l.spans);
            Line { spans, ..l }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Stylize;

    #[test]
    fn consuming_line_retains_owned_strings_and_style() {
        let content = String::from("owned body");
        let allocation = content.as_ptr();
        let line = Line::from(vec![Span::from(content).bold(), Span::from(" borrowed")])
            .cyan()
            .right_aligned();
        let expected = line.clone();
        let owned = line_into_static(line);
        assert_eq!(owned, expected);
        assert_eq!(owned.spans[0].content.as_ptr(), allocation);
    }

    #[test]
    fn prefixes_preserve_line_metadata() {
        let line = Line::from("body").cyan().right_aligned();
        let prefixed = prefix_lines(vec![line.clone(), line.clone()], "> ".into(), "  ".into());
        assert_eq!(prefixed[0].to_string(), "> body");
        assert_eq!(prefixed[1].to_string(), "  body");
        for result in prefixed {
            assert_eq!(result.style, line.style);
            assert_eq!(result.alignment, line.alignment);
        }
    }
}
