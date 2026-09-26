use core::fmt;
use std::io;

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
#[cfg(not(unix))]
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
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

// Numeric group control is valid only while the owned root cannot be reaped.
#[cfg(unix)]
pub(crate) struct ProcessGroupControl {
    pid: u32,
    active: StdMutex<bool>,
}

#[cfg(unix)]
impl ProcessGroupControl {
    pub(crate) fn new(pid: u32) -> Self {
        Self {
            pid,
            active: StdMutex::new(true),
        }
    }

    pub(crate) fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*active {
            return Ok(());
        }
        match signal {
            ProcessSignal::Interrupt => crate::process_group::interrupt_process_group(self.pid),
        }
    }

    pub(crate) fn kill(&self) -> io::Result<()> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *active {
            crate::process_group::kill_process_group(self.pid)
        } else {
            Ok(())
        }
    }

    pub(crate) async fn disarm_after_exit(&self) {
        // Only the backend waiter owns/reaps the child. WNOWAIT protects its identity
        // until all in-flight signals finish and future signals have been disabled.
        let mut warned = false;
        loop {
            match crate::process_group::wait_for_exit_without_reaping(self.pid).await {
                Ok(()) => break,
                Err(error) => {
                    if !warned {
                        log::warn!("failed to observe child exit without reaping: {error}");
                        warned = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        *self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }
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
    #[cfg(not(unix))]
    Resizable(Box<dyn MasterPty + Send>),
    #[cfg(unix)]
    Owned(std::fs::File),
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

/// PTY handles shared by a session and the waiter that may release them once
/// the root process exits.
pub(crate) type SharedPtyHandles = Arc<StdMutex<Option<PtyHandles>>>;

/// Bounds how long a finished ConPTY reader may keep draining once its
/// pseudoconsole has been released. Draining normally ends within
/// milliseconds; the bound only applies when the output receiver stops reading.
#[cfg(windows)]
const PTY_OUTPUT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Releases PTY handles without blocking the async executor.
pub(crate) fn release_pty_handles(pty_handles: &StdMutex<Option<PtyHandles>>) {
    let handles = pty_handles
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    #[cfg(windows)]
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        // ClosePseudoConsole can wait for output to drain. Leave the async
        // executor free to run the reader (or complete its cancellation).
        if let Some(handles) = handles {
            runtime.spawn_blocking(move || drop(handles));
        }
        return;
    }
    drop(handles);
}

/// Lets a ConPTY reader deliver the final frame that the pseudoconsole flushes
/// when it is released; releasing it always closes the reader's pipe. The
/// reader is aborted only if its receiver stops reading.
#[cfg(windows)]
fn drain_released_pty_reader(reader: JoinHandle<()>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        reader.abort();
        return;
    };
    let abort = reader.abort_handle();
    runtime.spawn(async move {
        if tokio::time::timeout(PTY_OUTPUT_DRAIN_TIMEOUT, reader)
            .await
            .is_err()
        {
            abort.abort();
        }
    });
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
    // slave is closed. A Windows PTY waiter releases them once the root exits.
    pty_handles: SharedPtyHandles,
    // Whether this session was created with local PTY handles, which remain
    // meaningful for resize and reader draining after the handles are released.
    has_pty: bool,
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
        pty_handles: SharedPtyHandles,
        resizer: Option<ResizeFn>,
    ) -> Self {
        let has_pty = pty_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            reader_abort_handles: StdMutex::new(reader_abort_handles),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            pty_handles,
            has_pty,
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
                .pty_handles
                .lock()
                .map_err(|_| anyhow!("failed to lock PTY handles"))?;
            if let Some(handles) = handles.as_ref() {
                return match &handles._master {
                    #[cfg(not(unix))]
                    PtyMasterHandle::Resizable(master) => master.resize(size.into()),
                    #[cfg(unix)]
                    PtyMasterHandle::Owned(master) => {
                        use std::os::fd::AsRawFd;
                        let size = libc::winsize {
                            ws_row: size.rows,
                            ws_col: size.cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        // SAFETY: master owns the descriptor and size is live winsize storage.
                        if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) }
                            == -1
                        {
                            Err(io::Error::last_os_error().into())
                        } else {
                            Ok(())
                        }
                    }
                };
            }
        }
        if self.has_pty {
            // The terminal was released after exit or termination; nothing is left to resize.
            return Ok(());
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

    /// Drop this handle's stdin sender. EOF follows queued input only after all
    /// sender clones returned by `writer_sender` have also been dropped.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Closes stdin, drops the terminator, and releases the pseudoconsole after
    /// the root process exits.
    ///
    /// ConPTY keeps its output pipe open until the pseudoconsole handles are
    /// closed. The Windows PTY waiter already releases them once it observes
    /// the root exit; releasing them here as well is harmless and lets the
    /// reader drain buffered output and then observe EOF without aborting it.
    pub fn release_pty_after_exit(&self) {
        self.close_stdin();
        if let Ok(mut killer_opt) = self.killer.lock() {
            killer_opt.take();
        }
        release_pty_handles(&self.pty_handles);
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
    /// would retain the otherwise-finished process indefinitely. A Windows
    /// pseudoconsole is different: releasing it closes the reader's pipe after
    /// its final frame, so that reader drains to EOF (bounded) instead of
    /// discarding output the child wrote before it exited.
    pub fn finish(&self) {
        self.close_stdin();

        if let Ok(mut killer_opt) = self.killer.lock() {
            killer_opt.take();
        }

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            #[cfg(windows)]
            if self.has_pty {
                drain_released_pty_reader(handle);
            } else {
                handle.abort();
            }
            #[cfg(not(windows))]
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
        if let Ok(mut h) = self.wait_handle.lock() {
            // The waiter must still observe termination and publish the exit
            // status. Dropping its handle detaches it instead of cancelling it.
            h.take();
        }
        release_pty_handles(&self.pty_handles);
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
            SharedPtyHandles::default(),
            None,
        );

        handle.finish();

        assert!(dropped.load(Ordering::SeqCst));
        assert!(!killed.load(Ordering::SeqCst));
        assert!(handle.writer_sender().is_closed());
    }

    #[cfg(windows)]
    #[tokio::test(start_paused = true)]
    async fn released_pty_reader_is_aborted_only_after_the_drain_bound() {
        let (dropped_tx, mut dropped_rx) = oneshot::channel::<()>();
        // A reader whose receiver stopped reading never reaches EOF on its own.
        let reader = tokio::spawn(async move {
            let _dropped_tx = dropped_tx;
            std::future::pending::<()>().await;
        });

        drain_released_pty_reader(reader);

        assert!(
            tokio::time::timeout(
                PTY_OUTPUT_DRAIN_TIMEOUT - Duration::from_millis(1),
                &mut dropped_rx
            )
            .await
            .is_err(),
            "a draining reader must not be aborted before its bound"
        );
        let aborted = tokio::time::timeout(Duration::from_secs(60), dropped_rx).await;
        assert!(
            matches!(aborted, Ok(Err(_))),
            "a reader still running at the bound must be aborted"
        );
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
            SharedPtyHandles::default(),
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
    async fn collect_split_output(mut output_rx: mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        let mut collected = Vec::new();
        while let Some(chunk) = output_rx.recv().await {
            collected.extend_from_slice(&chunk);
        }
        collected
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_backed_process_can_expose_split_stdout_and_stderr() -> anyhow::Result<()> {
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let (stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
        let (stderr_tx, stderr_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

        let spawned = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_driver_rx.into(),
            stderr_rx: Some(stderr_driver_rx.into()),
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });

        let SpawnedProcess {
            session: _session,
            stdout_rx,
            stderr_rx,
            exit_rx,
        } = spawned;
        let stdout_task = tokio::spawn(async move { collect_split_output(stdout_rx).await });
        let stderr_task = tokio::spawn(async move { collect_split_output(stderr_rx).await });

        stdout_tx.send(b"driver-out".to_vec())?;
        stderr_tx.send(b"driver-err".to_vec())?;
        drop(stdout_tx);
        drop(stderr_tx);
        exit_tx.send(0).expect("send exit code");

        let timeout = tokio::time::Duration::from_secs(2);
        let code = tokio::time::timeout(timeout, exit_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for driver exit"))?
            .unwrap_or(-1);
        let stdout = tokio::time::timeout(timeout, stdout_task)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stdout"))??;
        let stderr = tokio::time::timeout(timeout, stderr_task)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stderr"))??;

        assert_eq!(stdout, b"driver-out".to_vec());
        assert_eq!(stderr, b"driver-err".to_vec());
        assert_eq!(code, 0);

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_backed_process_can_resize_via_resizer_hook() -> anyhow::Result<()> {
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let (_stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();
        let (size_tx, size_rx) = tokio::sync::oneshot::channel::<TerminalSize>();

        let size_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(size_tx)));
        let spawned = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_driver_rx.into(),
            stderr_rx: None,
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: Some(Box::new(move |size| {
                if let Ok(mut guard) = size_tx.lock()
                    && let Some(size_tx) = guard.take()
                {
                    let _ = size_tx.send(size);
                }
                Ok(())
            })),
        });

        spawned.session.resize(TerminalSize {
            rows: 40,
            cols: 120,
        })?;
        exit_tx.send(0).expect("send exit code");

        let resized = tokio::time::timeout(tokio::time::Duration::from_secs(2), size_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for resize"))?
            .expect("receive resized terminal size");
        assert_eq!(
            resized,
            TerminalSize {
                rows: 40,
                cols: 120
            }
        );

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_backed_process_drains_output_that_arrives_after_exit_signal()
    -> anyhow::Result<()> {
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let (stdout_tx, stdout_driver_rx) = tokio::sync::broadcast::channel::<Vec<u8>>(8);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<i32>();

        let spawned = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_driver_rx.into(),
            stderr_rx: None,
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });

        let SpawnedProcess {
            session: _session,
            stdout_rx,
            stderr_rx: _stderr_rx,
            exit_rx,
        } = spawned;
        let stdout_task = tokio::spawn(async move { collect_split_output(stdout_rx).await });

        exit_tx.send(0).expect("send exit code");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        stdout_tx.send(b"tail".to_vec())?;
        drop(stdout_tx);

        let timeout = tokio::time::Duration::from_secs(2);
        let code = tokio::time::timeout(timeout, exit_rx)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for driver exit"))?
            .unwrap_or(-1);
        let stdout = tokio::time::timeout(timeout, stdout_task)
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting to drain driver stdout"))??;

        assert_eq!(stdout, b"tail".to_vec());
        assert_eq!(code, 0);

        Ok(())
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

/// Combine split stdout/stderr receivers into a lossy broadcast convenience receiver.
/// Slow consumers may lag and lose chunks; retain split receivers for exact byte delivery.
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
    let spawn_stream_reader = |mut output_rx: ProcessOutputReceiver,
                               output_tx: mpsc::Sender<Vec<u8>>| {
        tokio::spawn(async move {
            // Drivers must close their output senders after forwarding the final bytes,
            // which can arrive after their exit notification.
            loop {
                let recv_result = output_rx.recv().await;
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
    let reader_handle = spawn_stream_reader(stdout_driver_rx, stdout_tx);
    let stderr_reader_handle = stderr_driver_rx
        .take()
        .map(|rx| spawn_stream_reader(rx, stderr_tx));

    let writer_handle = writer_handle.unwrap_or_else(|| tokio::spawn(async {}));

    let (exit_tx, exit_rx_out) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle = tokio::spawn(async move {
        let code = exit_rx.await.unwrap_or(-1);
        publish_exit_status(&wait_exit_status, &wait_exit_code, code);
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
        SharedPtyHandles::default(),
        resizer,
    );

    SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx: exit_rx_out,
    }
}
