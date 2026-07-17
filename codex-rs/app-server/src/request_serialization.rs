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
use crate::connection_rpc_gate::RpcAdmissionError;
use crate::connection_rpc_gate::RpcAdmissionPermit;
use crate::outgoing_message::ConnectionId;

type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

const PER_SERIALIZATION_KEY_ADMISSION_LIMIT: usize = 64;
const SHARED_READ_BATCH_LIMIT: usize = 16;

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
    admission: RpcAdmissionPermit,
    future: BoxFutureUnit,
}

impl QueuedInitializedRequest {
    pub(crate) fn try_new(
        gate: Arc<ConnectionRpcGate>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Result<Self, RpcAdmissionError> {
        Ok(Self {
            admission: gate.try_admit()?,
            future: Box::pin(future),
        })
    }

    #[cfg(test)]
    pub(crate) fn new(
        gate: Arc<ConnectionRpcGate>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self::try_new(gate, future).expect("test request should be admitted")
    }

    pub(crate) async fn run(self) {
        let Self { admission, future } = self;
        admission.run(future).await;
    }

    fn is_admitted(&self) -> bool {
        self.admission.is_active()
    }
}

struct QueuedSerializedRequest {
    access: RequestSerializationAccess,
    request: QueuedInitializedRequest,
}

#[derive(Default)]
struct RequestSerializationQueue {
    pending: VecDeque<QueuedSerializedRequest>,
    admitted_count: usize,
}

impl RequestSerializationQueue {
    fn prune_cancelled(&mut self) {
        let before = self.pending.len();
        self.pending
            .retain(|request| request.request.is_admitted());
        self.admitted_count -= before - self.pending.len();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestEnqueueResult {
    Enqueued,
    Dropped,
    Overloaded,
}

#[derive(Clone)]
pub(crate) struct RequestSerializationQueues {
    inner: Arc<Mutex<HashMap<RequestSerializationQueueKey, RequestSerializationQueue>>>,
    per_key_limit: usize,
    shared_read_batch_limit: usize,
}

impl Default for RequestSerializationQueues {
    fn default() -> Self {
        Self::with_limits(
            PER_SERIALIZATION_KEY_ADMISSION_LIMIT,
            SHARED_READ_BATCH_LIMIT,
        )
    }
}

impl RequestSerializationQueues {
    fn with_limits(per_key_limit: usize, shared_read_batch_limit: usize) -> Self {
        assert!(per_key_limit > 0, "per-key RPC admission limit must be positive");
        assert!(
            shared_read_batch_limit > 0,
            "shared-read batch limit must be positive"
        );
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            per_key_limit,
            shared_read_batch_limit,
        }
    }

    pub(crate) async fn enqueue(
        &self,
        key: RequestSerializationQueueKey,
        access: RequestSerializationAccess,
        request: QueuedInitializedRequest,
    ) -> RequestEnqueueResult {
        if !request.is_admitted() {
            return RequestEnqueueResult::Dropped;
        }
        let request = QueuedSerializedRequest { access, request };
        let should_spawn = {
            let mut queues = self.inner.lock().await;
            let queue = queues.entry(key.clone()).or_default();
            queue.prune_cancelled();
            if queue.admitted_count >= self.per_key_limit {
                return RequestEnqueueResult::Overloaded;
            }
            let should_spawn = queue.admitted_count == 0;
            queue.admitted_count += 1;
            queue.pending.push_back(request);
            should_spawn
        };

        if should_spawn {
            let queues = self.clone();
            let span = tracing::debug_span!("app_server.serialized_request_queue", ?key);
            tokio::spawn(async move { queues.drain(key).await }.instrument(span));
        }
        RequestEnqueueResult::Enqueued
    }

    async fn drain(self, key: RequestSerializationQueueKey) {
        loop {
            let requests = {
                let mut queues = self.inner.lock().await;
                let Some(queue) = queues.get_mut(&key) else {
                    return;
                };
                queue.prune_cancelled();
                match queue.pending.pop_front() {
                    Some(request) => {
                        let access = request.access;
                        let mut requests = vec![request];
                        if access == RequestSerializationAccess::SharedRead {
                            while requests.len() < self.shared_read_batch_limit
                                && queue.pending.front().is_some_and(|request| {
                                    request.access == RequestSerializationAccess::SharedRead
                                })
                            {
                                let Some(request) = queue.pending.pop_front() else {
                                    break;
                                };
                                requests.push(request);
                            }
                        }
                        requests
                    }
                    None => {
                        debug_assert_eq!(queue.admitted_count, 0);
                        queues.remove(&key);
                        return;
                    }
                }
            };

            let completed_count = requests.len();
            join_all(requests.into_iter().map(|request| request.request.run())).await;
            let mut queues = self.inner.lock().await;
            let Some(queue) = queues.get_mut(&key) else {
                return;
            };
            queue.admitted_count -= completed_count;
            if queue.admitted_count == 0 {
                debug_assert!(queue.pending.is_empty());
                queues.remove(&key);
                return;
            }
        }
    }

    #[cfg(test)]
    async fn queue_count(&self) -> usize {
        self.inner.lock().await.len()
    }

    #[cfg(test)]
    async fn admitted_count(&self) -> usize {
        self.inner
            .lock()
            .await
            .values()
            .map(|queue| queue.admitted_count)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
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

    async fn wait_until_drained(queues: &RequestSerializationQueues) {
        timeout(queue_drain_timeout(), async {
            loop {
                if queues.queue_count().await == 0 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("serialized request queues should drain");
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
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (blocked_tx, blocked_rx) = oneshot::channel::<()>();
        let closed_request = {
            let tx = tx.clone();
            QueuedInitializedRequest::new(Arc::clone(&closed_gate), async move {
                tx.send(SECOND_REQUEST_VALUE)
                    .expect("receiver should be open");
            })
        };
        closed_gate.close().await;

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
        queues
            .enqueue(
                key.clone(),
                RequestSerializationAccess::Exclusive,
                closed_request,
            )
            .await;
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
    async fn shutdown_of_live_gate_skips_already_queued_requests() {
        let queues = RequestSerializationQueues::default();
        let key = RequestSerializationQueueKey::Global("test");
        let live_gate = gate();
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

        let gate_for_shutdown = Arc::clone(&live_gate);
        let shutdown_task = tokio::spawn(async move {
            gate_for_shutdown.shutdown().await;
        });

        timeout(shutdown_wait_timeout(), shutdown_task)
            .await
            .expect_err("shutdown should wait for the running request");

        blocked_tx
            .send(())
            .expect("blocked request should still be waiting");

        assert_eq!(
            timeout(queue_drain_timeout(), rx.recv())
                .await
                .expect("timed out waiting for queue to drain"),
            None
        );
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

    #[tokio::test]
    async fn same_key_flood_keeps_queued_and_running_counts_bounded() {
        let per_key_limit = 3;
        let queues = RequestSerializationQueues::with_limits(per_key_limit, /*batch_limit*/ 2);
        let gate = Arc::new(ConnectionRpcGate::with_limits(
            /*global_limit*/ 10,
            /*per_connection_limit*/ 10,
        ));
        let key = RequestSerializationQueueKey::Global("same-key-flood");
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, _) = broadcast::channel::<()>(/*capacity*/ 1);
        let mut enqueued = 0;
        let mut overloaded = 0;

        for request_index in 0..8 {
            let started_tx = started_tx.clone();
            let mut release_rx = release_tx.subscribe();
            let request = QueuedInitializedRequest::try_new(Arc::clone(&gate), async move {
                started_tx
                    .send(request_index)
                    .expect("receiver should be open");
                let _ = release_rx.recv().await;
            })
            .expect("per-connection admission should have room");
            match queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    request,
                )
                .await
            {
                RequestEnqueueResult::Enqueued => enqueued += 1,
                RequestEnqueueResult::Dropped => panic!("test gate should remain open"),
                RequestEnqueueResult::Overloaded => overloaded += 1,
            }
        }
        drop(started_tx);

        timeout(queue_drain_timeout(), started_rx.recv())
            .await
            .expect("first same-key request should start")
            .expect("sender should be open");
        assert_eq!(enqueued, per_key_limit);
        assert_eq!(overloaded, 8 - per_key_limit);
        assert_eq!(queues.queue_count().await, 1);
        assert_eq!(queues.admitted_count().await, per_key_limit);
        assert_eq!(gate.admitted_count(), per_key_limit);
        assert_eq!(gate.inflight_count(), 1);

        release_tx
            .send(())
            .expect("running same-key request should be waiting");
        let mut started_count = 1;
        while let Some(_request_index) = timeout(queue_drain_timeout(), started_rx.recv())
            .await
            .expect("same-key queue should finish")
        {
            started_count += 1;
        }
        assert_eq!(started_count, per_key_limit);
        wait_until_drained(&queues).await;
        assert_eq!(queues.admitted_count().await, 0);
        assert_eq!(gate.admitted_count(), 0);
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn unique_key_flood_keeps_queue_tasks_and_running_counts_bounded() {
        let per_connection_limit = 4;
        let queues = RequestSerializationQueues::with_limits(
            /*per_key_limit*/ 4,
            /*batch_limit*/ 2,
        );
        let gate = Arc::new(ConnectionRpcGate::with_limits(
            /*global_limit*/ per_connection_limit,
            per_connection_limit,
        ));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, _) = broadcast::channel::<()>(/*capacity*/ per_connection_limit);
        let mut enqueued = 0;
        let mut overloaded = 0;

        for request_index in 0..10 {
            let started_tx = started_tx.clone();
            let mut release_rx = release_tx.subscribe();
            let request = QueuedInitializedRequest::try_new(Arc::clone(&gate), async move {
                started_tx
                    .send(request_index)
                    .expect("receiver should be open");
                let _ = release_rx.recv().await;
            });
            match request {
                Ok(request) => {
                    assert_eq!(
                        queues
                            .enqueue(
                                RequestSerializationQueueKey::Thread {
                                    thread_id: format!("thread-{request_index}"),
                                },
                                RequestSerializationAccess::Exclusive,
                                request,
                            )
                            .await,
                        RequestEnqueueResult::Enqueued
                    );
                    enqueued += 1;
                }
                Err(RpcAdmissionError::Overloaded) => overloaded += 1,
                Err(RpcAdmissionError::Closed) => panic!("test gate should remain open"),
            }
        }
        drop(started_tx);

        for _ in 0..per_connection_limit {
            timeout(queue_drain_timeout(), started_rx.recv())
                .await
                .expect("admitted unique-key request should start")
                .expect("sender should be open");
        }
        assert_eq!(enqueued, per_connection_limit);
        assert_eq!(overloaded, 10 - per_connection_limit);
        assert_eq!(queues.queue_count().await, per_connection_limit);
        assert_eq!(queues.admitted_count().await, per_connection_limit);
        assert_eq!(gate.admitted_count(), per_connection_limit);
        assert_eq!(gate.inflight_count(), per_connection_limit);

        release_tx
            .send(())
            .expect("running unique-key requests should be waiting");
        assert_eq!(
            timeout(queue_drain_timeout(), started_rx.recv())
                .await
                .expect("all unique-key senders should close"),
            None
        );
        wait_until_drained(&queues).await;
        assert_eq!(queues.admitted_count().await, 0);
        assert_eq!(gate.admitted_count(), 0);
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn shared_read_batch_size_is_bounded() {
        let batch_limit = 2;
        let queues = RequestSerializationQueues::with_limits(
            /*per_key_limit*/ 8,
            batch_limit,
        );
        let gate = Arc::new(ConnectionRpcGate::with_limits(
            /*global_limit*/ 8,
            /*per_connection_limit*/ 8,
        ));
        let key = RequestSerializationQueueKey::Global("bounded-shared-read-batch");
        let (blocker_started_tx, blocker_started_rx) = oneshot::channel();
        let (blocker_release_tx, blocker_release_rx) = oneshot::channel();
        let (read_started_tx, mut read_started_rx) = mpsc::unbounded_channel();

        assert_eq!(
            queues
                .enqueue(
                    key.clone(),
                    RequestSerializationAccess::Exclusive,
                    QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                        blocker_started_tx
                            .send(())
                            .expect("receiver should be open");
                        let _ = blocker_release_rx.await;
                    }),
                )
                .await,
            RequestEnqueueResult::Enqueued
        );
        timeout(queue_drain_timeout(), blocker_started_rx)
            .await
            .expect("blocker should start")
            .expect("sender should be open");

        let mut release_senders = Vec::new();
        for request_index in 0..3 {
            let (release_tx, release_rx) = oneshot::channel();
            release_senders.push(release_tx);
            let read_started_tx = read_started_tx.clone();
            assert_eq!(
                queues
                    .enqueue(
                        key.clone(),
                        RequestSerializationAccess::SharedRead,
                        QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                            read_started_tx
                                .send(request_index)
                                .expect("receiver should be open");
                            let _ = release_rx.await;
                        }),
                    )
                    .await,
                RequestEnqueueResult::Enqueued
            );
        }
        drop(read_started_tx);
        blocker_release_tx
            .send(())
            .expect("blocker should still be waiting");

        for _ in 0..batch_limit {
            timeout(queue_drain_timeout(), read_started_rx.recv())
                .await
                .expect("first shared-read batch should start")
                .expect("sender should be open");
        }
        timeout(shutdown_wait_timeout(), read_started_rx.recv())
            .await
            .expect_err("next shared read should wait for the bounded batch");
        assert_eq!(gate.inflight_count(), batch_limit);

        for release_tx in release_senders.drain(..batch_limit) {
            release_tx
                .send(())
                .expect("first shared-read batch should be waiting");
        }
        timeout(queue_drain_timeout(), read_started_rx.recv())
            .await
            .expect("next shared-read batch should start")
            .expect("sender should be open");
        release_senders
            .pop()
            .expect("last shared read should have a release sender")
            .send(())
            .expect("last shared read should be waiting");
        wait_until_drained(&queues).await;
        assert_eq!(gate.admitted_count(), 0);
    }
}
