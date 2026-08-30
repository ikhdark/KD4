use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Component;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use codex_app_server_protocol::ClientRequestSerializationScope;
use futures::future::join_all;
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
const MAX_QUEUED_BYTES_PER_KEY: usize = 5 * 1024 * 1024;
const MAX_CONCURRENT_SHARED_READS: usize = 16;
/// Reserved slots per key for out-of-band control requests such as `turn/interrupt`.
///
/// Control requests carry no queued payload and never mutate ordered thread state, so
/// they are admitted from their own small budget instead of competing with queued
/// mutations. The cap still bounds control flooding.
const MAX_QUEUED_CONTROL_REQUESTS_PER_KEY: usize = 8;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RequestSerializationQueueKey {
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
    /// Out-of-band control traffic for a key, such as `turn/interrupt`.
    ///
    /// Control requests run ahead of queued work on the same key because they observe or
    /// cancel in-flight state rather than mutating the ordered sequence. They cannot reorder
    /// two mutations relative to each other: mutations keep their own FIFO lane.
    Control,
}

impl RequestSerializationQueueKey {
    pub(crate) fn from_scope(
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
                Self::Thread { thread_id },
                RequestSerializationAccess::Exclusive,
            ),
            // Control traffic shares the thread key so it stays serialized against that
            // thread, but is admitted and drained through the reserved control lane.
            ClientRequestSerializationScope::ThreadControl { thread_id } => (
                Self::Thread { thread_id },
                RequestSerializationAccess::Control,
            ),
            ClientRequestSerializationScope::ThreadPath { path } => (
                Self::ThreadPath {
                    path: normalize_thread_path(path),
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
                Self::FuzzyFileSearchSession { session_id },
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

fn normalize_thread_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
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
    #[cfg(test)]
    pub(crate) fn new(
        gate: Arc<ConnectionRpcGate>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            gate,
            estimated_bytes: 0,
            future: Box::pin(future),
        }
    }

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
}

impl KeyQueues {
    fn ordered_len(&self) -> usize {
        self.ordered.len()
    }

    fn control_len(&self) -> usize {
        self.control.len()
    }

    fn ordered_bytes(&self) -> usize {
        self.ordered
            .iter()
            .map(QueuedSerializedRequest::estimated_bytes)
            .fold(0usize, usize::saturating_add)
    }

    fn push(&mut self, request: QueuedSerializedRequest) {
        if request.access == RequestSerializationAccess::Control {
            self.control.push_back(request);
        } else {
            self.ordered.push_back(request);
        }
    }
}

#[derive(Default)]
struct RequestSerializationState {
    queues: HashMap<RequestSerializationQueueKey, KeyQueues>,
    total_queued: usize,
    total_queued_bytes: usize,
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
    #[cfg(test)]
    fn with_limits(
        max_total_queued: usize,
        max_queued_per_key: usize,
        max_concurrent_shared_reads: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RequestSerializationState::default())),
            limits: RequestSerializationLimits {
                max_total_queued: max_total_queued.max(1),
                max_queued_per_key: max_queued_per_key.max(1),
                max_total_queued_bytes: MAX_TOTAL_QUEUED_BYTES,
                max_queued_bytes_per_key: MAX_QUEUED_BYTES_PER_KEY,
                max_concurrent_shared_reads: max_concurrent_shared_reads.max(1),
                max_queued_control_per_key: MAX_QUEUED_CONTROL_REQUESTS_PER_KEY,
            },
        }
    }

    #[cfg(test)]
    fn with_limits_and_bytes(
        max_total_queued: usize,
        max_queued_per_key: usize,
        max_total_queued_bytes: usize,
        max_queued_bytes_per_key: usize,
        max_concurrent_shared_reads: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RequestSerializationState::default())),
            limits: RequestSerializationLimits {
                max_total_queued: max_total_queued.max(1),
                max_queued_per_key: max_queued_per_key.max(1),
                max_total_queued_bytes: max_total_queued_bytes.max(1),
                max_queued_bytes_per_key: max_queued_bytes_per_key.max(1),
                max_concurrent_shared_reads: max_concurrent_shared_reads.max(1),
                max_queued_control_per_key: MAX_QUEUED_CONTROL_REQUESTS_PER_KEY,
            },
        }
    }

    pub(crate) async fn enqueue(
        &self,
        key: RequestSerializationQueueKey,
        access: RequestSerializationAccess,
        request: QueuedInitializedRequest,
    ) -> RequestAdmission {
        let request = QueuedSerializedRequest { access, request };
        let is_control = access == RequestSerializationAccess::Control;
        let should_spawn = {
            let mut state = self.inner.lock().await;
            let existing = state.queues.get(&key);
            if is_control {
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
            state.total_queued += 1;
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
        let mut cancelled_bytes = 0usize;
        for queues in state.queues.values_mut() {
            let previous_control_len = queues.control_len();
            queues
                .control
                .retain(|request| !request.request.belongs_to_gate(gate));
            cancelled += previous_control_len - queues.control_len();

            let previous_len = queues.ordered_len();
            let previous_bytes = queues.ordered_bytes();
            queues
                .ordered
                .retain(|request| !request.request.belongs_to_gate(gate));
            cancelled += previous_len - queues.ordered_len();
            let retained_bytes = queues.ordered_bytes();
            cancelled_bytes =
                cancelled_bytes.saturating_add(previous_bytes.saturating_sub(retained_bytes));
        }
        state.total_queued -= cancelled;
        state.total_queued_bytes = state.total_queued_bytes.saturating_sub(cancelled_bytes);
        cancelled
    }

    async fn drain(self, key: RequestSerializationQueueKey) {
        loop {
            let requests = {
                let mut state = self.inner.lock().await;
                let Some(queues) = state.queues.get_mut(&key) else {
                    return;
                };
                // Control traffic runs ahead of queued mutations. Mutations keep strict FIFO
                // among themselves because they are only ever taken from the ordered lane.
                let next = queues
                    .control
                    .pop_front()
                    .map(|request| (request, true))
                    .or_else(|| queues.ordered.pop_front().map(|request| (request, false)));
                match next {
                    Some((request, from_control)) => {
                        let mut popped = 1;
                        let access = request.access;
                        let mut requests = vec![request];
                        if access == RequestSerializationAccess::SharedRead {
                            while requests.len() < self.limits.max_concurrent_shared_reads
                                && queues.ordered.front().is_some_and(|request| {
                                    request.access == RequestSerializationAccess::SharedRead
                                })
                            {
                                let Some(request) = queues.ordered.pop_front() else {
                                    break;
                                };
                                requests.push(request);
                                popped += 1;
                            }
                        }
                        state.total_queued -= popped;
                        if !from_control {
                            state.total_queued_bytes = state.total_queued_bytes.saturating_sub(
                                requests
                                    .iter()
                                    .map(QueuedSerializedRequest::estimated_bytes)
                                    .fold(0usize, usize::saturating_add),
                            );
                        }
                        requests
                    }
                    None => {
                        state.queues.remove(&key);
                        return;
                    }
                }
            };

            join_all(requests.into_iter().map(|request| request.request.run())).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;
    use tokio::time::Duration;
    use tokio::time::timeout;

    const FIRST_REQUEST_VALUE: i32 = 1;
    const SECOND_REQUEST_VALUE: i32 = 2;
    const THIRD_REQUEST_VALUE: i32 = 3;

    fn gate() -> Arc<ConnectionRpcGate> {
        Arc::new(ConnectionRpcGate::new())
    }

    fn queue_drain_timeout() -> Duration {
        Duration::from_secs(/*secs*/ 1)
    }

    fn shutdown_wait_timeout() -> Duration {
        Duration::from_millis(/*millis*/ 50)
    }

    #[test]
    fn thread_path_scope_normalizes_lexical_aliases() {
        let (aliased, _) = RequestSerializationQueueKey::from_scope(
            ConnectionId(1),
            ClientRequestSerializationScope::ThreadPath {
                path: PathBuf::from("threads")
                    .join("old")
                    .join("..")
                    .join("active"),
            },
        );
        let (direct, _) = RequestSerializationQueueKey::from_scope(
            ConnectionId(2),
            ClientRequestSerializationScope::ThreadPath {
                path: PathBuf::from("threads").join("active"),
            },
        );

        assert_eq!(aliased, direct);
    }

    #[tokio::test]
    async fn same_key_requests_run_fifo() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let gate = gate();
        let (tx, mut rx) = mpsc::unbounded_channel();

        for value in [
            FIRST_REQUEST_VALUE,
            SECOND_REQUEST_VALUE,
            THIRD_REQUEST_VALUE,
        ] {
            let tx = tx.clone();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                        tx.send(value).expect("receiver should be open");
                    }),
                )
                .await;
        }
        drop(tx);

        let mut values = Vec::new();
        while let Some(value) = timeout(queue_drain_timeout(), rx.recv())
            .await
            .expect("timed out waiting for queued request")
        {
            values.push(value);
        }

        assert_eq!(
            values,
            vec![
                FIRST_REQUEST_VALUE,
                SECOND_REQUEST_VALUE,
                THIRD_REQUEST_VALUE
            ]
        );
    }

    #[tokio::test]
    async fn queue_rejects_bytes_before_admission() {
        let queues = RequestSerializationQueues::with_limits_and_bytes(10, 10, 10, 10, 1);
        let key = RequestSerializationQueueKey::Global("byte-limit");
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();

        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );
        timeout(queue_drain_timeout(), started_rx)
            .await
            .expect("blocker should start")
            .expect("blocker signal should remain open");

        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new_with_estimated_bytes(gate(), 6, async {}),
                )
                .await,
            RequestAdmission::Accepted
        );
        assert_eq!(
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new_with_estimated_bytes(gate(), 5, async {}),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::TotalBytes)
        );
        let _ = release_tx.send(());
    }

    /// N14: `turn/interrupt` must reach the runtime even when the thread's ordered lane is
    /// saturated with large queued mutations, because control traffic carries no queued
    /// payload and is admitted from its own reserved budget.
    #[tokio::test]
    async fn control_requests_are_admitted_when_the_ordered_byte_budget_is_full() {
        let queues = RequestSerializationQueues::with_limits_and_bytes(10, 10, 10, 10, 1);
        let key = RequestSerializationQueueKey::Thread {
            thread_id: "thread-1".to_string(),
        };
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();

        // Occupy the drain so later requests stay queued.
        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );
        timeout(queue_drain_timeout(), started_rx)
            .await
            .expect("blocker should start")
            .expect("blocker signal should remain open");

        // Fill the ordered byte budget.
        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new_with_estimated_bytes(gate(), 10, async {}),
                )
                .await,
            RequestAdmission::Accepted
        );
        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new_with_estimated_bytes(gate(), 5, async {}),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::TotalBytes),
            "an ordered mutation must still be rejected once the byte budget is full"
        );

        let (interrupt_tx, interrupt_rx) = oneshot::channel::<()>();
        assert_eq!(
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Control,
                    QueuedInitializedRequest::new_with_estimated_bytes(gate(), 5, async move {
                        let _ = interrupt_tx.send(());
                    }),
                )
                .await,
            RequestAdmission::Accepted,
            "an interrupt must be admitted even when the ordered byte budget is full"
        );

        let _ = release_tx.send(());
        timeout(queue_drain_timeout(), interrupt_rx)
            .await
            .expect("interrupt should run")
            .expect("interrupt signal should remain open");
    }

    /// N14: a queued interrupt runs ahead of ordered work that was queued before it, while
    /// the ordered mutations keep their own FIFO order.
    #[tokio::test]
    async fn control_requests_run_before_queued_mutations_without_reordering_them() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Thread {
            thread_id: "thread-1".to_string(),
        };
        let gate = gate();
        let (tx, mut rx) = mpsc::unbounded_channel::<&'static str>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();

        let blocker_tx = tx.clone();
        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    blocker_tx.send("blocker").expect("receiver open");
                }),
            )
            .await;
        timeout(queue_drain_timeout(), started_rx)
            .await
            .expect("blocker should start")
            .expect("blocker signal should remain open");

        for label in ["start", "rollback", "inject"] {
            let tx = tx.clone();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                        tx.send(label).expect("receiver open");
                    }),
                )
                .await;
        }

        let interrupt_tx = tx.clone();
        queues
            .enqueue(
                key,
                RequestSerializationAccess::Control,
                QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                    interrupt_tx.send("interrupt").expect("receiver open");
                }),
            )
            .await;

        let _ = release_tx.send(());
        let mut observed = Vec::new();
        for _ in 0..5 {
            let value = timeout(queue_drain_timeout(), rx.recv())
                .await
                .expect("request should run")
                .expect("sender should remain open");
            observed.push(value);
        }

        assert_eq!(
            observed,
            vec!["blocker", "interrupt", "start", "rollback", "inject"],
            "the interrupt must jump the queued mutations while they stay FIFO"
        );
    }

    /// N14: the reserved control lane is bounded so interrupt traffic cannot flood a thread.
    #[tokio::test]
    async fn control_lane_capacity_bounds_interrupt_flooding() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Thread {
            thread_id: "thread-1".to_string(),
        };
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (started_tx, started_rx) = oneshot::channel::<()>();

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), started_rx)
            .await
            .expect("blocker should start")
            .expect("blocker signal should remain open");

        for _ in 0..MAX_QUEUED_CONTROL_REQUESTS_PER_KEY {
            assert_eq!(
                queues
                    .enqueue(
                        key.clone(),
                        RequestSerializationAccess::Control,
                        QueuedInitializedRequest::new(gate(), async {}),
                    )
                    .await,
                RequestAdmission::Accepted
            );
        }
        assert_eq!(
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Control,
                    QueuedInitializedRequest::new(gate(), async {}),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::ControlLane)
        );
        let _ = release_tx.send(());
    }

    #[test]
    fn turn_interrupt_uses_the_thread_control_lane() {
        let (key, access) = RequestSerializationQueueKey::from_scope(
            ConnectionId(1),
            ClientRequestSerializationScope::ThreadControl {
                thread_id: "thread-1".to_string(),
            },
        );
        let (mutation_key, mutation_access) = RequestSerializationQueueKey::from_scope(
            ConnectionId(1),
            ClientRequestSerializationScope::Thread {
                thread_id: "thread-1".to_string(),
            },
        );

        assert_eq!(
            key, mutation_key,
            "control traffic must stay serialized against its own thread"
        );
        assert_eq!(access, RequestSerializationAccess::Control);
        assert_eq!(mutation_access, RequestSerializationAccess::Exclusive);
    }

    #[tokio::test]
    async fn different_keys_run_concurrently() {
        let queues = RequestSerializationQueues::default();
        let (blocked_tx, blocked_rx) = oneshot::channel::<()>();
        let (ran_tx, ran_rx) = oneshot::channel::<()>();

        queues
            .enqueue(
                RequestSerializationQueueKey::Global("blocked"),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    let _ = blocked_rx.await;
                }),
            )
            .await;
        queues
            .enqueue(
                RequestSerializationQueueKey::Global("other"),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    ran_tx.send(()).expect("receiver should be open");
                }),
            )
            .await;

        timeout(queue_drain_timeout(), ran_rx)
            .await
            .expect("other key should not be blocked")
            .expect("sender should be open");
        blocked_tx
            .send(())
            .expect("blocked request should be waiting");
    }

    #[tokio::test]
    async fn closed_gate_request_is_skipped_and_following_requests_continue() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let live_gate = gate();
        let closed_gate = gate();
        closed_gate.close().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (blocked_tx, blocked_rx) = oneshot::channel::<()>();

        {
            let tx = tx.clone();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&live_gate), async move {
                        tx.send(FIRST_REQUEST_VALUE)
                            .expect("receiver should be open");
                        let _ = blocked_rx.await;
                    }),
                )
                .await;
        }
        {
            let tx = tx.clone();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(closed_gate, async move {
                        tx.send(SECOND_REQUEST_VALUE)
                            .expect("receiver should be open");
                    }),
                )
                .await;
        }
        {
            let tx = tx.clone();
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(live_gate, async move {
                        tx.send(THIRD_REQUEST_VALUE)
                            .expect("receiver should be open");
                    }),
                )
                .await;
        }
        drop(tx);

        assert_eq!(
            timeout(queue_drain_timeout(), rx.recv())
                .await
                .expect("timed out waiting for first request"),
            Some(FIRST_REQUEST_VALUE)
        );
        blocked_tx
            .send(())
            .expect("blocked request should be waiting");

        let mut values = Vec::new();
        while let Some(value) = timeout(queue_drain_timeout(), rx.recv())
            .await
            .expect("timed out waiting for queue to drain")
        {
            values.push(value);
        }

        assert_eq!(values, vec![THIRD_REQUEST_VALUE]);
    }

    #[tokio::test]
    async fn shutdown_of_live_gate_cancels_running_and_skips_already_queued_requests() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let live_gate = gate();
        let cancellation = live_gate.cancellation_token();
        let (tx, mut rx) = mpsc::unbounded_channel();

        {
            let tx = tx.clone();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&live_gate), async move {
                        tx.send(FIRST_REQUEST_VALUE)
                            .expect("receiver should be open");
                        cancellation.cancelled().await;
                    }),
                )
                .await;
        }
        {
            let tx = tx.clone();
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(live_gate.clone(), async move {
                        tx.send(SECOND_REQUEST_VALUE)
                            .expect("receiver should be open");
                    }),
                )
                .await;
        }
        drop(tx);

        assert_eq!(
            timeout(queue_drain_timeout(), rx.recv())
                .await
                .expect("timed out waiting for first request"),
            Some(FIRST_REQUEST_VALUE)
        );

        live_gate.close().await;
        assert_eq!(queues.cancel_for_gate(&live_gate).await, 1);
        let gate_for_shutdown = Arc::clone(&live_gate);
        let shutdown_task = tokio::spawn(async move {
            gate_for_shutdown.shutdown().await;
        });

        timeout(queue_drain_timeout(), shutdown_task)
            .await
            .expect("shutdown should cancel the running request")
            .expect("shutdown task should complete");

        assert_eq!(
            timeout(queue_drain_timeout(), rx.recv())
                .await
                .expect("timed out waiting for queue to drain"),
            None
        );
    }

    #[tokio::test]
    async fn per_key_limit_rejects_without_polling_request() {
        let queues = RequestSerializationQueues::with_limits(4, 1, 2);
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (queued_ran_tx, queued_ran_rx) = oneshot::channel::<()>();

        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        blocker_started_tx
                            .send(())
                            .expect("receiver should be open");
                        let _ = blocker_release_rx.await;
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        queued_ran_tx.send(()).expect("receiver should be open");
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );

        let rejected_polled = Arc::new(AtomicBool::new(false));
        let rejected_polled_for_future = Arc::clone(&rejected_polled);
        assert_eq!(
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        rejected_polled_for_future.store(true, Ordering::SeqCst);
                    }),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::PerKeyQueue)
        );
        assert!(!rejected_polled.load(Ordering::SeqCst));

        blocker_release_tx
            .send(())
            .expect("blocker should be waiting");
        timeout(queue_drain_timeout(), queued_ran_rx)
            .await
            .expect("queued request should run")
            .expect("sender should be open");
        assert!(!rejected_polled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn total_limit_rejects_across_keys() {
        let queues = RequestSerializationQueues::with_limits(2, 2, 2);
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (first_release_tx, first_release_rx) = oneshot::channel::<()>();
        let (second_release_tx, second_release_rx) = oneshot::channel::<()>();

        for (key, value, release_rx) in [
            (
                RequestSerializationQueueKey::Global("first"),
                FIRST_REQUEST_VALUE,
                first_release_rx,
            ),
            (
                RequestSerializationQueueKey::Global("second"),
                SECOND_REQUEST_VALUE,
                second_release_rx,
            ),
        ] {
            let started_tx = started_tx.clone();
            assert_eq!(
                queues
                    .enqueue(
                        key,
                        RequestSerializationAccess::Exclusive,
                        QueuedInitializedRequest::new(gate(), async move {
                            started_tx.send(value).expect("receiver should be open");
                            let _ = release_rx.await;
                        }),
                    )
                    .await,
                RequestAdmission::Accepted
            );
        }
        for _ in 0..2 {
            timeout(queue_drain_timeout(), started_rx.recv())
                .await
                .expect("blocker should start")
                .expect("sender should be open");
        }

        for key in [
            RequestSerializationQueueKey::Global("first"),
            RequestSerializationQueueKey::Global("second"),
        ] {
            assert_eq!(
                queues
                    .enqueue(
                        key,
                        RequestSerializationAccess::Exclusive,
                        QueuedInitializedRequest::new(gate(), async {}),
                    )
                    .await,
                RequestAdmission::Accepted
            );
        }
        let rejected_polled = Arc::new(AtomicBool::new(false));
        let rejected_polled_for_future = Arc::clone(&rejected_polled);
        assert_eq!(
            queues
                .enqueue(
                    RequestSerializationQueueKey::Global("first"),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        rejected_polled_for_future.store(true, Ordering::SeqCst);
                    }),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::TotalQueue)
        );
        assert!(!rejected_polled.load(Ordering::SeqCst));

        first_release_tx
            .send(())
            .expect("blocker should be waiting");
        second_release_tx
            .send(())
            .expect("blocker should be waiting");
    }

    #[tokio::test]
    async fn admission_recovers_after_queue_drains() {
        let queues = RequestSerializationQueues::with_limits(1, 1, 1);
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (queued_ran_tx, queued_ran_rx) = oneshot::channel::<()>();

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    blocker_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = blocker_release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");
        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        queued_ran_tx.send(()).expect("receiver should be open");
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );
        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async {}),
                )
                .await,
            RequestAdmission::Rejected(RequestAdmissionError::TotalQueue)
        );

        blocker_release_tx
            .send(())
            .expect("blocker should be waiting");
        timeout(queue_drain_timeout(), queued_ran_rx)
            .await
            .expect("queued request should run")
            .expect("sender should be open");

        let (recovered_tx, recovered_rx) = oneshot::channel::<()>();
        assert_eq!(
            queues
                .enqueue(
                    key,
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(gate(), async move {
                        recovered_tx.send(()).expect("receiver should be open");
                    }),
                )
                .await,
            RequestAdmission::Accepted
        );
        timeout(queue_drain_timeout(), recovered_rx)
            .await
            .expect("admission should recover")
            .expect("sender should be open");
    }

    #[tokio::test]
    async fn shared_read_batch_respects_concurrency_limit() {
        let queues = RequestSerializationQueues::with_limits(8, 8, 2);
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, _) = broadcast::channel::<()>(1);

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    blocker_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = blocker_release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        for value in [
            FIRST_REQUEST_VALUE,
            SECOND_REQUEST_VALUE,
            THIRD_REQUEST_VALUE,
        ] {
            let started_tx = started_tx.clone();
            let mut release_rx = release_tx.subscribe();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::SharedRead,
                    QueuedInitializedRequest::new(gate(), async move {
                        started_tx.send(value).expect("receiver should be open");
                        let _ = release_rx.recv().await;
                    }),
                )
                .await;
        }
        blocker_release_tx
            .send(())
            .expect("blocker should be waiting");

        for _ in 0..2 {
            timeout(queue_drain_timeout(), started_rx.recv())
                .await
                .expect("shared read should start")
                .expect("sender should be open");
        }
        assert!(
            timeout(shutdown_wait_timeout(), started_rx.recv())
                .await
                .is_err()
        );

        release_tx.send(()).expect("shared reads should be waiting");
        timeout(queue_drain_timeout(), started_rx.recv())
            .await
            .expect("next shared-read batch should start")
            .expect("sender should be open");
    }

    #[tokio::test]
    async fn same_key_shared_reads_run_concurrently() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, _) = broadcast::channel::<()>(/*capacity*/ 1);

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    blocker_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = blocker_release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        for value in [FIRST_REQUEST_VALUE, SECOND_REQUEST_VALUE] {
            let started_tx = started_tx.clone();
            let mut release_rx = release_tx.subscribe();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::SharedRead,
                    QueuedInitializedRequest::new(gate(), async move {
                        started_tx.send(value).expect("receiver should be open");
                        let _ = release_rx.recv().await;
                    }),
                )
                .await;
        }
        drop(started_tx);
        blocker_release_tx
            .send(())
            .expect("blocker should still be waiting");

        let mut started = Vec::new();
        for _ in 0..2 {
            started.push(
                timeout(queue_drain_timeout(), started_rx.recv())
                    .await
                    .expect("timed out waiting for shared read")
                    .expect("sender should be open"),
            );
        }
        assert_eq!(started, vec![FIRST_REQUEST_VALUE, SECOND_REQUEST_VALUE]);

        release_tx
            .send(())
            .expect("shared reads should still be waiting");
    }

    #[tokio::test]
    async fn exclusive_write_waits_for_running_shared_reads() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (read_started_tx, mut read_started_rx) = mpsc::unbounded_channel();
        let (read_release_tx, _) = broadcast::channel::<()>(/*capacity*/ 1);
        let (write_started_tx, write_started_rx) = oneshot::channel::<()>();

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    blocker_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = blocker_release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        for value in [FIRST_REQUEST_VALUE, SECOND_REQUEST_VALUE] {
            let read_started_tx = read_started_tx.clone();
            let mut read_release_rx = read_release_tx.subscribe();
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::SharedRead,
                    QueuedInitializedRequest::new(gate(), async move {
                        read_started_tx
                            .send(value)
                            .expect("receiver should be open");
                        let _ = read_release_rx.recv().await;
                    }),
                )
                .await;
        }
        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    write_started_tx.send(()).expect("receiver should be open");
                }),
            )
            .await;
        drop(read_started_tx);
        blocker_release_tx
            .send(())
            .expect("blocker should still be waiting");

        for _ in 0..2 {
            timeout(queue_drain_timeout(), read_started_rx.recv())
                .await
                .expect("timed out waiting for shared read")
                .expect("sender should be open");
        }
        let mut write_started_rx = Box::pin(write_started_rx);
        timeout(shutdown_wait_timeout(), &mut write_started_rx)
            .await
            .expect_err("write should wait for running shared reads");

        read_release_tx
            .send(())
            .expect("shared reads should still be waiting");
        timeout(queue_drain_timeout(), &mut write_started_rx)
            .await
            .expect("write should start after shared reads finish")
            .expect("sender should be open");
    }

    #[tokio::test]
    async fn later_shared_reads_do_not_jump_ahead_of_queued_write() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
        let (first_read_started_tx, first_read_started_rx) = oneshot::channel::<()>();
        let (first_read_release_tx, first_read_release_rx) = oneshot::channel::<()>();
        let (write_started_tx, write_started_rx) = oneshot::channel::<()>();
        let (write_release_tx, write_release_rx) = oneshot::channel::<()>();
        let (later_read_started_tx, later_read_started_rx) = oneshot::channel::<()>();

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    blocker_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = blocker_release_rx.await;
                }),
            )
            .await;
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::SharedRead,
                QueuedInitializedRequest::new(gate(), async move {
                    first_read_started_tx
                        .send(())
                        .expect("receiver should be open");
                    let _ = first_read_release_rx.await;
                }),
            )
            .await;
        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                QueuedInitializedRequest::new(gate(), async move {
                    write_started_tx.send(()).expect("receiver should be open");
                    let _ = write_release_rx.await;
                }),
            )
            .await;
        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::SharedRead,
                QueuedInitializedRequest::new(gate(), async move {
                    later_read_started_tx
                        .send(())
                        .expect("receiver should be open");
                }),
            )
            .await;
        blocker_release_tx
            .send(())
            .expect("blocker should still be waiting");

        timeout(queue_drain_timeout(), first_read_started_rx)
            .await
            .expect("first read should start")
            .expect("sender should be open");
        let mut write_started_rx = Box::pin(write_started_rx);
        timeout(shutdown_wait_timeout(), &mut write_started_rx)
            .await
            .expect_err("write should wait for the first read");
        let mut later_read_started_rx = Box::pin(later_read_started_rx);
        timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
            .await
            .expect_err("later read should wait behind the queued write");

        first_read_release_tx
            .send(())
            .expect("first read should still be waiting");
        timeout(queue_drain_timeout(), &mut write_started_rx)
            .await
            .expect("write should start after the first read")
            .expect("sender should be open");
        timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
            .await
            .expect_err("later read should still wait while the write is running");

        write_release_tx
            .send(())
            .expect("write should still be waiting");
        timeout(queue_drain_timeout(), &mut later_read_started_rx)
            .await
            .expect("later read should start after the write")
            .expect("sender should be open");
    }
}
