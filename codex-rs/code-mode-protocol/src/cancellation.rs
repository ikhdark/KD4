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
