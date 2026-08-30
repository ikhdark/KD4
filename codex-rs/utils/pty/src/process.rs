use core::fmt;
use std::io;

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSignal {
    Interrupt,
}

pub(crate) fn unsupported_signal(signal: ProcessSignal) -> io::Error {
    match signal {
        ProcessSignal::Interrupt => io::Error::new(
            io::ErrorKind::Unsupported,
            "process interrupt is not supported by this process backend",
        ),
    }
}

pub(crate) fn exit_code_from_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    -1
}

pub(crate) fn publish_exit_status(
    exit_status: &AtomicBool,
    exit_code: &StdMutex<Option<i32>>,
    code: i32,
) {
    *exit_code
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(code);
    exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) trait ChildTerminator: Send + Sync {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()>;

    fn kill(&mut self) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

pub(crate) enum PtyMasterHandle {
    Resizable(Box<dyn MasterPty + Send>),
}

pub struct PtyHandles {
    pub _slave: Option<Box<dyn SlavePty + Send>>,
    pub(crate) _master: PtyMasterHandle,
}

impl fmt::Debug for PtyHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyHandles").finish()
    }
}

/// Callback used by driver-backed sessions to resize a PTY-like backend when
/// there is no local `PtyHandles` instance to resize directly.
type ResizeFn = Box<dyn FnMut(TerminalSize) -> anyhow::Result<()> + Send>;

/// Handle for driving an interactive process (PTY or pipe).
pub struct ProcessHandle {
    writer_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    killer: StdMutex<Option<Box<dyn ChildTerminator>>>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    reader_abort_handles: StdMutex<Vec<AbortHandle>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    wait_handle: StdMutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    // PtyHandles must be preserved because the process will receive Control+C if the
    // slave is closed
    _pty_handles: StdMutex<Option<PtyHandles>>,
    // Optional resize hook for driver-backed sessions that proxy PTY control to
    // another backend instead of owning local PTY handles.
    resizer: StdMutex<Option<ResizeFn>>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle").finish()
    }
}

impl ProcessHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        killer: Box<dyn ChildTerminator>,
        reader_handle: JoinHandle<()>,
        reader_abort_handles: Vec<AbortHandle>,
        writer_handle: JoinHandle<()>,
        wait_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
        pty_handles: Option<PtyHandles>,
        resizer: Option<ResizeFn>,
    ) -> Self {
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            reader_abort_handles: StdMutex::new(reader_abort_handles),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            _pty_handles: StdMutex::new(pty_handles),
            resizer: StdMutex::new(resizer),
        }
    }

    /// Returns a channel sender for writing raw bytes to the child stdin.
    pub fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        if let Ok(writer_tx) = self.writer_tx.lock()
            && let Some(writer_tx) = writer_tx.as_ref()
        {
            return writer_tx.clone();
        }

        let (writer_tx, writer_rx) = mpsc::channel(1);
        drop(writer_rx);
        writer_tx
    }

    /// True if the child process has exited.
    pub fn has_exited(&self) -> bool {
        self.exit_status.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns the exit code if known.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }

    /// Resize the PTY in character cells.
    pub fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        {
            let handles = self
                ._pty_handles
                .lock()
                .map_err(|_| anyhow!("failed to lock PTY handles"))?;
            if let Some(handles) = handles.as_ref() {
                return match &handles._master {
                    PtyMasterHandle::Resizable(master) => master.resize(size.into()),
                };
            }
        }

        let mut resizer = self
            .resizer
            .lock()
            .map_err(|_| anyhow!("failed to lock PTY resizer"))?;
        if let Some(resizer) = resizer.as_mut() {
            resizer(size)
        } else {
            Err(anyhow!("process is not attached to a PTY"))
        }
    }

    /// Close the child's stdin channel.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Releases the Windows pseudoconsole after its root process exits.
    ///
    /// ConPTY keeps its output pipe open until the pseudoconsole handles are
    /// closed. Releasing them here lets the reader drain buffered output and
    /// then observe EOF without aborting the reader task.
    pub fn release_pty_after_exit(&self) {
        self.close_stdin();
        if let Ok(mut killer_opt) = self.killer.lock() {
            killer_opt.take();
        }
        if let Ok(mut handles) = self._pty_handles.lock() {
            handles.take();
        }
    }

    /// Attempts to kill the child while leaving the reader/writer tasks alive
    /// so callers can still drain output until EOF.
    pub fn request_terminate(&self) -> io::Result<()> {
        let mut killer_opt = self
            .killer
            .lock()
            .map_err(|_| io::Error::other("failed to lock process terminator"))?;
        let Some(killer) = killer_opt.as_mut() else {
            return Ok(());
        };

        killer.kill()?;
        killer_opt.take();
        Ok(())
    }

    pub fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let Ok(mut killer_opt) = self.killer.lock() else {
            return Ok(());
        };
        let Some(killer) = killer_opt.as_mut() else {
            return Ok(());
        };

        killer.signal(signal)
    }

    /// Attempts to kill the child and abort helper tasks.
    pub fn terminate(&self) -> io::Result<()> {
        self.request_terminate()?;
        self.finish();
        Ok(())
    }

    /// Releases a finished child without signalling it and aborts helper tasks.
    ///
    /// This is used after the root process has already exited. In particular,
    /// pipe descendants may intentionally outlive the root while retaining an
    /// inherited output handle, so waiting for the reader tasks to observe EOF
    /// would retain the otherwise-finished process indefinitely.
    pub fn finish(&self) {
        self.close_stdin();

        if let Ok(mut killer_opt) = self.killer.lock() {
            killer_opt.take();
        }

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut handles) = self.reader_abort_handles.lock() {
            for handle in handles.drain(..) {
                handle.abort();
            }
        }
        if let Ok(mut h) = self.writer_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut h) = self.wait_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut handles) = self._pty_handles.lock() {
            handles.take();
        }
        if let Ok(mut resizer) = self.resizer.lock() {
            resizer.take();
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            log::warn!("failed to terminate process while dropping its handle: {error}");
            self.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct TestTerminator {
        dropped: Arc<AtomicBool>,
        killed: Arc<AtomicBool>,
    }

    struct RetryTerminator {
        attempts: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ChildTerminator for RetryTerminator {
        fn signal(&mut self, _signal: ProcessSignal) -> io::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> io::Result<()> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                Err(io::Error::other("injected termination failure"))
            } else {
                Ok(())
            }
        }
    }

    impl ChildTerminator for TestTerminator {
        fn signal(&mut self, _signal: ProcessSignal) -> io::Result<()> {
            Ok(())
        }

        fn kill(&mut self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Drop for TestTerminator {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn exit_status_is_not_visible_before_exit_code() {
        let exit_status = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(StdMutex::new(None));
        let exit_code_guard = exit_code.lock().expect("lock exit code");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let publisher_status = Arc::clone(&exit_status);
        let publisher_code = Arc::clone(&exit_code);
        let publisher = std::thread::spawn(move || {
            started_tx.send(()).expect("announce publisher start");
            publish_exit_status(&publisher_status, &publisher_code, 17);
        });

        started_rx.recv().expect("publisher should start");
        std::thread::sleep(Duration::from_millis(25));
        assert!(
            !exit_status.load(Ordering::SeqCst),
            "exit status must remain unpublished while the exit code is locked"
        );

        drop(exit_code_guard);
        publisher.join().expect("publisher should finish");
        assert!(exit_status.load(Ordering::SeqCst));
        assert_eq!(
            *exit_code.lock().expect("read published exit code"),
            Some(17)
        );
    }

    #[tokio::test]
    async fn finish_releases_terminator_without_killing_child() {
        let dropped = Arc::new(AtomicBool::new(false));
        let killed = Arc::new(AtomicBool::new(false));
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let idle_task = || tokio::spawn(std::future::pending::<()>());
        let handle = ProcessHandle::new(
            writer_tx,
            Box::new(TestTerminator {
                dropped: Arc::clone(&dropped),
                killed: Arc::clone(&killed),
            }),
            idle_task(),
            Vec::new(),
            idle_task(),
            idle_task(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(StdMutex::new(Some(0))),
            None,
            None,
        );

        handle.finish();

        assert!(dropped.load(Ordering::SeqCst));
        assert!(!killed.load(Ordering::SeqCst));
        assert!(handle.writer_sender().is_closed());
    }

    #[tokio::test]
    async fn failed_termination_keeps_terminator_for_retry() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let idle_task = || tokio::spawn(std::future::pending::<()>());
        let handle = ProcessHandle::new(
            writer_tx,
            Box::new(RetryTerminator {
                attempts: Arc::clone(&attempts),
            }),
            idle_task(),
            Vec::new(),
            idle_task(),
            idle_task(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(StdMutex::new(None)),
            None,
            None,
        );

        let error = handle
            .terminate()
            .expect_err("first termination attempt should fail");
        assert_eq!(error.to_string(), "injected termination failure");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(!handle.writer_sender().is_closed());

        handle
            .terminate()
            .expect("retained terminator should succeed on retry");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(handle.writer_sender().is_closed());
    }

    #[tokio::test]
    async fn lossless_driver_output_survives_backpressure() {
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (driver_tx, driver_rx) = mpsc::channel(1);
        let (exit_tx, exit_rx) = oneshot::channel();
        let SpawnedProcess {
            session,
            mut stdout_rx,
            ..
        } = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: driver_rx.into(),
            stderr_rx: None,
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        let expected = (0_u32..512).map(u32::to_le_bytes).collect::<Vec<[u8; 4]>>();
        let producer_expected = expected.clone();
        let producer = tokio::spawn(async move {
            for chunk in producer_expected {
                driver_tx.send(chunk.to_vec()).await.expect("send output");
            }
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        let received = tokio::time::timeout(Duration::from_secs(5), async {
            let mut received = Vec::new();
            while let Some(chunk) = stdout_rx.recv().await {
                received.push(chunk);
            }
            received
        })
        .await
        .expect("lossless output should drain");
        producer.await.expect("output producer should finish");
        exit_tx.send(0).expect("send process exit");

        assert_eq!(
            received,
            expected
                .into_iter()
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>()
        );
        session.finish();
    }

    #[test]
    fn opaque_pty_handle_uses_send_directly() {
        let removed_trait = ["trait PtyHandle", "KeepAlive"].concat();
        assert!(!include_str!("process.rs").contains(&removed_trait));
    }
}

/// Adapts a closure into a `ChildTerminator` implementation.
struct ClosureTerminator {
    inner: Option<Box<dyn FnMut() -> io::Result<()> + Send + Sync>>,
}

impl ChildTerminator for ClosureTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()> {
        Err(unsupported_signal(signal))
    }

    fn kill(&mut self) -> io::Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            (inner)()?;
        }
        Ok(())
    }
}

/// Combine split stdout/stderr receivers into a single broadcast receiver.
pub fn combine_output_receivers(
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    mut stderr_rx: mpsc::Receiver<Vec<u8>>,
) -> broadcast::Receiver<Vec<u8>> {
    let (combined_tx, combined_rx) = broadcast::channel(256);
    tokio::spawn(async move {
        let mut stdout_open = true;
        let mut stderr_open = true;

        loop {
            tokio::select! {
                stdout = stdout_rx.recv(), if stdout_open => match stdout {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stdout_open = false;
                    }
                },
                stderr = stderr_rx.recv(), if stderr_open => match stderr {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stderr_open = false;
                    }
                },
                else => break,
            }
        }
    });
    combined_rx
}

/// Return value from PTY or pipe spawn helpers.
#[derive(Debug)]
pub struct SpawnedProcess {
    pub session: ProcessHandle,
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

/// Driver-backed process handles for non-standard spawn backends.
pub struct ProcessDriver {
    pub writer_tx: mpsc::Sender<Vec<u8>>,
    pub stdout_rx: ProcessOutputReceiver,
    pub stderr_rx: Option<ProcessOutputReceiver>,
    pub exit_rx: oneshot::Receiver<i32>,
    pub terminator: Option<Box<dyn FnMut() -> io::Result<()> + Send + Sync>>,
    pub writer_handle: Option<JoinHandle<()>>,
    pub resizer: Option<ResizeFn>,
}

/// Output receiver supplied by a driver-backed process. Backends that require
/// exact byte delivery should use the bounded `Lossless` variant.
pub enum ProcessOutputReceiver {
    Broadcast(broadcast::Receiver<Vec<u8>>),
    Lossless(mpsc::Receiver<Vec<u8>>),
}

impl From<broadcast::Receiver<Vec<u8>>> for ProcessOutputReceiver {
    fn from(receiver: broadcast::Receiver<Vec<u8>>) -> Self {
        Self::Broadcast(receiver)
    }
}

impl From<mpsc::Receiver<Vec<u8>>> for ProcessOutputReceiver {
    fn from(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
        Self::Lossless(receiver)
    }
}

enum ProcessOutputRecvError {
    Lagged,
    Closed,
}

impl ProcessOutputReceiver {
    async fn recv(&mut self) -> Result<Vec<u8>, ProcessOutputRecvError> {
        match self {
            Self::Broadcast(receiver) => receiver.recv().await.map_err(|error| match error {
                broadcast::error::RecvError::Lagged(_) => ProcessOutputRecvError::Lagged,
                broadcast::error::RecvError::Closed => ProcessOutputRecvError::Closed,
            }),
            Self::Lossless(receiver) => receiver.recv().await.ok_or(ProcessOutputRecvError::Closed),
        }
    }
}

/// Build a `SpawnedProcess` from a driver that supplies stdin/output/exit channels.
pub fn spawn_from_driver(driver: ProcessDriver) -> SpawnedProcess {
    let ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: mut stderr_driver_rx,
        exit_rx,
        terminator,
        writer_handle,
        resizer,
    } = driver;

    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(256);
    let (exit_seen_tx, exit_seen_rx) = watch::channel(false);
    let spawn_stream_reader =
        |mut output_rx: ProcessOutputReceiver,
         output_tx: mpsc::Sender<Vec<u8>>,
         mut exit_seen_rx: watch::Receiver<bool>| {
            tokio::spawn(async move {
                loop {
                    let recv_result = if *exit_seen_rx.borrow() {
                        // Once exit has been observed, we no longer want a timer here. Some
                        // backends publish the exit code before their final stdout/stderr bytes
                        // have been forwarded through the broadcast channel, so a fixed grace
                        // period can still drop the tail of the stream under load.
                        //
                        // Instead, keep waiting until the driver closes the broadcast sender.
                        // That makes the shutdown contract explicit: the backend is responsible
                        // for dropping its sender when it has truly finished forwarding output.
                        output_rx.recv().await
                    } else {
                        tokio::select! {
                            _ = exit_seen_rx.changed() => {
                                continue;
                            }
                            result = output_rx.recv() => result,
                        }
                    };
                    match recv_result {
                        Ok(chunk) => {
                            if output_tx.send(chunk).await.is_err() {
                                break;
                            }
                        }
                        Err(ProcessOutputRecvError::Lagged) => continue,
                        Err(ProcessOutputRecvError::Closed) => break,
                    }
                }
            })
        };
    let reader_handle = spawn_stream_reader(stdout_driver_rx, stdout_tx, exit_seen_rx.clone());
    let stderr_reader_handle = stderr_driver_rx
        .take()
        .map(|rx| spawn_stream_reader(rx, stderr_tx, exit_seen_rx));

    let writer_handle = writer_handle.unwrap_or_else(|| tokio::spawn(async {}));

    let (exit_tx, exit_rx_out) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle = tokio::spawn(async move {
        let code = exit_rx.await.unwrap_or(-1);
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
        let _ = exit_seen_tx.send(true);
        let _ = exit_tx.send(code);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(ClosureTerminator { inner: terminator }),
        reader_handle,
        stderr_reader_handle
            .map(|handle| handle.abort_handle())
            .into_iter()
            .collect(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        /*pty_handles*/ None,
        resizer,
    );

    SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx: exit_rx_out,
    }
}
