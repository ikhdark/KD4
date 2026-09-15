//! Status indicator and terminal-title state for `ChatWidget`.

use crate::status_indicator_widget::STATUS_DETAILS_DEFAULT_MAX_LINES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StatusIndicatorState {
    pub(super) header: String,
    pub(super) details: Option<String>,
    pub(super) details_max_lines: usize,
}

impl StatusIndicatorState {
    pub(super) fn working() -> Self {
        Self {
            header: String::from("Working"),
            details: None,
            details_max_lines: STATUS_DETAILS_DEFAULT_MAX_LINES,
        }
    }
}

/// Compact runtime states that can be rendered into the terminal title.
///
/// This is intentionally smaller than the full status-header vocabulary. The
/// title needs short, stable labels, so callers map richer lifecycle events
/// onto one of these buckets before rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum TerminalTitleStatusKind {
    Working,
    WaitingForBackgroundTerminal,
    #[default]
    Thinking,
}

#[derive(Debug)]
pub(super) struct StatusState {
    pub(super) current_status: StatusIndicatorState,
    pub(super) terminal_title_status_kind: TerminalTitleStatusKind,
    pub(super) retry_status_header: Option<String>,
    pub(super) pending_status_indicator_restore: bool,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            current_status: StatusIndicatorState::working(),
            terminal_title_status_kind: TerminalTitleStatusKind::Working,
            retry_status_header: None,
            pending_status_indicator_restore: false,
        }
    }
}

impl StatusState {
    pub(super) fn set_status(&mut self, status: StatusIndicatorState) {
        self.current_status = status;
    }

    pub(super) fn take_retry_status_header(&mut self) -> Option<String> {
        self.retry_status_header.take()
    }

    pub(super) fn remember_retry_status_header(&mut self) {
        if self.retry_status_header.is_none() {
            self.retry_status_header = Some(self.current_status.header.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn retry_status_header_is_taken_once() {
        let mut state = StatusState::default();
        state.current_status.header = "Thinking".to_string();

        state.remember_retry_status_header();

        assert_eq!(
            state.take_retry_status_header(),
            Some("Thinking".to_string())
        );
        assert_eq!(state.take_retry_status_header(), None);
    }
}
