use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode_protocol::CancellationCause;
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
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::MAX_IN_FLIGHT_REQUESTS;
use codex_code_mode_protocol::host::MAX_PENDING_DELEGATE_REQUESTS;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use codex_utils_pty::ManagedRootProcess;
use codex_utils_pty::process_observation::ObservedProcess;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use self::driver::ConnectionDriver;
use self::driver::DriverCommand;
use self::driver::DriverEvent;
use self::driver::DriverLifecycle;
use self::driver::ExecuteClaims;
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
const HOST_RESPONSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const OPEN_SESSION_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_SESSION_TIMEOUT: Duration = Duration::from_secs(15);
// Host-responsiveness budgets count time this backend could observe the host.
// Scheduler lateness and the blocking pool (which Windows child pipes share)
// reveal backend stalls; that much stall within a window re-arms it, a bounded
// number of times, instead of retiring a host that may already have answered.
const BACKEND_STALL_TICK: Duration = Duration::from_millis(250);
const BACKEND_STALL_EVIDENCE: Duration = Duration::from_secs(1);
const MAX_BACKEND_STALL_EXTENSIONS: u32 = 5;

pub(super) struct Connection {
    observation: ObservedProcess,
    interrupt_cause_supported: bool,
    command_tx: mpsc::Sender<DriverCommand>,
    execute_claims: Arc<ExecuteClaims>,
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

struct OwnedHostProcess {
    child: Child,
    managed: ManagedRootProcess,
    observation: ObservedProcess,
    attached: bool,
}

/// Keep a post-birth cleanup owner through startup and supervision, even if
/// either future is dropped before its explicit retirement path runs.
struct HostProcessGuard {
    owned: Option<OwnedHostProcess>,
    runtime: Handle,
}

impl HostProcessGuard {
    fn owned_mut(&mut self) -> &mut OwnedHostProcess {
        self.owned.as_mut().expect("code-mode host still owned")
    }

    fn start_cleanup(&mut self) -> Option<impl Future<Output = io::Result<()>>> {
        let OwnedHostProcess {
            child,
            managed,
            observation,
            attached,
        } = self.owned.take()?;
        // A caller can drop its pending spawn future outside the process's
        // runtime context. Dispatch cleanup on the original runtime.
        let _runtime = self.runtime.enter();
        Some(managed.cleanup_observed_child(child, observation, attached))
    }

    async fn cleanup(mut self) {
        if let Some(cleanup) = self.start_cleanup()
            && let Err(error) = cleanup.await
        {
            warn!("code-mode host cleanup: {error}");
        }
    }

    fn record_reaped(mut self) {
        if let Some(owned) = self.owned.take() {
            owned
                .observation
                .cleanup_observed("root_reaped; descendants_unknown");
        }
    }
}

impl Drop for HostProcessGuard {
    fn drop(&mut self) {
        // Dispatch is synchronous; only the caller's bounded waiter is dropped.
        drop(self.start_cleanup());
    }
}

struct ConnectionSupervisor {
    process: HostProcessGuard,
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

        let managed = ManagedRootProcess::reserve_with_reclaim()
            .await
            .map_err(|err| format!("failed to reserve code-mode host process: {err}"))?;
        let runtime = Handle::current();
        let child = command
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
        let mut process = HostProcessGuard {
            owned: Some(OwnedHostProcess {
                child,
                managed,
                observation: observation.clone(),
                attached: false,
            }),
            runtime,
        };
        let host = process.owned_mut();
        let process_id = host
            .child
            .id()
            .ok_or_else(|| "spawned code-mode host has no process id".to_string())?;

        if let Err(err) = host.managed.attach(process_id) {
            process.cleanup().await;
            return Err(format!(
                "failed to contain code-mode host {}: {err}",
                host_program.display()
            ));
        }
        host.attached = true;

        let host_process_id = host.child.id();
        tracing::debug!(?host_process_id, host_program = %host_program.display(),
            stage = "spawned", "code-mode host lifecycle");
        if let Some(stderr) = host.child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => tracing::info!(
                            event.name = "codex.code_mode_host.observation",
                            ?host_process_id,
                            host_event = %line,
                            "code-mode host stderr observation"
                        ),
                        Ok(None) => break,
                        Err(err) => {
                            warn!("failed to read code-mode host stderr: {err}");
                            break;
                        }
                    }
                }
            });
        }

        let stdin = host
            .child
            .stdin
            .take()
            .ok_or_else(|| "spawned code-mode host has no stdin".to_string())?;
        let stdout = host
            .child
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
            let interrupt_capability =
                codex_code_mode_protocol::host::Capability::new(HostRequest::INTERRUPT_CAUSE_CAPABILITY)
                    .map_err(|error| error.to_string())?;
            let hello = ClientHello::new(
                // Timeout pause is optional so older matched hosts remain usable.
                SupportedProtocolVersions::try_new([ProtocolVersion::V1])
                    .map_err(|err| err.to_string())?,
                CapabilitySet::empty(),
                CapabilitySet::try_new([
                    catalog_capability.clone(),
                    state_capability.clone(),
                    receipt_capability.clone(),
                    interrupt_capability.clone(),
                    codex_code_mode_protocol::host::Capability::new(
                        codex_code_mode_protocol::host::TIMEOUT_PAUSE_CAPABILITY,
                    ).map_err(|error| error.to_string())?,
                    codex_code_mode_protocol::host::Capability::new(
                        codex_code_mode_protocol::host::CAUSAL_INPUTS_CAPABILITY,
                    ).map_err(|error| error.to_string())?,
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
                        hello.capabilities().contains(&codex_code_mode_protocol::host::Capability::new(
                            codex_code_mode_protocol::host::TIMEOUT_PAUSE_CAPABILITY,
                        ).map_err(|error| error.to_string())?),
                        hello.capabilities().contains(&interrupt_capability),
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
        let handshake = within_host_budget("negotiate", HOST_HANDSHAKE_TIMEOUT, handshake);
        let handshake_result = match handshake.await {
            Some(result) => result,
            None => {
                process.cleanup().await;
                return Err("timed out negotiating with the code-mode host".to_string());
            }
        };
        let (
            tool_catalog_references,
            named_state_snapshots,
            receipt_recovery,
            timeout_pause,
            interrupt_cause_supported,
        ) = match handshake_result {
            Ok(enabled) => enabled,
            Err(err) => {
                process.cleanup().await;
                return Err(err);
            }
        };

        tracing::debug!(?host_process_id, stage = "handshake_ready",
            "code-mode host lifecycle");
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

        let (mut driver, execute_claims) = ConnectionDriver::new(
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
        driver.timeout_pause = timeout_pause;
        let driver_task = tokio::spawn(driver.run());
        tokio::spawn(
            ConnectionSupervisor {
                process,
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
            interrupt_cause_supported,
            command_tx,
            execute_claims,
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
        on_possible_execution: Box<dyn FnOnce() + Send>,
    ) -> Result<StartedCell, String> {
        let cancellation = CallerCancellation::new();
        let (response_tx, response_rx) = oneshot::channel();
        self.send(DriverCommand::Execute {
            session,
            request,
            on_possible_execution,
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
        // Record the exact claim before exposing StartedCell, without an
        // await that could let caller cancellation abandon the admitted cell.
        if !self.execute_claims.submit(delivered.request_id) {
            return Err(self.failure_message());
        }
        cancellation.disarm();
        Ok(delivered.started)
    }

    pub(super) fn cell_is_current(session: &RemoteSession, cell_id: &CellId) -> bool {
        driver::remote_cell_id(session, cell_id).is_ok()
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
        interrupt_cause: Option<CancellationCause>,
    ) -> Result<WaitOutcome, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.control_request(
            "terminate cell",
            TERMINATE_TIMEOUT,
            DriverCommand::Terminate {
                session,
                cell_id,
                interrupt_cause: interrupt_cause.filter(|_| self.interrupt_cause_supported),
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
        let mut submitted = false;
        let result = within_host_budget(operation, timeout, async {
            self.send(command).await?;
            submitted = true;
            // No await separates command acceptance from cleanup ownership.
            // OpenSession retains its existing abandoned-session cleanup.
            accepted.armed = destructive;
            self.receive(response_rx).await
        })
        .await;
        accepted.armed = false;
        match result {
            Some(result) => result,
            None if !submitted => Err(format!(
                "timed out submitting code-mode request to {operation}; no command was accepted"
            )),
            None => {
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
        let mut root_reaped = false;
        let mut reader_finished = false;
        let mut driver_finished = false;
        let reason = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => failure_message(&self.failure),
            result = &mut self.driver_task => {
                driver_finished = true;
                match result {
                    Ok(()) => "code-mode connection driver exited unexpectedly".to_string(),
                    Err(err) => format!("code-mode connection driver task failed: {err}"),
                }
            },
            result = &mut self.reader_task => {
                reader_finished = true;
                task_failure("reader", result)
            },
            result = &mut self.writer_task => task_failure("writer", result),
            result = self.process.owned_mut().child.wait() => {
                match result {
                    Ok(status) => {
                        root_reaped = true;
                        format!("code-mode host exited with status {status}")
                    }
                    Err(err) => format!("failed waiting for code-mode host: {err}"),
                }
            }
        };
        mark_connection_dead(&self.alive, &self.failure, reason.clone());
        if !driver_finished && !self.cancellation.is_cancelled() {
            // Exit closes admission, not response custody. Drain stdout before
            // queueing retirement behind the reader's already-decoded messages.
            // Inherited pipes or a stuck driver must not delay explicit teardown.
            let drain = drain_host_responses(
                reader_finished, &mut self.reader_task, &self.event_tx,
                &mut self.driver_task, reason,
            );
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => {},
                _ = tokio::time::timeout(HOST_RESPONSE_DRAIN_TIMEOUT, drain) => {},
            }
        }
        self.cancellation.cancel();
        if root_reaped {
            self.process.record_reaped();
        } else {
            self.process.cleanup().await;
        }
    }
}

/// Runs `future` within a host-responsiveness `budget`, returning `None` when
/// the budget expires. A window in which this backend itself stalled for at
/// least `BACKEND_STALL_EVIDENCE` (late scheduler ticks, or a blocking-pool
/// probe that cannot start) proves nothing about the host, so it is re-armed,
/// at most `MAX_BACKEND_STALL_EXTENSIONS` times.
async fn within_host_budget<F: std::future::Future>(
    operation: &str,
    budget: Duration,
    future: F,
) -> Option<F::Output> {
    tokio::pin!(future);
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    let mut ticks = tokio::time::interval(BACKEND_STALL_TICK);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut observed = tokio::time::Instant::now();
    let mut stalled = Duration::ZERO;
    let mut extensions = 0;
    loop {
        tokio::select! {
            biased;
            output = &mut future => return Some(output),
            _ = ticks.tick() => note_backend_gap(&mut observed, &mut stalled),
            () = &mut deadline => {
                note_backend_gap(&mut observed, &mut stalled);
                // Windows child pipe reads and writes run on the blocking pool.
                let probe_started = tokio::time::Instant::now();
                let probe = tokio::time::timeout(
                    BACKEND_STALL_EVIDENCE,
                    tokio::task::spawn_blocking(|| ()),
                );
                tokio::select! {
                    biased;
                    output = &mut future => return Some(output),
                    _ = probe => {}
                }
                let probe_wait = probe_started.elapsed();
                if probe_wait > BACKEND_STALL_TICK {
                    stalled += probe_wait;
                }
                observed = tokio::time::Instant::now();
                if stalled < BACKEND_STALL_EVIDENCE || extensions >= MAX_BACKEND_STALL_EXTENSIONS {
                    return None;
                }
                extensions += 1;
                warn!(
                    operation,
                    extensions,
                    stalled = ?stalled,
                    "code-mode backend stalled during the host response window; re-arming it"
                );
                stalled = Duration::ZERO;
                deadline.as_mut().reset(observed + budget);
            }
        }
    }
}

/// Adds the part of a gap between observations that exceeds two ticks.
fn note_backend_gap(observed: &mut tokio::time::Instant, stalled: &mut Duration) {
    let now = tokio::time::Instant::now();
    let gap = now.saturating_duration_since(*observed);
    if gap > 2 * BACKEND_STALL_TICK {
        *stalled += gap - BACKEND_STALL_TICK;
    }
    *observed = now;
}

async fn drain_host_responses(
    reader_finished: bool,
    reader_task: &mut JoinHandle<Result<(), String>>,
    events: &mpsc::Sender<DriverEvent>,
    driver_task: &mut JoinHandle<()>,
    reason: String,
) {
    if !reader_finished {
        let _ = reader_task.await;
    }
    let _ = events.send(DriverEvent::Failed(reason)).await;
    let _ = driver_task.await;
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

/// Whether this process is attached to a console, with or without a window.
#[cfg(windows)]
fn process_has_console() -> bool {
    let mut process_id = 0;
    // SAFETY: the buffer is the one u32 its length declares; the call only
    // reads this process's console attachment.
    unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(&mut process_id, 1) != 0 }
}
