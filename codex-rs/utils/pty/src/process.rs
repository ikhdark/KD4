use core::fmt;
use std::io;

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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

    pub(crate) fn signal_terminal(
        &self,
        master: &std::fs::File,
        signal: ProcessSignal,
    ) -> io::Result<()> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*active {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let signo = match signal {
                ProcessSignal::Interrupt => libc::SIGINT,
            };
            // SAFETY: master owns the PTY; TIOCSIG takes an integer signal and
            // resolves the foreground group under the kernel's terminal lock.
            if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSIG as _, signo) } == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = master;
            Err(unsupported_signal(signal))
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

/// Bounds native PTY drain after root exit, including inherited slave handles
/// and a consumer that stops reading. An aborted producer leaves capture incomplete.
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

/// Keep the authoritative native reader through EOF or an explicit bounded
/// cutoff. Its producer receipt, not channel closure, certifies complete capture.
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

pub(crate) type StdinCloseReceipt =
    tokio::sync::watch::Receiver<Option<Result<(), (io::ErrorKind, String)>>>;

/// Producer EOF receipts. Channel closure alone is not proof of capture: a
/// failed or aborted reader leaves its receipt false. Only the producer sets it.
#[derive(Clone, Debug, Default)]
pub struct OutputCapture {
    pub(crate) stdout: Arc<AtomicBool>,
    pub(crate) stderr: Arc<AtomicBool>,
}

impl OutputCapture {
    pub fn stdout_complete(&self) -> bool {
        self.stdout.load(Ordering::Acquire)
    }
    pub fn stderr_complete(&self) -> bool {
        self.stderr.load(Ordering::Acquire)
    }
    pub fn complete(&self) -> bool {
        self.stdout_complete() && self.stderr_complete()
    }
}

/// Handle for driving an interactive process (PTY or pipe).
pub struct ProcessHandle {
    stdin_close_receipt: Option<StdinCloseReceipt>,
    output_capture: OutputCapture,
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
        pty_handles: Option<SharedPtyHandles>,
        resizer: Option<ResizeFn>,
    ) -> Self {
        // The waiter can release the terminal before this handle is constructed. Backend
        // identity must survive that release so finish still drains the final ConPTY frame.
        let has_pty = pty_handles.is_some();
        Self {
            stdin_close_receipt: None,
            output_capture: OutputCapture::default(),
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            reader_abort_handles: StdMutex::new(reader_abort_handles),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            pty_handles: pty_handles.unwrap_or_default(),
            has_pty,
            resizer: StdMutex::new(resizer),
        }
    }

    pub(crate) fn with_output_capture(mut self, capture: OutputCapture) -> Self {
        self.output_capture = capture;
        self
    }

    /// The Unix PTY writer reports ordered input-drain/close errors;
    /// pipe and ConPTY sessions keep sender-drop close semantics.
    #[cfg(unix)]
    pub(crate) fn with_stdin_close_receipt(mut self, receipt: StdinCloseReceipt) -> Self {
        self.stdin_close_receipt = Some(receipt);
        self
    }

    pub fn output_capture(&self) -> OutputCapture {
        self.output_capture.clone()
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

    /// Drop this handle's stdin sender. All sender clones must also be dropped.
    /// This alone is not evidence of terminal EOF; use `close_stdin_ordered`
    /// when the caller needs an ordered close result.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Close pipe input after queued bytes. Unix PTYs drain accepted input then
    /// return Unsupported: terminal input cannot be half-closed reliably, and
    /// no terminal-control bytes are synthesized.
    /// External writer clones must be dropped before this operation can finish.
    pub async fn close_stdin_ordered(&self) -> io::Result<()> {
        self.close_stdin();
        if let Some(mut receipt) = self.stdin_close_receipt.clone() {
            let result = receipt.wait_for(Option::is_some).await.map_err(|_| {
                io::Error::other("stdin writer stopped before reporting ordered input close")
            })?;
            return match result.as_ref() {
                Some(Ok(())) => Ok(()),
                Some(Err((kind, message))) => Err(io::Error::new(*kind, message.clone())),
                // `wait_for(Option::is_some)` never yields an empty result.
                None => Err(io::Error::other(
                    "stdin writer stopped before reporting ordered input close",
                )),
            };
        }
        Ok(())
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
    /// inherited output handle. Native PTY readers drain to EOF with a bounded
    /// cutoff, rather than discarding kernel-buffered bytes at root exit.
    pub fn finish(&self) {
        self.close_stdin();

        if let Ok(mut killer_opt) = self.killer.lock() {
            killer_opt.take();
        }

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            if self.has_pty {
                drain_released_pty_reader(handle);
            } else {
                handle.abort();
            }
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
    let capture = OutputCapture::default();
    if stderr_driver_rx.is_none() {
        capture.stderr.store(true, Ordering::Release);
    }
    let spawn_stream_reader = |mut output_rx: ProcessOutputReceiver,
                               output_tx: mpsc::Sender<Vec<u8>>,
                               complete: Arc<AtomicBool>| {
        tokio::spawn(async move {
            let mut lossless = true;
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
                    Err(ProcessOutputRecvError::Lagged) => lossless = false,
                    Err(ProcessOutputRecvError::Closed) => {
                        complete.store(lossless, Ordering::Release);
                        break;
                    }
                }
            }
        })
    };
    let reader_handle =
        spawn_stream_reader(stdout_driver_rx, stdout_tx, Arc::clone(&capture.stdout));
    let stderr_reader_handle = stderr_driver_rx
        .take()
        .map(|rx| spawn_stream_reader(rx, stderr_tx, Arc::clone(&capture.stderr)));

    let writer_handle = writer_handle.unwrap_or_else(|| tokio::spawn(async {}));

    let (exit_tx, exit_rx_out) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle = tokio::spawn(async move {
        // A closed driver channel means observation was lost, not that the
        // child exited with a sentinel code. Propagate closure to the owner.
        if let Ok(code) = exit_rx.await {
            publish_exit_status(&wait_exit_status, &wait_exit_code, code);
            let _ = exit_tx.send(code);
        }
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
        None,
        resizer,
    )
    .with_output_capture(capture);

    SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx: exit_rx_out,
    }
}
