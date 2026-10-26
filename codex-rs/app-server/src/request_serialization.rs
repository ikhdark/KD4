use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Component;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use codex_app_server_protocol::ClientRequestSerializationScope;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::Mutex;
use tracing::Instrument;

use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::outgoing_message::ConnectionId;

type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const MAX_TOTAL_QUEUED_REQUESTS: usize = 1024;
const MAX_QUEUED_REQUESTS_PER_KEY: usize = 64;
// A valid turn can contain up to 1 MiB Unicode scalar values, which can occupy
// four bytes each in the serialized request. Keep queue admission above that
// protocol limit so turn validation, rather than overload handling, owns the
// boundary response.
const MAX_TOTAL_QUEUED_BYTES: usize = 32 * 1024 * 1024;
// Admit a 10 MiB fs/writeFile payload after base64 encoding and its JSON envelope.
const MAX_QUEUED_BYTES_PER_KEY: usize = 16 * 1024 * 1024;
const MAX_CONCURRENT_SHARED_READS: usize = 16;
/// Reserved slots per key for out-of-band control requests such as `turn/interrupt`.
///
/// Control requests carry no queued payload and never mutate ordered thread state, so
/// they are admitted from their own small budget instead of competing with queued
/// mutations. The cap still bounds control flooding.
const MAX_QUEUED_CONTROL_REQUESTS_PER_KEY: usize = 8;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RequestSerializationQueueKey {
    Control(Box<RequestSerializationQueueKey>),
    /// Steering is ordered with other steering, never behind thread mutations.
    /// It retains ordinary payload accounting rather than the small control cap.
    Steering(Box<RequestSerializationQueueKey>),
    Global(&'static str),
    Thread {
        thread_id: String,
    },
    ThreadPath {
        path: PathBuf,
    },
    CommandExecProcess {
        connection_id: ConnectionId,
        process_id: String,
    },
    Process {
        connection_id: ConnectionId,
        process_handle: String,
    },
    FuzzyFileSearchSession {
        connection_id: ConnectionId,
        session_id: String,
    },
    FsWatch {
        connection_id: ConnectionId,
        watch_id: String,
    },
    McpOauth {
        server_name: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSerializationAccess {
    Exclusive,
    SharedRead,
    /// Reserved admission, but FIFO execution behind process registration.
    OrderedControl,
    /// Out-of-band control traffic for a key, such as `turn/interrupt`.
    ///
    /// Control requests run ahead of queued work on the same key because they observe or
    /// cancel in-flight state rather than mutating the ordered sequence. They cannot reorder
    /// two mutations relative to each other: mutations keep their own FIFO lane.
    Control,
}

impl RequestSerializationQueueKey {
    /// Production preparation is fallible and bounds the OS work even after a
    /// caller times out. The permit lives in the blocking closure, not its waiter.
    pub(crate) async fn try_from_scope(
        connection_id: ConnectionId,
        scope: ClientRequestSerializationScope,
    ) -> std::io::Result<(Self, RequestSerializationAccess)> {
        if let ClientRequestSerializationScope::ThreadPath { path } = scope {
            static RESOLVERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
                std::sync::OnceLock::new();
            let admission = RESOLVERS
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
                .clone()
                .acquire_owned();
            let permit = tokio::time::timeout(std::time::Duration::from_secs(1), admission)
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "thread path preparation admission timed out",
                    )
                })?
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "thread path preparation capacity exhausted",
                    )
                })?;
            let resolution = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                std::fs::canonicalize(path)
            });
            let path = tokio::time::timeout(std::time::Duration::from_secs(1), resolution)
                .await
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "thread path preparation timed out; request not dispatched",
                    )
                })?
                .map_err(std::io::Error::other)??;
            return Ok((
                Self::ThreadPath { path },
                RequestSerializationAccess::Exclusive,
            ));
        }
        Ok(Self::from_scope(connection_id, scope).await)
    }

    pub(crate) async fn from_scope(
        connection_id: ConnectionId,
        scope: ClientRequestSerializationScope,
    ) -> (Self, RequestSerializationAccess) {
        match scope {
            ClientRequestSerializationScope::Global(name) => {
                (Self::Global(name), RequestSerializationAccess::Exclusive)
            }
            ClientRequestSerializationScope::GlobalSharedRead(name) => {
                (Self::Global(name), RequestSerializationAccess::SharedRead)
            }
            ClientRequestSerializationScope::Thread { thread_id } => (
                Self::Thread {
                    thread_id: canonical_thread_key(thread_id),
                },
                RequestSerializationAccess::Exclusive,
            ),
            // Enqueue wraps this thread key in a separate control lane, allowing
            // cancellation while an ordered request is still running.
            ClientRequestSerializationScope::ThreadControl { thread_id } => (
                Self::Thread {
                    thread_id: canonical_thread_key(thread_id),
                },
                RequestSerializationAccess::Control,
            ),
            ClientRequestSerializationScope::ThreadPath { path } => (
                Self::ThreadPath {
                    path: normalize_thread_path(path).await,
                },
                RequestSerializationAccess::Exclusive,
            ),
            ClientRequestSerializationScope::CommandExecProcess { process_id } => (
                Self::CommandExecProcess {
                    connection_id,
                    process_id,
                },
                RequestSerializationAccess::Exclusive,
            ),
            ClientRequestSerializationScope::Process { process_handle } => (
                Self::Process {
                    connection_id,
                    process_handle,
                },
                RequestSerializationAccess::Exclusive,
            ),
            ClientRequestSerializationScope::FuzzyFileSearchSession { session_id } => (
                Self::FuzzyFileSearchSession {
                    connection_id,
                    session_id,
                },
                RequestSerializationAccess::Exclusive,
            ),
            ClientRequestSerializationScope::FsWatch { watch_id } => (
                Self::FsWatch {
                    connection_id,
                    watch_id,
                },
                RequestSerializationAccess::Exclusive,
            ),
            ClientRequestSerializationScope::McpOauth { server_name } => (
                Self::McpOauth { server_name },
                RequestSerializationAccess::Exclusive,
            ),
        }
    }
}

fn canonical_thread_key(thread_id: String) -> String {
    codex_protocol::ThreadId::from_string(&thread_id)
        .map(|id| id.to_string())
        .unwrap_or(thread_id)
}

async fn normalize_thread_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = tokio::fs::canonicalize(&path).await {
        return canonical;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub(crate) struct QueuedInitializedRequest {
    gate: Arc<ConnectionRpcGate>,
    estimated_bytes: usize,
    future: BoxFutureUnit,
}

impl QueuedInitializedRequest {

    pub(crate) fn new_with_estimated_bytes(
        gate: Arc<ConnectionRpcGate>,
        estimated_bytes: usize,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            gate,
            estimated_bytes,
            future: Box::pin(future),
        }
    }

    pub(crate) async fn run(self) {
        let Self { gate, future, .. } = self;
        if let Err(error) = gate.run(future).await {
            tracing::error!(?error, "initialized request handler task failed");
        }
    }

    fn belongs_to_gate(&self, gate: &Arc<ConnectionRpcGate>) -> bool {
        Arc::ptr_eq(&self.gate, gate)
    }
}

struct QueuedSerializedRequest {
    access: RequestSerializationAccess,
    request: QueuedInitializedRequest,
}

impl QueuedSerializedRequest {
    fn estimated_bytes(&self) -> usize {
        self.request.estimated_bytes
    }
}

#[derive(Clone, Copy)]
struct RequestSerializationLimits {
    max_total_queued: usize,
    max_queued_per_key: usize,
    max_total_queued_bytes: usize,
    max_queued_bytes_per_key: usize,
    max_concurrent_shared_reads: usize,
    max_queued_control_per_key: usize,
}

impl Default for RequestSerializationLimits {
    fn default() -> Self {
        Self {
            max_total_queued: MAX_TOTAL_QUEUED_REQUESTS,
            max_queued_per_key: MAX_QUEUED_REQUESTS_PER_KEY,
            max_total_queued_bytes: MAX_TOTAL_QUEUED_BYTES,
            max_queued_bytes_per_key: MAX_QUEUED_BYTES_PER_KEY,
            max_concurrent_shared_reads: MAX_CONCURRENT_SHARED_READS,
            max_queued_control_per_key: MAX_QUEUED_CONTROL_REQUESTS_PER_KEY,
        }
    }
}

/// Per-key queues split into an ordered mutation lane and a control lane.
///
/// The ordered lane preserves strict FIFO between state mutations. The control lane is
/// drained first so an interrupt is never stuck behind unrelated queued work.
#[derive(Default)]
struct KeyQueues {
    control: VecDeque<QueuedSerializedRequest>,
    ordered: VecDeque<QueuedSerializedRequest>,
    changed: Arc<tokio::sync::Notify>,
}

impl KeyQueues {
    fn ordered_len(&self) -> usize {
        self.ordered
            .iter()
            .filter(|request| request.access != RequestSerializationAccess::OrderedControl)
            .count()
    }

    fn control_len(&self) -> usize {
        self.control.len()
            + self
                .ordered
                .iter()
                .filter(|request| request.access == RequestSerializationAccess::OrderedControl)
                .count()
    }

    fn ordered_bytes(&self) -> usize {
        self.ordered
            .iter()
            .filter(|request| request.access != RequestSerializationAccess::OrderedControl)
            .map(QueuedSerializedRequest::estimated_bytes)
            .fold(0usize, usize::saturating_add)
    }

    fn push(&mut self, request: QueuedSerializedRequest) {
        if request.access == RequestSerializationAccess::Control {
            self.control.push_back(request);
        } else {
            self.ordered.push_back(request);
        }
        self.changed.notify_one();
    }
}

#[derive(Default)]
struct RequestSerializationState {
    queues: HashMap<RequestSerializationQueueKey, KeyQueues>,
    total_queued: usize,
    total_queued_bytes: usize,
    total_control: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestAdmissionError {
    TotalQueue,
    PerKeyQueue,
    TotalBytes,
    PerKeyBytes,
    /// The per-key control lane is saturated. Bounds interrupt flooding without letting
    /// queued mutations starve control traffic.
    ControlLane,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestAdmission {
    Accepted,
    Rejected(RequestAdmissionError),
}

#[derive(Clone)]
pub(crate) struct RequestSerializationQueues {
    inner: Arc<Mutex<RequestSerializationState>>,
    limits: RequestSerializationLimits,
}

impl Default for RequestSerializationQueues {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RequestSerializationState::default())),
            limits: RequestSerializationLimits::default(),
        }
    }
}

impl RequestSerializationQueues {


    pub(crate) async fn enqueue(
        &self,
        key: RequestSerializationQueueKey,
        access: RequestSerializationAccess,
        request: QueuedInitializedRequest,
    ) -> RequestAdmission {
        let request = QueuedSerializedRequest { access, request };
        let is_control = matches!(
            access,
            RequestSerializationAccess::Control | RequestSerializationAccess::OrderedControl
        );
        // A separate drain is required: queue priority alone cannot interrupt a
        // handler that is already running and waiting for cancellation.
        let key = if access == RequestSerializationAccess::Control {
            RequestSerializationQueueKey::Control(Box::new(key))
        } else {
            key
        };
        let should_spawn = {
            let mut state = self.inner.lock().await;
            let existing = state.queues.get(&key);
            if is_control {
                // Match the independent outer control envelope. Never charge
                // ordinary bytes here: they may be full during an interrupt.
                if request.estimated_bytes() > 64 * 1024 {
                    return RequestAdmission::Rejected(RequestAdmissionError::TotalBytes);
                }
                if state.total_control >= MAX_TOTAL_QUEUED_REQUESTS {
                    return RequestAdmission::Rejected(RequestAdmissionError::ControlLane);
                }
                // Control traffic is admitted from its own reserved capacity and is excluded
                // from the queued-payload byte budget, so a thread saturated with large
                // queued mutations still accepts an interrupt.
                if existing.is_some_and(|queues| {
                    queues.control_len() >= self.limits.max_queued_control_per_key
                }) {
                    return RequestAdmission::Rejected(RequestAdmissionError::ControlLane);
                }
            } else {
                if state.total_queued >= self.limits.max_total_queued {
                    return RequestAdmission::Rejected(RequestAdmissionError::TotalQueue);
                }
                if existing
                    .is_some_and(|queues| queues.ordered_len() >= self.limits.max_queued_per_key)
                {
                    return RequestAdmission::Rejected(RequestAdmissionError::PerKeyQueue);
                }
                let request_bytes = request.estimated_bytes();
                if state.total_queued_bytes.saturating_add(request_bytes)
                    > self.limits.max_total_queued_bytes
                {
                    return RequestAdmission::Rejected(RequestAdmissionError::TotalBytes);
                }
                let key_bytes = existing.map(KeyQueues::ordered_bytes).unwrap_or(0);
                if key_bytes.saturating_add(request_bytes) > self.limits.max_queued_bytes_per_key {
                    return RequestAdmission::Rejected(RequestAdmissionError::PerKeyBytes);
                }
            }
            let request_bytes = if is_control {
                0
            } else {
                request.estimated_bytes()
            };
            if is_control {
                state.total_control += 1;
            } else {
                state.total_queued += 1;
            }
            state.total_queued_bytes = state.total_queued_bytes.saturating_add(request_bytes);
            match state.queues.get_mut(&key) {
                Some(queues) => {
                    queues.push(request);
                    false
                }
                None => {
                    let mut queues = KeyQueues::default();
                    queues.push(request);
                    state.queues.insert(key.clone(), queues);
                    true
                }
            }
        };

        if should_spawn {
            let queues = self.clone();
            let span = tracing::debug_span!("app_server.serialized_request_queue", ?key);
            tokio::spawn(async move { queues.drain(key).await }.instrument(span));
        }
        RequestAdmission::Accepted
    }

    /// Drops work that has not yet left a serialization queue for a closing
    /// connection. Requests already handed to a drain are still rejected or
    /// cancelled by the connection gate itself.
    pub(crate) async fn cancel_for_gate(&self, gate: &Arc<ConnectionRpcGate>) -> usize {
        let mut state = self.inner.lock().await;
        let mut cancelled = 0;
        let mut cancelled_control = 0;
        let mut cancelled_bytes = 0usize;
        for queues in state.queues.values_mut() {
            let previous_control_len = queues.control_len();
            queues
                .control
                .retain(|request| !request.request.belongs_to_gate(gate));

            let previous_len = queues.ordered_len();
            let previous_bytes = queues.ordered_bytes();
            queues
                .ordered
                .retain(|request| !request.request.belongs_to_gate(gate));
            cancelled_control += previous_control_len - queues.control_len();
            cancelled += previous_len - queues.ordered_len();
            let retained_bytes = queues.ordered_bytes();
            cancelled_bytes =
                cancelled_bytes.saturating_add(previous_bytes.saturating_sub(retained_bytes));
            if previous_control_len != queues.control_len() || previous_len != queues.ordered_len()
            {
                // Removing a queued writer can admit readers while an earlier
                // read is still running. Wake the existing drain rather than
                // waiting for that unrelated read or another enqueue to finish.
                queues.changed.notify_one();
            }
        }
        state.total_queued -= cancelled;
        state.total_control -= cancelled_control;
        state.total_queued_bytes = state.total_queued_bytes.saturating_sub(cancelled_bytes);
        cancelled + cancelled_control
    }

    async fn drain(self, key: RequestSerializationQueueKey) {
        let mut running = FuturesUnordered::new();
        let mut exclusive = false;
        loop {
            let changed = {
                let mut state = self.inner.lock().await;
                let RequestSerializationState {
                    queues,
                    total_queued,
                    total_queued_bytes,
                    ..
                } = &mut *state;
                let Some(queue) = queues.get_mut(&key) else {
                    return;
                };
                loop {
                    let next = queue.control.front().or_else(|| queue.ordered.front());
                    let Some(next) = next else { break };
                    // Refill free reader slots, but never cross a queued writer.
                    if !running.is_empty()
                        && (exclusive
                            || next.access != RequestSerializationAccess::SharedRead
                            || running.len() >= self.limits.max_concurrent_shared_reads)
                    {
                        break;
                    }
                    let control = next.access == RequestSerializationAccess::Control;
                    let Some(request) = (if control {
                        queue.control.pop_front()
                    } else {
                        queue.ordered.pop_front()
                    }) else {
                        break;
                    };
                    exclusive = request.access != RequestSerializationAccess::SharedRead;
                    let reserved = matches!(
                        request.access,
                        RequestSerializationAccess::Control
                            | RequestSerializationAccess::OrderedControl
                    );
                    if !reserved {
                        *total_queued -= 1;
                        *total_queued_bytes =
                            total_queued_bytes.saturating_sub(request.estimated_bytes());
                    }
                    running.push(async move {
                        request.request.run().await;
                        reserved
                    });
                }
                if running.is_empty() {
                    queues.remove(&key);
                    return;
                }
                Arc::clone(&queue.changed)
            };
            tokio::select! {
                biased;
                completed = running.next() => {
                    if running.is_empty() { exclusive = false; }
                    if completed == Some(true) {
                        self.inner.lock().await.total_control -= 1;
                    }
                }
                _ = changed.notified() => {}
            }
        }
    }
}


