use std::io;
use std::io::stdout;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use ratatui::layout::Size;

use super::Tui;
use crate::custom_terminal::Terminal;

pub(crate) fn make_test_tui() -> io::Result<Tui> {
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::with_screen_size_and_cursor_position_for_test(
        backend,
        Size {
            width: 80,
            height: 24,
        },
        Position { x: 0, y: 0 },
    );
    Ok(Tui::new(terminal, /*enhanced_keys_supported*/ false))
}
