use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::NestedCancellation;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::delivery::Delivery;
use crate::runtime::OutputAdmission;
use crate::runtime::StoredValueWrite;
use crate::session_runtime::CellEvent;
use crate::session_runtime::ObserveMode;
use crate::session_runtime::OutputItem;
use crate::session_runtime::ToolKind;
use crate::session_runtime::ToolName;

pub(crate) type CellEventFuture =
    Pin<Box<dyn Future<Output = Result<CellEvent, CellError>> + Send + 'static>>;

pub(super) type ResponseSender = oneshot::Sender<Result<Delivery<BufferedEvent>, CellError>>;

pub(crate) struct BufferedEvent {
    event: CellEvent,
    _budget: Option<OutputBudget>,
}

struct OutputBudget {
    admission: Option<Arc<OutputAdmission>>,
    retained_bytes: Arc<AtomicUsize>,
    retained: usize,
    bytes: usize,
    explicit: bool,
}

impl Drop for OutputBudget {
    fn drop(&mut self) {
        self.retained_bytes
            .fetch_sub(self.retained, Ordering::AcqRel);
        if let Some(admission) = &self.admission {
            admission.release(self.bytes);
            if self.explicit {
                admission.release_yield();
            }
        }
    }
}

impl BufferedEvent {
    pub(super) fn into_event(self) -> CellEvent {
        self.event
    }
}

impl std::fmt::Debug for BufferedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.event.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CellError {
    Busy,
    AlreadyTerminating,
    Closed,
}

pub(crate) struct CellToolCall {
    pub(crate) id: String,
    pub(crate) name: ToolName,
    pub(crate) kind: ToolKind,
    pub(crate) input: Option<JsonValue>,
    pub(crate) timeout: std::time::Duration,
    /// Absolute instant the wrapper timeout fires, set by `spawn_tool` at the
    /// point its own `sleep(timeout)` begins. `None` until then, so a handler
    /// downstream never sees a deadline the wrapper is not actually enforcing.
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
        stored_value_writes: HashMap<String, StoredValueWrite>,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        cell_state: Arc<CellState>,
    ) -> impl Future<Output = CompletionCommit> + Send;

    fn closed(&self, event: Option<CellEvent>) -> impl Future<Output = ()> + Send;
}

#[derive(Clone)]
pub(crate) struct CellHandle {
    command_tx: mpsc::UnboundedSender<CellCommand>,
    state: Arc<CellState>,
}

impl CellHandle {
    pub(super) fn new(
        command_tx: mpsc::UnboundedSender<CellCommand>,
        state: Arc<CellState>,
    ) -> Self {
        Self { command_tx, state }
    }

    pub(crate) fn observe(&self, mode: ObserveMode) -> CellEventFuture {
        if !self.state.accepting_observations() {
            return closed_event();
        }
        let (response_tx, response_rx) = oneshot::channel();
        if self
            .command_tx
            .send(CellCommand::Observe { mode, response_tx })
            .is_err()
        {
            return closed_event();
        }
        response_event(response_rx)
    }

    pub(crate) fn terminate(&self) -> CellEventFuture {
        self.state.request_termination()
    }

    pub(crate) fn buffered_completion_bytes(&self) -> Option<usize> {
        let phase = self
            .state
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*phase {
            CellPhase::Completed {
                event,
                pending_initial_yield_items,
            } => Some(
                crate::session_runtime::cell_event_bytes(event)
                    .saturating_add(
                        pending_initial_yield_items
                            .iter()
                            .flatten()
                            .map(crate::session_runtime::output_item_bytes)
                            .fold(0usize, usize::saturating_add),
                    )
                    .saturating_add(self.state.retained_delivery_bytes.load(Ordering::Acquire)),
            ),
            _ => None,
        }
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
    terminal_event: Mutex<Option<CellEvent>>,
    recovered: Mutex<VecDeque<BufferedEvent>>,
    delivery_pending: AtomicBool,
    retained_delivery_bytes: Arc<AtomicUsize>,
    cancellation_token: CancellationToken,
}

enum CellPhase {
    Running,
    Terminating {
        response_tx: ResponseSender,
    },
    Completed {
        // Set only when `yield_control()` races the create-to-first-observe handoff.
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        event: CellEvent,
    },
    CompletionClaimed(CellEvent),
    Tombstone,
}

pub(crate) enum CompletionDelivery {
    Delivered,
    Buffered,
    Rejected(Option<ResponseSender>),
}

/// Result of atomically publishing a completed cell and its session side effects.
#[derive(Debug, PartialEq)]
pub(crate) enum CompletionCommit {
    Committed,
    Rejected(CellEvent),
}

pub(crate) enum ObservationDelivery {
    Running(ResponseSender),
    Delivered,
    Buffered,
    Closed,
}

impl CellState {
    pub(crate) fn new(cancellation_token: CancellationToken) -> Self {
        Self {
            phase: Mutex::new(CellPhase::Running),
            terminal_event: Mutex::new(None),
            recovered: Mutex::new(VecDeque::new()),
            delivery_pending: AtomicBool::new(false),
            retained_delivery_bytes: Arc::new(AtomicUsize::new(0)),
            cancellation_token,
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

    pub(crate) fn request_termination(self: &Arc<Self>) -> CellEventFuture {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
            CellPhase::Running => {
                let (response_tx, response_rx) = oneshot::channel();
                *phase = CellPhase::Terminating { response_tx };
                self.cancellation_token.cancel();
                response_event(response_rx)
            }
            CellPhase::Terminating { response_tx } => {
                *phase = CellPhase::Terminating { response_tx };
                Box::pin(async { Err(CellError::AlreadyTerminating) })
            }
            CellPhase::Completed {
                pending_initial_yield_items,
                event,
            } => {
                let event = self
                    .prepend_recovered(prepend_initial_yield(event, pending_initial_yield_items));
                self.record_terminal_event(&event);
                *phase = CellPhase::CompletionClaimed(event.clone());
                self.cancellation_token.cancel();
                ready_event(event)
            }
            CellPhase::CompletionClaimed(event) => {
                *phase = CellPhase::CompletionClaimed(event);
                Box::pin(async { Err(CellError::AlreadyTerminating) })
            }
            CellPhase::Tombstone => self.terminal_event().map_or_else(closed_event, ready_event),
        }
    }

    pub(crate) fn commit_completion(
        &self,
        event: CellEvent,
        pending_initial_yield_items: Option<Vec<OutputItem>>,
        commit: impl FnOnce(),
    ) -> CompletionCommit {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*phase, CellPhase::Running) || self.cancellation_token.is_cancelled() {
            return CompletionCommit::Rejected(event);
        }
        commit();
        *phase = CellPhase::Completed {
            pending_initial_yield_items,
            event,
        };
        CompletionCommit::Committed
    }

    pub(crate) fn deliver_completion(
        self: &Arc<Self>,
        response_tx: Option<ResponseSender>,
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
        let terminal_event = event.clone();
        match self.send_event(response_tx, event) {
            Ok(()) => {
                self.record_terminal_event(&terminal_event);
                self.cancellation_token.cancel();
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

    pub(crate) fn route_observation(
        self: &Arc<Self>,
        mode: ObserveMode,
        response_tx: ResponseSender,
    ) -> ObservationDelivery {
        if self.delivery_pending.load(Ordering::Acquire) {
            let _ = response_tx.send(Err(CellError::Busy));
            return ObservationDelivery::Buffered;
        }
        let recovered = self
            .recovered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if let Some(event) = recovered {
            let _ = response_tx.send(Ok(self.delivery(event)));
            return ObservationDelivery::Buffered;
        }
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
            } if matches!(
                mode,
                ObserveMode::YieldAfter(_) | ObserveMode::StateChange | ObserveMode::Decision
            ) =>
            {
                match self.send_event(response_tx, CellEvent::ExplicitYield { content_items }) {
                    Ok(()) => {
                        *phase = CellPhase::Completed {
                            pending_initial_yield_items: None,
                            event,
                        };
                        ObservationDelivery::Buffered
                    }
                    Err(CellEvent::ExplicitYield { content_items }) => {
                        *phase = CellPhase::Completed {
                            pending_initial_yield_items: Some(content_items),
                            event,
                        };
                        ObservationDelivery::Buffered
                    }
                    Err(event) => {
                        panic!("initial yield delivery returned an unexpected event: {event:?}")
                    }
                }
            }
            CellPhase::Completed {
                pending_initial_yield_items,
                event,
            } => {
                let delivered_event =
                    prepend_initial_yield(event.clone(), pending_initial_yield_items.clone());
                let terminal_event = delivered_event.clone();
                match self.send_event(response_tx, delivered_event) {
                    Ok(()) => {
                        self.record_terminal_event(&terminal_event);
                        self.cancellation_token.cancel();
                        ObservationDelivery::Delivered
                    }
                    Err(_) => {
                        *phase = CellPhase::Completed {
                            pending_initial_yield_items,
                            event,
                        };
                        ObservationDelivery::Buffered
                    }
                }
            }
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
            CellPhase::Tombstone => match self.terminal_event() {
                Some(event) => {
                    let _ = self.send_event(response_tx, event);
                    ObservationDelivery::Delivered
                }
                None => {
                    let _ = response_tx.send(Err(CellError::Closed));
                    ObservationDelivery::Closed
                }
            },
        }
    }

    pub(crate) fn finish_termination(self: &Arc<Self>, event: CellEvent) -> Option<CellEvent> {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (observer_event, termination_tx) =
            match std::mem::replace(&mut *phase, CellPhase::Tombstone) {
                CellPhase::Running => (Some(event), None),
                CellPhase::Terminating { response_tx } => (Some(event), Some(response_tx)),
                CellPhase::Completed {
                    pending_initial_yield_items,
                    event,
                } => (
                    Some(prepend_initial_yield(event, pending_initial_yield_items)),
                    None,
                ),
                CellPhase::CompletionClaimed(completed_event) => (Some(completed_event), None),
                CellPhase::Tombstone => (None, None),
            };
        let observer_event = observer_event.map(|event| self.prepend_recovered(event));
        if let Some(event) = observer_event.as_ref() {
            self.record_terminal_event(event);
            if let Some(response_tx) = termination_tx {
                let _ = self.send_event(response_tx, event.clone());
            }
        }
        self.cancellation_token.cancel();
        observer_event
    }

    pub(crate) fn terminal_event(&self) -> Option<CellEvent> {
        self.terminal_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record_terminal_event(&self, event: &CellEvent) {
        let mut terminal_event = self
            .terminal_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if terminal_event.is_none() {
            *terminal_event = Some(event.clone());
        }
    }

    pub(crate) fn tombstone(&self) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = CellPhase::Tombstone;
        self.cancellation_token.cancel();
    }

    fn delivery(self: &Arc<Self>, event: BufferedEvent) -> Delivery<BufferedEvent> {
        let yielded = matches!(
            event.event,
            CellEvent::Yielded { .. } | CellEvent::ExplicitYield { .. }
        );
        if yielded {
            self.delivery_pending.store(true, Ordering::Release);
        }
        let state = Arc::clone(self);
        let claimed_state = Arc::clone(self);
        Delivery::new(event, move |event| {
            if yielded {
                state
                    .recovered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_front(event);
                state.delivery_pending.store(false, Ordering::Release);
            }
        })
        .on_claim(move || {
            if yielded {
                claimed_state
                    .delivery_pending
                    .store(false, Ordering::Release);
            }
        })
    }

    pub(super) fn send_event(
        self: &Arc<Self>,
        tx: ResponseSender,
        event: CellEvent,
    ) -> Result<(), CellEvent> {
        let buffered = if matches!(
            event,
            CellEvent::Yielded { .. } | CellEvent::ExplicitYield { .. }
        ) {
            self.buffer_yield(event, None, 0)
        } else {
            BufferedEvent {
                event,
                _budget: None,
            }
        };
        match tx.send(Ok(self.delivery(buffered))) {
            Ok(()) => Ok(()),
            Err(Ok(delivery)) => Err(delivery.claim().into_event()),
            Err(Err(error)) => panic!("event delivery returned an actor error: {error:?}"),
        }
    }

    pub(super) fn send_yield(
        self: &Arc<Self>,
        tx: ResponseSender,
        event: CellEvent,
        admission: Arc<OutputAdmission>,
        bytes: usize,
    ) {
        let event = self.buffer_yield(event, Some(admission), bytes);
        // Even a successful send can be abandoned later. The receipt owns both
        // the output and its admission budget until the future consumes it.
        let _ = tx.send(Ok(self.delivery(event)));
    }

    fn buffer_yield(
        &self,
        event: CellEvent,
        admission: Option<Arc<OutputAdmission>>,
        bytes: usize,
    ) -> BufferedEvent {
        let explicit = matches!(event, CellEvent::ExplicitYield { .. });
        let retained = crate::session_runtime::cell_event_bytes(&event);
        self.retained_delivery_bytes
            .fetch_add(retained, Ordering::AcqRel);
        BufferedEvent {
            event,
            _budget: Some(OutputBudget {
                admission,
                bytes,
                explicit,
                retained,
                retained_bytes: Arc::clone(&self.retained_delivery_bytes),
            }),
        }
    }

    fn prepend_recovered(&self, event: CellEvent) -> CellEvent {
        let recovered = std::mem::take(
            &mut *self
                .recovered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let items = recovered
            .into_iter()
            .flat_map(|buffered| match buffered.into_event() {
                CellEvent::Yielded { content_items }
                | CellEvent::ExplicitYield { content_items } => content_items,
                _ => unreachable!("only abandoned yields are retained"),
            })
            .collect();
        prepend_initial_yield(event, Some(items))
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }
}

fn prepend_initial_yield(
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
        response_tx: ResponseSender,
    },
}

pub(super) fn response_event(
    response_rx: oneshot::Receiver<Result<Delivery<BufferedEvent>, CellError>>,
) -> CellEventFuture {
    Box::pin(async move {
        response_rx
            .await
            .unwrap_or(Err(CellError::Closed))
            .map(|delivery| delivery.claim().into_event())
    })
}

fn ready_event(event: CellEvent) -> CellEventFuture {
    Box::pin(async move { Ok(event) })
}

fn closed_event() -> CellEventFuture {
    Box::pin(async { Err(CellError::Closed) })
}
