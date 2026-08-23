use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
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
const MAX_TOTAL_QUEUED_BYTES: usize = 8 * 1024 * 1024;
const MAX_QUEUED_BYTES_PER_KEY: usize = 1024 * 1024;
const MAX_CONCURRENT_SHARED_READS: usize = 16;

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
            ClientRequestSerializationScope::ThreadPath { path } => (
                Self::ThreadPath { path },
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

pub(crate) struct QueuedInitializedRequest {
    gate: Arc<ConnectionRpcGate>,
    estimated_bytes: usize,
    future: BoxFutureUnit,
}

impl QueuedInitializedRequest {
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
        gate.run(future).await;
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
}

impl Default for RequestSerializationLimits {
    fn default() -> Self {
        Self {
            max_total_queued: MAX_TOTAL_QUEUED_REQUESTS,
            max_queued_per_key: MAX_QUEUED_REQUESTS_PER_KEY,
            max_total_queued_bytes: MAX_TOTAL_QUEUED_BYTES,
            max_queued_bytes_per_key: MAX_QUEUED_BYTES_PER_KEY,
            max_concurrent_shared_reads: MAX_CONCURRENT_SHARED_READS,
        }
    }
}

#[derive(Default)]
struct RequestSerializationState {
    queues: HashMap<RequestSerializationQueueKey, VecDeque<QueuedSerializedRequest>>,
    total_queued: usize,
    total_queued_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestAdmissionError {
    TotalQueueFull,
    PerKeyQueueFull,
    TotalBytesFull,
    PerKeyBytesFull,
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
        let should_spawn = {
            let mut state = self.inner.lock().await;
            if state.total_queued >= self.limits.max_total_queued {
                return RequestAdmission::Rejected(RequestAdmissionError::TotalQueueFull);
            }
            if state
                .queues
                .get(&key)
                .is_some_and(|queue| queue.len() >= self.limits.max_queued_per_key)
            {
                return RequestAdmission::Rejected(RequestAdmissionError::PerKeyQueueFull);
            }
            let request_bytes = request.estimated_bytes();
            if state.total_queued_bytes.saturating_add(request_bytes)
                > self.limits.max_total_queued_bytes
            {
                return RequestAdmission::Rejected(RequestAdmissionError::TotalBytesFull);
            }
            let key_bytes = state
                .queues
                .get(&key)
                .map(|queue| {
                    queue
                        .iter()
                        .map(QueuedSerializedRequest::estimated_bytes)
                        .fold(0usize, usize::saturating_add)
                })
                .unwrap_or(0);
            if key_bytes.saturating_add(request_bytes) > self.limits.max_queued_bytes_per_key {
                return RequestAdmission::Rejected(RequestAdmissionError::PerKeyBytesFull);
            }
            state.total_queued += 1;
            state.total_queued_bytes = state.total_queued_bytes.saturating_add(request_bytes);
            match state.queues.get_mut(&key) {
                Some(queue) => {
                    queue.push_back(request);
                    false
                }
                None => {
                    let mut queue = VecDeque::new();
                    queue.push_back(request);
                    state.queues.insert(key.clone(), queue);
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
        for queue in state.queues.values_mut() {
            let previous_len = queue.len();
            let previous_bytes = queue
                .iter()
                .map(QueuedSerializedRequest::estimated_bytes)
                .fold(0usize, usize::saturating_add);
            queue.retain(|request| !request.request.belongs_to_gate(gate));
            cancelled += previous_len - queue.len();
            let retained_bytes = queue
                .iter()
                .map(QueuedSerializedRequest::estimated_bytes)
                .fold(0usize, usize::saturating_add);
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
                let Some(queue) = state.queues.get_mut(&key) else {
                    return;
                };
                match queue.pop_front() {
                    Some(request) => {
                        let mut popped = 1;
                        let access = request.access;
                        let mut requests = vec![request];
                        if access == RequestSerializationAccess::SharedRead {
                            while requests.len() < self.limits.max_concurrent_shared_reads
                                && queue.front().is_some_and(|request| {
                                    request.access == RequestSerializationAccess::SharedRead
                                })
                            {
                                let Some(request) = queue.pop_front() else {
                                    break;
                                };
                                requests.push(request);
                                popped += 1;
                            }
                        }
                        state.total_queued -= popped;
                        state.total_queued_bytes = state.total_queued_bytes.saturating_sub(
                            requests
                                .iter()
                                .map(QueuedSerializedRequest::estimated_bytes)
                                .fold(0usize, usize::saturating_add),
                        );
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
            RequestAdmission::Rejected(RequestAdmissionError::TotalBytesFull)
        );
        let _ = release_tx.send(());
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
            RequestAdmission::Rejected(RequestAdmissionError::PerKeyQueueFull)
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
            RequestAdmission::Rejected(RequestAdmissionError::TotalQueueFull)
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
            RequestAdmission::Rejected(RequestAdmissionError::TotalQueueFull)
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
