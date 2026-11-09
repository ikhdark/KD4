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
const CONNECTION_RPC_ABORT_JOIN_GRACE: Duration = Duration::from_secs(/*secs*/ 1);

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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_handles
            .remove(&self.task_id);
    }
}

/// Per-connection gate that owns initialized RPC handler tasks.
///
/// Closing the gate is a terminal transition: it prevents queued handlers from
/// starting, publishes cooperative cancellation, and rejects fenced resource
/// registrations. Shutdown gives active handlers a bounded grace period before
/// aborting ordinary tasks. Admitted effectful handlers retain their settlement
/// owner through actual completion, including beyond that observation grace.
#[derive(Debug)]
pub struct ConnectionRpcGate {
    state: Arc<Mutex<ConnectionRpcGateState>>,
    cancellation: CancellationToken,
    tasks: TaskTracker,
    settlements: TaskTracker,
}

impl ConnectionRpcGate {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionRpcGateState::default())),
            cancellation: CancellationToken::new(),
            tasks: TaskTracker::new(),
            settlements: TaskTracker::new(),
        }
    }

    pub(crate) async fn run<F>(&self, future: F, retain_settlement: bool) -> Result<bool, tokio::task::JoinError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let Some(join_handle) = self
            .spawn_with_commit_policy(|| Ok::<_, std::convert::Infallible>(future), retain_settlement)
            .unwrap_or_else(|never| match never {})
        else {
            return Ok(false);
        };
        join_handle.await?;
        Ok(true)
    }

    /// Commit ordered admission and transfer its completion to the connection
    /// owner atomically with close. A closed gate never invokes the commit.
    pub fn spawn_with_commit<F, E>(
        &self,
        commit: impl FnOnce() -> Result<F, E>,
    ) -> Result<Option<tokio::task::JoinHandle<()>>, E>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_with_commit_policy(commit, false)
    }

    fn spawn_with_commit_policy<F, E>(
        &self,
        commit: impl FnOnce() -> Result<F, E>,
        retain_settlement: bool,
    ) -> Result<Option<tokio::task::JoinHandle<()>>, E>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Ok(None);
        }
        let future = commit()?;
        let task_id = state.next_task_id;
        state.next_task_id = state.next_task_id.wrapping_add(1);
        let registration = TaskRegistration {
            state: Arc::downgrade(&self.state),
            task_id,
        };
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let tracker = if retain_settlement { &self.settlements } else { &self.tasks };
        let join_handle = tracker.spawn(async move {
            let _registration = registration;
            if start_rx.await.is_ok() {
                future.await;
            }
        });
        if !retain_settlement {
            state.abort_handles.insert(task_id, join_handle.abort_handle());
        }
        let _ = start_tx.send(());
        Ok(Some(join_handle))
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Runs a synchronous registration commit while holding the same terminal
    /// fence used by [`Self::close`]. If close wins the fence, the commit is not
    /// called; if the commit wins, connection cleanup necessarily observes it.
    pub fn try_commit<R>(&self, commit: impl FnOnce() -> R) -> Option<R> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting.then(commit)
    }

    pub async fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return;
        }
        state.accepting = false;
        self.tasks.close();
        self.settlements.close();
        self.cancellation.cancel();
    }

    pub async fn shutdown(&self) -> usize {
        self.shutdown_with_grace(CONNECTION_RPC_SHUTDOWN_GRACE)
            .await
    }

    async fn shutdown_with_grace(&self, grace: Duration) -> usize {
        self.close().await;
        if timeout(grace, self.tasks.wait()).await.is_ok() {
            self.settlements.wait().await;
            return 0;
        }

        let abort_handles = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_handles
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let aborted_tasks = abort_handles.len();
        for abort_handle in abort_handles {
            abort_handle.abort();
        }
        if timeout(CONNECTION_RPC_ABORT_JOIN_GRACE, self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                remaining_tasks = self.tasks.len(),
                aborted_tasks,
                "connection RPC tasks did not stop within the abort join grace period"
            );
        }
        self.settlements.wait().await;
        aborted_tasks
    }


}

impl Default for ConnectionRpcGate {
    fn default() -> Self {
        Self::new()
    }
}

