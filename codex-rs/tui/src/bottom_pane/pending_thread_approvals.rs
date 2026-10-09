use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::render::renderable::Renderable;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_lines;

/// Widget that lists inactive threads with outstanding approval requests.
pub(crate) struct PendingThreadApprovals {
    threads: Vec<String>,
}

impl PendingThreadApprovals {
    pub(crate) fn new() -> Self {
        Self {
            threads: Vec::new(),
        }
    }

    pub(crate) fn set_threads(&mut self, threads: Vec<String>) -> bool {
        if self.threads == threads {
            return false;
        }
        self.threads = threads;
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn threads(&self) -> &[String] {
        &self.threads
    }

    fn as_renderable(&self, width: u16) -> Box<dyn Renderable> {
        if self.threads.is_empty() || width < 4 {
            return Box::new(());
        }

        let mut lines = Vec::new();
        for thread in self.threads.iter().take(3) {
            let wrapped = adaptive_wrap_lines(
                std::iter::once(Line::from(format!("Approval needed in {thread}"))),
                RtOptions::new(width as usize)
                    .initial_indent(Line::from(vec!["  ".into(), "!".red().bold(), " ".into()]))
                    .subsequent_indent(Line::from("    ")),
            );
            lines.extend(wrapped);
        }

        if self.threads.len() > 3 {
            lines.push(Line::from("    ...".dim().italic()));
        }

        lines.push(
            Line::from(vec![
                "    ".into(),
                "/agent".cyan().bold(),
                " to switch threads".dim(),
            ])
            .dim(),
        );

        Paragraph::new(lines).into()
    }
}

impl Renderable for PendingThreadApprovals {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        self.as_renderable(area.width).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable(width).desired_height(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;

    fn snapshot_rows(widget: &PendingThreadApprovals, width: u16) -> String {
        let height = widget.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        widget.render(Rect::new(0, 0, width, height), &mut buf);

        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_single_thread_snapshot() {
        let mut widget = PendingThreadApprovals::new();
        assert!(widget.is_empty());
        assert_eq!(widget.desired_height(/*width*/ 40), 0);
        assert!(widget.set_threads(vec!["Robie [explorer]".to_string()]));
        assert!(!widget.is_empty());
        assert!(!widget.set_threads(vec!["Robie [explorer]".to_string()]));

        assert_snapshot!(
            snapshot_rows(&widget, /*width*/ 40).replace(' ', "."),
            @r"
        ..!.Approval.needed.in.Robie.[explorer].
        ..../agent.to.switch.threads............
        "
        );
    }

    #[test]
    fn render_multiple_threads_snapshot() {
        let mut widget = PendingThreadApprovals::new();
        widget.set_threads(vec![
            "Main [default]".to_string(),
            "Robie [explorer]".to_string(),
            "Inspector".to_string(),
            "Extra agent".to_string(),
        ]);

        let rendered = snapshot_rows(&widget, /*width*/ 44);
        // The preview omits a pending thread, so it must tell the user the list
        // is incomplete. The whitespace visualization below turns spaces and
        // literal dots into the same character and cannot enforce that signal.
        assert!(rendered.lines().any(|line| line.trim() == "..."));
        assert!(!rendered.contains("Extra agent"));
        assert_snapshot!(
            rendered.replace(' ', "."),
            @r"
        ..!.Approval.needed.in.Main.[default].......
        ..!.Approval.needed.in.Robie.[explorer].....
        ..!.Approval.needed.in.Inspector............
        ............................................
        ..../agent.to.switch.threads................
        "
        );
    }
}
