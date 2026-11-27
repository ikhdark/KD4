use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode_protocol::CancellationCause;
use codex_code_mode_protocol::NestedCancellation;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::runtime::StoredValue;
use crate::session_runtime::CellEvent;
use crate::session_runtime::ObserveMode;
use crate::session_runtime::OutputItem;
use crate::session_runtime::ToolKind;
use crate::session_runtime::ToolName;

pub(crate) type CellEventFuture =
    Pin<Box<dyn Future<Output = Result<CellEvent, CellError>> + Send + 'static>>;
type TerminationFuture =
    Pin<Box<dyn Future<Output = Result<TerminalReceipt, CellError>> + Send + 'static>>;

pub(super) type ObservationSender = oneshot::Sender<Result<CellResponse, CellError>>;

pub(super) enum CellResponse {
    Yield(CellEvent),
    Terminal(TerminalReceipt),
}

impl CellResponse {
    pub(super) fn into_event(self) -> CellEvent {
        match self {
            Self::Yield(event) => event,
            Self::Terminal(receipt) => receipt.into_event(),
        }
    }
}

/// An immutable live receipt version. A claimed completion folds in any
/// unobserved initial yield, without copying or joining payloads under a lock.
#[derive(Clone)]
pub(super) struct TerminalReceipt {
    event: Arc<CellEvent>,
    pending_initial_yield_items: Option<Arc<Vec<OutputItem>>>,
}

impl TerminalReceipt {
    fn new(event: Arc<CellEvent>, pending_initial_yield_items: Option<Vec<OutputItem>>) -> Self {
        Self {
            event,
            pending_initial_yield_items: pending_initial_yield_items.map(Arc::new),
        }
    }

    fn into_event(self) -> CellEvent {
        prepend_initial_yield(
            Arc::unwrap_or_clone(self.event),
            self.pending_initial_yield_items.map(Arc::unwrap_or_clone),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CellError {
    Busy,
    AlreadyTerminating,
    Closed,
    CleanupFailed,
    ObservationReleased,
}

pub(crate) struct CellToolCall {
    pub(crate) id: String,
    pub(crate) causal_predecessors: Option<Vec<String>>,
    pub(crate) name: ToolName,
    pub(crate) kind: ToolKind,
    pub(crate) input: Option<JsonValue>,
    pub(crate) timeout: std::time::Duration,
    pub(crate) buffered_output_bytes: usize,
    /// Absolute instant enforced by the wrapper, set by `spawn_tool` before
    /// constructing or polling the callback. `None` when timeouts are disabled.
    pub(crate) deadline: Option<std::time::Instant>,
}

/// Connects a cell actor to session-owned callbacks and stored values.
///
/// Implementations should forward callback cancellation to downstream work.
/// Implementations must not return from `closed` until the session can no longer
/// route requests to the cell.
pub(crate) trait CellHost: Send + Sync + 'static {
    fn invoke_tool(
        &self,
        invocation: CellToolCall,
        cancellation: NestedCancellation,
    ) -> impl Future<Output = Result<JsonValue, String>> + Send;

    fn notify(
        &self,
        call_id: String,
        text: String,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<(), String>> + Send;

    fn commit_completion(
        &self,
        stored_value_writes: HashMap<String, StoredValue>,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        cell_state: Arc<CellState>,
    ) -> impl Future<Output = CompletionCommit> + Send;

    fn closed(&self, event: Option<CellEvent>) -> impl Future<Output = Option<CellEvent>> + Send;
}

#[derive(Clone)]
pub(crate) struct CellHandle {
    command_tx: mpsc::Sender<CellCommand>,
    state: Arc<CellState>,
}

impl CellHandle {
    pub(super) fn new(
        command_tx: mpsc::Sender<CellCommand>,
        state: Arc<CellState>,
    ) -> Self {
        Self { command_tx, state }
    }

    pub(crate) fn observe(
        &self,
        mode: ObserveMode,
        release: Option<CancellationToken>,
    ) -> CellEventFuture {
        if !self.state.accepting_observations() {
            return self
                .state
                .terminal_event()
                .map_or_else(closed_event, ready_event);
        }
        let (response_tx, mut response_rx) = oneshot::channel();
        let pending = match self.command_tx.try_send(CellCommand::Observe { mode, response_tx }) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(command)) => {
                Some((self.command_tx.clone(), command))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return self
                    .state
                    .terminal_event()
                    .map_or_else(closed_event, ready_event);
            }
        };
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            if let Some((command_tx, command)) = pending {
                // Until admission the caller still owns this command, and no
                // observation packet could have been sent to its receiver.
                let sent = if let Some(release) = &release {
                    tokio::select! {
                        biased;
                        _ = release.cancelled() => return Err(CellError::ObservationReleased),
                        sent = command_tx.send(command) => sent,
                    }
                } else {
                    command_tx.send(command).await
                };
                if sent.is_err() {
                    return state.terminal_event().ok_or(CellError::Closed);
                }
            }
            let event = if let Some(release) = release {
                tokio::select! {
                    biased;
                    _ = release.cancelled() => {
                        // Close and drain atomically against the actor's send:
                        // an already-enqueued packet belongs to this observer;
                        // a later send fails and restores it to actor custody.
                        response_rx.close();
                        response_rx.try_recv().unwrap_or(Err(CellError::ObservationReleased))
                    }
                    event = &mut response_rx => event.unwrap_or(Err(CellError::Closed)),
                }
            } else {
                response_rx.await.unwrap_or(Err(CellError::Closed))
            };
            match event {
                Err(CellError::Closed) => state.terminal_event().ok_or(CellError::Closed),
                result => result.map(CellResponse::into_event),
            }
        })
    }

    pub(crate) fn terminate(&self, cause: CancellationCause) -> CellEventFuture {
        let event = self.state.request_termination(cause);
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let event = event.await?;
            // Termination is not complete until the host releases session resources.
            state.closed.cancelled().await;
            if state
                .close_failed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(CellError::CleanupFailed);
            }
            if matches!(event.event.as_ref(), CellEvent::Terminated { .. })
                && let Some(finalized) = state.terminal_event()
            {
                return Ok(finalized);
            }
            Ok(event.into_event())
        })
    }
}

/// The single linearization point for a cell's terminal outcome.
///
/// The cancellation token is a child of the owning session token. Callback
/// tokens are children of this token, so cancellation flows strictly from the
/// session to the cell and then to its callbacks.
///
/// The mutex is held only for synchronous phase transitions and terminal
/// delivery. Runtime execution, observation waits, and callbacks never run
/// while it is held.
pub(crate) struct CellState {
    phase: Mutex<CellPhase>,
    terminal_event: Mutex<Option<TerminalReceipt>>,
    cancellation_token: CancellationToken,
    tool_cancellation: NestedCancellation,
    closed: CancellationToken,
    close_failed: std::sync::atomic::AtomicBool,
}

enum CellPhase {
    Running,
    Terminating {
        response_tx: oneshot::Sender<TerminalReceipt>,
    },
    Completed {
        // Set only when `yield_control()` races the create-to-first-observe handoff.
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        event: Arc<CellEvent>,
    },
    CompletionClaimed(TerminalReceipt),
    Tombstone,
}

pub(super) enum CompletionDelivery {
    Delivered,
    Buffered,
    Rejected(Option<ObservationSender>),
}

/// Result of atomically publishing a completed cell and its session side effects.
#[derive(Debug, PartialEq)]
pub(crate) enum CompletionCommit {
    Committed,
    Rejected(CellEvent),
}

pub(super) enum ObservationDelivery {
    Running(ObservationSender),
    Delivered,
    Buffered,
    Closed,
}

impl CellState {

    pub(crate) fn with_cancellation(tool_cancellation: NestedCancellation) -> Self {
        Self {
            phase: Mutex::new(CellPhase::Running),
            terminal_event: Mutex::new(None),
            cancellation_token: tool_cancellation.token().clone(),
            tool_cancellation,
            closed: CancellationToken::new(),
            close_failed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn accepting_observations(&self) -> bool {
        let accepting_phase = matches!(
            *self
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            CellPhase::Running | CellPhase::Completed { .. }
        );
        accepting_phase && !self.cancellation_token.is_cancelled()
    }

    fn request_termination(&self, cause: CancellationCause) -> TerminationFuture {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
            CellPhase::Running => {
                let (response_tx, response_rx) = oneshot::channel();
                *phase = CellPhase::Terminating { response_tx };
                // Record the winning origin before waking callbacks. An earlier
                // call-local deadline remains authoritative in its child token.
                self.tool_cancellation.cancel_with(cause);
                Box::pin(async move { response_rx.await.map_err(|_| CellError::Closed) })
            }
            CellPhase::Terminating { response_tx } => {
                *phase = CellPhase::Terminating { response_tx };
                Box::pin(async { Err(CellError::AlreadyTerminating) })
            }
            CellPhase::Completed {
                pending_initial_yield_items,
                event,
            } => {
                let receipt = TerminalReceipt::new(event, pending_initial_yield_items);
                *phase = CellPhase::CompletionClaimed(receipt.clone());
                self.tool_cancellation.cancel_with(CancellationCause::RuntimeShutdown);
                Box::pin(async move { Ok(receipt) })
            }
            CellPhase::CompletionClaimed(event) => {
                *phase = CellPhase::CompletionClaimed(event);
                Box::pin(async { Err(CellError::AlreadyTerminating) })
            }
            CellPhase::Tombstone => Box::pin(async { Err(CellError::Closed) }),
        }
    }

    pub(crate) fn commit_completion_with_event(
        &self,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        commit: impl FnOnce(CellEvent) -> CellEvent,
    ) -> CompletionCommit {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*phase, CellPhase::Running) || self.cancellation_token.is_cancelled() {
            return CompletionCommit::Rejected(event);
        }
        let event = Arc::new(commit(event));
        *phase = CellPhase::Completed {
            pending_initial_yield_items,
            event,
        };
        CompletionCommit::Committed
    }

    pub(super) fn deliver_completion(
        &self,
        response_tx: Option<ObservationSender>,
    ) -> CompletionDelivery {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (pending_initial_yield_items, event) =
            match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
                CellPhase::Completed {
                    pending_initial_yield_items,
                    event,
                } => (pending_initial_yield_items, event),
                previous => {
                    *phase = previous;
                    return CompletionDelivery::Rejected(response_tx);
                }
            };
        let Some(response_tx) = response_tx else {
            *phase = CellPhase::Completed {
                pending_initial_yield_items,
                event,
            };
            return CompletionDelivery::Buffered;
        };
        match self.send_completion(response_tx, event) {
            Ok(()) => {
                drop(phase);
                drop(pending_initial_yield_items);
                CompletionDelivery::Delivered
            }
            Err(event) => {
                *phase = CellPhase::Completed {
                    pending_initial_yield_items,
                    event,
                };
                CompletionDelivery::Buffered
            }
        }
    }

    // Keep the phase barrier through the send and terminal publication: a
    // closed receiver leaves the same completion available for the next claim.
    fn send_completion(
        &self,
        response_tx: ObservationSender,
        event: Arc<CellEvent>,
    ) -> Result<(), Arc<CellEvent>> {
        let receipt = TerminalReceipt::new(event, None);
        if response_tx
            .send(Ok(CellResponse::Terminal(receipt.clone())))
            .is_err()
        {
            return Err(receipt.event);
        }
        self.record_terminal_event(&receipt);
        self.tool_cancellation.cancel_with(CancellationCause::RuntimeShutdown);
        Ok(())
    }

    /// Refine a receipt after owned I/O, but never rewrite a terminal outcome
    /// already claimed by termination or delivered to an observer.
    pub(crate) fn update_unclaimed_completion(&self, completed: CellEvent) {
        let completed = Arc::new(completed);
        let retired = {
            let mut phase = self
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *phase {
                CellPhase::Completed { event, .. } => Some(std::mem::replace(event, completed)),
                _ => None,
            }
        };
        drop(retired);
    }

    pub(super) fn route_observation(&self, response_tx: ObservationSender) -> ObservationDelivery {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
            CellPhase::Running => {
                *phase = CellPhase::Running;
                ObservationDelivery::Running(response_tx)
            }
            CellPhase::Completed {
                pending_initial_yield_items: Some(content_items),
                event,
            } => {
                // The pending explicit yield precedes completion for every
                // observation mode; a failed send leaves it pending.
                match response_tx.send(Ok(CellResponse::Yield(CellEvent::ExplicitYield {
                    content_items,
                }))) {
                    Ok(()) => {
                        *phase = CellPhase::Completed {
                            pending_initial_yield_items: None,
                            event,
                        };
                        ObservationDelivery::Buffered
                    }
                    Err(Ok(CellResponse::Yield(CellEvent::ExplicitYield { content_items }))) => {
                        *phase = CellPhase::Completed {
                            pending_initial_yield_items: Some(content_items),
                            event,
                        };
                        ObservationDelivery::Buffered
                    }
                    Err(_) => unreachable!("only the sent initial yield can be returned"),
                }
            }
            CellPhase::Completed {
                pending_initial_yield_items: None,
                event,
            } => match self.send_completion(response_tx, event) {
                Ok(()) => ObservationDelivery::Delivered,
                Err(event) => {
                    *phase = CellPhase::Completed {
                        pending_initial_yield_items: None,
                        event,
                    };
                    ObservationDelivery::Buffered
                }
            },
            CellPhase::Terminating {
                response_tx: termination_tx,
            } => {
                *phase = CellPhase::Terminating {
                    response_tx: termination_tx,
                };
                let _ = response_tx.send(Err(CellError::Closed));
                ObservationDelivery::Closed
            }
            CellPhase::CompletionClaimed(event) => {
                *phase = CellPhase::CompletionClaimed(event);
                let _ = response_tx.send(Err(CellError::Closed));
                ObservationDelivery::Closed
            }
            CellPhase::Tombstone => {
                let _ = response_tx.send(Err(CellError::Closed));
                ObservationDelivery::Closed
            }
        }
    }

    pub(super) fn finish_termination(&self, event: CellEvent) -> Option<CellResponse> {
        let event = TerminalReceipt::new(Arc::new(event), None);
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observer_event = match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
            CellPhase::Running => Some(event),
            CellPhase::Terminating { response_tx } => {
                let _ = response_tx.send(event.clone());
                Some(event)
            }
            CellPhase::Completed {
                pending_initial_yield_items,
                event,
            } => Some(TerminalReceipt::new(event, pending_initial_yield_items)),
            CellPhase::CompletionClaimed(completed_event) => Some(completed_event),
            CellPhase::Tombstone => None,
        };
        if let Some(receipt) = observer_event.as_ref() {
            self.record_terminal_event(receipt);
        }
        self.tool_cancellation.cancel_with(CancellationCause::RuntimeShutdown);
        drop(phase);
        observer_event.map(CellResponse::Terminal)
    }

    pub(super) fn terminal_event(&self) -> Option<CellEvent> {
        // A late reader keeps the exact version it acquired, including when
        // close replaces a preliminary termination receipt during conversion.
        let receipt = self
            .terminal_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        receipt.map(TerminalReceipt::into_event)
    }

    fn record_terminal_event(&self, receipt: &TerminalReceipt) {
        let mut terminal_event = self
            .terminal_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if terminal_event.is_none() {
            *terminal_event = Some(receipt.clone());
        }
    }

    pub(crate) fn tombstone(&self) {
        let retired = {
            let mut phase = self
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *phase, CellPhase::Tombstone)
        };
        drop(retired);
        self.tool_cancellation.cancel_with(CancellationCause::RuntimeShutdown);
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    pub(crate) fn tool_cancellation(&self) -> NestedCancellation {
        // Callback cleanup must not cancel the cell before its completion can
        // commit. The actor owns this one child scope for all of its callbacks;
        // explicit cell/session cancellation still flows down with its cause.
        self.tool_cancellation.child(self.cancellation_token.child_token())
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.cancel();
    }

    pub(crate) fn mark_close_failed(&self) {
        self.close_failed
            .store(true, std::sync::atomic::Ordering::Release);
        // Drop any still-pending termination sender as well as waking callers
        // that already received their terminal event before actor failure.
        self.tombstone();
        self.closed.cancel();
    }

    pub(super) fn finalize_termination_receipt(&self, event: Option<CellEvent>) {
        if let Some(event @ CellEvent::Terminated { .. }) = event {
            let receipt = TerminalReceipt::new(Arc::new(event), None);
            let retired = self
                .terminal_event
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .replace(receipt);
            drop(retired);
        }
    }
}

pub(crate) fn prepend_initial_yield(
    event: CellEvent,
    pending_initial_yield_items: Option<Vec<OutputItem>>,
) -> CellEvent {
    let Some(mut pending_initial_yield_items) = pending_initial_yield_items else {
        return event;
    };
    match event {
        CellEvent::Yielded { mut content_items } => {
            pending_initial_yield_items.append(&mut content_items);
            CellEvent::Yielded {
                content_items: pending_initial_yield_items,
            }
        }
        CellEvent::ExplicitYield { mut content_items } => {
            pending_initial_yield_items.append(&mut content_items);
            CellEvent::ExplicitYield {
                content_items: pending_initial_yield_items,
            }
        }
        CellEvent::Completed {
            mut content_items,
            error_text,
            output_loss,
        } => {
            pending_initial_yield_items.append(&mut content_items);
            CellEvent::Completed {
                content_items: pending_initial_yield_items,
                error_text,
                output_loss,
            }
        }
        CellEvent::Terminated { mut content_items } => {
            pending_initial_yield_items.append(&mut content_items);
            CellEvent::Terminated {
                content_items: pending_initial_yield_items,
            }
        }
    }
}

pub(super) enum CellCommand {
    Observe {
        mode: ObserveMode,
        response_tx: ObservationSender,
    },
}

fn ready_event(event: CellEvent) -> CellEventFuture {
    Box::pin(async move { Ok(event) })
}

fn closed_event() -> CellEventFuture {
    Box::pin(async { Err(CellError::Closed) })
}
