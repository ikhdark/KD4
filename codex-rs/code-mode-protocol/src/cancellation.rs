//! Provenance for a cancelled nested tool call.
//!
//! A [`CancellationToken`] carries no reason, so every cancelled call used to
//! read as "aborted by user" whoever actually stopped it. A user interrupt, a
//! turn shutdown, and a runtime wrapper reaching its own hard deadline are
//! different events and must stay distinguishable in the text the model sees.
//!
//! The cause is recorded at the origin, *before* the cancellation is signalled,
//! into a write-once cell that travels next to the token. Deadline arithmetic
//! decides only how long to wait; it never decides who cancelled.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::protocol::TurnAbortReason;
use serde::Deserialize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// Why a nested tool call's cancellation token was signalled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CancellationCause {
    /// The runtime wrapper reached the hard deadline for this nested call.
    NestedDeadline,
    /// The enclosing turn was aborted.
    TurnAborted { reason: TurnAbortReason },
    /// The cell closed or the dispatcher shut down.
    RuntimeShutdown,
}

impl CancellationCause {
    /// Model-facing description of who stopped the call.
    ///
    /// A user interrupt keeps its existing wording; the other origins say what
    /// actually happened rather than blaming the user.
    pub fn describe(&self) -> String {
        match self {
            Self::TurnAborted {
                reason: TurnAbortReason::Interrupted,
            } => "aborted by user".to_string(),
            Self::TurnAborted { reason } => {
                format!("cancelled: turn {}", turn_abort_reason_label(reason))
            }
            Self::NestedDeadline => "nested runtime deadline reached".to_string(),
            Self::RuntimeShutdown => "cancelled by runtime".to_string(),
        }
    }
}

fn turn_abort_reason_label(reason: &TurnAbortReason) -> &'static str {
    match reason {
        TurnAbortReason::Interrupted => "interrupted",
        TurnAbortReason::Replaced => "replaced",
        TurnAbortReason::ReviewEnded => "review ended",
        TurnAbortReason::BudgetLimited => "budget limited",
        TurnAbortReason::InternalError => "internal error",
        TurnAbortReason::ProcessLost => "process lost",
        TurnAbortReason::SessionShutdown => "session shutdown",
    }
}

/// Read-only stable effective cause, including an earlier ancestor cause.
#[derive(Debug)]
pub struct CancellationCauseCell {
    value: OnceLock<CancellationCause>,
    parent: Option<Arc<Self>>,
    arbitration: Arc<Mutex<()>>,
}

impl CancellationCauseCell {
    fn new() -> Self {
        Self { value: OnceLock::new(), parent: None, arbitration: Arc::new(Mutex::new(())) }
    }

    fn inherited(parent: Arc<Self>) -> Self {
        Self { value: OnceLock::new(), arbitration: Arc::clone(&parent.arbitration), parent: Some(parent) }
    }

    // Caller holds the tree-wide lock; no callbacks or asynchronous waits.
    fn resolve(&self) -> Option<&CancellationCause> {
        if self.value.get().is_none()
            && let Some(cause) = self.parent.as_ref().and_then(|parent| parent.resolve())
        {
            let _ = self.value.set(cause.clone());
        }
        self.value.get()
    }

    pub fn get(&self) -> Option<&CancellationCause> {
        let _guard = self.arbitration.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        self.resolve()
    }

    fn record(&self, cause: CancellationCause) -> bool {
        let _guard = self.arbitration.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.resolve().is_some() { return false; }
        self.value.set(cause).is_ok()
    }
}

impl PartialEq for CancellationCauseCell {
    fn eq(&self, other: &Self) -> bool { self.get() == other.get() }
}
impl Eq for CancellationCauseCell {}

/// A cancellation token paired with the write-once record of why it fired.
///
/// Arbitration between competing origins is deterministic by construction: the
/// cell is write-once, every origin records before it signals, the first
/// recorded cause wins, and later causes are discarded. A cause can never be
/// overwritten, including after it has been reported.
#[derive(Clone, Debug)]
pub struct NestedCancellation {
    token: CancellationToken,
    cause: Arc<CancellationCauseCell>,
}

impl NestedCancellation {
    pub fn new(token: CancellationToken) -> Self {
        Self {
            token,
            cause: Arc::new(CancellationCauseCell::new()),
        }
    }

    /// Wraps a token whose cause is recorded elsewhere, sharing that cell.
    pub fn with_cause(token: CancellationToken, cause: Arc<CancellationCauseCell>) -> Self {
        Self { token, cause }
    }

    /// A cancellation for one call inside this scope: its own write-once cell,
    /// falling back to this scope's cause.
    pub fn child(&self, token: CancellationToken) -> Self {
        Self {
            token,
            cause: Arc::new(CancellationCauseCell::inherited(Arc::clone(&self.cause))),
        }
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// The shared cell, for an origin that signals a different token derived
    /// from the same cancellation.
    pub fn cause_cell(&self) -> Arc<CancellationCauseCell> {
        Arc::clone(&self.cause)
    }

    /// Records why this call is being cancelled. Call before cancelling.
    ///
    /// Returns whether this caller's cause is the one that will be reported.
    /// A losing origin is not an error: two origins can genuinely race, and the
    /// first to record is the one that decided the outcome.
    pub fn record_cause(&self, cause: CancellationCause) -> bool {
        self.cause.record(cause)
    }

    /// Records the cause, then cancels. Ordering matters: an observer that
    /// wakes on cancellation must already be able to read the cause.
    pub fn cancel_with(&self, cause: CancellationCause) {
        self.record_cause(cause);
        self.token.cancel();
    }

    /// The recorded cause, or `None` when nothing recorded one.
    ///
    /// An earlier local or inherited cause wins permanently.
    pub fn cause(&self) -> Option<CancellationCause> {
        self.cause
            .get()
            .cloned()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use super::CancellationCause;
    use super::NestedCancellation;
    use codex_protocol::protocol::TurnAbortReason;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn inherited_cause_is_stable_through_exported_cells_and_multiple_generations() {
        let parent = NestedCancellation::new(CancellationToken::new());
        let child = parent.child(parent.token().child_token());
        let grandchild = child.child(child.token().child_token());
        let exported = grandchild.cause_cell();
        parent.cancel_with(CancellationCause::RuntimeShutdown);
        assert_eq!(exported.get(), Some(&CancellationCause::RuntimeShutdown));
        assert!(!grandchild.record_cause(CancellationCause::NestedDeadline));
        assert_eq!(grandchild.cause(), Some(CancellationCause::RuntimeShutdown));
        assert!(grandchild.is_cancelled());
    }

    #[test]
    fn racing_parent_and_child_causes_cannot_change_after_observation() {
        for _ in 0..100 {
            let parent = NestedCancellation::new(CancellationToken::new());
            let child = parent.child(parent.token().child_token());
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| { barrier.wait(); parent.cancel_with(CancellationCause::RuntimeShutdown); });
                barrier.wait();
                child.cancel_with(CancellationCause::NestedDeadline);
                let observed = child.cause();
                parent.cancel_with(CancellationCause::RuntimeShutdown);
                assert_eq!(child.cause(), observed);
            });
        }
    }

    #[test]
    fn the_first_recorded_cause_wins_and_cannot_be_overwritten() {
        let cancellation = NestedCancellation::new(CancellationToken::new());

        assert!(cancellation.record_cause(CancellationCause::NestedDeadline));
        assert!(
            !cancellation.record_cause(CancellationCause::TurnAborted {
                reason: TurnAbortReason::Interrupted,
            }),
            "a second origin must lose the race rather than replace the cause"
        );
        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::NestedDeadline)
        );

        // Reporting does not open the cell back up.
        let reported = cancellation.cause().expect("cause").describe();
        assert_eq!(reported, "nested runtime deadline reached");
        assert!(!cancellation.record_cause(CancellationCause::RuntimeShutdown));
        assert_eq!(
            cancellation.cause().expect("cause").describe(),
            reported,
            "the reported cause must not change after it was reported"
        );
    }

    #[test]
    fn a_shared_cell_is_visible_through_every_clone() {
        let cancellation = NestedCancellation::new(CancellationToken::new());
        let cloned = cancellation.clone();
        let derived =
            NestedCancellation::with_cause(CancellationToken::new(), cancellation.cause_cell());

        derived.cancel_with(CancellationCause::RuntimeShutdown);

        assert!(derived.is_cancelled());
        assert!(!cancellation.is_cancelled());
        assert!(!cloned.is_cancelled());
        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::RuntimeShutdown),
            "an origin holding a derived token records into the shared cell"
        );
        assert_eq!(cloned.cause(), cancellation.cause());
        cloned.cancel_with(CancellationCause::NestedDeadline);
        assert!(cancellation.is_cancelled());
        assert_eq!(
            cancellation.cause(),
            Some(CancellationCause::RuntimeShutdown)
        );
    }

    #[test]
    fn each_origin_preserves_its_wire_tag_and_model_facing_text() {
        use serde_json::json;

        for (cause, expected, wire) in [
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::Interrupted,
                },
                "aborted by user",
                json!({"type": "turn_aborted", "reason": "interrupted"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::Replaced,
                },
                "cancelled: turn replaced",
                json!({"type": "turn_aborted", "reason": "replaced"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::ReviewEnded,
                },
                "cancelled: turn review ended",
                json!({"type": "turn_aborted", "reason": "review_ended"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::BudgetLimited,
                },
                "cancelled: turn budget limited",
                json!({"type": "turn_aborted", "reason": "budget_limited"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::InternalError,
                },
                "cancelled: turn internal error",
                json!({"type": "turn_aborted", "reason": "internal_error"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::ProcessLost,
                },
                "cancelled: turn process lost",
                json!({"type": "turn_aborted", "reason": "process_lost"}),
            ),
            (
                CancellationCause::TurnAborted {
                    reason: TurnAbortReason::SessionShutdown,
                },
                "cancelled: turn session shutdown",
                json!({"type": "turn_aborted", "reason": "session_shutdown"}),
            ),
            (
                CancellationCause::NestedDeadline,
                "nested runtime deadline reached",
                json!({"type": "nested_deadline"}),
            ),
            (
                CancellationCause::RuntimeShutdown,
                "cancelled by runtime",
                json!({"type": "runtime_shutdown"}),
            ),
        ] {
            assert_eq!(cause.describe(), expected);
            assert_eq!(serde_json::to_value(&cause).expect("encode"), wire);
            assert_eq!(
                serde_json::from_value::<CancellationCause>(wire).expect("decode"),
                cause
            );
        }
    }

    #[test]
    fn a_scope_cause_reaches_calls_that_recorded_none_without_overriding_those_that_did() {
        let cell = NestedCancellation::new(CancellationToken::new());
        let quiet_call = cell.child(cell.token().child_token());
        let expired_call = cell.child(cell.token().child_token());

        // One call hit its own deadline before the cell tore down.
        expired_call.cancel_with(CancellationCause::NestedDeadline);
        cell.cancel_with(CancellationCause::RuntimeShutdown);

        assert_eq!(
            quiet_call.cause(),
            Some(CancellationCause::RuntimeShutdown),
            "a call with no cause of its own inherits the scope's"
        );
        assert_eq!(
            expired_call.cause(),
            Some(CancellationCause::NestedDeadline),
            "a call's own cause must outrank the scope tearing down around it"
        );
        assert!(quiet_call.is_cancelled(), "the scope cancels its children");
    }
}
