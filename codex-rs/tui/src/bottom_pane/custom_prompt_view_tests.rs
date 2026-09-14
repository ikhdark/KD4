use super::*;
use pretty_assertions::assert_eq;
use std::sync::mpsc::Receiver;

#[test]
fn paste_burst_newline_does_not_submit_short_first_line() {
    let now = Instant::now();

    for (first_line, second_line) in [("x", "rest"), ("id", "body"), ("foo", "bar")] {
        let (mut view, submitted_rx) = custom_prompt_view();
        let mut ms = 0;

        for ch in first_line.chars() {
            view.handle_key_event_at(KeyEvent::from(KeyCode::Char(ch)), now + elapsed(ms));
            ms += 1;
        }
        view.handle_key_event_at(KeyEvent::from(KeyCode::Enter), now + elapsed(ms));
        ms += 1;
        for ch in second_line.chars() {
            view.handle_key_event_at(KeyEvent::from(KeyCode::Char(ch)), now + elapsed(ms));
            ms += 1;
        }

        assert!(submitted_rx.try_recv().is_err());
        assert!(!view.is_complete());

        view.handle_key_event_at(KeyEvent::from(KeyCode::Enter), now + elapsed(/*ms*/ 200));

        assert_eq!(
            submitted_rx.try_recv(),
            Ok(format!("{first_line}\n{second_line}"))
        );
        assert!(view.is_complete());
    }
}

#[test]
fn paste_burst_newline_after_tab_does_not_submit() {
    let (mut view, submitted_rx) = custom_prompt_view();
    let now = Instant::now();
    let mut ms = 0;

    view.handle_key_event_at(KeyEvent::from(KeyCode::Char('x')), now + elapsed(ms));
    ms += 1;
    view.handle_key_event_at(KeyEvent::from(KeyCode::Tab), now + elapsed(ms));
    ms += 1;
    view.handle_key_event_at(KeyEvent::from(KeyCode::Enter), now + elapsed(ms));
    ms += 1;
    for ch in "rest".chars() {
        view.handle_key_event_at(KeyEvent::from(KeyCode::Char(ch)), now + elapsed(ms));
        ms += 1;
    }

    assert!(submitted_rx.try_recv().is_err());
    assert!(!view.is_complete());

    view.handle_key_event_at(KeyEvent::from(KeyCode::Enter), now + elapsed(/*ms*/ 200));

    assert_eq!(submitted_rx.try_recv(), Ok("x\nrest".to_string()));
    assert!(view.is_complete());
}

#[test]
fn delayed_enter_after_typing_submits() {
    let (mut view, submitted_rx) = custom_prompt_view();
    let now = Instant::now();

    for (idx, ch) in "foo".chars().enumerate() {
        view.handle_key_event_at(KeyEvent::from(KeyCode::Char(ch)), now + elapsed(idx * 20));
    }
    view.handle_key_event_at(KeyEvent::from(KeyCode::Enter), now + elapsed(/*ms*/ 80));

    assert_eq!(submitted_rx.try_recv(), Ok("foo".to_string()));
    assert!(view.is_complete());
}

fn custom_prompt_view() -> (CustomPromptView, Receiver<String>) {
    let (submitted, submitted_rx) = std::sync::mpsc::channel();
    let view = CustomPromptView::new(
        "Edit goal".to_string(),
        "Type a goal objective and press Enter".to_string(),
        String::new(),
        /*context_label*/ None,
        Box::new(move |text| {
            submitted.send(text).expect("send submitted text");
        }),
    );
    (view, submitted_rx)
}

fn elapsed(ms: usize) -> std::time::Duration {
    std::time::Duration::from_millis(ms as u64)
}

#[test]
fn constrained_prompt_render_and_cursor_stay_inside_allocation() {
    for context in [None, Some("Context".to_string())] {
        for height in 0..8 {
            for width in [0, 1, 2, 10] {
                let (mut view, _rx) = custom_prompt_view();
                view.context_label = context.clone();
                view.textarea
                    .set_text_clearing_elements("one\ntwo\nthree\nfour");
                let area = Rect::new(3, 2, width, height);
                let mut buf = Buffer::empty(Rect::new(0, 0, 20, 15));
                for cell in &mut buf.content {
                    cell.set_symbol("~");
                }
                view.render(area, &mut buf);
                for y in 0..15 {
                    for x in 0..20 {
                        if x < area.x || x >= area.right() || y < area.y || y >= area.bottom() {
                            assert_eq!(
                                buf[(x, y)].symbol(),
                                "~",
                                "write outside {area:?} at {x},{y}"
                            );
                        }
                    }
                }
                if width > 2 && height >= 4 {
                    let (x, y) = view.cursor_pos(area).expect("visible input has a cursor");
                    assert!(x >= area.x && x < area.right() && y >= area.y && y < area.bottom());
                } else if let Some((x, y)) = view.cursor_pos(area) {
                    assert!(x >= area.x && x < area.right() && y >= area.y && y < area.bottom());
                }
            }
        }
    }
}
