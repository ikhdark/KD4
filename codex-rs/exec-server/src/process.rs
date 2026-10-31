use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::ExecServerError;
use crate::ProcessId;
use crate::protocol::ExecParams;
use crate::protocol::ProcessOutputChunk;
use crate::protocol::ProcessSignal;
use crate::protocol::ReadResponse;
use crate::protocol::WriteResponse;

pub struct StartedExecProcess {
    pub process: Arc<dyn ExecProcess>,
}

/// Pushed process events for consumers that want to follow process output as it
/// arrives instead of polling retained output with [`ExecProcess::read`].
///
/// The stream is scoped to one [`ExecProcess`] handle. `Output` events carry
/// stdout, stderr, or pty bytes. `Exited` reports the process exit status, while
/// `Closed` means all output streams have ended and no more output events will
/// arrive. `Failed` is used when the process session cannot continue, for
/// example because the remote environment connection disconnected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecProcessEvent {
    Output(ProcessOutputChunk),
    /// Missing output only; does not imply exit, closure, or process failure.
    OutputGap {
        through_seq: u64,
    },
    Exited {
        seq: u64,
        exit_code: i32,
        sandbox_denied: Option<bool>,
    },
    Closed {
        seq: u64,
        sandbox_denied: Option<bool>,
    },
    Failed(String),
}

/// Replay buffer plus live fan-out for pushed process events.
///
/// New subscribers first drain a bounded replay history, then continue on the
/// live broadcast channel. The history is bounded by event count and retained
/// output bytes: count protects against many tiny events, while bytes protects
/// against a few very large output chunks.
#[derive(Clone)]
pub(crate) struct ExecProcessEventLog {
    inner: Arc<ExecProcessEventLogInner>,
}

struct ExecProcessEventLogInner {
    history: StdMutex<ExecProcessEventHistory>,
    live_tx: broadcast::Sender<Arc<ExecProcessEvent>>,
    event_capacity: usize,
    byte_capacity: usize,
}

#[derive(Default)]
struct ExecProcessEventHistory {
    events: VecDeque<Arc<ExecProcessEvent>>,
    retained_bytes: usize,
}

impl ExecProcessEvent {
    /// Sequence number used to order process-owned events.
    ///
    /// `Failed` is intentionally unsequenced because it is synthesized by the
    /// session observer when exit observation or its transport fails, rather
    /// than being evidence of a process exit.
    pub(crate) fn seq(&self) -> Option<u64> {
        match self {
            ExecProcessEvent::Output(chunk) => Some(chunk.seq),
            ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq, .. } => {
                Some(*seq)
            }
            ExecProcessEvent::OutputGap { through_seq } => Some(*through_seq),
            ExecProcessEvent::Failed(_) => None,
        }
    }

    fn retained_len(&self) -> usize {
        match self {
            ExecProcessEvent::Output(chunk) => chunk.chunk.0.len(),
            ExecProcessEvent::Failed(message) => message.len(),
            ExecProcessEvent::Exited { .. }
            | ExecProcessEvent::Closed { .. }
            | ExecProcessEvent::OutputGap { .. } => 0,
        }
    }
}

impl ExecProcessEventLog {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn new(event_capacity: usize, byte_capacity: usize) -> Self {
        let (live_tx, _live_rx) = broadcast::channel(event_capacity);
        Self {
            inner: Arc::new(ExecProcessEventLogInner {
                history: StdMutex::new(ExecProcessEventHistory::default()),
                live_tx,
                event_capacity,
                byte_capacity,
            }),
        }
    }

    pub(crate) fn publish(&self, event: ExecProcessEvent) {
        // Keep byte copies out of the history and broadcast locks. Consumers
        // still receive owned events, materialized after releasing those locks.
        self.publish_shared(Arc::new(event));
    }

    pub(crate) fn publish_shared(&self, event: Arc<ExecProcessEvent>) {
        let mut history = self
            .inner
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.retained_bytes += event.retained_len();
        history.events.push_back(Arc::clone(&event));
        while history.events.len() > self.inner.event_capacity
            || history.retained_bytes > self.inner.byte_capacity
        {
            let Some(evicted) = history.events.pop_front() else {
                break;
            };
            history.retained_bytes = history
                .retained_bytes
                .saturating_sub(evicted.retained_len());
        }

        let _ = self.inner.live_tx.send(event);
    }

    pub(crate) fn subscribe(&self) -> ExecProcessEventReceiver {
        let history = self
            .inner
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live_rx = self.inner.live_tx.subscribe();
        let replay = history.events.iter().cloned().collect();

        ExecProcessEventReceiver {
            replay,
            live_rx,
            _keepalive: None,
        }
    }

    // Only used by a private replay validator with no subscribers. Move the
    // validated payloads out instead of materializing a second byte copy.
    pub(crate) fn drain_owned(&self) -> Vec<ExecProcessEvent> {
        let mut history = self
            .inner
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.retained_bytes = 0;
        history.events.drain(..).map(Arc::unwrap_or_clone).collect()
    }
}

pub struct ExecProcessEventReceiver {
    replay: VecDeque<Arc<ExecProcessEvent>>,
    live_rx: broadcast::Receiver<Arc<ExecProcessEvent>>,
    _keepalive: Option<broadcast::Sender<Arc<ExecProcessEvent>>>,
}

impl ExecProcessEventReceiver {
    /// Returns a receiver that remains open without yielding events.
    pub fn empty() -> Self {
        let (live_tx, live_rx) = broadcast::channel(1);
        Self {
            replay: VecDeque::new(),
            live_rx,
            _keepalive: Some(live_tx),
        }
    }

    /// Returns the next replayed or live event.
    ///
    /// `Lagged` means this receiver fell behind the bounded live channel. The
    /// caller should recover through [`ExecProcess::read`] using the last
    /// delivered sequence number, then continue receiving pushed events.
    /// Discard sequenced pushed events at or below the last sequence already
    /// delivered, including events delivered by the recovery read. A sequence
    /// gap or truncated initial replay means earlier history may be missing.
    pub async fn recv(&mut self) -> Result<ExecProcessEvent, broadcast::error::RecvError> {
        self.recv_shared().await.map(Arc::unwrap_or_clone)
    }

    pub(crate) async fn recv_shared(&mut self) -> Result<Arc<ExecProcessEvent>, broadcast::error::RecvError> {
        if let Some(event) = self.replay.pop_front() {
            return Ok(event);
        }

        self.live_rx.recv().await
    }
}

/// Handle for an executor-managed process.
///
/// Implementations must support both retained-output reads and pushed events:
/// `read` is the request/response API for callers that want to page through
/// buffered output, while `subscribe_events` is the streaming API for callers
/// that want output and lifecycle changes delivered as they happen.
pub trait ExecProcess: Send + Sync {
    fn process_id(&self) -> &ProcessId;

    fn subscribe_wake(&self) -> watch::Receiver<u64>;

    fn subscribe_events(&self) -> ExecProcessEventReceiver;

    fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse>;

    fn write(&self, chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse>;

    /// Close piped input after accepted writes. Repeated closes are harmless.
    fn close_stdin(&self) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async {
            Err(ExecServerError::Protocol("explicit stdin close is unsupported by this backend".into()))
        })
    }

    /// Whether dropping an unfinished `write` future guarantees that none of its
    /// bytes were queued for the process, then or later. Only then may a caller
    /// report the abandoned input as safely retryable; the default makes no claim.
    fn cancelled_write_is_unsent(&self) -> bool {
        false
    }

    fn signal(&self, signal: ProcessSignal) -> ExecProcessFuture<'_, ()>;

    fn terminate(&self) -> ExecProcessFuture<'_, ()>;

    /// The pinned owner can no longer recover; a fresh session is not this process.
    fn termination_is_unreachable(&self) -> bool {
        false
    }

    /// Preserve unknown outcome outside active task/persistence barriers.
    fn record_unresolved_termination(&self, _reason: &str) {}
}

pub type ExecProcessFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ExecServerError>> + Send + 'a>>;

pub trait ExecBackend: Send + Sync {
    fn start(&self, params: ExecParams) -> ExecBackendFuture<'_>;
}

pub type ExecBackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<StartedExecProcess, ExecServerError>> + Send + 'a>>;

