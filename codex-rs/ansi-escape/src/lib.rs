use ansi_to_tui::Error;
use ansi_to_tui::IntoText;
use ratatui::text::Line;
use ratatui::text::Text;

const DIAGNOSTIC_PREVIEW_CHARS: usize = 256;

// Expand tabs in a best-effort way for transcript rendering.
// Tabs can interact poorly with left-gutter prefixes in our TUI and CLI
// transcript views (e.g., `nl` separates line numbers from content with a tab).
// Replacing tabs with spaces avoids odd visual artifacts without changing
// semantics for our use cases.
fn expand_tabs(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\t') {
        // Keep it simple: replace each tab with 4 spaces.
        // We do not try to align to tab stops since most usages (like `nl`)
        // look acceptable with a fixed substitution and this avoids stateful math
        // across spans.
        std::borrow::Cow::Owned(s.replace('\t', "    "))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// This function should be used when the contents of `s` are expected to match
/// a single line. If multiple lines are found, a warning is logged and only the
/// first line is returned.
pub fn ansi_escape_line(s: &str) -> Line<'static> {
    // Normalize tabs to spaces to avoid odd gutter collisions in transcript mode.
    let s = expand_tabs(s);
    let text = ansi_escape(&s);
    match text.lines.as_slice() {
        [] => "".into(),
        [only] => only.clone(),
        [first, rest @ ..] => {
            tracing::warn!(
                line_count = rest.len().saturating_add(1),
                preview = %bounded_escaped_preview(&s),
                "ansi_escape_line: expected a single line"
            );
            first.clone()
        }
    }
}

pub fn ansi_escape(s: &str) -> Text<'static> {
    // to_text() claims to be faster, but introduces complex lifetime issues
    // such that it's not worth it.
    match s.into_text() {
        Ok(text) => text,
        Err(err) => match err {
            Error::NomError(message) => {
                log_parse_failure(ParseFailureKind::Nom, &message, s);
                Text::raw(s.to_owned())
            }
            Error::Utf8Error(utf8error) => {
                log_parse_failure(ParseFailureKind::Utf8, &utf8error.to_string(), s);
                Text::raw(s.to_owned())
            }
        },
    }
}

#[derive(Clone, Copy)]
enum ParseFailureKind {
    Nom,
    Utf8,
}

fn log_parse_failure(kind: ParseFailureKind, error: &str, input: &str) {
    let error = bounded_escaped_preview(error);
    let preview = bounded_escaped_preview(input);
    match kind {
        ParseFailureKind::Nom => {
            tracing::error!(
                error = %error,
                preview = %preview,
                "ansi_to_tui failed to parse ANSI text"
            );
        }
        ParseFailureKind::Utf8 => {
            tracing::error!(
                error = %error,
                preview = %preview,
                "ansi_to_tui reported invalid UTF-8"
            );
        }
    }
}

fn bounded_escaped_preview(input: &str) -> String {
    let content_budget = DIAGNOSTIC_PREVIEW_CHARS.saturating_sub(1);
    let mut preview = String::new();
    let mut rendered_chars = 0_usize;

    for ch in input.chars() {
        let escaped = ch.escape_default();
        let escaped_len = escaped.clone().count();
        if rendered_chars.saturating_add(escaped_len) > content_budget {
            preview.push('…');
            return preview;
        }
        preview.extend(escaped);
        rendered_chars = rendered_chars.saturating_add(escaped_len);
    }

    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tracing::Event;
    use tracing::Id;
    use tracing::Metadata;
    use tracing::Subscriber;
    use tracing::field::Field;
    use tracing::field::Visit;
    use tracing::span::Attributes;
    use tracing::span::Record;

    #[derive(Clone, Default)]
    struct EventCollector {
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    impl Subscriber for EventCollector {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(visitor.fields);
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: HashMap<String, String>,
    }

    impl FieldVisitor {
        fn insert(&mut self, field: &Field, value: impl Into<String>) {
            self.fields.insert(field.name().to_string(), value.into());
        }
    }

    impl Visit for FieldVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.insert(field, value);
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.insert(field, format!("{value:?}"));
        }
    }

    #[test]
    fn diagnostic_preview_is_bounded_after_escaping() {
        let input = "\n".repeat(DIAGNOSTIC_PREVIEW_CHARS);
        let preview = bounded_escaped_preview(&input);

        assert!(preview.chars().count() <= DIAGNOSTIC_PREVIEW_CHARS);
        assert!(preview.ends_with('…'));
        assert!(!preview.contains('\n'));
    }

    #[test]
    fn malformed_ansi_never_panics() {
        for input in ["\u{1b}", "\u{1b}[", "\u{1b}[38;2", "\u{1b}]8;;"] {
            let _ = ansi_escape(input);
        }
    }

    #[test]
    fn deterministic_fuzz_corpus_never_panics_or_exposes_unbounded_diagnostics() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for case in 0..4_096 {
            let mut input = String::new();
            let len = case % 96;
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let ch = match state % 8 {
                    0 => '\u{1b}',
                    1 => '\n',
                    2 => '\r',
                    3 => '\t',
                    4 => char::from_u32((state as u32) & 0x7f).unwrap_or('\u{fffd}'),
                    _ => char::from_u32((state as u32) % 0x11_0000)
                        .filter(|ch| !matches!(*ch as u32, 0xd800..=0xdfff))
                        .unwrap_or('\u{fffd}'),
                };
                input.push(ch);
            }

            let rendered = std::panic::catch_unwind(|| ansi_escape(&input));
            assert!(
                rendered.is_ok(),
                "ANSI parser panicked for fuzz case {case}"
            );

            let preview = bounded_escaped_preview(&input);
            assert!(preview.chars().count() <= DIAGNOSTIC_PREVIEW_CHARS);
            assert!(!preview.chars().any(|ch| matches!(ch, '\n' | '\r' | '\t')));
        }
    }

    #[test]
    fn emitted_parse_failure_fields_are_bounded_and_escaped() {
        let collector = EventCollector::default();
        let events = Arc::clone(&collector.events);
        let error = format!("{}\nERROR_TAIL", "error".repeat(DIAGNOSTIC_PREVIEW_CHARS));
        let input = format!("{}\r\nINPUT_TAIL", "input".repeat(DIAGNOSTIC_PREVIEW_CHARS));

        tracing::subscriber::with_default(collector, || {
            log_parse_failure(ParseFailureKind::Nom, &error, &input);
        });

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let event = events
            .last()
            .expect("parse failure event should be captured");
        for field in ["error", "preview", "message"] {
            let value = event.get(field).expect("expected diagnostic field");
            assert!(value.chars().count() <= DIAGNOSTIC_PREVIEW_CHARS);
            assert!(!value.chars().any(|ch| matches!(ch, '\n' | '\r' | '\t')));
        }
        assert!(!event["error"].contains("ERROR_TAIL"));
        assert!(!event["preview"].contains("INPUT_TAIL"));
    }
}
