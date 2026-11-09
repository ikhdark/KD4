use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol_types::CommandExecOutputDeltaNotification;
use codex_app_server_protocol_types::CommandExecOutputStream;
use codex_app_server_protocol_types::CommandExecResizeParams;
use codex_app_server_protocol_types::CommandExecResizeResponse;
use codex_app_server_protocol_types::CommandExecResponse;
use codex_app_server_protocol_types::CommandExecTerminateParams;
use codex_app_server_protocol_types::CommandExecTerminateResponse;
use codex_app_server_protocol_types::CommandExecWriteParams;
use codex_app_server_protocol_types::CommandExecWriteResponse;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::PtyTerminalSize;
use codex_app_server_protocol_types::ServerNotification;
use codex_core::exec::ExecExpiration;
use codex_core::exec::ExecExpirationOutcome;
use codex_core::exec::IO_DRAIN_TIMEOUT_MS;
use codex_core::exec::StdoutStream;
use codex_core::sandboxing::ExecRequest;
use codex_protocol::exec_output::bytes_to_string_smart;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecOutputStream;
use codex_sandboxing::SandboxType;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use codex_utils_pty::ProcessHandle;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use codex_app_server_rpc::connection_rpc_gate::ConnectionRpcGate;
use codex_app_server_rpc::error_code::internal_error;
use codex_app_server_rpc::error_code::invalid_params;
use codex_app_server_rpc::error_code::invalid_request;
use codex_app_server_rpc::outgoing_message::ConnectionId;
use codex_app_server_rpc::outgoing_message::ConnectionRequestId;
use codex_app_server_rpc::outgoing_message::OutgoingMessageSender;

const EXEC_TIMEOUT_EXIT_CODE: i32 = 124;
const MAX_PENDING_STDIN_WRITES: usize = 32;
const OUTPUT_CHUNK_SIZE_HINT: usize = 64 * 1024;
const OUTPUT_DELIVERY_MAX_QUEUED_BYTES: usize = 256 * 1024;
const OUTPUT_DELIVERY_QUEUE_ITEMS: usize = 256;
const OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES: usize = 1024;

pub(crate) struct OutputByteCap {
    limit: Option<usize>,
    retained: usize,
    truncated: bool,
}

impl OutputByteCap {
    pub(crate) fn new(limit: Option<usize>) -> Self {
        Self {
            limit,
            retained: 0,
            truncated: false,
        }
    }

    pub(crate) fn accept<'a>(&mut self, chunk: &'a [u8]) -> (&'a [u8], bool) {
        let Some(limit) = self.limit else {
            self.retained = self.retained.saturating_add(chunk.len());
            return (chunk, false);
        };
        let retained_len = limit.saturating_sub(self.retained).min(chunk.len());
        self.retained = self.retained.saturating_add(retained_len);
        let observed_excess = retained_len < chunk.len();
        let newly_truncated = observed_excess && !self.truncated;
        self.truncated |= observed_excess;
        (&chunk[..retained_len], newly_truncated)
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }
}

pub(crate) async fn observe_process_exit(session: Arc<ProcessHandle>, exit_rx: oneshot::Receiver<i32>) -> i32 {
    if let Some(code) = session.exit_code() { return code; }
    if let Ok(code) = exit_rx.await { return code; }
    tracing::error!(cleanup_pending = true, "native exit observer failed; retaining process ownership until confirmed exit");
    loop {
        if let Some(code) = session.exit_code() { return code; }
        if let Err(error) = session.request_terminate() {
            tracing::warn!(%error, "native exit recovery termination failed");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Validate command argv at an app-server request boundary.
///
/// Execution managers only accept requests produced by those boundaries and
/// may rely on the first argv entry being present.
pub(crate) fn validate_command_argv(command: &[String]) -> Result<(), JSONRPCErrorError> {
    if command.is_empty() {
        return Err(invalid_request("command must not be empty"));
    }
    Ok(())
}

fn attach_connection_cancellation(
    exec_request: &mut ExecRequest,
    connection_cancellation: CancellationToken,
) {
    exec_request.expiration = exec_request
        .expiration
        .clone()
        .with_cancellation(connection_cancellation);
}

pub(crate) trait ConnectionSessionKey: Eq + std::hash::Hash {
    fn connection_id(&self) -> ConnectionId;
}

/// Session ownership is indexed by connection so close never scans or clones
/// another connection's process keys under the shared admission lock.
pub(crate) struct ConnectionSessionMap<K, V> {
    connections: HashMap<ConnectionId, HashMap<K, V>>,
}

impl<K, V> Default for ConnectionSessionMap<K, V> {
    fn default() -> Self { Self { connections: HashMap::new() } }
}

impl<K: ConnectionSessionKey, V> ConnectionSessionMap<K, V> {
    fn contains_key(&self, key: &K) -> bool { self.get(key).is_some() }
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.connections.get(&key.connection_id())?.get(key)
    }
    fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.connections.entry(key.connection_id()).or_default().insert(key, value)
    }
    pub(crate) fn entry(&mut self, key: K) -> std::collections::hash_map::Entry<'_, K, V> {
        self.connections.entry(key.connection_id()).or_default().entry(key)
    }
    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        let connection_id = key.connection_id();
        let entries = self.connections.get_mut(&connection_id)?;
        let removed = entries.remove(key);
        if entries.is_empty() { self.connections.remove(&connection_id); }
        removed
    }
    pub(crate) fn remove_connection(&mut self, connection_id: ConnectionId) -> Vec<V> {
        self.connections.remove(&connection_id).unwrap_or_default().into_values().collect()
    }
}

#[derive(Clone)]
pub(crate) struct CommandExecManager {
    sessions: Arc<Mutex<ConnectionSessionMap<ConnectionProcessId, CommandExecSession>>>,
    run_tasks: tokio_util::task::TaskTracker,
    next_generated_process_id: Arc<AtomicI64>,
}

impl Default for CommandExecManager {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(ConnectionSessionMap::default())),
            run_tasks: tokio_util::task::TaskTracker::new(),
            next_generated_process_id: Arc::new(AtomicI64::new(1)),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionProcessId {
    connection_id: ConnectionId,
    process_id: InternalProcessId,
}

impl ConnectionSessionKey for ConnectionProcessId {
    fn connection_id(&self) -> ConnectionId { self.connection_id }
}

#[derive(Clone)]
enum CommandExecSession {
    Active {
        control_tx: mpsc::Sender<CommandControlRequest>,
        write_tx: mpsc::Sender<StdinWriteRequest>,
        write_slots: Arc<Semaphore>,
    },
    UnsupportedWindowsSandbox,
}

enum CommandControl {
    Resize { size: TerminalSize },
    Terminate,
}

struct CommandControlRequest {
    control: CommandControl,
    response_tx: Option<oneshot::Sender<Result<(), JSONRPCErrorError>>>,
}

pub(crate) struct StdinWriteRequest {
    pub(crate) delta: Vec<u8>,
    pub(crate) close_stdin: bool,
    pub(crate) response_tx: Option<oneshot::Sender<Result<(), JSONRPCErrorError>>>,
}

pub(crate) struct StartCommandExecParams {
    pub(crate) outgoing: Arc<OutgoingMessageSender>,
    pub(crate) request_id: ConnectionRequestId,
    pub(crate) process_id: Option<String>,
    pub(crate) exec_request: ExecRequest,
    pub(crate) tty: bool,
    pub(crate) stream_stdin: bool,
    pub(crate) stream_stdout_stderr: bool,
    pub(crate) output_bytes_cap: Option<usize>,
    pub(crate) size: Option<TerminalSize>,
}

struct RunCommandParams {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: ConnectionRequestId,
    process_id: Option<String>,
    spawned: SpawnedProcess,
    control_rx: mpsc::Receiver<CommandControlRequest>,
    write_rx: mpsc::Receiver<StdinWriteRequest>,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    expiration: ExecExpiration,
    output_bytes_cap: Option<usize>,
    connection_cancellation: CancellationToken,
    terminal_cleanup: CommandTerminalCleanup,
}

struct CommandTerminalCleanup {
    sessions: Arc<Mutex<ConnectionSessionMap<ConnectionProcessId, CommandExecSession>>>,
    process_key: ConnectionProcessId,
}

struct SpawnProcessOutputParams {
    process_id: Option<String>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    stdio_timeout_rx: watch::Receiver<bool>,
    delivery_relay: Option<OutputDeliveryRelay>,
    stream: CommandExecOutputStream,
    stream_output: bool,
    output_bytes_cap: Option<usize>,
}

struct CapturedOutput {
    bytes: Vec<u8>,
    cap_reached: bool,
    drain_timed_out: bool,
    reached_eof: bool,
}

#[derive(Clone)]
struct OutputDeliveryRelay {
    tx: mpsc::Sender<QueuedOutputDelivery>,
    byte_budget: Arc<Semaphore>,
    capacity_changed: Arc<tokio::sync::Notify>,
    settlement: watch::Sender<Option<tokio::time::Instant>>,
}

struct QueuedOutputDelivery {
    notification: CommandExecOutputDeltaNotification,
    _byte_permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct UndeliveredOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_cap_reached: bool,
    stderr_cap_reached: bool,
}

impl UndeliveredOutput {
    fn note_cap(&mut self, stream: CommandExecOutputStream, cap_reached: bool) {
        match stream {
            CommandExecOutputStream::Stdout => self.stdout_cap_reached |= cap_reached,
            CommandExecOutputStream::Stderr => self.stderr_cap_reached |= cap_reached,
        }
    }
    fn append(&mut self, stream: CommandExecOutputStream, bytes: &[u8]) {
        match stream {
            CommandExecOutputStream::Stdout => self.stdout.extend_from_slice(bytes),
            CommandExecOutputStream::Stderr => self.stderr.extend_from_slice(bytes),
        }
    }

    fn append_tail(&mut self, tail: Self) {
        self.stdout_cap_reached |= tail.stdout_cap_reached;
        self.stderr_cap_reached |= tail.stderr_cap_reached;
        if self.stdout.is_empty() { self.stdout = tail.stdout; } else { self.stdout.extend(tail.stdout); }
        if self.stderr.is_empty() { self.stderr = tail.stderr; } else { self.stderr.extend(tail.stderr); }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum InternalProcessId {
    Generated(i64),
    Client(String),
}

impl InternalProcessId {
    fn error_repr(&self) -> String {
        match self {
            Self::Generated(id) => id.to_string(),
            Self::Client(id) => serde_json::to_string(id).unwrap_or_else(|_| format!("{id:?}")),
        }
    }
}

impl CommandExecManager {
    pub(crate) async fn start_with_gate(
        &self,
        params: StartCommandExecParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        let StartCommandExecParams {
            outgoing,
            request_id,
            process_id,
            exec_request,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            size,
        } = params;
        let connection_cancellation = rpc_gate.cancellation_token().child_token();
        // Register before spawning, under the connection's admission fence.
        // The token follows the run owner through child exit, output and reply.
        let run_owner = rpc_gate
            .try_commit(|| self.run_tasks.token())
            .ok_or_else(|| invalid_request("connection is closed"))?;
        if process_id.is_none() && (tty || stream_stdin || stream_stdout_stderr) {
            return Err(invalid_request(
                "command/exec tty or streaming requires a client-supplied processId",
            ));
        }
        let process_id = process_id.map_or_else(
            || {
                InternalProcessId::Generated(
                    self.next_generated_process_id
                        .fetch_add(1, Ordering::Relaxed),
                )
            },
            InternalProcessId::Client,
        );
        let process_key = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: process_id.clone(),
        };

        if matches!(exec_request.sandbox, SandboxType::WindowsRestrictedToken) {
            if tty || stream_stdin {
                return Err(invalid_request(
                    "tty and stdin streaming are not supported with windows sandbox",
                ));
            }
            if output_bytes_cap != Some(DEFAULT_OUTPUT_BYTES_CAP) {
                return Err(invalid_request(
                    "custom outputBytesCap is not supported with windows sandbox",
                ));
            }
            if let InternalProcessId::Client(_) = &process_id {
                let mut sessions = self.sessions.lock().await;
                rpc_gate
                    .try_commit(|| {
                        if sessions.contains_key(&process_key) {
                            return Err(invalid_request(format!(
                                "duplicate active command/exec process id: {}",
                                process_key.process_id.error_repr(),
                            )));
                        }
                        sessions.insert(
                            process_key.clone(),
                            CommandExecSession::UnsupportedWindowsSandbox,
                        );
                        Ok(())
                    })
                    .ok_or_else(|| invalid_request("connection is closed"))??;
            }
            let sessions = Arc::clone(&self.sessions);
            let mut exec_request = exec_request;
            attach_connection_cancellation(&mut exec_request, connection_cancellation.clone());
            tokio::spawn(async move {
                let _run_owner = run_owner;
                let (delivery_relay, delivery_handle) = if stream_stdout_stderr {
                    let (relay, handle) = spawn_output_delivery_relay(
                        Arc::clone(&outgoing),
                        request_id.connection_id,
                        connection_cancellation.clone(),
                    );
                    (Some(relay), Some(handle))
                } else {
                    (None, None)
                };
                let notification_process_id = match &process_id {
                    InternalProcessId::Generated(_) => None,
                    InternalProcessId::Client(id) => Some(id.clone()),
                };
                let (stdout_stream, event_relay_handle, first_hop_recovery) =
                    match (delivery_relay.clone(), notification_process_id) {
                        (Some(delivery_relay), Some(process_id)) => {
                            let (tx_event, rx_event) =
                                async_channel::bounded::<codex_protocol::protocol::Event>(1);
                            let handle = spawn_windows_output_relay(
                                rx_event, delivery_relay, process_id, output_bytes_cap,
                            );
                            let (stream, recovery) = StdoutStream::with_delivery_recovery(
                                String::new(),
                                String::new(),
                                tx_event,
                                output_bytes_cap
                                    .unwrap_or(DEFAULT_OUTPUT_BYTES_CAP)
                                    .min(DEFAULT_OUTPUT_BYTES_CAP),
                            );
                            (Some(stream), Some(handle), Some(recovery))
                        }
                        _ => (None, None, None),
                    };
                let output = codex_core::sandboxing::execute_env(exec_request, stdout_stream).await;
                if let Some(relay) = delivery_relay.as_ref() { relay.begin_settlement(); }
                let event_fallback = if let Some(handle) = event_relay_handle {
                    handle.await
                } else {
                    Ok(UndeliveredOutput::default())
                };
                drop(delivery_relay);
                let undelivered = if let Some(handle) = delivery_handle {
                    handle.await
                } else {
                    Ok(UndeliveredOutput::default())
                };
                sessions.lock().await.remove(&process_key);
                let undelivered = match undelivered.and_then(|mut output| {
                    output.append_tail(event_fallback?);
                    if let Some(recovery) = first_hop_recovery {
                        let (stdout_cap_reached, stderr_cap_reached) = recovery.cap_reached();
                        let (stdout, stderr) = recovery.take_suffix();
                        output.append_tail(UndeliveredOutput { stdout, stderr, stdout_cap_reached, stderr_cap_reached });
                    }
                    Ok(output)
                }) {
                    Ok(output) => output,
                    Err(err) => {
                        outgoing
                            .send_error(
                                request_id,
                                internal_error(format!("command output capture failed: {err}")),
                            )
                            .await;
                        return;
                    }
                };
                match output {
                    Ok(output) => {
                        outgoing
                            .send_response(
                                request_id,
                                CommandExecResponse {
                                    exit_code: output.exit_code,
                                    stdout_base64: (stream_stdout_stderr
                                        && !undelivered.stdout.is_empty())
                                    .then(|| STANDARD.encode(&undelivered.stdout)),
                                    stderr_base64: (stream_stdout_stderr
                                        && !undelivered.stderr.is_empty())
                                    .then(|| STANDARD.encode(&undelivered.stderr)),
                                    // This legacy sandbox backend has no per-stream EOF receipt.
                                    stdout_capture: undelivered.stdout_cap_reached.then_some(codex_app_server_protocol_types::ProcessOutputCaptureState {
                                        reached_eof: false, cap_reached: true, drain_timed_out: false,
                                    }),
                                    stderr_capture: undelivered.stderr_cap_reached.then_some(codex_app_server_protocol_types::ProcessOutputCaptureState {
                                        reached_eof: false, cap_reached: true, drain_timed_out: false,
                                    }),
                                    stdout: if stream_stdout_stderr {
                                        bytes_to_string_smart(&undelivered.stdout)
                                    } else {
                                        output.stdout.text
                                    },
                                    stderr: if stream_stdout_stderr {
                                        bytes_to_string_smart(&undelivered.stderr)
                                    } else {
                                        output.stderr.text
                                    },
                                },
                            )
                            .await;
                    }
                    Err(err) => {
                        outgoing
                            .send_error(request_id, internal_error(format!("exec failed: {err}")))
                            .await;
                    }
                }
            });
            return Ok(());
        }

        let ExecRequest {
            command,
            cwd,
            env,
            expiration,
            sandbox: _sandbox,
            arg0,
            ..
        } = exec_request;
        // TODO(anp): Keep PathUri through the local command launch boundary.
        let cwd = cwd
            .to_abs_path()
            .map_err(|err| invalid_request(format!("invalid command cwd: {err}")))?;

        let stream_stdin = tty || stream_stdin;
        let stream_stdout_stderr = tty || stream_stdout_stderr;
        let (control_tx, control_rx) = mpsc::channel(32);
        // Stdin writes preserve ordered backpressure on a dedicated worker so a
        // slow child cannot block terminate, resize, expiration, or exit handling.
        let (write_tx, write_rx) = mpsc::channel(MAX_PENDING_STDIN_WRITES);
        let write_slots = Arc::new(Semaphore::new(MAX_PENDING_STDIN_WRITES));
        let notification_process_id = match &process_id {
            InternalProcessId::Generated(_) => None,
            InternalProcessId::Client(process_id) => Some(process_id.clone()),
        };

        let sessions = Arc::clone(&self.sessions);
        {
            let mut sessions = self.sessions.lock().await;
            rpc_gate
                .try_commit(|| {
                    if sessions.contains_key(&process_key) {
                        return Err(invalid_request(format!(
                            "duplicate active command/exec process id: {}",
                            process_key.process_id.error_repr(),
                        )));
                    }
                    sessions.insert(
                        process_key.clone(),
                        CommandExecSession::Active {
                            control_tx,
                            write_tx,
                            write_slots,
                        },
                    );
                    Ok(())
                })
                .ok_or_else(|| invalid_request("connection is closed"))??;
        }
        // Native spawn and handoff must survive cancellation of the RPC waiter.
        let spawn_owner = tokio::spawn(async move {
        let Some((program, args)) = command.split_first() else {
            return Err(internal_error("validated command unexpectedly empty"));
        };
        let spawned = if tty {
            codex_utils_pty::spawn_pty_process(
                program,
                args,
                cwd.as_path(),
                &env,
                &arg0,
                size.unwrap_or_default(),
            )
            .await
        } else if stream_stdin {
            codex_utils_pty::spawn_pipe_process(program, args, cwd.as_path(), &env, &arg0).await
        } else {
            codex_utils_pty::spawn_pipe_process_no_stdin(program, args, cwd.as_path(), &env, &arg0)
                .await
        };
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(err) => {
                sessions.lock().await.remove(&process_key);
                return Err(internal_error(format!("failed to spawn command: {err}")));
            }
        };
        tokio::spawn(async move {
            let _run_owner = run_owner;
            run_command(RunCommandParams {
                outgoing,
                request_id: request_id.clone(),
                process_id: notification_process_id,
                spawned,
                control_rx,
                write_rx,
                stream_stdin,
                stream_stdout_stderr,
                expiration,
                output_bytes_cap,
                connection_cancellation,
                terminal_cleanup: CommandTerminalCleanup {
                    sessions,
                    process_key,
                },
            })
            .await;
        });
        Ok::<_, JSONRPCErrorError>(())
        });
        spawn_owner.await.map_err(|err| internal_error(format!("command spawn owner failed: {err}")))?
    }


    pub(crate) async fn write_with_gate(
        &self,
        outgoing: Arc<OutgoingMessageSender>,
        request_id: ConnectionRequestId,
        params: CommandExecWriteParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        if params.delta_base64.is_none() && !params.close_stdin {
            return Err(invalid_params(
                "command/exec/write requires deltaBase64 or closeStdin",
            ));
        }

        if let Some(encoded) = params.delta_base64.as_deref() {
            validate_stdin_delta_length(encoded)?;
        }

        let target_process_id = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: InternalProcessId::Client(params.process_id),
        };
        let session = self
            .sessions
            .lock()
            .await
            .get(&target_process_id)
            .cloned()
            .ok_or_else(|| {
                invalid_request(format!(
                    "no active command/exec for process id {}",
                    target_process_id.process_id.error_repr(),
                ))
            })?;
        let CommandExecSession::Active {
            write_tx,
            write_slots,
            ..
        } = session
        else {
            return Err(invalid_request(
                "command/exec/write, command/exec/terminate, and command/exec/resize are not supported for windows sandbox processes",
            ));
        };
        let busy = || invalid_request("command/exec stdin write queue is full; retry after a pending write completes");
        let slot = write_slots.try_acquire_owned().map_err(|_| busy())?;
        let write_permit = write_tx.try_reserve_owned().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => busy(),
            mpsc::error::TrySendError::Closed(_) => command_no_longer_running_error(&target_process_id.process_id),
        })?;
        let delta = params.delta_base64.as_deref().map(decode_stdin_delta).transpose()?.unwrap_or_default();
        let cancellation = rpc_gate.cancellation_token();
        // Admission stays in the same FIFO lane as command start and other writes.
        // Transfer the acknowledgement to the connection's owner under its close
        // fence, so blocked stdin cannot keep termination queued behind this RPC.
        drop(rpc_gate.spawn_with_commit(|| {
            let (response_tx, response_rx) = oneshot::channel();
            write_permit.send(StdinWriteRequest {
                delta,
                close_stdin: params.close_stdin,
                response_tx: Some(response_tx),
            });
            Ok::<_, JSONRPCErrorError>(async move {
                // Count owners awaiting delivery as well as writes awaiting stdin.
                let _slot = slot;
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = response_rx => result.unwrap_or_else(|_| {
                        Err(command_no_longer_running_error(&target_process_id.process_id))
                    }),
                };
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {},
                    _ = async {
                        match result {
                            Ok(()) => outgoing.send_response(request_id, CommandExecWriteResponse {}).await,
                            Err(error) => outgoing.send_error(request_id, error).await,
                        }
                    } => {},
                }
            })
        })?.ok_or_else(|| invalid_request("connection is closed"))?);
        Ok(())
    }

    pub(crate) async fn terminate(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecTerminateParams,
    ) -> Result<CommandExecTerminateResponse, JSONRPCErrorError> {
        let target_process_id = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: InternalProcessId::Client(params.process_id),
        };
        self.send_control(target_process_id, CommandControl::Terminate)
            .await?;
        Ok(CommandExecTerminateResponse {})
    }

    pub(crate) async fn resize(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecResizeParams,
    ) -> Result<CommandExecResizeResponse, JSONRPCErrorError> {
        let target_process_id = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: InternalProcessId::Client(params.process_id),
        };
        self.send_control(
            target_process_id,
            CommandControl::Resize {
                size: terminal_size_from_protocol(params.size.into_inner(), "command/exec")?,
            },
        )
        .await?;
        Ok(CommandExecResizeResponse {})
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let controls = {
            self.sessions.lock().await.remove_connection(connection_id)
        };

        for control in controls {
            if let CommandExecSession::Active { control_tx, .. } = control {
                let _ = control_tx
                    .send(CommandControlRequest {
                        control: CommandControl::Terminate,
                        response_tx: None,
                    })
                    .await;
            }
        }
    }

    pub(crate) async fn drain_run_tasks(&self) {
        self.run_tasks.close();
        self.run_tasks.wait().await;
    }

    async fn send_control(
        &self,
        process_id: ConnectionProcessId,
        control: CommandControl,
    ) -> Result<(), JSONRPCErrorError> {
        let session = {
            self.sessions
                .lock()
                .await
                .get(&process_id)
                .cloned()
                .ok_or_else(|| {
                    invalid_request(format!(
                        "no active command/exec for process id {}",
                        process_id.process_id.error_repr(),
                    ))
                })?
        };
        let CommandExecSession::Active { control_tx, .. } = session else {
            return Err(invalid_request(
                "command/exec/write, command/exec/terminate, and command/exec/resize are not supported for windows sandbox processes",
            ));
        };
        let (response_tx, response_rx) = oneshot::channel();
        control_tx
            .send(CommandControlRequest {
                control,
                response_tx: Some(response_tx),
            })
            .await
            .map_err(|_| command_no_longer_running_error(&process_id.process_id))?;
        response_rx
            .await
            .map_err(|_| command_no_longer_running_error(&process_id.process_id))?
    }
}

async fn run_command(params: RunCommandParams) {
    let RunCommandParams {
        outgoing,
        request_id,
        process_id,
        spawned,
        control_rx,
        write_rx,
        stream_stdin,
        stream_stdout_stderr,
        expiration,
        output_bytes_cap,
        connection_cancellation,
        terminal_cleanup,
    } = params;
    let mut control_rx = control_rx;
    let mut control_open = true;
    let expiration = expiration
        .with_cancellation(connection_cancellation.clone())
        .wait_with_outcome();
    tokio::pin!(expiration);
    let SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    let session = Arc::new(session);
    let native_exit = observe_process_exit(Arc::clone(&session), exit_rx);
    tokio::pin!(native_exit);
    let mut expiration_outcome = None;
    let mut termination_deadline = None;
    let mut termination_reported = false;
    let mut termination_error = None;
    let mut termination_delivery: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>> = None;
    let (stdio_timeout_tx, stdio_timeout_rx) = watch::channel(false);
    let stdin_writer_handle = spawn_stdin_writer(
        Arc::clone(&session),
        write_rx,
        stream_stdin,
        "stdin streaming is not enabled for this command/exec",
    );

    let (delivery_relay, delivery_handle) = if stream_stdout_stderr {
        let (relay, handle) = spawn_output_delivery_relay(
            Arc::clone(&outgoing),
            request_id.connection_id,
            connection_cancellation,
        );
        (Some(relay), Some(handle))
    } else {
        (None, None)
    };

    let stdout_handle = spawn_process_output(SpawnProcessOutputParams {
        process_id: process_id.clone(),
        output_rx: stdout_rx,
        stdio_timeout_rx: stdio_timeout_rx.clone(),
        delivery_relay: delivery_relay.clone(),
        stream: CommandExecOutputStream::Stdout,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
    });
    let stderr_handle = spawn_process_output(SpawnProcessOutputParams {
        process_id: process_id.clone(),
        output_rx: stderr_rx,
        stdio_timeout_rx,
        delivery_relay: delivery_relay.clone(),
        stream: CommandExecOutputStream::Stderr,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
    });

    let exit_code = loop {
        tokio::select! {
            biased;
            exit = &mut native_exit => {
                break if matches!(expiration_outcome, Some(ExecExpirationOutcome::TimedOut)) {
                    EXEC_TIMEOUT_EXIT_CODE
                } else { exit };
            }
            _ = async { if let Some(delivery) = termination_delivery.as_mut() { delivery.await; } },
                if termination_delivery.is_some() => {
                termination_delivery = None;
            }
            outcome = &mut expiration, if expiration_outcome.is_none() => {
                if let Some(code) = session.exit_code() { break code; }
                expiration_outcome = Some(outcome);
                termination_error = session.request_terminate().err().map(|error| error.to_string());
                termination_deadline = Some(tokio::time::Instant::now() + codex_app_server_rpc::outgoing_message::RESOURCE_DELIVERY_TIMEOUT);
            }
            _ = async {
                if let Some(deadline) = termination_deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if termination_deadline.is_some() => {
                // A failed kill remains retryable in ProcessHandle. Report an
                // honest unknown outcome, but keep this run, process ID, native
                // waiter and admission owned until exit is actually observed.
                let retry_error = session.request_terminate().err().map(|error| error.to_string());
                termination_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1));
                if termination_reported { continue; }
                let mut error = internal_error("command expiration requested termination, but process exit is not confirmed; cleanup remains pending; do not repeat the command");
                error.data = Some(serde_json::json!({
                    "kind": "process_exit_unconfirmed", "cleanupPending": true,
                    "terminationError": retry_error.or_else(|| termination_error.take()),
                }));
                termination_delivery = Some(Box::pin(outgoing.send_error(request_id.clone(), error)));
                termination_reported = true;
            }
            control = control_rx.recv(), if control_open => {
                match control {
                    Some(CommandControlRequest { control, response_tx }) => {
                        let result = match control {
                            CommandControl::Resize { size } => {
                                handle_process_resize(&session, size)
                            }
                            CommandControl::Terminate => session
                                .request_terminate()
                                .map_err(|error| internal_error(error.to_string())),
                        };
                        if let Some(response_tx) = response_tx {
                            let _ = response_tx.send(result);
                        }
                    },
                    None => {
                        control_open = false;
                        let _ = session.request_terminate();
                    }
                }
            }
        }
    };
    // Retain the identifier for delivery, but reject controls immediately.
    drop(control_rx);
    if let Some(relay) = delivery_relay.as_ref() { relay.begin_settlement(); }
    stdin_writer_handle.abort();
    let _ = stdin_writer_handle.await;

    let timeout_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(IO_DRAIN_TIMEOUT_MS)).await;
        let _ = stdio_timeout_tx.send(true);
    });

    let stdout = stdout_handle.await;
    let stderr = stderr_handle.await;
    let native_capture = session.output_capture();
    let stdout_capture =
        stdout.as_ref().ok().map(
            |output| codex_app_server_protocol_types::ProcessOutputCaptureState {
                reached_eof: output.reached_eof && native_capture.stdout_complete(),
                cap_reached: output.cap_reached,
                drain_timed_out: output.drain_timed_out,
            },
        );
    let stderr_capture =
        stderr.as_ref().ok().map(
            |output| codex_app_server_protocol_types::ProcessOutputCaptureState {
                reached_eof: output.reached_eof && native_capture.stderr_complete(),
                cap_reached: output.cap_reached,
                drain_timed_out: output.drain_timed_out,
            },
        );
    timeout_handle.abort();
    drop(delivery_relay);
    let undelivered = if let Some(delivery_handle) = delivery_handle {
        delivery_handle.await
    } else {
        Ok(UndeliveredOutput::default())
    };
    terminal_cleanup
        .sessions
        .lock()
        .await
        .remove(&terminal_cleanup.process_key);
    let undelivered = match undelivered.and_then(|mut output| {
        // Relay failures precede chunks rejected by relay admission. Decode the
        // entire suffix once, including split UTF-8 codepoints.
        output.append_tail(UndeliveredOutput {
            stdout: stdout?.bytes,
            stderr: stderr?.bytes,
            ..UndeliveredOutput::default()
        });
        Ok(output)
    }) {
        Ok(output) => output,
        Err(err) => {
            if termination_reported {
                if let Some(delivery) = termination_delivery.take() { delivery.await; }
            } else {
                outgoing.send_error(request_id, internal_error(format!("command output capture failed: {err}"))).await;
            }
            return;
        }
    };

    if expiration_outcome.is_none() {
        session.finish();
    }
    if termination_reported {
        if let Some(delivery) = termination_delivery { delivery.await; }
        return;
    }
    outgoing
        .send_response(
            request_id,
            CommandExecResponse {
                exit_code,
                stdout_base64: (stream_stdout_stderr && !undelivered.stdout.is_empty())
                    .then(|| STANDARD.encode(&undelivered.stdout)),
                stderr_base64: (stream_stdout_stderr && !undelivered.stderr.is_empty())
                    .then(|| STANDARD.encode(&undelivered.stderr)),
                stdout_capture,
                stderr_capture,
                stdout: bytes_to_string_smart(&undelivered.stdout),
                stderr: bytes_to_string_smart(&undelivered.stderr),
            },
        )
        .await;
}

fn spawn_windows_output_relay(
    events: async_channel::Receiver<codex_protocol::protocol::Event>,
    relay: OutputDeliveryRelay,
    process_id: String,
    output_bytes_cap: Option<usize>,
) -> tokio::task::JoinHandle<UndeliveredOutput> {
    tokio::spawn(async move {
        let capacity_changed = Arc::clone(&relay.capacity_changed);
        let mut relay = Some(relay);
        let mut pending = std::collections::VecDeque::<(CommandExecOutputStream, Vec<u8>, bool)>::new();
        let mut undelivered = UndeliveredOutput::default();
        let mut stdout_cap = OutputByteCap::new(output_bytes_cap);
        let mut stderr_cap = OutputByteCap::new(output_bytes_cap);
        let mut events_open = true;
        loop {
            let ready = Arc::clone(&capacity_changed).notified_owned();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some((stream, bytes, cap_reached)) = pending.front() {
                if let Some(delivery) = relay.as_ref() {
                    let accounted = bytes.len().div_ceil(3).saturating_mul(4)
                        .saturating_add(process_id.len()).saturating_add(OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES);
                    if delivery.tx.is_closed() || accounted > OUTPUT_DELIVERY_MAX_QUEUED_BYTES {
                        relay = None;
                        continue;
                    }
                    if delivery.tx.capacity() > 0 && delivery.byte_budget.available_permits() >= accounted
                        && delivery.enqueue(CommandExecOutputDeltaNotification {
                            process_id: process_id.clone(), stream: *stream,
                            delta_base64: STANDARD.encode(bytes), cap_reached: *cap_reached,
                        }, accounted).is_ok()
                    {
                        pending.pop_front();
                        continue;
                    }
                } else {
                    let Some((stream, bytes, cap_reached)) = pending.pop_front() else { continue; };
                    undelivered.append(stream, &bytes);
                    undelivered.note_cap(stream, cap_reached);
                    continue;
                }
            }
            if !events_open && pending.is_empty() { break; }
            tokio::select! {
                _ = &mut ready, if !pending.is_empty() => {},
                event = events.recv(), if events_open => {
                    let Ok(event) = event else { events_open = false; continue; };
                    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else { continue; };
                    let (stream, cap) = match delta.stream {
                        ExecOutputStream::Stdout => (CommandExecOutputStream::Stdout, &mut stdout_cap),
                        ExecOutputStream::Stderr => (CommandExecOutputStream::Stderr, &mut stderr_cap),
                    };
                    let (bytes, cap_reached) = cap.accept(&delta.chunk);
                    undelivered.note_cap(stream, cap_reached);
                    if bytes.is_empty() {
                        if cap_reached { pending.push_back((stream, Vec::new(), true)); }
                    } else {
                        for (index, chunk) in bytes.chunks(OUTPUT_CHUNK_SIZE_HINT).enumerate() {
                            pending.push_back((stream, chunk.to_vec(),
                                cap_reached && (index + 1) * OUTPUT_CHUNK_SIZE_HINT >= bytes.len()));
                        }
                    }
                }
            }
        }
        undelivered
    })
}

#[expect(
    clippy::expect_used,
    reason = "Queued deltas are encoded internally with the same base64 engine"
)]
fn spawn_output_delivery_relay(
    outgoing: Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    cancellation: CancellationToken,
) -> (
    OutputDeliveryRelay,
    tokio::task::JoinHandle<UndeliveredOutput>,
) {
    let (tx, mut rx) = mpsc::channel::<QueuedOutputDelivery>(OUTPUT_DELIVERY_QUEUE_ITEMS);
    let (settlement, mut settlement_rx) = watch::channel(None);
    let capacity_changed = Arc::new(tokio::sync::Notify::new());
    let relay = OutputDeliveryRelay {
        tx,
        byte_budget: Arc::new(Semaphore::new(OUTPUT_DELIVERY_MAX_QUEUED_BYTES)),
        capacity_changed: Arc::clone(&capacity_changed),
        settlement,
    };
    let handle = tokio::spawn(async move {
        let mut undelivered = UndeliveredOutput::default();
        let mut delivery_failed = false;
        while let Some(queued) = rx.recv().await {
            if !delivery_failed {
                let delivery = outgoing.send_server_notification_to_connection_bounded(
                        connection_id,
                        ServerNotification::CommandExecOutputDelta(queued.notification.clone()),
                        &cancellation,
                    );
                tokio::pin!(delivery);
                delivery_failed = loop {
                    let deadline = *settlement_rx.borrow_and_update();
                    tokio::select! {
                        biased;
                        _ = async {
                            if let Some(deadline) = deadline {
                                tokio::time::sleep_until(deadline).await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        } => break true,
                        delivered = &mut delivery => break !delivered,
                        changed = settlement_rx.changed(), if deadline.is_none() => {
                            if changed.is_err() {
                                // All producers are gone; queued work still has
                                // the ordinary per-send bound when no exit was signalled.
                                break !delivery.await;
                            }
                        }
                    }
                };
            }
            if delivery_failed {
                // Publish terminal failure through the existing channel owner.
                // Already-admitted chunks drain in order; later collectors keep
                // raw bytes instead of another Base64 queue round trip.
                rx.close();
                let bytes = STANDARD
                    .decode(&queued.notification.delta_base64)
                    .expect("command output base64 is generated internally");
                undelivered.append(queued.notification.stream, &bytes);
            }
            // After a delivery failure retain every later chunk in order. A
            // successful relay admission alone is not a delivered delta.
            drop(queued);
            capacity_changed.notify_waiters();
        }
        undelivered
    });
    (relay, handle)
}

impl OutputDeliveryRelay {
    fn begin_settlement(&self) {
        self.settlement.send_if_modified(|deadline| {
            if deadline.is_some() { return false; }
            *deadline = Some(tokio::time::Instant::now() + codex_app_server_rpc::outgoing_message::RESOURCE_DELIVERY_TIMEOUT);
            true
        });
    }

    fn enqueue(
        &self,
        notification: CommandExecOutputDeltaNotification,
        accounted_payload_bytes: usize,
    ) -> Result<(), ()> {
        if accounted_payload_bytes > OUTPUT_DELIVERY_MAX_QUEUED_BYTES {
            return Err(());
        }
        let accounted_payload_bytes: u32 = accounted_payload_bytes.try_into().map_err(|_| ())?;
        // Collectors spool on temporary congestion, never wait on transport
        // admission while they own pipe draining.
        let permit = Arc::clone(&self.byte_budget)
            .try_acquire_many_owned(accounted_payload_bytes)
            .map_err(|_| ())?;
        self.tx
            .try_send(QueuedOutputDelivery {
                notification,
                _byte_permit: permit,
            })
            .map_err(|_| ())
    }
}

fn accounted_output_delivery_bytes(delta_base64: &str, process_id: &str) -> usize {
    delta_base64
        .len()
        .saturating_add(process_id.len())
        .saturating_add(OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES)
}

pub(crate) fn coalesce_output_batch(
    mut chunk: Vec<u8>,
    output_rx: &mut mpsc::Receiver<Vec<u8>>,
    discard: bool,
) -> Vec<u8> {
    let mut batch_bytes = chunk.len();
    while batch_bytes < OUTPUT_CHUNK_SIZE_HINT && let Ok(next_chunk) = output_rx.try_recv() {
        batch_bytes = batch_bytes.saturating_add(next_chunk.len());
        if !discard {
            chunk.extend_from_slice(&next_chunk);
        }
    }
    chunk
}

fn spawn_process_output(
    params: SpawnProcessOutputParams,
) -> tokio::task::JoinHandle<CapturedOutput> {
    let SpawnProcessOutputParams {
        process_id,
        mut output_rx,
        mut stdio_timeout_rx,
        mut delivery_relay,
        stream,
        mut stream_output,
        output_bytes_cap,
    } = params;
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        // The spool shares the command's cumulative capture cap. Explicitly
        // uncapped commands retain their existing unlimited-capture policy.
        let mut delivered_prefix = 0_usize;
        let mut pending_cap_signal = false;
        let capacity_changed = delivery_relay.as_ref()
            .map(|relay| Arc::clone(&relay.capacity_changed)).unwrap_or_default();
        let mut cap = OutputByteCap::new(output_bytes_cap);
        let mut drain_timed_out = false;
        // Every loop exit decides whether the pipe reached EOF.
        let reached_eof;
        loop {
            if delivery_relay.as_ref().is_some_and(|relay| relay.tx.is_closed()) {
                delivery_relay = None;
                stream_output = false;
            }
            let capacity_ready = Arc::clone(&capacity_changed).notified_owned();
            tokio::pin!(capacity_ready);
            capacity_ready.as_mut().enable();
            if *stdio_timeout_rx.borrow() {
                reached_eof = output_rx.is_closed() && output_rx.is_empty();
                drain_timed_out = !reached_eof;
                break;
            }
            if stream_output && (delivered_prefix < buffer.len() || pending_cap_signal)
                && let (Some(relay), Some(process_id)) = (delivery_relay.as_ref(), process_id.as_ref())
            {
                let end = delivered_prefix.saturating_add(OUTPUT_CHUNK_SIZE_HINT).min(buffer.len());
                let encoded_len = (end - delivered_prefix).div_ceil(3).saturating_mul(4);
                let accounted = encoded_len.saturating_add(process_id.len())
                    .saturating_add(OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES);
                // Do not encode the same blocked suffix on every pipe read.
                if accounted <= relay.byte_budget.available_permits() && relay.tx.capacity() > 0 {
                    let delta_base64 = STANDARD.encode(&buffer[delivered_prefix..end]);
                    if relay.enqueue(CommandExecOutputDeltaNotification {
                        process_id: process_id.clone(), stream, delta_base64,
                        cap_reached: pending_cap_signal && end == buffer.len(),
                    }, accounted).is_ok() {
                        delivered_prefix = end;
                        if end == buffer.len() {
                            buffer.clear();
                            delivered_prefix = 0;
                            pending_cap_signal = false;
                        } else if delivered_prefix >= buffer.len() - delivered_prefix {
                            // Amortized compaction bounds consumed-prefix retention
                            // by the pending suffix rather than total process output.
                            buffer.drain(..delivered_prefix);
                            delivered_prefix = 0;
                        }
                        continue;
                    }
                }
                if accounted > OUTPUT_DELIVERY_MAX_QUEUED_BYTES || relay.byte_budget.is_closed() || relay.tx.is_closed() {
                    delivery_relay = None;
                    stream_output = false;
                }
            }
            let chunk = tokio::select! {
                _ = &mut capacity_ready, if stream_output && (delivered_prefix < buffer.len() || pending_cap_signal) => continue,
                chunk = output_rx.recv() => match chunk {
                    Some(chunk) => chunk,
                    None => { reached_eof = true; break; },
                },
                _ = stdio_timeout_rx.wait_for(|&v| v) => {
                    reached_eof = output_rx.is_closed() && output_rx.is_empty();
                    drain_timed_out = !reached_eof;
                    break;
                },
            };
            // Once excess is observed, drain a bounded batch without copying
            // bytes the cap can never retain. Equality alone is not truncation.
            let discard = cap.truncated();
            let chunk = coalesce_output_batch(chunk, &mut output_rx, discard);
            if discard { continue; }
            let (capped_chunk, cap_reached) = cap.accept(&chunk);
            if let (true, Some(process_id)) = (stream_output, process_id.as_ref()) {
                if capped_chunk.is_empty() && !cap_reached {
                    continue;
                }
                if delivered_prefix < buffer.len() || pending_cap_signal {
                    buffer.extend_from_slice(capped_chunk);
                    pending_cap_signal |= cap_reached;
                    continue;
                }
                let delta_base64 = STANDARD.encode(capped_chunk);
                let accounted_payload_bytes =
                    accounted_output_delivery_bytes(&delta_base64, process_id);
                if let Some(relay) = delivery_relay.as_ref()
                    && relay
                        .enqueue(
                            CommandExecOutputDeltaNotification {
                                process_id: process_id.clone(),
                                stream,
                                delta_base64,
                                cap_reached,
                            },
                            accounted_payload_bytes,
                        )
                        .is_err()
                {
                    buffer.extend_from_slice(capped_chunk);
                    pending_cap_signal |= cap_reached;
                    if accounted_payload_bytes > OUTPUT_DELIVERY_MAX_QUEUED_BYTES || relay.byte_budget.is_closed() || relay.tx.is_closed() {
                        delivery_relay = None;
                        stream_output = false;
                    }
                }
            } else if !stream_output {
                buffer.extend_from_slice(capped_chunk);
            }
        }
        if delivered_prefix > 0 {
            buffer.drain(..delivered_prefix);
        }
        CapturedOutput {
            bytes: buffer,
            cap_reached: cap.truncated(),
            drain_timed_out,
            reached_eof,
        }
    })
}

pub(crate) fn validate_stdin_delta_length(delta_base64: &str) -> Result<(), JSONRPCErrorError> {
    if delta_base64.len() > codex_utils_pty::MAX_STDIN_CHUNK_BYTES.div_ceil(3) * 4 {
        return Err(invalid_params("stdin chunk exceeds the 4 MiB limit"));
    }
    Ok(())
}

pub(crate) fn decode_stdin_delta(delta_base64: &str) -> Result<Vec<u8>, JSONRPCErrorError> {
    let max_bytes = codex_utils_pty::MAX_STDIN_CHUNK_BYTES;
    validate_stdin_delta_length(delta_base64)?;
    let delta = STANDARD
        .decode(delta_base64)
        .map_err(|err| invalid_params(format!("invalid deltaBase64: {err}")))?;
    if delta.len() > max_bytes {
        return Err(invalid_params("stdin chunk exceeds the 4 MiB limit"));
    }
    Ok(delta)
}

pub(crate) fn spawn_stdin_writer(
    session: Arc<ProcessHandle>,
    mut write_rx: mpsc::Receiver<StdinWriteRequest>,
    stream_stdin: bool,
    streaming_disabled_message: &'static str,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(StdinWriteRequest {
            delta,
            close_stdin,
            response_tx,
        }) = write_rx.recv().await
        {
            let result = handle_process_write(
                &session,
                stream_stdin,
                delta,
                close_stdin,
                streaming_disabled_message,
            )
            .await;
            if let Some(response_tx) = response_tx {
                let _ = response_tx.send(result);
            }
        }
    })
}

async fn handle_process_write(
    session: &ProcessHandle,
    stream_stdin: bool,
    delta: Vec<u8>,
    close_stdin: bool,
    streaming_disabled_message: &'static str,
) -> Result<(), JSONRPCErrorError> {
    if !stream_stdin {
        return Err(invalid_request(streaming_disabled_message));
    }
    if !delta.is_empty() {
        session
            .writer_sender()
            .send(delta)
            .await
            .map_err(|_| invalid_request("stdin is already closed"))?;
    }
    if close_stdin {
        session
            .close_stdin_ordered()
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
    }
    Ok(())
}

fn handle_process_resize(
    session: &ProcessHandle,
    size: TerminalSize,
) -> Result<(), JSONRPCErrorError> {
    session
        .resize(size)
        .map_err(|err| invalid_request(format!("failed to resize PTY: {err}")))
}

pub(crate) fn terminal_size_from_protocol(
    size: PtyTerminalSize,
    request_name: &str,
) -> Result<TerminalSize, JSONRPCErrorError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(invalid_params(format!(
            "{request_name} size rows and cols must be greater than 0"
        )));
    }
    Ok(TerminalSize {
        rows: size.rows,
        cols: size.cols,
    })
}

fn command_no_longer_running_error(process_id: &InternalProcessId) -> JSONRPCErrorError {
    invalid_request(format!(
        "command/exec {} is no longer running",
        process_id.error_repr(),
    ))
}

