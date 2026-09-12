use crate::test_backend::VT100Backend;
use ratatui::layout::Rect;
use ratatui::text::Line;

#[test]
fn live_001_commit_on_overflow() {
    let backend = VT100Backend::new(/*width*/ 20, /*height*/ 6);
    let mut term =
        codex_tui::Terminal::with_options(backend).expect("terminal should be constructed");
    let area = Rect::new(
        /*x*/ 0, /*y*/ 5, /*width*/ 20, /*height*/ 1,
    );
    term.set_viewport_area(area);

    // Build 5 explicit rows at width 20.
    let mut rb = codex_tui::RowBuilder::new(/*target_width*/ 20);
    rb.push_fragment("one\n");
    rb.push_fragment("two\n");
    rb.push_fragment("three\n");
    rb.push_fragment("four\n");
    rb.push_fragment("five\n");

    // Keep the last 3 in the live ring; commit the first 2.
    let commit_rows = rb.drain_commit_ready(/*max_keep*/ 3);
    let lines: Vec<Line<'static>> = commit_rows.into_iter().map(|r| r.text.into()).collect();

    codex_tui::insert_history_lines(&mut term, lines)
        .expect("Failed to insert history lines in test");

    let screen = term.backend().vt100().screen();

    let joined = screen.contents();
    assert_eq!(
        joined
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    let retained = rb.drain_commit_ready(0);
    assert_eq!(
        retained.into_iter().map(|row| row.text).collect::<Vec<_>>(),
        vec!["three", "four", "five"]
    );
}
