use super::*;

use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::WriteResponse;

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio::time::timeout;

struct TerminationTrackingProcess {
    process_id: ProcessId,
    terminated: AtomicBool,
}

impl ExecProcess for TerminationTrackingProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        let (_wake_tx, wake_rx) = watch::channel(0);
        wake_rx
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(async { unreachable!("read is not used by this test") })
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { unreachable!("write is not used by this test") })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { unreachable!("signal is not used by this test") })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        self.terminated.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn remote_user_shell_read_failure_terminates_process() {
    let process = Arc::new(TerminationTrackingProcess {
        process_id: ProcessId::new("test-process"),
        terminated: AtomicBool::new(false),
    });
    let exec_process: Arc<dyn ExecProcess> = process.clone();
    let cleanup_tasks = tokio_util::task::TaskTracker::new();

    let result = terminate_remote_process_after_error(
        &cleanup_tasks,
        exec_process,
        Err(UserShellExecError::Failed("read failed".to_string())),
    )
    .await;

    assert!(matches!(result, Err(UserShellExecError::Failed(_))));
    assert!(process.terminated.load(Ordering::SeqCst));
}

struct RetryingTerminationProcess {
    process_id: ProcessId,
    termination_attempts: AtomicUsize,
    allow_confirmation: Arc<Notify>,
}

impl ExecProcess for RetryingTerminationProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        let (_wake_tx, wake_rx) = watch::channel(0);
        wake_rx
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(async { unreachable!("read is not used by this test") })
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { unreachable!("write is not used by this test") })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { unreachable!("signal is not used by this test") })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        let attempt = self.termination_attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                return Err(ExecServerError::Protocol(
                    "injected termination failure".to_string(),
                ));
            }
            self.allow_confirmation.notified().await;
            Ok(())
        })
    }
}

#[tokio::test]
async fn remote_user_shell_termination_failure_is_not_reported_terminal_without_cleanup_owner() {
    let allow_confirmation = Arc::new(Notify::new());
    let process = Arc::new(RetryingTerminationProcess {
        process_id: ProcessId::new("retrying-process"),
        termination_attempts: AtomicUsize::new(0),
        allow_confirmation: Arc::clone(&allow_confirmation),
    });
    let weak_process = Arc::downgrade(&process);
    let exec_process: Arc<dyn ExecProcess> = process.clone();
    let cleanup_tasks = tokio_util::task::TaskTracker::new();

    let result = terminate_remote_process_after_error(
        &cleanup_tasks,
        exec_process,
        Err(UserShellExecError::Cancelled),
    )
    .await;

    assert!(matches!(result, Err(UserShellExecError::Cancelled)));
    timeout(Duration::from_secs(1), async {
        while process.termination_attempts.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("session-owned cleanup should retry termination");
    drop(process);
    assert!(
        weak_process.upgrade().is_some(),
        "cleanup task must retain the process after terminal reporting"
    );

    allow_confirmation.notify_one();
    cleanup_tasks.close();
    timeout(Duration::from_secs(1), cleanup_tasks.wait())
        .await
        .expect("cleanup owner should observe confirmed termination");
    assert!(weak_process.upgrade().is_none());
}
