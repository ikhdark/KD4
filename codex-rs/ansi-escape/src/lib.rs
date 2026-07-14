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
                tracing::error!(
                    error = %message,
                    preview = %bounded_escaped_preview(s),
                    "ansi_to_tui failed to parse ANSI text"
                );
                Text::raw(s.to_owned())
            }
            Error::Utf8Error(utf8error) => {
                tracing::error!(
                    error = %utf8error,
                    preview = %bounded_escaped_preview(s),
                    "ansi_to_tui reported invalid UTF-8"
                );
                Text::raw(s.to_owned())
            }
        },
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
}
