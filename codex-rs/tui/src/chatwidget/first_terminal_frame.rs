use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

#[derive(Debug)]
enum PendingTurnState {
    AwaitingTurnStart {
        submission_id: u64,
        submitted_at: Instant,
    },
    AwaitingOutput {
        submission_id: u64,
        turn_id: String,
        submitted_at: Instant,
    },
}

#[derive(Debug)]
struct AwaitingFrame {
    submission_id: u64,
    turn_id: String,
    submitted_at: Instant,
}

/// Content-free correlation state for submit-to-first-terminal-frame timing.
///
/// The tracker stores only IDs and monotonic timestamps. Callers arm it only
/// when metrics are enabled, so turn-ID allocation stays off the disabled hot
/// path. Render-ready turns are separate from the active turn so a queued
/// follow-up can start before the prior turn's requested frame is painted.
#[derive(Debug, Default)]
pub(super) struct FirstTerminalFrameTracker {
    pending_turn: Option<PendingTurnState>,
    awaiting_frames: VecDeque<AwaitingFrame>,
    next_submission_id: u64,
}

impl FirstTerminalFrameTracker {
    pub(super) fn arm_submission(&mut self) -> Option<u64> {
        if self.pending_turn.is_some() {
            return None;
        }
        self.next_submission_id = self.next_submission_id.wrapping_add(1).max(1);
        let submission_id = self.next_submission_id;
        self.pending_turn = Some(PendingTurnState::AwaitingTurnStart {
            submission_id,
            submitted_at: Instant::now(),
        });
        Some(submission_id)
    }

    pub(super) fn cancel_pending_submission(&mut self) {
        if matches!(
            self.pending_turn.as_ref(),
            Some(PendingTurnState::AwaitingTurnStart { .. })
        ) {
            self.pending_turn = None;
        }
    }

    pub(super) fn on_turn_started(&mut self, turn_id: &str) -> Option<u64> {
        let pending = self.pending_turn.take()?;
        match pending {
            PendingTurnState::AwaitingTurnStart {
                submission_id,
                submitted_at,
            } => {
                self.pending_turn = Some(PendingTurnState::AwaitingOutput {
                    submission_id,
                    turn_id: turn_id.to_owned(),
                    submitted_at,
                });
                Some(submission_id)
            }
            pending @ PendingTurnState::AwaitingOutput { .. } => {
                self.pending_turn = Some(pending);
                None
            }
        }
    }

    pub(super) fn on_first_output(&mut self, turn_id: &str) -> Option<u64> {
        let pending = self.pending_turn.take()?;
        match pending {
            PendingTurnState::AwaitingOutput {
                submission_id,
                turn_id: pending_turn_id,
                submitted_at,
            } if pending_turn_id == turn_id => {
                self.awaiting_frames.push_back(AwaitingFrame {
                    submission_id,
                    turn_id: pending_turn_id,
                    submitted_at,
                });
                Some(submission_id)
            }
            pending => {
                self.pending_turn = Some(pending);
                None
            }
        }
    }

    pub(super) fn on_turn_completed(&mut self, turn_id: &str) {
        let should_clear = match self.pending_turn.as_ref() {
            Some(PendingTurnState::AwaitingTurnStart { .. }) => true,
            Some(PendingTurnState::AwaitingOutput {
                turn_id: pending_turn_id,
                ..
            }) => pending_turn_id == turn_id,
            None => false,
        };
        if should_clear {
            self.pending_turn = None;
        }
    }

    pub(super) fn take_rendered_duration(
        &mut self,
        rendered_at: Instant,
    ) -> Option<(u64, String, Duration)> {
        let AwaitingFrame {
            submission_id,
            turn_id,
            submitted_at,
        } = self.awaiting_frames.pop_front()?;
        Some((
            submission_id,
            turn_id,
            rendered_at.saturating_duration_since(submitted_at),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::FirstTerminalFrameTracker;

    #[test]
    fn emits_exactly_once_after_output_is_rendered() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let submission_id = tracker.arm_submission().expect("first submission arms");
        assert!(tracker.arm_submission().is_none());
        assert_eq!(tracker.on_turn_started("turn-1"), Some(submission_id));
        assert_eq!(tracker.on_first_output("turn-1"), Some(submission_id));

        let (rendered_id, turn_id, _) = tracker
            .take_rendered_duration(std::time::Instant::now())
            .expect("first frame emits");
        assert_eq!(rendered_id, submission_id);
        assert_eq!(turn_id, "turn-1");
        assert!(
            tracker
                .take_rendered_duration(std::time::Instant::now())
                .is_none()
        );
    }

    #[test]
    fn completion_without_output_does_not_emit() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let _ = tracker.arm_submission();
        let _ = tracker.on_turn_started("turn-1");
        tracker.on_turn_completed("turn-1");

        assert!(
            tracker
                .take_rendered_duration(std::time::Instant::now())
                .is_none()
        );
    }

    #[test]
    fn completion_after_output_preserves_the_pending_frame() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let _ = tracker.arm_submission();
        let _ = tracker.on_turn_started("turn-1");
        let _ = tracker.on_first_output("turn-1");
        tracker.on_turn_completed("turn-1");

        assert!(
            tracker
                .take_rendered_duration(std::time::Instant::now())
                .is_some()
        );
    }

    #[test]
    fn failed_submission_can_be_rearmed() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let first_id = tracker.arm_submission().expect("first submission arms");
        tracker.cancel_pending_submission();
        let second_id = tracker.arm_submission().expect("second submission arms");
        assert_ne!(first_id, second_id);
        let _ = tracker.on_turn_started("turn-2");
        let _ = tracker.on_first_output("turn-2");

        assert!(
            tracker
                .take_rendered_duration(std::time::Instant::now())
                .is_some()
        );
    }

    #[test]
    fn queued_follow_up_can_start_while_prior_frame_is_pending() {
        let mut tracker = FirstTerminalFrameTracker::default();

        let first_submission = tracker.arm_submission().expect("first submission arms");
        assert_eq!(tracker.on_turn_started("turn-1"), Some(first_submission));
        assert_eq!(tracker.on_first_output("turn-1"), Some(first_submission));
        tracker.on_turn_completed("turn-1");

        let second_submission = tracker
            .arm_submission()
            .expect("follow-up arms before prior frame renders");
        assert_eq!(tracker.on_turn_started("turn-2"), Some(second_submission));
        assert_eq!(tracker.on_first_output("turn-2"), Some(second_submission));

        let rendered_at = std::time::Instant::now();
        let first = tracker
            .take_rendered_duration(rendered_at)
            .expect("first turn frame");
        let second = tracker
            .take_rendered_duration(rendered_at)
            .expect("follow-up frame");
        assert_eq!((first.0, first.1.as_str()), (first_submission, "turn-1"));
        assert_eq!((second.0, second.1.as_str()), (second_submission, "turn-2"));
        assert!(tracker.take_rendered_duration(rendered_at).is_none());
    }
}
