use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use tokio::task::AbortHandle;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const CONNECTION_RPC_SHUTDOWN_GRACE: Duration = Duration::from_secs(/*secs*/ 30);

#[derive(Debug)]
struct ConnectionRpcGateState {
    accepting: bool,
    next_task_id: u64,
    abort_handles: HashMap<u64, AbortHandle>,
}

impl Default for ConnectionRpcGateState {
    fn default() -> Self {
        Self {
            accepting: true,
            next_task_id: 0,
            abort_handles: HashMap::new(),
        }
    }
}

struct TaskRegistration {
    state: Weak<Mutex<ConnectionRpcGateState>>,
    task_id: u64,
}

impl Drop for TaskRegistration {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .abort_handles
            .remove(&self.task_id);
    }
}

/// Per-connection gate that owns initialized RPC handler tasks.
///
/// Closing the gate is a terminal transition: it prevents queued handlers from
/// starting, publishes cooperative cancellation, and rejects fenced resource
/// registrations. Shutdown gives active handlers a bounded grace period before
/// aborting and joining every remaining gate-owned task.
#[derive(Debug)]
pub(crate) struct ConnectionRpcGate {
    state: Arc<Mutex<ConnectionRpcGateState>>,
    cancellation: CancellationToken,
    tasks: TaskTracker,
}

impl ConnectionRpcGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionRpcGateState::default())),
            cancellation: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    pub(crate) async fn run<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let join_handle = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !state.accepting {
                return;
            }

            let task_id = state.next_task_id;
            state.next_task_id = state.next_task_id.wrapping_add(1);
            let registration = TaskRegistration {
                state: Arc::downgrade(&self.state),
                task_id,
            };
            let (start_tx, start_rx) = tokio::sync::oneshot::channel();
            let join_handle = self.tasks.spawn(async move {
                let _registration = registration;
                if start_rx.await.is_ok() {
                    future.await;
                }
            });
            state
                .abort_handles
                .insert(task_id, join_handle.abort_handle());
            let _ = start_tx.send(());
            join_handle
        };

        let _ = join_handle.await;
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Runs a synchronous registration commit while holding the same terminal
    /// fence used by [`Self::close`]. If close wins the fence, the commit is not
    /// called; if the commit wins, connection cleanup necessarily observes it.
    pub(crate) fn try_commit<R>(&self, commit: impl FnOnce() -> R) -> Option<R> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.accepting.then(commit)
    }

    pub(crate) async fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.accepting {
            return;
        }
        state.accepting = false;
        self.tasks.close();
        self.cancellation.cancel();
    }

    pub(crate) async fn shutdown(&self) -> usize {
        self.shutdown_with_grace(CONNECTION_RPC_SHUTDOWN_GRACE)
            .await
    }

    async fn shutdown_with_grace(&self, grace: Duration) -> usize {
        self.close().await;
        if timeout(grace, self.tasks.wait()).await.is_ok() {
            return 0;
        }

        let abort_handles = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .abort_handles
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let aborted_tasks = abort_handles.len();
        for abort_handle in abort_handles {
            abort_handle.abort();
        }
        self.tasks.wait().await;
        aborted_tasks
    }

    #[cfg(test)]
    fn is_accepting(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .accepting
    }

    #[cfg(test)]
    fn inflight_count(&self) -> usize {
        self.tasks.len()
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
    use std::future::pending;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::oneshot;
    use tokio::time::Duration;

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
        assert!(!gate.is_accepting());
    }

    #[tokio::test]
    async fn cooperative_handler_observes_connection_cancellation() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let cancellation = gate.cancellation_token();
        let cancellation_observed = Arc::new(AtomicBool::new(/*v*/ false));
        let cancellation_observed_for_task = Arc::clone(&cancellation_observed);
        let (started_tx, started_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    cancellation.cancelled().await;
                    cancellation_observed_for_task.store(/*val*/ true, Ordering::Release);
                })
                .await;
        });

        started_rx.await.expect("run should start");
        let aborted_tasks = gate.shutdown_with_grace(Duration::from_secs(1)).await;
        run_task.await.expect("run task should complete");

        assert!(cancellation_observed.load(Ordering::Acquire));
        assert_eq!(aborted_tasks, 0);
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_aborts_and_joins_noncooperative_handler_after_grace() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let (started_tx, started_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    pending::<()>().await;
                })
                .await;
        });

        started_rx.await.expect("run should start");
        let aborted_tasks = gate.shutdown_with_grace(Duration::from_millis(10)).await;
        run_task
            .await
            .expect("run task should complete after abort");

        assert_eq!(aborted_tasks, 1);
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn shutdown_drops_late_runs_while_waiting_for_inflight_work() {
        let gate = Arc::new(ConnectionRpcGate::new());
        let cancellation = gate.cancellation_token();
        let (started_tx, started_rx) = oneshot::channel();
        let gate_for_run = Arc::clone(&gate);
        let run_task = tokio::spawn(async move {
            gate_for_run
                .run(async move {
                    started_tx.send(()).expect("receiver should be open");
                    cancellation.cancelled().await;
                })
                .await;
        });

        started_rx.await.expect("run should start");
        assert_eq!(gate.shutdown_with_grace(Duration::from_secs(1)).await, 0);
        run_task.await.expect("run task should complete");

        let late_polled = Arc::new(AtomicBool::new(/*v*/ false));
        let late_polled_clone = Arc::clone(&late_polled);
        gate.run(async move {
            late_polled_clone.store(/*val*/ true, Ordering::Release);
        })
        .await;

        assert!(!late_polled.load(Ordering::Acquire));
        assert_eq!(gate.inflight_count(), 0);
    }

    #[tokio::test]
    async fn terminal_fence_rejects_post_close_registration() {
        let gate = ConnectionRpcGate::new();
        let registrations = Arc::new(Mutex::new(Vec::new()));
        let registrations_for_commit = Arc::clone(&registrations);
        assert!(
            gate.try_commit(|| {
                registrations_for_commit
                    .lock()
                    .expect("registrations lock")
                    .push("open")
            })
            .is_some()
        );

        gate.close().await;
        let registrations_for_late_commit = Arc::clone(&registrations);
        assert!(
            gate.try_commit(|| {
                registrations_for_late_commit
                    .lock()
                    .expect("registrations lock")
                    .push("closed")
            })
            .is_none()
        );

        assert_eq!(
            *registrations.lock().expect("registrations lock"),
            vec!["open"]
        );
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
}
