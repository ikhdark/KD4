use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FirstTerminalFrameState {
    #[default]
    Idle,
    AwaitingTurnStart {
        correlation_id: u64,
        submitted_at: Instant,
    },
    AwaitingOutput {
        correlation_id: u64,
        submitted_at: Instant,
    },
    AwaitingFrame {
        correlation_id: u64,
        submitted_at: Instant,
    },
}

/// Content-free correlation state for submit-to-first-terminal-frame timing.
///
/// The state machine stores only a monotonic timestamp and an enum tag. Any
/// telemetry allocation remains behind `SessionTelemetry`'s enabled check.
#[derive(Debug, Default)]
pub(super) struct FirstTerminalFrameTracker {
    state: FirstTerminalFrameState,
    next_correlation_id: u64,
}

impl FirstTerminalFrameTracker {
    pub(super) fn arm_submission(&mut self) -> Option<u64> {
        if self.state != FirstTerminalFrameState::Idle {
            return None;
        }
        self.next_correlation_id = self.next_correlation_id.wrapping_add(1).max(1);
        let correlation_id = self.next_correlation_id;
        self.state = FirstTerminalFrameState::AwaitingTurnStart {
            correlation_id,
            submitted_at: Instant::now(),
        };
        Some(correlation_id)
    }

    pub(super) fn cancel_pending_submission(&mut self) {
        if matches!(
            self.state,
            FirstTerminalFrameState::AwaitingTurnStart { .. }
        ) {
            self.state = FirstTerminalFrameState::Idle;
        }
    }

    pub(super) fn on_turn_started(&mut self) -> Option<u64> {
        let FirstTerminalFrameState::AwaitingTurnStart {
            correlation_id,
            submitted_at,
        } = self.state
        else {
            return None;
        };
        self.state = FirstTerminalFrameState::AwaitingOutput {
            correlation_id,
            submitted_at,
        };
        Some(correlation_id)
    }

    pub(super) fn on_first_output(&mut self) -> Option<u64> {
        let FirstTerminalFrameState::AwaitingOutput {
            correlation_id,
            submitted_at,
        } = self.state
        else {
            return None;
        };
        self.state = FirstTerminalFrameState::AwaitingFrame {
            correlation_id,
            submitted_at,
        };
        Some(correlation_id)
    }

    pub(super) fn on_turn_completed(&mut self) {
        if matches!(
            self.state,
            FirstTerminalFrameState::AwaitingTurnStart { .. }
                | FirstTerminalFrameState::AwaitingOutput { .. }
        ) {
            self.state = FirstTerminalFrameState::Idle;
        }
    }

    pub(super) fn take_rendered_duration(&mut self) -> Option<(u64, Duration)> {
        let FirstTerminalFrameState::AwaitingFrame {
            correlation_id,
            submitted_at,
        } = self.state
        else {
            return None;
        };
        self.state = FirstTerminalFrameState::Idle;
        Some((correlation_id, submitted_at.elapsed()))
    }
}

#[cfg(test)]
mod tests {
    use super::FirstTerminalFrameTracker;

    #[test]
    fn emits_exactly_once_after_output_is_rendered() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let correlation_id = tracker.arm_submission().expect("first submission arms");
        assert!(tracker.arm_submission().is_none());
        assert_eq!(tracker.on_turn_started(), Some(correlation_id));
        assert_eq!(tracker.on_first_output(), Some(correlation_id));

        let (rendered_id, _) = tracker.take_rendered_duration().expect("first frame emits");
        assert_eq!(rendered_id, correlation_id);
        assert!(tracker.take_rendered_duration().is_none());
    }

    #[test]
    fn completion_without_output_does_not_emit() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let _ = tracker.arm_submission();
        let _ = tracker.on_turn_started();
        tracker.on_turn_completed();

        assert!(tracker.take_rendered_duration().is_none());
    }

    #[test]
    fn completion_after_output_preserves_the_pending_frame() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let _ = tracker.arm_submission();
        let _ = tracker.on_turn_started();
        let _ = tracker.on_first_output();
        tracker.on_turn_completed();

        assert!(tracker.take_rendered_duration().is_some());
    }

    #[test]
    fn failed_submission_can_be_rearmed() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let first_id = tracker.arm_submission().expect("first submission arms");
        tracker.cancel_pending_submission();
        let second_id = tracker.arm_submission().expect("second submission arms");
        assert_ne!(first_id, second_id);
        let _ = tracker.on_turn_started();
        let _ = tracker.on_first_output();

        assert!(tracker.take_rendered_duration().is_some());
    }
}
