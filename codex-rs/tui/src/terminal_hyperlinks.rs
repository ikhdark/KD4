//! Semantic terminal hyperlinks carried separately from visible TUI text.
//!
//! Layout code measures and wraps ordinary ratatui lines. Hyperlink annotations are applied only
//! when text reaches a terminal buffer or scrollback writer so OSC 8 bytes never affect geometry.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::render::line_utils::line_into_static;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_line;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalHyperlink {
    pub(crate) columns: Range<usize>,
    pub(crate) destination: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct HyperlinkLine {
    pub(crate) line: Line<'static>,
    pub(crate) hyperlinks: Vec<TerminalHyperlink>,
}

impl HyperlinkLine {
    pub(crate) fn new(line: Line<'static>) -> Self {
        Self {
            line,
            hyperlinks: Vec::new(),
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.line.width()
    }

    pub(crate) fn push_span(&mut self, span: Span<'static>, destination: Option<&str>) {
        let start = self.width();
        let end = start + span.content.width();
        self.line.push_span(span);
        if end > start
            && let Some(destination) = destination.and_then(terminal_destination)
        {
            self.hyperlinks.push(TerminalHyperlink {
                columns: start..end,
                destination,
            });
        }
    }

    pub(crate) fn style(mut self, style: ratatui::style::Style) -> Self {
        self.line = self.line.style(style);
        self
    }
}

impl From<Line<'static>> for HyperlinkLine {
    fn from(line: Line<'static>) -> Self {
        Self::new(line)
    }
}

impl From<&'static str> for HyperlinkLine {
    fn from(text: &'static str) -> Self {
        Self::new(Line::from(text))
    }
}

impl From<String> for HyperlinkLine {
    fn from(text: String) -> Self {
        Self::new(Line::from(text))
    }
}

pub(crate) fn visible_lines(lines: Vec<HyperlinkLine>) -> Vec<Line<'static>> {
    lines.into_iter().map(|line| line.line).collect()
}

pub(crate) fn plain_hyperlink_lines(lines: Vec<Line<'static>>) -> Vec<HyperlinkLine> {
    lines.into_iter().map(HyperlinkLine::new).collect()
}

pub(crate) fn prefix_hyperlink_lines(
    lines: Vec<HyperlinkLine>,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
) -> Vec<HyperlinkLine> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let prefix = if index == 0 {
                initial_prefix.clone()
            } else {
                subsequent_prefix.clone()
            };
            let shift = prefix.content.width();
            let mut spans = Vec::with_capacity(line.line.spans.len() + 1);
            spans.push(prefix);
            spans.extend(line.line.spans);
            line.line = Line::from(spans).style(line.line.style);
            for hyperlink in &mut line.hyperlinks {
                hyperlink.columns = hyperlink.columns.start + shift..hyperlink.columns.end + shift;
            }
            line
        })
        .collect()
}

pub(crate) fn adaptive_wrap_hyperlink_lines(
    lines: &[HyperlinkLine],
    options: RtOptions<'static>,
) -> Vec<HyperlinkLine> {
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let options = if index == 0 {
            options.clone()
        } else {
            options
                .clone()
                .initial_indent(options.subsequent_indent.clone())
        };
        out.extend(remap_wrapped_line(
            line,
            adaptive_wrap_line(&line.line, options)
                .into_iter()
                .map(line_into_static)
                .collect(),
        ));
    }
    out
}

pub(crate) fn annotate_web_urls(lines: Vec<Line<'static>>) -> Vec<HyperlinkLine> {
    lines.into_iter().map(annotate_web_urls_in_line).collect()
}

pub(crate) fn annotate_web_urls_in_line(line: Line<'static>) -> HyperlinkLine {
    let hyperlinks = web_links_in_text(&line_text(&line));
    HyperlinkLine { line, hyperlinks }
}

/// Re-attach source hyperlink ranges after visible-text wrapping has split a line.
///
/// Link text is matched in display order so a URL split across table rows retains the complete
/// destination on every rendered fragment. Whitespace inserted or removed at line boundaries is
/// ignored while matching; hyperlink destinations themselves are never reconstructed from output.
pub(crate) fn remap_wrapped_line(
    source: &HyperlinkLine,
    wrapped: Vec<Line<'static>>,
) -> Vec<HyperlinkLine> {
    let mut out = plain_hyperlink_lines(wrapped);
    if source.hyperlinks.is_empty() {
        return out;
    }
    let source_text = line_text(&source.line);
    let mut source_byte = 0usize;
    let mut source_column = 0usize;
    for (index, line) in out.iter_mut().enumerate() {
        if index > 0 {
            let trimmed = source_text[source_byte..].trim_start_matches(char::is_whitespace);
            let skipped = source_text[source_byte..].len() - trimmed.len();
            source_column += source_text[source_byte..source_byte + skipped].width();
            source_byte += skipped;
        }

        let rendered = line_text(&line.line);
        let remaining = &source_text[source_byte..];
        let Some(rendered_start) = longest_suffix_matching_prefix(&rendered, remaining) else {
            continue;
        };
        // The matched suffix is also a prefix of the source text. Borrow it
        // there so updating this line's link ranges does not borrow its text.
        let mapped = &remaining[..rendered.len() - rendered_start];
        let mut output_column = rendered[..rendered_start].width();
        for grapheme in mapped.graphemes(true) {
            let width = grapheme.width();
            if let Some(link) = source
                .hyperlinks
                .iter()
                .find(|link| link.columns.contains(&source_column))
            {
                push_link_range(
                    line,
                    output_column..output_column + width,
                    &link.destination,
                );
            }
            source_column += width;
            output_column += width;
        }
        source_byte += mapped.len();
    }
    out
}

fn line_text<'a>(line: &'a Line<'_>) -> std::borrow::Cow<'a, str> {
    match line.spans.as_slice() {
        [] => std::borrow::Cow::Borrowed(""),
        [span] => std::borrow::Cow::Borrowed(span.content.as_ref()),
        spans => std::borrow::Cow::Owned(spans.iter().map(|span| span.content.as_ref()).collect()),
    }
}

fn longest_suffix_matching_prefix(rendered: &str, source: &str) -> Option<usize> {
    if rendered.is_empty() || source.is_empty() {
        return None;
    }
    if source.starts_with(rendered) {
        return Some(0);
    }
    // KMP finds the longest source prefix matching the rendered suffix. Match
    // bytes so a prefix may end inside a source grapheme; its start in rendered
    // must still be a grapheme boundary, as required by hyperlink remapping.
    let pattern = &source.as_bytes()[..source.len().min(rendered.len())];
    let mut failure = vec![0; pattern.len()];
    for index in 1..pattern.len() {
        let mut matched = failure[index - 1];
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = failure[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        failure[index] = matched;
    }
    let mut matched = 0;
    for &byte in rendered.as_bytes() {
        while matched > 0 && (matched == pattern.len() || byte != pattern[matched]) {
            matched = failure[matched - 1];
        }
        if byte == pattern[matched] {
            matched += 1;
        }
    }
    let mut boundaries = vec![false; rendered.len()];
    for (index, _) in rendered.grapheme_indices(true) {
        boundaries[index] = true;
    }
    while matched > 0 {
        let start = rendered.len() - matched;
        if boundaries[start] {
            return Some(start);
        }
        matched = failure[matched - 1];
    }
    None
}

fn push_link_range(line: &mut HyperlinkLine, range: Range<usize>, destination: &str) {
    if range.is_empty() {
        return;
    }
    if let Some(previous) = line.hyperlinks.last_mut()
        && previous.destination == destination
        && previous.columns.end == range.start
    {
        previous.columns.end = range.end;
        return;
    }
    line.hyperlinks.push(TerminalHyperlink {
        columns: range,
        destination: destination.to_string(),
    });
}

pub(crate) fn web_links_in_text(text: &str) -> Vec<TerminalHyperlink> {
    let mut links = Vec::new();
    let mut measured_byte = 0;
    let mut measured_columns = 0;
    for raw_token in text.split_ascii_whitespace() {
        // split_ascii_whitespace returns slices of this allocation.
        let raw_start = raw_token.as_ptr() as usize - text.as_ptr() as usize;
        let trimmed_start = raw_token
            .find(|ch: char| !is_leading_punctuation(ch))
            .unwrap_or(raw_token.len());
        let trimmed_end = trailing_url_end(&raw_token[trimmed_start..]) + trimmed_start;
        if trimmed_start >= trimmed_end {
            continue;
        }
        let candidate = &raw_token[trimmed_start..trimmed_end];
        let Some(destination) = web_destination(candidate) else {
            continue;
        };
        let candidate_start = raw_start + trimmed_start;
        measured_columns += text[measured_byte..candidate_start].width();
        measured_byte = candidate_start;
        let start = measured_columns;
        let end = start + candidate.width();
        links.push(TerminalHyperlink {
            columns: start..end,
            destination,
        });
    }
    links
}

fn is_leading_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | '.' | ';' | '!' | '\'' | '"'
    )
}

fn trailing_url_end(candidate: &str) -> usize {
    let mut end = candidate.len();
    while end > 0 {
        let remaining = &candidate[..end];
        let Some(ch) = remaining.chars().next_back() else {
            break;
        };
        let trim = matches!(ch, ',' | '.' | ';' | '!' | '\'' | '"')
            || matches!(ch, ')' | ']' | '}' | '>')
                && has_unmatched_closing_delimiter(remaining, ch);
        if !trim {
            break;
        }
        end -= ch.len_utf8();
    }
    end
}

fn has_unmatched_closing_delimiter(candidate: &str, closing: char) -> bool {
    let opening = match closing {
        ')' => '(',
        ']' => '[',
        '}' => '{',
        '>' => '<',
        _ => return false,
    };
    candidate.chars().filter(|ch| *ch == closing).count()
        > candidate.chars().filter(|ch| *ch == opening).count()
}

pub(crate) fn web_destination(destination: &str) -> Option<String> {
    let safe_destination = destination
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let parsed = Url::parse(&safe_destination).ok()?;
    matches!(parsed.scheme(), "http" | "https")
        .then(|| parsed.host_str())
        .flatten()?;
    Some(safe_destination)
}

fn terminal_destination(destination: &str) -> Option<String> {
    if let Some(destination) = web_destination(destination) {
        return Some(destination);
    }

    let safe_destination = destination
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let parsed = Url::parse(&safe_destination).ok()?;
    let is_editor_file_uri = matches!(
        parsed.scheme(),
        "vscode" | "vscode-insiders" | "windsurf" | "cursor"
    ) && parsed.host_str() == Some("file")
        && !parsed.path().is_empty()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    is_editor_file_uri.then_some(safe_destination)
}

#[cfg(test)]
pub(crate) fn osc8_hyperlink(destination: &str, text: &str) -> String {
    let Some(safe_destination) = terminal_destination(destination) else {
        return text.to_string();
    };
    validated_osc8_hyperlink(&safe_destination, text)
}

fn validated_osc8_hyperlink(safe_destination: &str, text: &str) -> String {
    format!("\x1b]8;;{safe_destination}\x07{text}\x1b]8;;\x07")
}

#[cfg(test)]
pub(crate) fn strip_osc8(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut stripped = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b]8;;") {
            index += 5;
            while index < bytes.len() {
                if bytes[index] == b'\x07' {
                    index += 1;
                    break;
                }
                if index + 1 < bytes.len() && bytes[index] == b'\x1b' && bytes[index + 1] == b'\\' {
                    index += 2;
                    break;
                }
                index += 1;
            }
            continue;
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("current byte index starts a character");
        stripped.push(ch);
        index += ch.len_utf8();
    }

    stripped
}

/// Prepare spans for raw scrollback writes, which bypass ratatui's control-character filtering.
///
/// Graphemes containing control characters other than tab are dropped so untrusted text cannot
/// emit terminal sequences; they still advance the column so link ranges stay aligned.
pub(crate) fn decorate_spans(line: &HyperlinkLine) -> Vec<Span<'static>> {
    if line.hyperlinks.is_empty()
        && !line
            .line
            .spans
            .iter()
            .any(|span| span.content.contains(is_scrollback_control))
    {
        return line.line.spans.clone();
    }

    let mut out = Vec::new();
    let mut column = 0usize;
    let mut link_index = 0usize;
    let mut active_link_index = None;
    let mut active_destination: Option<String> = None;
    for span in &line.line.spans {
        for grapheme in span.content.graphemes(true) {
            let width = grapheme.width();
            while line
                .hyperlinks
                .get(link_index)
                .is_some_and(|link| link.columns.end <= column)
            {
                link_index += 1;
            }
            let selected_link_index = line
                .hyperlinks
                .get(link_index)
                .and_then(|link| link.columns.contains(&column).then_some(link_index));
            if active_link_index != selected_link_index {
                if active_destination.is_some() {
                    append_to_last_span(&mut out, "\x1b]8;;\x07");
                }
                active_destination = selected_link_index
                    .and_then(|index| terminal_destination(&line.hyperlinks[index].destination));
                if let Some(destination) = active_destination.as_ref() {
                    push_styled_content(
                        &mut out,
                        &format!("\x1b]8;;{destination}\x07"),
                        span.style,
                    );
                }
                active_link_index = selected_link_index;
            }
            if !grapheme.contains(is_scrollback_control) {
                push_styled_content(&mut out, grapheme, span.style);
            }
            column += width;
        }
    }
    if active_destination.is_some() {
        append_to_last_span(&mut out, "\x1b]8;;\x07");
    }
    out
}

fn is_scrollback_control(ch: char) -> bool {
    ch.is_control() && ch != '\t'
}

fn push_styled_content(out: &mut Vec<Span<'static>>, content: &str, style: ratatui::style::Style) {
    if let Some(last) = out.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(content);
        return;
    }
    out.push(Span::styled(content.to_string(), style));
}

fn append_to_last_span(out: &mut [Span<'static>], content: &str) {
    if let Some(last) = out.last_mut() {
        last.content.to_mut().push_str(content);
    }
}

pub(crate) fn mark_buffer_hyperlinks(
    buf: &mut Buffer,
    area: Rect,
    lines: &[HyperlinkLine],
    scroll_rows: usize,
) {
    if area.is_empty() {
        return;
    }
    let mut logical_row = 0usize;
    for line in lines {
        if logical_row >= scroll_rows.saturating_add(usize::from(area.height)) {
            break;
        }
        let paragraph = Paragraph::new(Text::from(line.line.clone())).wrap(Wrap { trim: false });
        let rendered_height = paragraph.line_count(area.width).max(/*other*/ 1);
        if line.hyperlinks.is_empty() || logical_row + rendered_height <= scroll_rows {
            logical_row += rendered_height;
            continue;
        }

        let layout_area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            area.width,
            u16::try_from(rendered_height).unwrap_or(u16::MAX),
        );
        let mut layout = Buffer::empty(layout_area);
        paragraph.render(layout_area, &mut layout);
        let rendered_lines = (0..layout_area.height)
            .map(|row| {
                let mut text = String::new();
                let mut column = 0;
                while column < layout_area.width {
                    let cell = &layout[(column, row)];
                    if cell.diff_option != CellDiffOption::Skip {
                        text.push_str(cell.symbol());
                    }
                    // Wide graphemes occupy continuation cells with blank symbols.
                    column += cell.symbol().width().max(1) as u16;
                }
                Line::from(text.trim_end().to_string())
            })
            .collect();
        for (row, rendered) in remap_wrapped_line(line, rendered_lines).iter().enumerate() {
            let row = logical_row + row;
            if row < scroll_rows || row - scroll_rows >= usize::from(area.height) {
                continue;
            }
            for link in &rendered.hyperlinks {
                let Some(destination) = terminal_destination(&link.destination) else {
                    continue;
                };
                for column in link.columns.clone() {
                    let x = area.x + column as u16;
                    let y = area.y + (row - scroll_rows) as u16;
                    let cell = &mut buf[(x, y)];
                    if cell.diff_option == CellDiffOption::Skip || cell.symbol().trim().is_empty() {
                        continue;
                    }
                    let symbol = validated_osc8_hyperlink(&destination, cell.symbol());
                    cell.set_symbol(&symbol);
                }
            }
        }
        logical_row += rendered_height;
    }
}

pub(crate) fn mark_url_hyperlink(buf: &mut Buffer, area: Rect, destination: &str) {
    mark_matching_cells(buf, area, destination, |cell| {
        cell.fg == Color::Cyan && cell.modifier.contains(Modifier::UNDERLINED)
    });
}

pub(crate) fn mark_underlined_hyperlink(buf: &mut Buffer, area: Rect, destination: &str) {
    mark_matching_cells(buf, area, destination, |cell| {
        cell.modifier.contains(Modifier::UNDERLINED)
    });
}

fn mark_matching_cells(
    buf: &mut Buffer,
    area: Rect,
    destination: &str,
    matches: impl Fn(&ratatui::buffer::Cell) -> bool,
) {
    let Some(destination) = terminal_destination(destination) else {
        return;
    };
    for position in area.positions() {
        let cell = &mut buf[position];
        if cell.diff_option != CellDiffOption::Skip
            && !cell.symbol().trim().is_empty()
            && matches(cell)
        {
            let symbol = validated_osc8_hyperlink(&destination, cell.symbol());
            cell.set_symbol(&symbol);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn suffix_matching_preserves_grapheme_starts_and_partial_source_graphemes() {
        assert_eq!(
            longest_suffix_matching_prefix("prefix abc", "abcdef"),
            Some(7)
        );
        assert_eq!(longest_suffix_matching_prefix("aaaaab", "aaaaac"), None);
        assert_eq!(
            longest_suffix_matching_prefix("prefix e", "e\u{301}x"),
            Some(7)
        );
        assert_eq!(longest_suffix_matching_prefix("e\u{301}", "\u{301}x"), None);
        assert_eq!(longest_suffix_matching_prefix("😀中", "中x"), Some(4));
        assert_eq!(longest_suffix_matching_prefix("", "abc"), None);
        assert_eq!(longest_suffix_matching_prefix("abc", ""), None);
        let repeated = "a".repeat(20_000) + "b";
        assert_eq!(
            longest_suffix_matching_prefix(&repeated, &("a".repeat(20_000) + "c")),
            None
        );
    }

    #[test]
    fn joined_emoji_preserve_link_columns_and_complete_graphemes() {
        let destination = "https://example.com/";
        let mut source = HyperlinkLine::from("👩‍💻 ");
        source.push_span(Span::raw("👨‍👩‍👧‍👦 link"), Some(destination));
        source.push_span(Span::raw(" end"), None);
        let wrapped = remap_wrapped_line(&source, vec![source.line.clone()]);
        assert_eq!(wrapped[0].hyperlinks[0].columns, 3..10);
        let decorated: String = decorate_spans(&wrapped[0])
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            decorated,
            format!("👩‍💻 {} end", osc8_hyperlink(destination, "👨‍👩‍👧‍👦 link"))
        );
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(source.line.clone()).render(area, &mut buffer);
        mark_buffer_hyperlinks(&mut buffer, area, &[source], 0);
        assert_eq!(buffer[(0, 0)].symbol(), "👩‍💻");
        assert_eq!(buffer[(3, 0)].symbol(), osc8_hyperlink(destination, "👨‍👩‍👧‍👦"));
        assert_eq!(buffer[(6, 0)].symbol(), osc8_hyperlink(destination, "l"));
    }

    #[test]
    fn only_supported_destinations_receive_osc8() {
        assert!(osc8_hyperlink("https://example.com/a", "a").contains("\x1b]8;;"));
        assert!(osc8_hyperlink("cursor://file/workspace/src/main.rs:12", "a").contains("\x1b]8;;"));
        assert_eq!(osc8_hyperlink("mailto:a@example.com", "a"), "a");
        assert_eq!(osc8_hyperlink("cursor://host/workspace/a", "a"), "a");
        assert_eq!(
            osc8_hyperlink("https://example.com/\u{7}safe", "a"),
            "\x1b]8;;https://example.com/safe\x07a\x1b]8;;\x07"
        );
        assert_eq!(
            strip_osc8(&osc8_hyperlink("https://example.com/a", "visible")),
            "visible"
        );
    }

    #[test]
    fn discovers_punctuated_web_url_columns() {
        assert_eq!(
            web_links_in_text("See (https://example.com/a)."),
            vec![TerminalHyperlink {
                columns: 5..26,
                destination: "https://example.com/a".to_string(),
            }]
        );
    }

    #[test]
    fn preserves_balanced_parentheses_in_bare_web_urls() {
        let destination = "https://en.wikipedia.org/wiki/Function_(mathematics)";
        assert_eq!(
            web_links_in_text(&format!("See ({destination}).")),
            vec![TerminalHyperlink {
                columns: 5..5 + destination.width(),
                destination: destination.to_string(),
            }]
        );
    }

    #[test]
    fn decorates_a_contiguous_web_link_with_one_osc8_pair() {
        let destination = "https://example.com/a/very/long/path";
        let line = HyperlinkLine {
            line: Line::from(destination),
            hyperlinks: vec![TerminalHyperlink {
                columns: 0..destination.width(),
                destination: destination.to_string(),
            }],
        };

        assert_eq!(
            decorate_spans(&line),
            vec![Span::from(osc8_hyperlink(destination, destination))]
        );
        assert_eq!(
            decorate_spans(&HyperlinkLine::new(Line::from("not linked"))),
            vec![Span::from("not linked")]
        );
    }

    #[test]
    fn decorated_spans_drop_terminal_controls_and_keep_link_columns() {
        let destination = "https://example.com/";
        let mut line = HyperlinkLine::from("a\x1b[2J\tb\u{9b}0m ");
        line.push_span(Span::raw("link"), Some(destination));
        let decorated: String = decorate_spans(&line)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            decorated,
            format!("a[2J\tb0m {}", osc8_hyperlink(destination, "link"))
        );

        let unlinked: String = decorate_spans(&HyperlinkLine::from("x\x1b]0;title\x07y"))
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(unlinked, "x]0;titley");
    }

    #[test]
    fn wrapping_maps_repeated_link_labels_by_source_position() {
        let mut source = HyperlinkLine::new(Line::from("here here"));
        source.hyperlinks.push(TerminalHyperlink {
            columns: 5..9,
            destination: "https://example.com".to_string(),
        });

        let wrapped = remap_wrapped_line(&source, vec![Line::from("here here")]);

        assert_eq!(
            wrapped[0].hyperlinks,
            vec![TerminalHyperlink {
                columns: 5..9,
                destination: "https://example.com".to_string(),
            }]
        );
    }

    #[test]
    fn buffer_hyperlinks_follow_word_wrapping() {
        let destination = "https://example.com/path";
        let mut line = HyperlinkLine::new(Line::from(format!("See {destination} now")));
        line.hyperlinks.push(TerminalHyperlink {
            columns: 4..4 + destination.width(),
            destination: destination.to_string(),
        });
        let area = Rect::new(
            /*x*/ 0, /*y*/ 0, /*width*/ 18, /*height*/ 4,
        );
        let mut buf = Buffer::empty(area);

        Paragraph::new(Text::from(line.line.clone()))
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        mark_buffer_hyperlinks(&mut buf, area, &[line], /*scroll_rows*/ 0);

        let linked_text = area
            .positions()
            .filter_map(|position| {
                let symbol = buf[position].symbol();
                symbol
                    .contains(&format!("\x1b]8;;{destination}\x07"))
                    .then(|| strip_osc8(symbol))
            })
            .collect::<String>();
        assert_eq!(linked_text, destination);
    }
}
