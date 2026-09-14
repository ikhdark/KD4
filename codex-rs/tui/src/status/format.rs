use ratatui::prelude::*;
use ratatui::style::Stylize;
use std::collections::BTreeSet;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub(crate) struct FieldFormatter {
    indent: &'static str,
    label_width: usize,
    value_offset: usize,
    value_indent: String,
}

impl FieldFormatter {
    pub(crate) const INDENT: &'static str = " ";

    pub(crate) fn from_labels<S>(labels: impl IntoIterator<Item = S>) -> Self
    where
        S: AsRef<str>,
    {
        let label_width = labels
            .into_iter()
            .map(|label| UnicodeWidthStr::width(label.as_ref()))
            .max()
            .unwrap_or(0);
        let indent_width = UnicodeWidthStr::width(Self::INDENT);
        let value_offset = indent_width + label_width + 1 + 3;

        Self {
            indent: Self::INDENT,
            label_width,
            value_offset,
            value_indent: " ".repeat(value_offset),
        }
    }

    pub(crate) fn line(
        &self,
        label: &'static str,
        value_spans: Vec<Span<'static>>,
    ) -> Line<'static> {
        Line::from(self.full_spans(label, value_spans))
    }

    pub(crate) fn continuation(&self, mut spans: Vec<Span<'static>>) -> Line<'static> {
        let mut all_spans = Vec::with_capacity(spans.len() + 1);
        all_spans.push(Span::from(self.value_indent.clone()).dim());
        all_spans.append(&mut spans);
        Line::from(all_spans)
    }

    pub(crate) fn value_width(&self, available_inner_width: usize) -> usize {
        available_inner_width.saturating_sub(self.value_offset)
    }

    pub(crate) fn full_spans(
        &self,
        label: &str,
        mut value_spans: Vec<Span<'static>>,
    ) -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(value_spans.len() + 1);
        spans.push(self.label_span(label));
        spans.append(&mut value_spans);
        spans
    }

    fn label_span(&self, label: &str) -> Span<'static> {
        let mut buf = String::with_capacity(self.value_offset);
        buf.push_str(self.indent);

        buf.push_str(label);
        buf.push(':');

        let label_width = UnicodeWidthStr::width(label);
        let padding = 3 + self.label_width.saturating_sub(label_width);
        for _ in 0..padding {
            buf.push(' ');
        }

        Span::from(buf).dim()
    }
}

pub(crate) fn push_label(labels: &mut Vec<String>, seen: &mut BTreeSet<String>, label: &str) {
    if seen.contains(label) {
        return;
    }

    let owned = label.to_string();
    seen.insert(owned.clone());
    labels.push(owned);
}

pub(crate) fn line_display_width(line: &Line<'static>) -> usize {
    line.iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

pub(crate) fn truncate_line_to_width(mut line: Line<'static>, max_width: usize) -> Line<'static> {
    if max_width == 0 {
        line.spans.clear();
        return line;
    }

    let mut used = 0usize;
    let mut spans_out: Vec<Span<'static>> = Vec::new();

    for span in std::mem::take(&mut line.spans) {
        let text = span.content.as_ref();
        let style = span.style;
        let span_width = UnicodeWidthStr::width(text);

        if span_width == 0 {
            spans_out.push(span);
            continue;
        }

        if used >= max_width {
            break;
        }

        if used + span_width <= max_width {
            used += span_width;
            spans_out.push(span);
            continue;
        }

        let mut truncated = String::new();
        for ch in text.graphemes(true) {
            let ch_width = UnicodeWidthStr::width(ch);
            if used + ch_width > max_width {
                break;
            }
            truncated.push_str(ch);
            used += ch_width;
        }

        if !truncated.is_empty() {
            spans_out.push(Span::styled(truncated, style));
        }

        break;
    }

    line.spans = spans_out;
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_graphemes_styles_and_alignment() {
        let line = Line::from(vec![Span::raw("👩‍💻x").bold()])
            .cyan()
            .right_aligned();
        let truncated = truncate_line_to_width(line.clone(), 2);
        assert_eq!(truncated.to_string(), "👩‍💻");
        assert_eq!(truncated.style, line.style);
        assert_eq!(truncated.alignment, line.alignment);
        assert_eq!(truncated.spans[0].style, line.spans[0].style);
        let empty = truncate_line_to_width(line.clone(), 0);
        assert!(empty.spans.is_empty());
        assert_eq!(empty.style, line.style);
        assert_eq!(empty.alignment, line.alignment);
    }

    #[test]
    fn retained_spans_keep_borrowed_content() {
        let truncated = truncate_line_to_width(Line::from("label"), 5);
        assert_eq!(
            truncated.spans[0].content,
            std::borrow::Cow::Borrowed("label")
        );
        assert!(matches!(
            truncated.spans[0].content,
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
