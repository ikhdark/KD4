//! Render composition for the main chat widget surface.

use super::*;

impl ChatWidget {
    pub(super) fn as_renderable(&self) -> RenderableItem<'_> {
        let active_cell_right_reserve = self.ambient_pet_wrap_reserved_cols();
        let active_cell_renderable = match &self.transcript.active_cell {
            Some(cell) => RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                child: cell.as_ref(),
                top: 1,
                right: active_cell_right_reserve,
            })),
            None => RenderableItem::Owned(Box::new(())),
        };
        let active_hook_cell_renderable = match &self.active_hook_cell {
            Some(cell) if cell.should_render() => {
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                }))
            }
            _ => RenderableItem::Owned(Box::new(())),
        };
        let mut flex = FlexRenderable::new();
        flex.push(/*flex*/ 1, active_cell_renderable);
        flex.push(/*flex*/ 0, active_hook_cell_renderable);
        if let Some(cell) = self.pending_token_activity_output() {
            flex.push(
                /*flex*/ 1,
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                })),
            );
        }
        if let Some(cell) = self.pending_rate_limit_reset_hint() {
            flex.push(
                /*flex*/ 1,
                RenderableItem::Owned(Box::new(TranscriptAreaRenderable {
                    child: cell,
                    top: 1,
                    right: active_cell_right_reserve,
                })),
            );
        }
        flex.push(
            /*flex*/ 0,
            RenderableItem::Owned(Box::new(BottomPaneComposerReserveRenderable {
                bottom_pane: &self.bottom_pane,
                right_reserve: active_cell_right_reserve,
            }))
            .inset(Insets::tlbr(
                /*top*/ 1, /*left*/ 0, /*bottom*/ 0, /*right*/ 0,
            )),
        );
        RenderableItem::Owned(Box::new(flex))
    }
}

struct BottomPaneComposerReserveRenderable<'a> {
    bottom_pane: &'a BottomPane,
    right_reserve: u16,
}

impl Renderable for BottomPaneComposerReserveRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.bottom_pane
            .render_with_composer_right_reserve(area, buf, self.right_reserve);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane
            .desired_height_with_composer_right_reserve(width, self.right_reserve)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.bottom_pane
            .cursor_pos_with_composer_right_reserve(area, self.right_reserve)
    }

    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        self.bottom_pane
            .cursor_style_with_composer_right_reserve(area, self.right_reserve)
    }
}

struct TranscriptAreaRenderable<'a> {
    child: &'a dyn HistoryCell,
    top: u16,
    right: u16,
}

impl Renderable for TranscriptAreaRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let area = self.child_area(area);
        Clear.render(area, buf);
        if area.is_empty() {
            return;
        }
        let lines = self.child.display_lines(area.width);
        // Drop complete logical lines before the visible suffix. Paragraph's scroll and row
        // counters are u16, so scrolling through the entire history can overflow even though the
        // terminal only needs a few rows. Count with Paragraph itself to preserve its wrapping.
        let mut first_line = lines.len();
        let mut suffix_rows = 0usize;
        while first_line > 0 && suffix_rows < usize::from(area.height) {
            first_line -= 1;
            suffix_rows = suffix_rows.saturating_add(
                Paragraph::new(lines[first_line].clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width),
            );
        }
        let lines: Vec<_> = lines.into_iter().skip(first_line).collect();
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        // A single logical line wrapping beyond u16::MAX still exceeds Paragraph's API; keep
        // that case within its counters instead of overflowing the renderer.
        let y = suffix_rows
            .saturating_sub(usize::from(area.height))
            .min(usize::from(u16::MAX.saturating_sub(area.height))) as u16;
        paragraph.scroll((y, 0)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let child_width = width.saturating_sub(self.right).max(1);
        HistoryCell::desired_height(self.child, child_width).saturating_add(self.top)
    }
}

impl TranscriptAreaRenderable<'_> {
    fn child_area(&self, area: Rect) -> Rect {
        let y = area.y.saturating_add(self.top);
        let height = area.height.saturating_sub(self.top);
        Rect::new(
            area.x,
            y,
            area.width.saturating_sub(self.right).max(1),
            height,
        )
    }
}

impl Renderable for ChatWidget {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_renderable().render(area, buf);
        self.last_rendered_width.set(Some(area.width as usize));
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable().desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.as_renderable().cursor_pos(area)
    }

    fn cursor_style(&self, area: Rect) -> crossterm::cursor::SetCursorStyle {
        self.as_renderable().cursor_style(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryRenderMode;
    use crate::history_cell::PlainHistoryCell;
    use ratatui::text::Span;

    #[test]
    fn transcript_height_saturates_and_preserves_visible_tail_and_reserved_area() {
        for (line_count, child_height, composed_height) in [
            (3_usize, 3_u16, 4_u16),
            (65_535, u16::MAX, u16::MAX),
            (65_536, u16::MAX, u16::MAX),
            (65_537, u16::MAX, u16::MAX),
            (131_075, u16::MAX, u16::MAX),
        ] {
            let mut lines = vec![Line::from("x"); line_count];
            lines[line_count - 1] = Line::from("tail");
            let child = PlainHistoryCell::new(lines);
            for mode in [HistoryRenderMode::Rich, HistoryRenderMode::Raw] {
                assert_eq!(child.desired_height_for_mode(4, mode), child_height);
            }
            let renderable = TranscriptAreaRenderable {
                child: &child,
                top: 1,
                right: 2,
            };
            assert_eq!(renderable.desired_height(6), composed_height);
            let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
            for y in 0..4 {
                buf.set_string(0, y, "........", Style::default());
            }
            renderable.render(Rect::new(1, 1, 6, 2), &mut buf);
            let rows: Vec<String> = (0..4)
                .map(|y| (0..8).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            assert_eq!(rows, ["........", "........", ".tail...", "........"]);
        }
    }

    #[test]
    fn long_transcript_preserves_wrapped_styled_final_rows() {
        let mut lines = vec![Line::from("x"); 65_537];
        lines.push(Line::from(vec![
            Span::styled("HEAD", Style::default().fg(Color::Red)),
            Span::styled("LAST", Style::default().fg(Color::Green)),
            Span::styled("TAIL", Style::default().fg(Color::Yellow)),
        ]));
        let child = PlainHistoryCell::new(lines);
        let renderable = TranscriptAreaRenderable {
            child: &child,
            top: 1,
            right: 2,
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 5));
        for y in 0..5 {
            buf.set_string(0, y, "........", Style::default());
        }
        renderable.render(Rect::new(1, 1, 6, 3), &mut buf);
        let rows: Vec<String> = (0..5)
            .map(|y| (0..8).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert_eq!(
            rows,
            ["........", "........", ".LAST...", ".TAIL...", "........"]
        );
        for x in 1..5 {
            assert_eq!(buf[(x, 2)].fg, Color::Green);
            assert_eq!(buf[(x, 3)].fg, Color::Yellow);
        }
    }
}
