use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::JSONRPCErrorError;
use tokio_util::task::TaskTracker;

use crate::error_code::OVERLOADED_ERROR_CODE;

const GLOBAL_RPC_ADMISSION_LIMIT: usize = 1024;
const PER_CONNECTION_RPC_ADMISSION_LIMIT: usize = 256;
const RPC_OVERLOADED_MESSAGE: &str = "app-server RPC admission limit exceeded";

static GLOBAL_RPC_ADMISSION: OnceLock<Arc<GlobalRpcAdmission>> = OnceLock::new();

#[derive(Debug)]
struct GlobalRpcAdmission {
    limit: usize,
    admitted: AtomicUsize,
}

impl GlobalRpcAdmission {
    fn new(limit: usize) -> Self {
        assert!(limit > 0, "global RPC admission limit must be positive");
        Self {
            limit,
            admitted: AtomicUsize::new(0),
        }
    }

    fn try_acquire(&self) -> bool {
        self.admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
                (admitted < self.limit).then_some(admitted + 1)
            })
            .is_ok()
    }

    fn release(&self, count: usize) {
        if count == 0 {
            return;
        }
        let released = self
            .admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admitted| {
                admitted.checked_sub(count)
            })
            .is_ok();
        debug_assert!(released, "released more RPC admissions than held");
    }

    #[cfg(test)]
    fn admitted_count(&self) -> usize {
        self.admitted.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionStage {
    Queued,
    Running,
}

#[derive(Debug)]
struct ConnectionRpcGateState {
    accepting: bool,
    next_admission_id: u64,
    admissions: HashMap<u64, AdmissionStage>,
}

#[derive(Debug)]
struct ConnectionRpcGateInner {
    state: Mutex<ConnectionRpcGateState>,
    tasks: TaskTracker,
    global: Arc<GlobalRpcAdmission>,
    per_connection_limit: usize,
}

/// Per-connection gate for initialized RPC handler execution.
///
/// Admission is reserved before handlers are queued or spawned. Closing the
/// gate revokes queued reservations while allowing running handlers to finish.
#[derive(Debug)]
pub(crate) struct ConnectionRpcGate {
    inner: Arc<ConnectionRpcGateInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RpcAdmissionError {
    Closed,
    Overloaded,
}

#[derive(Debug)]
pub(crate) struct RpcAdmissionPermit {
    inner: Arc<ConnectionRpcGateInner>,
    admission_id: u64,
}

impl ConnectionRpcGate {
    pub(crate) fn new() -> Self {
        let global = Arc::clone(
            GLOBAL_RPC_ADMISSION
                .get_or_init(|| Arc::new(GlobalRpcAdmission::new(GLOBAL_RPC_ADMISSION_LIMIT))),
        );
        Self::with_global(global, PER_CONNECTION_RPC_ADMISSION_LIMIT)
    }

    fn with_global(global: Arc<GlobalRpcAdmission>, per_connection_limit: usize) -> Self {
        assert!(
            per_connection_limit > 0,
            "per-connection RPC admission limit must be positive"
        );
        Self {
            inner: Arc::new(ConnectionRpcGateInner {
                state: Mutex::new(ConnectionRpcGateState {
                    accepting: true,
                    next_admission_id: 0,
                    admissions: HashMap::new(),
                }),
                tasks: TaskTracker::new(),
                global,
                per_connection_limit,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(global_limit: usize, per_connection_limit: usize) -> Self {
        Self::with_global(
            Arc::new(GlobalRpcAdmission::new(global_limit)),
            per_connection_limit,
        )
    }

    pub(crate) fn try_admit(&self) -> Result<RpcAdmissionPermit, RpcAdmissionError> {
        let mut state = self.inner.state.lock().unwrap_or_else(|err| err.into_inner());
        if !state.accepting {
            return Err(RpcAdmissionError::Closed);
        }
        if state.admissions.len() >= self.inner.per_connection_limit
            || !self.inner.global.try_acquire()
        {
            return Err(RpcAdmissionError::Overloaded);
        }

        let admission_id = loop {
            let candidate = state.next_admission_id;
            state.next_admission_id = state.next_admission_id.wrapping_add(1);
            if !state.admissions.contains_key(&candidate) {
                break candidate;
            }
        };
        state
            .admissions
            .insert(admission_id, AdmissionStage::Queued);
        Ok(RpcAdmissionPermit {
            inner: Arc::clone(&self.inner),
            admission_id,
        })
    }

    pub(crate) async fn run<F>(&self, future: F)
    where
        F: Future<Output = ()>,
    {
        if let Ok(permit) = self.try_admit() {
            permit.run(future).await;
        }
    }

    pub(crate) async fn close(&self) {
        let revoked_count = {
            let mut state = self.inner.state.lock().unwrap_or_else(|err| err.into_inner());
            state.accepting = false;
            let before = state.admissions.len();
            state
                .admissions
                .retain(|_, stage| *stage == AdmissionStage::Running);
            self.inner.tasks.close();
            before - state.admissions.len()
        };
        self.inner.global.release(revoked_count);
    }

    pub(crate) async fn shutdown(&self) {
        self.close().await;
        self.inner.tasks.wait().await;
    }

    #[cfg(test)]
    async fn is_accepting(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .accepting
    }

    #[cfg(test)]
    pub(crate) fn inflight_count(&self) -> usize {
        self.inner.tasks.len()
    }

    #[cfg(test)]
    pub(crate) fn admitted_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .admissions
            .len()
    }

    #[cfg(test)]
    fn global_admitted_count(&self) -> usize {
        self.inner.global.admitted_count()
    }
}

impl RpcAdmissionPermit {
    pub(crate) async fn run<F>(self, future: F)
    where
        F: Future<Output = ()>,
    {
        let token = {
            let mut state = self.inner.state.lock().unwrap_or_else(|err| err.into_inner());
            if !state.accepting {
                return;
            }
            let Some(stage) = state.admissions.get_mut(&self.admission_id) else {
                return;
            };
            if *stage != AdmissionStage::Queued {
                return;
            }
            *stage = AdmissionStage::Running;
            self.inner.tasks.token()
        };

        future.await;
        drop(token);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .admissions
            .contains_key(&self.admission_id)
    }
}

impl Drop for RpcAdmissionPermit {
    fn drop(&mut self) {
        let released = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .admissions
            .remove(&self.admission_id)
            .is_some();
        if released {
            self.inner.global.release(1);
        }
    }
}

pub(crate) fn rpc_overloaded_error() -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: OVERLOADED_ERROR_CODE,
        message: RPC_OVERLOADED_MESSAGE.to_string(),
        data: None,
    }
}

impl Default for ConnectionRpcGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::oneshot;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn run_executes_while_open() {
        let gate = ConnectionRpcGate::new();
        let ran = Arc::new(AtomicBool::new(/*v*/ false));
        let ran_clone = Arc::clone(&ran);

        gate.run(async move {
            ran_clone.store(/*val*/ true, Ordering::Release);
        })
        .await;

        assert!(ran.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn run_drops_future_without_polling_after_close() {
        let gate = ConnectionRpcGate::new();
        gate.close().await;
        let polled = Arc::new(AtomicBool::new(/*v*/ false));
        let polled_clone = Arc::clone(&polled);

        gate.run(async move {
            polled_clone.store(/*val*/ true, Ordering::Release);
        })
        .await;

        assert!(!polled.load(Ordering::Acquire));
        assert!(!gate.is_accepting().await);
    }

    #[tokio::test]
    async fn close_returns_while_started_run_remains_active() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    let _ = finish_rx.await;
                })
                .await;
        });

        started_rx.await.expect("run should start");
        gate.close().await;
        assert!(!gate.is_accepting().await);
        assert_eq!(gate.inflight_count(), 1);

        finish_tx
            .send(())
            .expect("running future should be waiting");
        run_task.await.expect("run task should complete");
        gate.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_waits_for_started_run_to_finish() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    let _ = finish_rx.await;
                })
                .await;
        });

        started_rx.await.expect("run should start");
        assert_eq!(gate.inflight_count(), 1);

        let gate_for_shutdown = Arc::clone(&gate);
        let shutdown_task = tokio::spawn(async move {
            gate_for_shutdown.shutdown().await;
        });

        timeout(Duration::from_millis(/*millis*/ 50), shutdown_task)
            .await
            .expect_err("shutdown should wait for the running future");

        finish_tx
            .send(())
            .expect("running future should be waiting");
        run_task.await.expect("run task should complete");
        gate.shutdown().await;
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_drops_late_runs_while_waiting_for_inflight_work() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let (started_tx, started_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    let _ = finish_rx.await;
                })
                .await;
        });

        started_rx.await.expect("run should start");
        let gate_for_shutdown = Arc::clone(&gate);
        let shutdown_task = tokio::spawn(async move {
            gate_for_shutdown.shutdown().await;
        });

        timeout(Duration::from_millis(/*millis*/ 50), shutdown_task)
            .await
            .expect_err("shutdown should wait for the running future");

        let late_polled = Arc::new(AtomicBool::new(/*v*/ false));
        let late_polled_clone = Arc::clone(&late_polled);
        gate.run(async move {
            late_polled_clone.store(/*val*/ true, Ordering::Release);
        })
        .await;

        assert!(!late_polled.load(Ordering::Acquire));

        finish_tx
            .send(())
            .expect("running future should still be waiting");
        run_task.await.expect("run task should complete");
        gate.shutdown().await;
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn run_is_counted_before_handler_body_continues() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (continue_tx, continue_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    entered_tx.send(()).expect("receiver should be open");
                    let _ = continue_rx.await;
                })
                .await;
        });

        entered_rx.await.expect("handler body should be entered");
        assert_eq!(gate.inflight_count(), 1);

        continue_tx
            .send(())
            .expect("handler body should still be waiting");
        run_task.await.expect("run task should complete");
        assert_eq!(gate.inflight_count(), 0);
    }

    #[test]
    fn global_and_per_connection_admission_are_bounded() {
        let global = Arc::new(GlobalRpcAdmission::new(/*limit*/ 2));
        let first_gate = ConnectionRpcGate::with_global(Arc::clone(&global), /*limit*/ 1);
        let second_gate = ConnectionRpcGate::with_global(Arc::clone(&global), /*limit*/ 1);
        let third_gate = ConnectionRpcGate::with_global(global, /*limit*/ 1);

        let first_permit = first_gate.try_admit().expect("first request should fit");
        assert_eq!(
            first_gate.try_admit().expect_err("connection should be full"),
            RpcAdmissionError::Overloaded
        );
        let second_permit = second_gate
            .try_admit()
            .expect("second global request should fit");
        assert_eq!(
            third_gate.try_admit().expect_err("global gate should be full"),
            RpcAdmissionError::Overloaded
        );
        assert_eq!(first_gate.admitted_count(), 1);
        assert_eq!(second_gate.admitted_count(), 1);
        assert_eq!(third_gate.global_admitted_count(), 2);

        drop(first_permit);
        let third_permit = third_gate
            .try_admit()
            .expect("released global slot should be reusable");
        assert_eq!(third_gate.global_admitted_count(), 2);

        drop(second_permit);
        drop(third_permit);
        assert_eq!(third_gate.global_admitted_count(), 0);
    }

    #[tokio::test]
    async fn close_revokes_queued_admission_without_polling_future() {
        let gate = ConnectionRpcGate::with_limits(/*global_limit*/ 2, /*per_connection_limit*/ 2);
        let permit = gate.try_admit().expect("request should be admitted");
        let polled = Arc::new(AtomicBool::new(/*v*/ false));
        let polled_clone = Arc::clone(&polled);

        gate.close().await;
        assert!(!permit.is_active());
        assert_eq!(gate.admitted_count(), 0);
        assert_eq!(gate.global_admitted_count(), 0);

        permit
            .run(async move {
                polled_clone.store(/*val*/ true, Ordering::Release);
            })
            .await;
        assert!(!polled.load(Ordering::Acquire));
    }

    #[test]
    fn overload_response_is_stable() {
        assert_eq!(
            rpc_overloaded_error(),
            JSONRPCErrorError {
                code: OVERLOADED_ERROR_CODE,
                message: RPC_OVERLOADED_MESSAGE.to_string(),
                data: None,
            }
        );
    }
}
