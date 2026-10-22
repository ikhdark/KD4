use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::StartedCell;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientHello;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::EncodedFrame;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS;
use codex_code_mode_protocol::host::MAX_PENDING_DELEGATE_REQUESTS;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use codex_utils_pty::ManagedRootProcess;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::warn;

use self::driver::ConnectionDriver;
use self::driver::DriverCommand;
use self::driver::DriverEvent;
use self::driver::DriverLifecycle;
pub(super) use self::driver::RemoteSession;
pub(super) use self::driver::SessionCleanup;
use self::reader::drive_reader;

mod driver;
mod reader;

const IPC_CHANNEL_CAPACITY: usize = 128;
// Frames the client can owe the host for work the host admits: a request and
// its cancellation per operation the host runs at once, and a response per
// delegate request it leaves pending. A full queue fails every session on the
// connection, so it must not fill below that bound.
const OUTGOING_FRAME_CAPACITY: usize = 2 * MAX_IN_FLIGHT_REQUESTS + MAX_PENDING_DELEGATE_REQUESTS;
const HOST_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_SESSION_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_SESSION_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Connection {
    observation: codex_utils_pty::process_observation::ObservedProcess,
    command_tx: mpsc::Sender<DriverCommand>,
    execute_claim_tx: mpsc::UnboundedSender<RequestId>,
    alive: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    cancellation: CancellationToken,
}

struct CallerCancellation {
    token: CancellationToken,
    armed: bool,
}

struct AcceptedControlRequest<'a> {
    connection: &'a Connection,
    operation: &'a str,
    armed: bool,
}

impl Drop for AcceptedControlRequest<'_> {
    fn drop(&mut self) {
        if self.armed {
            let reason = format!(
                "stopped waiting for code-mode host to {}; effects are uncertain; closing the unobserved host connection",
                self.operation,
            );
            // The existing supervisor owns process cleanup. An accepted
            // destructive control must not outlive its observer on a reusable
            // host, even when an outer deadline cancels our inner timeout.
            mark_connection_dead(&self.connection.alive, &self.connection.failure, reason);
            self.connection.cancellation.cancel();
        }
    }
}

struct ConnectionSupervisor {
    child: Child,
    managed: Arc<ManagedRootProcess>,
    event_tx: mpsc::Sender<DriverEvent>,
    cancellation: CancellationToken,
    alive: Arc<AtomicBool>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
    driver_task: JoinHandle<()>,
    reader_task: JoinHandle<Result<(), String>>,
    writer_task: JoinHandle<Result<(), String>>,
}

impl CallerCancellation {
    fn new() -> Self {
        Self::with_token(CancellationToken::new())
    }

    fn with_token(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

impl Connection {
    pub(super) fn observe_reference(&self) {
        self.observation.reference();
    }

    pub(super) async fn spawn(host_program: &Path) -> Result<Self, String> {
        let mut command = Command::new(host_program);
        // Without a console to inherit, as under a console-less Desktop
        // app-server, Windows opens a visible console for the host (~250 ms
        // per spawn). Inheriting one is ~5 ms faster than a hidden console.
        #[cfg(windows)]
        if !process_has_console() {
            command.creation_flags(/*flags*/ 0x0800_0000); // CREATE_NO_WINDOW
        }

        let managed = Arc::new(
            ManagedRootProcess::reserve_with_reclaim()
                .await
                .map_err(|err| format!("failed to reserve code-mode host process: {err}"))?,
        );
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| {
                format!(
                    "failed to spawn code-mode host {}: {err}",
                    host_program.display()
                )
            })?;
        let observation = codex_utils_pty::process_observation::observe_process_start(
            "code_mode_host",
            codex_utils_pty::process_observation::ProcessRole::AuxiliaryRoot,
            child.id(),
            None,
        );
        let process_id = child
            .id()
            .ok_or_else(|| "spawned code-mode host has no process id".to_string())?;

        if let Err(err) = managed.attach(process_id) {
            kill_and_reap(&mut child, &managed).await;
            return Err(format!(
                "failed to contain code-mode host {}: {err}",
                host_program.display()
            ));
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => debug!("code-mode host stderr: {line}"),
                        Ok(None) => break,
                        Err(err) => {
                            warn!("failed to read code-mode host stderr: {err}");
                            break;
                        }
                    }
                }
            });
        }

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "spawned code-mode host has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "spawned code-mode host has no stdout".to_string())?;
        let mut reader = FramedReader::new(stdout);
        let mut writer = FramedWriter::new(stdin);
        let handshake = async {
            let catalog_capability = codex_code_mode_protocol::host::Capability::new(
                codex_code_mode_protocol::host::TOOL_CATALOG_CAPABILITY,
            )
            .map_err(|error| error.to_string())?;
            let state_capability = codex_code_mode_protocol::host::Capability::new(
                codex_code_mode_protocol::host::NAMED_STATE_CAPABILITY,
            )
            .map_err(|error| error.to_string())?;
            let receipt_capability = codex_code_mode_protocol::host::Capability::new(
                codex_code_mode_protocol::host::RECEIPT_RECOVERY_CAPABILITY,
            )
            .map_err(|error| error.to_string())?;
            let hello = ClientHello::new(
                SupportedProtocolVersions::try_new([ProtocolVersion::V1])
                    .map_err(|err| err.to_string())?,
                CapabilitySet::empty(),
                CapabilitySet::try_new([
                    catalog_capability.clone(),
                    state_capability.clone(),
                    receipt_capability.clone(),
                ])
                .map_err(|error| error.to_string())?,
            )
            .map_err(|err| err.to_string())?;
            writer
                .write(&ClientToHost::ClientHello(hello))
                .await
                .map_err(|err| format!("failed to write code-mode host hello: {err}"))?;
            match reader
                .read::<HostToClient>()
                .await
                .map_err(|err| format!("failed to read code-mode host hello: {err}"))?
            {
                Some(HostToClient::HostHello(hello))
                    if hello.selected_version() == ProtocolVersion::V1 =>
                {
                    Ok((
                        hello.capabilities().contains(&catalog_capability),
                        hello.capabilities().contains(&state_capability),
                        hello.capabilities().contains(&receipt_capability),
                    ))
                }
                Some(HostToClient::HandshakeRejected { reason }) => {
                    Err(format!("code-mode host rejected the handshake: {reason:?}"))
                }
                Some(message) => Err(format!(
                    "code-mode host returned an invalid handshake response: {message:?}"
                )),
                None => Err("code-mode host exited during handshake".to_string()),
            }
        };
        let handshake_result = match tokio::time::timeout(HOST_HANDSHAKE_TIMEOUT, handshake).await {
            Ok(result) => result,
            Err(_) => {
                kill_and_reap(&mut child, &managed).await;
                return Err("timed out negotiating with the code-mode host".to_string());
            }
        };
        let (tool_catalog_references, named_state_snapshots, receipt_recovery) =
            match handshake_result {
                Ok(enabled) => enabled,
                Err(err) => {
                    kill_and_reap(&mut child, &managed).await;
                    return Err(err);
                }
            };

        let (command_tx, command_rx) = mpsc::channel(IPC_CHANNEL_CAPACITY);
        let (event_tx, event_rx) = mpsc::channel(IPC_CHANNEL_CAPACITY);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<EncodedFrame>(OUTGOING_FRAME_CAPACITY);
        let cancellation = CancellationToken::new();
        let alive = Arc::new(AtomicBool::new(true));
        let failure = Arc::new(std::sync::Mutex::new(None));

        let writer_cancellation = cancellation.clone();
        let writer_task = tokio::spawn(drive_writer(writer, outgoing_rx, writer_cancellation));

        let reader_events = event_tx.clone();
        let reader_cancellation = cancellation.clone();
        let reader_task =
            tokio::spawn(
                async move { drive_reader(reader, reader_events, reader_cancellation).await },
            );

        let (mut driver, execute_claim_tx) = ConnectionDriver::new(
            command_rx,
            event_rx,
            event_tx.clone(),
            outgoing_tx,
            DriverLifecycle {
                alive: Arc::clone(&alive),
                failure: Arc::clone(&failure),
                cancellation: cancellation.clone(),
            },
        );
        driver.tool_catalog_references = tool_catalog_references;
        driver.named_state_snapshots = named_state_snapshots;
        driver.receipt_recovery = receipt_recovery;
        let driver_task = tokio::spawn(driver.run());
        tokio::spawn(
            ConnectionSupervisor {
                child,
                managed,
                event_tx,
                cancellation: cancellation.clone(),
                alive: Arc::clone(&alive),
                failure: Arc::clone(&failure),
                driver_task,
                reader_task,
                writer_task,
            }
            .run(),
        );

        Ok(Self {
            observation,
            command_tx,
            execute_claim_tx,
            alive,
            failure,
            cancellation,
        })
    }

    pub(super) fn is_alive(&self) -> bool {
        if self.command_tx.is_closed() {
            mark_connection_dead(
                &self.alive,
                &self.failure,
                "code-mode connection driver closed".to_string(),
            );
        }
        self.alive.load(Ordering::Acquire)
    }

    pub(super) async fn open_session(
        &self,
        session: RemoteSession,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> Result<SessionCleanup, String> {
        let cleanup = SessionCleanup::new();
        let cancellation = CallerCancellation::new();
        let (response_tx, response_rx) = oneshot::channel();
        let result = self
            .control_request(
                "open session",
                OPEN_SESSION_TIMEOUT,
                DriverCommand::OpenSession {
                    session,
                    delegate,
                    cleanup: cleanup.clone(),
                    caller_cancellation: cancellation.token(),
                    response_tx,
                },
                response_rx,
            )
            .await;
        cancellation.disarm();
        result?;
        Ok(cleanup)
    }

    pub(super) async fn execute(
        &self,
        session: RemoteSession,
        request: ExecuteRequest,
    ) -> Result<StartedCell, String> {
        let cancellation = CallerCancellation::new();
        let (response_tx, response_rx) = oneshot::channel();
        self.send(DriverCommand::Execute {
            session,
            request,
            caller_cancellation: cancellation.token(),
            response_tx,
        })
        .await?;
        let delivered = match self.receive(response_rx).await {
            Ok(delivered) => delivered,
            Err(err) => {
                cancellation.disarm();
                return Err(err);
            }
        };
        self.execute_claim_tx
            .send(delivered.request_id)
            .map_err(|_| self.failure_message())?;
        cancellation.disarm();
        Ok(delivered.started)
    }

    #[cfg(test)]
    pub(super) async fn wait(
        &self,
        session: RemoteSession,
        request: WaitRequest,
    ) -> Result<WaitOutcome, String> {
        self.wait_releasable(session, request, CancellationToken::new())
            .await
    }

    /// Cancelling `release` asks the host to release this observer early while
    /// its response is still received; dropping the future abandons it.
    pub(super) async fn wait_releasable(
        &self,
        session: RemoteSession,
        request: WaitRequest,
        release: CancellationToken,
    ) -> Result<WaitOutcome, String> {
        let cancellation = CallerCancellation::with_token(release.child_token());
        let (response_tx, response_rx) = oneshot::channel();
        self.send(DriverCommand::Wait {
            session,
            request,
            caller_cancellation: cancellation.token(),
            response_tx,
        })
        .await?;
        let result = self.receive(response_rx).await;
        cancellation.disarm();
        result
    }

    pub(super) async fn terminate(
        &self,
        session: RemoteSession,
        cell_id: CellId,
    ) -> Result<WaitOutcome, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.control_request(
            "terminate cell",
            TERMINATE_TIMEOUT,
            DriverCommand::Terminate {
                session,
                cell_id,
                response_tx,
            },
            response_rx,
        )
        .await
    }

    pub(super) async fn shutdown_session(&self, session: RemoteSession) -> Result<(), String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.control_request(
            "shutdown session",
            SHUTDOWN_SESSION_TIMEOUT,
            DriverCommand::ShutdownSession {
                session,
                response_tx,
            },
            response_rx,
        )
        .await
    }

    async fn control_request<T>(
        &self,
        operation: &str,
        timeout: Duration,
        command: DriverCommand,
        response_rx: oneshot::Receiver<Result<T, String>>,
    ) -> Result<T, String> {
        let destructive = matches!(
            &command,
            DriverCommand::Terminate { .. } | DriverCommand::ShutdownSession { .. }
        );
        let mut accepted = AcceptedControlRequest {
            connection: self,
            operation,
            armed: false,
        };
        let result = tokio::time::timeout(timeout, async {
            self.send(command).await?;
            // No await separates command acceptance from cleanup ownership.
            // OpenSession retains its existing abandoned-session cleanup.
            accepted.armed = destructive;
            self.receive(response_rx).await
        })
        .await;
        accepted.armed = false;
        match result {
            Ok(result) => result,
            Err(_) => {
                let reason = format!(
                    "timed out waiting for code-mode host to {operation}; effects are uncertain; closing the unresponsive host connection"
                );
                // The supervisor retains process custody. Do not leave an
                // unacknowledged control operation on a reusable connection.
                mark_connection_dead(&self.alive, &self.failure, reason.clone());
                self.cancellation.cancel();
                Err(reason)
            }
        }
    }

    async fn send(&self, command: DriverCommand) -> Result<(), String> {
        if !self.is_alive() {
            return Err(self.failure_message());
        }
        self.command_tx
            .send(command)
            .await
            .map_err(|_| self.failure_message())
    }

    async fn receive<T>(
        &self,
        response_rx: oneshot::Receiver<Result<T, String>>,
    ) -> Result<T, String> {
        response_rx.await.map_err(|_| self.failure_message())?
    }

    fn failure_message(&self) -> String {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| "code-mode host connection closed".to_string())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        mark_connection_dead(
            &self.alive,
            &self.failure,
            "code-mode host connection closed".to_string(),
        );
        self.cancellation.cancel();
    }
}

impl ConnectionSupervisor {
    async fn run(mut self) {
        let mut child_exited = false;
        let reason = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => failure_message(&self.failure),
            result = &mut self.driver_task => match result {
                Ok(()) => "code-mode connection driver exited unexpectedly".to_string(),
                Err(err) => format!("code-mode connection driver task failed: {err}"),
            },
            result = &mut self.reader_task => task_failure("reader", result),
            result = &mut self.writer_task => task_failure("writer", result),
            result = self.child.wait() => {
                child_exited = true;
                match result {
                    Ok(status) => format!("code-mode host exited with status {status}"),
                    Err(err) => format!("failed waiting for code-mode host: {err}"),
                }
            }
        };
        mark_connection_dead(&self.alive, &self.failure, reason.clone());
        let _ = self.event_tx.try_send(DriverEvent::Failed(reason));
        self.cancellation.cancel();
        if !child_exited {
            kill_and_reap(&mut self.child, &self.managed).await;
        }
    }
}

async fn drive_writer<W: tokio::io::AsyncWrite + Unpin>(
    mut writer: FramedWriter<W>,
    mut frames: mpsc::Receiver<EncodedFrame>,
    cancellation: CancellationToken,
) -> Result<(), String> {
    loop {
        let frame = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            frame = frames.recv() => frame.ok_or_else(|| "code-mode host outgoing stream closed".to_string())?,
        };
        tokio::select! {
            biased;
            // A partial frame cannot be resumed on this stream. Cancellation
            // retires the entire connection; drop the writer, never replay it.
            _ = cancellation.cancelled() => return Ok(()),
            result = writer.write_frame(&frame) => {
                result.map_err(|err| format!("failed to write code-mode host message: {err}"))?;
            }
        }
    }
}

fn task_failure(
    task_name: &str,
    result: Result<Result<(), String>, tokio::task::JoinError>,
) -> String {
    match result {
        Ok(Ok(())) => format!("code-mode connection {task_name} exited unexpectedly"),
        Ok(Err(err)) => err,
        Err(err) => format!("code-mode connection {task_name} task failed: {err}"),
    }
}

fn mark_connection_dead(
    alive: &AtomicBool,
    failure: &std::sync::Mutex<Option<String>>,
    reason: String,
) {
    alive.store(false, Ordering::Release);
    let mut failure = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failure.is_none() {
        *failure = Some(reason);
    }
}

fn failure_message(failure: &std::sync::Mutex<Option<String>>) -> String {
    failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| "code-mode host connection closed".to_string())
}

async fn kill_and_reap(child: &mut Child, managed: &ManagedRootProcess) {
    let _ = managed.terminate();

    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Whether this process is attached to a console, with or without a window.
#[cfg(windows)]
fn process_has_console() -> bool {
    let mut process_id = 0;
    // SAFETY: the buffer is the one u32 its length declares; the call only
    // reads this process's console attachment.
    unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(&mut process_id, 1) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_code_mode_protocol::host::SessionId;

    #[tokio::test]
    async fn session_wait_keeps_host_result_when_observer_drops_before_or_after_send() {
        use super::super::ProcessOwnedCodeModeSession;
        use super::super::SessionBinding;
        use super::super::SessionState;
        for send_before_drop in [false, true] {
            let (command_tx, mut commands) = mpsc::channel(4);
            let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
            let connection = Arc::new(Connection {
                observation: Default::default(),
                command_tx,
                execute_claim_tx,
                alive: Arc::new(AtomicBool::new(true)),
                failure: Arc::new(std::sync::Mutex::new(None)),
                cancellation: CancellationToken::new(),
            });
            let session = ProcessOwnedCodeModeSession::new();
            *session.inner.state.lock().unwrap() = SessionState::Open(SessionBinding {
                connection,
                remote: RemoteSession {
                    id: SessionId::new("custody").unwrap(),
                    generation: 1,
                },
                cleanup: SessionCleanup::new(),
            });
            let request = WaitRequest {
                cell_id: CellId::new("1".into()),
                yield_time_ms: 0,
                recovery: None,
            };
            let outcome = || {
                WaitOutcome::LiveCell(codex_code_mode_protocol::RuntimeResponse::Yielded {
                    cell_id: request.cell_id.clone(),
                    content_items: Vec::new(),
                })
            };
            let mut observer = Box::pin(session.wait(request.clone()));
            assert!(futures::poll!(&mut observer).is_pending());
            let DriverCommand::Wait {
                response_tx,
                caller_cancellation,
                ..
            } = commands.recv().await.unwrap()
            else {
                panic!("expected one host wait");
            };
            let mut response = Some(response_tx);
            if send_before_drop {
                response.take().unwrap().send(Ok(outcome())).unwrap();
            }
            drop(observer);
            assert!(
                !caller_cancellation.is_cancelled(),
                "steering must not abandon receiving custody"
            );
            if let Some(response) = response {
                response.send(Ok(outcome())).unwrap();
            }
            assert_eq!(session.wait(request.clone()).await, Ok(outcome()));
            assert!(
                commands.try_recv().is_err(),
                "resume must consume the original wait, not run it again"
            );
            assert!(session.inner.pending_waits.lock().unwrap().is_empty());
            // Avoid starting shutdown IPC in this transport-only fixture.
            *session.inner.state.lock().unwrap() = SessionState::Closed;
        }
    }

    #[tokio::test]
    async fn writer_cancellation_releases_backpressured_pipe_without_replay() {
        use tokio::io::AsyncReadExt;

        let mut samples = Vec::new();
        for _ in 0..7 {
            let (pipe, mut host) = tokio::io::duplex(1);
            let (frames, rx) = mpsc::channel(2);
            frames
                .send(EncodedFrame::encode(&"first frame").unwrap())
                .await
                .unwrap();
            frames
                .send(EncodedFrame::encode(&"must not be replayed").unwrap())
                .await
                .unwrap();
            let cancellation = CancellationToken::new();
            let pump = drive_writer(FramedWriter::new(pipe), rx, cancellation.clone());
            tokio::pin!(pump);
            assert!(
                futures::poll!(&mut pump).is_pending(),
                "one-byte pipe must backpressure"
            );
            let started = std::time::Instant::now();
            cancellation.cancel();
            tokio::time::timeout(Duration::from_millis(100), &mut pump)
                .await
                .expect("cancel must not wait for a host read")
                .unwrap();
            samples.push(started.elapsed().as_micros());
            let mut partial = Vec::new();
            host.read_to_end(&mut partial).await.unwrap();
            assert_eq!(
                partial.len(),
                1,
                "never restart a partial frame or dispatch its successor"
            );
            assert!(frames.is_closed());
        }
        samples.sort_unstable();
        eprintln!(
            "writer_cancel_backpressure_us={samples:?} median={}",
            samples[3]
        );
    }

    #[tokio::test]
    async fn writer_preserves_order_and_reports_closed_transport() {
        let (pipe, host) = tokio::io::duplex(1024);
        let (frames, rx) = mpsc::channel(2);
        for message in ["first", "second"] {
            frames
                .send(EncodedFrame::encode(&message).unwrap())
                .await
                .unwrap();
        }
        drop(frames);
        let result = drive_writer(FramedWriter::new(pipe), rx, CancellationToken::new()).await;
        assert!(result.unwrap_err().contains("outgoing stream closed"));
        let mut reader = FramedReader::new(host);
        assert_eq!(reader.read::<String>().await.unwrap(), Some("first".into()));
        assert_eq!(
            reader.read::<String>().await.unwrap(),
            Some("second".into())
        );
        assert_eq!(reader.read::<String>().await.unwrap(), None);

        let (pipe, host) = tokio::io::duplex(1);
        drop(host);
        let (frames, rx) = mpsc::channel(1);
        frames
            .send(EncodedFrame::encode(&"failure").unwrap())
            .await
            .unwrap();
        let error = drive_writer(FramedWriter::new(pipe), rx, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.contains("failed to write code-mode host message"));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ack_deadline_fails_live_unresponsive_connection() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let shutdown = connection.shutdown_session(RemoteSession {
            id: codex_code_mode_protocol::host::SessionId::new("held-host").unwrap(),
            generation: 1,
        });
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        // Keep both the command channel and host reply sender open.
        let DriverCommand::ShutdownSession { response_tx, .. } = commands.recv().await.unwrap()
        else {
            panic!("expected shutdown command");
        };
        let started = tokio::time::Instant::now();
        let error = shutdown.await.unwrap_err();
        assert_eq!(started.elapsed(), SHUTDOWN_SESSION_TIMEOUT);
        assert!(error.contains("effects are uncertain"));
        assert!(!connection.is_alive());
        assert!(connection.cancellation.is_cancelled());
        assert!(
            response_tx.send(Ok(())).is_err(),
            "late ack cannot report success"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pending_shutdown_observation_keeps_the_last_host_lease_until_acknowledged() {
        use super::super::ProcessOwnedCodeModeSession;
        use super::super::SessionBinding;
        use super::super::SessionState;
        let (command_tx, mut commands) = mpsc::channel(4);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Arc::new(Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        });
        let session = ProcessOwnedCodeModeSession::new();
        *session.inner.state.lock().unwrap() = SessionState::Open(SessionBinding {
            connection: Arc::clone(&connection),
            remote: RemoteSession {
                id: SessionId::new("pending-storage").unwrap(),
                generation: 1,
            },
            cleanup: SessionCleanup::new(),
        });
        let mut observer = Box::pin(session.shutdown());
        assert!(futures::poll!(&mut observer).is_pending());
        for _ in 0..16 {
            let DriverCommand::ShutdownSession { response_tx, .. } = commands.recv().await.unwrap()
            else {
                panic!("only close observation may be retried");
            };
            response_tx
                .send(Err(
                    codex_code_mode_protocol::host::SESSION_SHUTDOWN_PENDING.into(),
                ))
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(
            observer.await,
            Err(codex_code_mode_protocol::host::SESSION_SHUTDOWN_PENDING.into())
        );
        assert!(connection.is_alive());
        assert!(!connection.cancellation.is_cancelled());
        assert_eq!(
            session
                .inner
                .process_host
                .state
                .lock()
                .unwrap()
                .active_sessions,
            1
        );
        assert!(matches!(
            *session.inner.state.lock().unwrap(),
            SessionState::Closing
        ));
        let DriverCommand::ShutdownSession { response_tx, .. } = commands.recv().await.unwrap()
        else {
            panic!("expected retained close owner");
        };
        response_tx.send(Ok(())).unwrap();
        session.shutdown().await.unwrap();
        assert_eq!(
            session
                .inner
                .process_host
                .state
                .lock()
                .unwrap()
                .active_sessions,
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_termination_observer_cannot_leave_host_reusable() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let cell_id = CellId::new("held-cell".to_string());
        // Core's terminate_interrupted_cell observes termination for five
        // seconds, less than this connection's ten-second control deadline.
        // Withhold the host acknowledgment while retaining a live transport.
        let response_tx = {
            let observer = tokio::time::timeout(
                Duration::from_secs(5),
                connection.terminate(
                    RemoteSession {
                        id: codex_code_mode_protocol::host::SessionId::new("held-host").unwrap(),
                        generation: 1,
                    },
                    cell_id.clone(),
                ),
            );
            tokio::pin!(observer);
            assert!(futures::poll!(&mut observer).is_pending());
            let DriverCommand::Terminate { response_tx, .. } = commands.recv().await.unwrap()
            else {
                panic!("expected termination command");
            };
            assert!(
                observer.await.is_err(),
                "the outer observer reports uncertainty, not termination"
            );
            response_tx
        }; // Drop the timed-out control future, as the core caller does.
        tokio::time::advance(TERMINATE_TIMEOUT).await;
        tokio::task::yield_now().await;

        assert!(
            !connection.is_alive(),
            "dropping the observer must not disable fail-closed control ownership"
        );
        assert!(connection.cancellation.is_cancelled());
        assert!(
            connection
                .failure_message()
                .contains("effects are uncertain")
        );
        assert!(
            response_tx
                .send(Ok(WaitOutcome::LiveCell(
                    codex_code_mode_protocol::RuntimeResponse::Terminated {
                        cell_id,
                        content_items: Vec::new(),
                    },
                )))
                .is_err(),
            "late acknowledgment must not become a success receipt"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unadmitted_termination_cancellation_preserves_connection() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let remote = RemoteSession {
            id: SessionId::new("busy-host").unwrap(),
            generation: 1,
        };
        let (response_tx, _response_rx) = oneshot::channel();
        connection
            .command_tx
            .send(DriverCommand::Terminate {
                session: remote.clone(),
                cell_id: CellId::new("already-queued".into()),
                response_tx,
            })
            .await
            .unwrap();
        let mut termination =
            Box::pin(connection.terminate(remote, CellId::new("unadmitted".into())));
        assert!(futures::poll!(&mut termination).is_pending());
        drop(termination);

        assert!(
            connection.is_alive(),
            "an unaccepted command has no host effects"
        );
        assert!(!connection.cancellation.is_cancelled());
        assert!(connection.failure.lock().unwrap().is_none());
        assert!(
            matches!(commands.recv().await, Some(DriverCommand::Terminate { cell_id, .. })
            if cell_id.as_str() == "already-queued")
        );
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn acknowledged_termination_preserves_connection() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let cell_id = CellId::new("finished-cell".into());
        let mut termination = Box::pin(connection.terminate(
            RemoteSession {
                id: SessionId::new("responsive-host").unwrap(),
                generation: 1,
            },
            cell_id.clone(),
        ));
        assert!(futures::poll!(&mut termination).is_pending());
        let DriverCommand::Terminate { response_tx, .. } = commands.recv().await.unwrap() else {
            panic!("expected termination command");
        };
        let outcome = || {
            WaitOutcome::LiveCell(codex_code_mode_protocol::RuntimeResponse::Terminated {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
            })
        };
        response_tx.send(Ok(outcome())).unwrap();
        assert_eq!(termination.await.unwrap(), outcome());
        assert!(connection.is_alive());
        assert!(!connection.cancellation.is_cancelled());
        assert!(connection.failure.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn dropped_open_observer_preserves_abandoned_session_cleanup() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let mut opening = Box::pin(connection.open_session(
            RemoteSession {
                id: SessionId::new("opening-host").unwrap(),
                generation: 1,
            },
            Arc::new(crate::NoopCodeModeSessionDelegate),
        ));
        assert!(futures::poll!(&mut opening).is_pending());
        let DriverCommand::OpenSession {
            caller_cancellation,
            response_tx,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("expected open command");
        };
        drop(opening);
        assert!(caller_cancellation.is_cancelled());
        assert!(response_tx.is_closed());
        assert!(
            connection.is_alive(),
            "open retains the driver's abandoned-session cleanup"
        );
        assert!(!connection.cancellation.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn execution_observation_has_no_control_plane_deadline() {
        let (command_tx, mut commands) = mpsc::channel(1);
        let (execute_claim_tx, _claims) = mpsc::unbounded_channel();
        let connection = Connection {
            observation: Default::default(),
            command_tx,
            execute_claim_tx,
            alive: Arc::new(AtomicBool::new(true)),
            failure: Arc::new(std::sync::Mutex::new(None)),
            cancellation: CancellationToken::new(),
        };
        let cell_id = CellId::new("1".to_string());
        let wait = connection.wait(
            RemoteSession {
                id: codex_code_mode_protocol::host::SessionId::new("long-cell").unwrap(),
                generation: 1,
            },
            WaitRequest {
                cell_id: cell_id.clone(),
                yield_time_ms: 0,
                recovery: None,
            },
        );
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());
        let DriverCommand::Wait { response_tx, .. } = commands.recv().await.unwrap() else {
            panic!("expected wait command");
        };
        tokio::time::advance(SHUTDOWN_SESSION_TIMEOUT * 2).await;
        assert!(futures::poll!(&mut wait).is_pending());
        assert!(connection.is_alive());
        assert!(!connection.cancellation.is_cancelled());
        let outcome = || {
            WaitOutcome::LiveCell(codex_code_mode_protocol::RuntimeResponse::Yielded {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
            })
        };
        response_tx.send(Ok(outcome())).unwrap();
        assert_eq!(wait.await, Ok(outcome()));
        assert!(connection.is_alive());
    }
}
