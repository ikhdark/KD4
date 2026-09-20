//! Vertical key/value rendering for markdown tables that no longer scan well as grids.

use super::TABLE_BODY_SEPARATOR_CHAR;
use super::TableCell;
use super::TableColumnKind;
use super::TableColumnMetrics;
use crate::render::line_utils::line_into_static;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::remap_wrapped_line;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

const FIELD_LEADING_PADDING: usize = 1;
const FIELD_GAP: usize = 2;
const MIN_VALUE_WIDTH: usize = 3;
const MIN_ALIGNED_COMPACT_VALUE_WIDTH: usize = 12;
const MIN_ALIGNED_EXPANSIVE_VALUE_WIDTH: usize = 24;
const MIN_SCANNABLE_NARRATIVE_WIDTH: usize = 12;
const MIN_SCANNABLE_TOKEN_HEAVY_WIDTH: usize = 12;
const CRAMPED_EXPANSIVE_CELL_LINES: usize = 4;
const CATASTROPHIC_NARRATIVE_CELL_LINES: usize = 7;
const STACKED_VALUE_INDENT: usize = 2;

/// Switch modes after enough records contain values the grid can no longer
/// present in useful chunks or expansive content collapses into tall strips.
pub(super) fn should_render_records(
    rows: &[Vec<TableCell>],
    column_widths: &[usize],
    metrics: &[TableColumnMetrics],
) -> bool {
    if rows.is_empty() {
        return false;
    }

    let threshold = if rows.len() == 1 {
        1
    } else {
        2.max(rows.len().div_ceil(3))
    };
    let affected_rows = rows
        .iter()
        .filter(|row| {
            let contains_fragmented_value =
                row.iter()
                    .zip(column_widths)
                    .zip(metrics)
                    .any(|((cell, width), metrics)| {
                        if metrics.kind == TableColumnKind::Narrative
                            || (metrics.kind == TableColumnKind::TokenHeavy
                                && *width >= MIN_SCANNABLE_TOKEN_HEAVY_WIDTH)
                        {
                            return false;
                        }
                        let has_fragmented_token = cell
                            .plain_text()
                            .split_whitespace()
                            .any(|token| token.width() > *width);
                        match metrics.kind {
                            TableColumnKind::Compact => has_fragmented_token,
                            TableColumnKind::TokenHeavy => {
                                *width < MIN_SCANNABLE_TOKEN_HEAVY_WIDTH && has_fragmented_token
                            }
                            TableColumnKind::Narrative => false,
                        }
                    });

            contains_fragmented_value || expansive_cells_are_starved(row, column_widths, metrics)
        })
        .take(threshold)
        .count();

    affected_rows >= threshold
}

fn expansive_cells_are_starved(
    row: &[TableCell],
    column_widths: &[usize],
    metrics: &[TableColumnMetrics],
) -> bool {
    let mut cramped_cells = 0;
    for ((cell, width), metrics) in row
        .iter()
        .zip(column_widths)
        .zip(metrics)
        .filter(|&((_cell, _width), metrics)| metrics.kind != TableColumnKind::Compact)
    {
        // Use exactly the renderer's wrapping without owning lines or remapping links.
        let height = cell
            .lines
            .iter()
            .map(|line| {
                word_wrap_line(&line.line, RtOptions::new((*width).max(1)))
                    .len()
                    .max(1)
            })
            .sum::<usize>()
            .max(1);
        if height >= CRAMPED_EXPANSIVE_CELL_LINES {
            cramped_cells += 1;
        }
        if cramped_cells >= 2
            || (metrics.kind == TableColumnKind::Narrative
                && *width < MIN_SCANNABLE_NARRATIVE_WIDTH
                && height >= CATASTROPHIC_NARRATIVE_CELL_LINES)
        {
            return true;
        }
    }
    false
}

pub(super) fn render_records(
    headers: &[TableCell],
    rows: &[Vec<TableCell>],
    metrics: &[TableColumnMetrics],
    available_width: Option<usize>,
    label_style: Style,
    separator_style: Style,
) -> Vec<HyperlinkLine> {
    let label_width = headers
        .iter()
        .map(|header| header.plain_text().width())
        .max()
        .unwrap_or(0);
    let minimum_value_width = if metrics
        .iter()
        .any(|metrics| metrics.kind != TableColumnKind::Compact)
    {
        MIN_ALIGNED_EXPANSIVE_VALUE_WIDTH
    } else {
        MIN_ALIGNED_COMPACT_VALUE_WIDTH
    };
    let aligned_fields = available_width.is_none_or(|width| {
        FIELD_LEADING_PADDING + label_width + FIELD_GAP + minimum_value_width <= width
    });
    let mut out = Vec::new();
    let mut widest_record = 0;

    for (row_index, row) in rows.iter().enumerate() {
        let record_start = out.len();
        for (header, value) in headers.iter().zip(row) {
            if aligned_fields {
                render_aligned_field(
                    &mut out,
                    header,
                    value,
                    label_width,
                    available_width,
                    label_style,
                );
            } else {
                render_stacked_field(&mut out, header, value, available_width, label_style);
            }
        }
        if row_index + 1 < rows.len() {
            let width = available_width.unwrap_or_else(|| {
                widest_record = widest_record.max(widest_line_width(&out[record_start..]));
                widest_record
            });
            out.push(HyperlinkLine::new(Line::from(Span::styled(
                TABLE_BODY_SEPARATOR_CHAR.to_string().repeat(width),
                separator_style,
            ))));
        }
    }

    out
}

fn render_aligned_field(
    out: &mut Vec<HyperlinkLine>,
    header: &TableCell,
    value: &TableCell,
    label_width: usize,
    available_width: Option<usize>,
    label_style: Style,
) {
    let value_indent = FIELD_LEADING_PADDING + label_width + FIELD_GAP;
    let value_width = available_width
        .map(|width| width.saturating_sub(value_indent).max(MIN_VALUE_WIDTH))
        .unwrap_or_else(|| cell_width(value).max(MIN_VALUE_WIDTH));
    let wrapped_value = wrap_cell(value, value_width);
    for (line_index, value_line) in wrapped_value.into_iter().enumerate() {
        let mut prefix = HyperlinkLine::default();
        if line_index == 0 {
            prefix.push_span(Span::raw(" ".repeat(FIELD_LEADING_PADDING)), None);
            append_line(&mut prefix, header_label(header, label_style));
            prefix.push_span(
                Span::raw(
                    " ".repeat(label_width.saturating_sub(header.plain_text().width()) + FIELD_GAP),
                ),
                None,
            );
        } else {
            prefix.push_span(Span::raw(" ".repeat(value_indent)), None);
        }
        append_line(&mut prefix, value_line);
        out.push(prefix);
    }
}

fn render_stacked_field(
    out: &mut Vec<HyperlinkLine>,
    header: &TableCell,
    value: &TableCell,
    available_width: Option<usize>,
    label_style: Style,
) {
    let label_width = available_width
        .map(|width| width.saturating_sub(FIELD_LEADING_PADDING).max(1))
        .unwrap_or_else(|| header.plain_text().width().max(1));
    let label = header_label(header, label_style);
    let wrapped_labels = remap_wrapped_line(
        &label,
        word_wrap_line(&label.line, RtOptions::new(label_width))
            .into_iter()
            .map(line_into_static)
            .collect(),
    );
    for label_line in wrapped_labels {
        let mut prefix = HyperlinkLine::from(" ".repeat(FIELD_LEADING_PADDING));
        append_line(&mut prefix, label_line);
        out.push(prefix);
    }

    let value_width = available_width
        .map(|width| width.saturating_sub(STACKED_VALUE_INDENT).max(1))
        .unwrap_or_else(|| cell_width(value).max(1));
    for value_line in wrap_cell(value, value_width) {
        let mut prefix = HyperlinkLine::from(" ".repeat(STACKED_VALUE_INDENT));
        append_line(&mut prefix, value_line);
        out.push(prefix);
    }
}

fn header_label(header: &TableCell, style: Style) -> HyperlinkLine {
    let mut label = HyperlinkLine::default();
    for (index, line) in header.lines.iter().enumerate() {
        if index > 0 {
            label.push_span(Span::raw(" "), None);
        }
        append_line(&mut label, line.clone());
    }
    for span in &mut label.line.spans {
        span.style = style;
    }
    label
}

fn append_line(prefix: &mut HyperlinkLine, mut value_line: HyperlinkLine) {
    let shift = prefix.width();
    prefix.line.spans.append(&mut value_line.line.spans);
    prefix
        .hyperlinks
        .extend(value_line.hyperlinks.into_iter().map(|mut link| {
            link.columns = link.columns.start + shift..link.columns.end + shift;
            link
        }));
}

fn wrap_cell(cell: &TableCell, width: usize) -> Vec<HyperlinkLine> {
    if cell.lines.is_empty() {
        return vec![HyperlinkLine::new(Line::default())];
    }

    let mut wrapped = Vec::new();
    for source_line in &cell.lines {
        let rendered = word_wrap_line(&source_line.line, RtOptions::new(width.max(1)))
            .into_iter()
            .map(line_into_static)
            .collect::<Vec<_>>();
        if rendered.is_empty() {
            wrapped.push(HyperlinkLine::new(Line::default()));
        } else {
            wrapped.extend(remap_wrapped_line(source_line, rendered));
        }
    }
    if wrapped.is_empty() {
        wrapped.push(HyperlinkLine::new(Line::default()));
    }
    wrapped
}

fn cell_width(cell: &TableCell) -> usize {
    cell.lines
        .iter()
        .map(|line| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.width())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
}

fn widest_line_width(lines: &[HyperlinkLine]) -> usize {
    lines
        .iter()
        .map(|line| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.width())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
}
