use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol::CommandExecOutputDeltaNotification;
use codex_app_server_protocol::CommandExecOutputStream;
use codex_app_server_protocol::CommandExecResizeParams;
use codex_app_server_protocol::CommandExecResizeResponse;
use codex_app_server_protocol::CommandExecResponse;
use codex_app_server_protocol::CommandExecTerminateParams;
use codex_app_server_protocol::CommandExecTerminateResponse;
use codex_app_server_protocol::CommandExecWriteParams;
use codex_app_server_protocol::CommandExecWriteResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::PtyTerminalSize;
use codex_app_server_protocol::ServerNotification;
use codex_core::config::StartedNetworkProxy;
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

use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::error_code::invalid_request;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;

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

#[derive(Clone)]
pub(crate) struct CommandExecManager {
    sessions: Arc<Mutex<HashMap<ConnectionProcessId, CommandExecSession>>>,
    next_generated_process_id: Arc<AtomicI64>,
}

impl Default for CommandExecManager {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_generated_process_id: Arc::new(AtomicI64::new(1)),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionProcessId {
    connection_id: ConnectionId,
    process_id: InternalProcessId,
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
    pub(crate) started_network_proxy: Option<StartedNetworkProxy>,
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
    terminal_cleanup: Option<CommandTerminalCleanup>,
}

struct CommandTerminalCleanup {
    sessions: Arc<Mutex<HashMap<ConnectionProcessId, CommandExecSession>>>,
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

#[derive(Clone)]
struct OutputDeliveryRelay {
    tx: mpsc::Sender<QueuedOutputDelivery>,
    byte_budget: Arc<Semaphore>,
}

struct QueuedOutputDelivery {
    notification: CommandExecOutputDeltaNotification,
    _byte_permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct UndeliveredOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl UndeliveredOutput {
    fn append(&mut self, stream: CommandExecOutputStream, bytes: &[u8]) {
        match stream {
            CommandExecOutputStream::Stdout => self.stdout.extend_from_slice(bytes),
            CommandExecOutputStream::Stderr => self.stderr.extend_from_slice(bytes),
        }
    }

    fn append_tail(&mut self, tail: Self) {
        self.stdout.extend(tail.stdout);
        self.stderr.extend(tail.stderr);
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
            started_network_proxy,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            size,
        } = params;
        let connection_cancellation = rpc_gate.cancellation_token().child_token();
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
                let _started_network_proxy = started_network_proxy;
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
                let (stdout_stream, event_relay_handle) =
                    match (delivery_relay.clone(), notification_process_id) {
                        (Some(delivery_relay), Some(process_id)) => {
                            let (tx_event, rx_event) =
                                async_channel::bounded::<codex_protocol::protocol::Event>(1);
                            let handle = tokio::spawn(async move {
                                let mut delivery_relay = Some(delivery_relay);
                                let mut undelivered = UndeliveredOutput::default();
                                let mut stdout_cap = OutputByteCap::new(output_bytes_cap);
                                let mut stderr_cap = OutputByteCap::new(output_bytes_cap);
                                while let Ok(event) = rx_event.recv().await {
                                    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
                                        continue;
                                    };
                                    let (stream, cap) = match delta.stream {
                                        ExecOutputStream::Stdout => {
                                            (CommandExecOutputStream::Stdout, &mut stdout_cap)
                                        }
                                        ExecOutputStream::Stderr => {
                                            (CommandExecOutputStream::Stderr, &mut stderr_cap)
                                        }
                                    };
                                    let (capped_chunk, cap_reached) = cap.accept(&delta.chunk);
                                    if capped_chunk.is_empty() && !cap_reached {
                                        continue;
                                    }
                                    let delta_base64 = STANDARD.encode(capped_chunk);
                                    let accounted_payload_bytes =
                                        accounted_output_delivery_bytes(&delta_base64, &process_id);
                                    let queued = if let Some(relay) = delivery_relay.as_ref() {
                                        relay
                                            .enqueue(
                                                CommandExecOutputDeltaNotification {
                                                    process_id: process_id.clone(),
                                                    stream,
                                                    delta_base64,
                                                    cap_reached,
                                                },
                                                accounted_payload_bytes,
                                            )
                                            .is_ok()
                                    } else {
                                        false
                                    };
                                    if !queued {
                                        delivery_relay = None;
                                        undelivered.append(stream, capped_chunk);
                                    }
                                }
                                undelivered
                            });
                            (
                                Some(StdoutStream::without_progress(
                                    String::new(),
                                    String::new(),
                                    tx_event,
                                )),
                                Some(handle),
                            )
                        }
                        _ => (None, None),
                    };
                let output = codex_core::sandboxing::execute_env(exec_request, stdout_stream).await;
                let event_fallback = if let Some(handle) = event_relay_handle {
                    handle.await.unwrap_or_default()
                } else {
                    UndeliveredOutput::default()
                };
                drop(delivery_relay);
                let mut undelivered = if let Some(handle) = delivery_handle {
                    handle.await.unwrap_or_default()
                } else {
                    UndeliveredOutput::default()
                };
                undelivered.append_tail(event_fallback);
                sessions.lock().await.remove(&process_key);
                match output {
                    Ok(output) => {
                        outgoing
                            .send_response(
                                request_id,
                                CommandExecResponse {
                                    exit_code: output.exit_code,
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
        let Some((program, args)) = command.split_first() else {
            return Err(internal_error("validated command unexpectedly empty"));
        };
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
                self.sessions.lock().await.remove(&process_key);
                return Err(internal_error(format!("failed to spawn command: {err}")));
            }
        };
        tokio::spawn(async move {
            let _started_network_proxy = started_network_proxy;
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
                terminal_cleanup: Some(CommandTerminalCleanup {
                    sessions,
                    process_key,
                }),
            })
            .await;
        });
        Ok(())
    }

    #[cfg(test)]
    async fn start(&self, params: StartCommandExecParams) -> Result<(), JSONRPCErrorError> {
        self.start_with_gate(params, &ConnectionRpcGate::new())
            .await
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

        let delta = match params.delta_base64 {
            Some(delta_base64) => STANDARD
                .decode(delta_base64)
                .map_err(|err| invalid_params(format!("invalid deltaBase64: {err}")))?,
            None => Vec::new(),
        };

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
        let cancellation = rpc_gate.cancellation_token();
        // Admission stays in the same FIFO lane as command start and other writes.
        // Transfer the acknowledgement to the connection's owner under its close
        // fence, so blocked stdin cannot keep termination queued behind this RPC.
        drop(rpc_gate.spawn_with_commit(|| {
            let busy = || invalid_request("command/exec stdin write queue is full; retry after a pending write completes");
            let slot = write_slots.try_acquire_owned().map_err(|_| busy())?;
            let (response_tx, response_rx) = oneshot::channel();
            write_tx.try_send(StdinWriteRequest {
                delta,
                close_stdin: params.close_stdin,
                response_tx: Some(response_tx),
            }).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => busy(),
                mpsc::error::TrySendError::Closed(_) => command_no_longer_running_error(&target_process_id.process_id),
            })?;
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
            let mut sessions = self.sessions.lock().await;
            let process_ids = sessions
                .keys()
                .filter(|process_id| process_id.connection_id == connection_id)
                .cloned()
                .collect::<Vec<_>>();
            let mut controls = Vec::with_capacity(process_ids.len());
            for process_id in process_ids {
                if let Some(control) = sessions.remove(&process_id) {
                    controls.push(control);
                }
            }
            controls
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
    let expiration = expiration.wait_with_outcome();
    tokio::pin!(expiration);
    let SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    } = spawned;
    let session = Arc::new(session);
    tokio::pin!(exit_rx);
    let mut expiration_outcome = None;
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
            outcome = &mut expiration, if expiration_outcome.is_none() => {
                expiration_outcome = Some(outcome);
                let _ = session.request_terminate();
            }
            exit = &mut exit_rx => {
                if matches!(expiration_outcome, Some(ExecExpirationOutcome::TimedOut)) {
                    break EXEC_TIMEOUT_EXIT_CODE;
                } else {
                    break exit.unwrap_or(-1);
                }
            }
        }
    };
    stdin_writer_handle.abort();
    let _ = stdin_writer_handle.await;

    let timeout_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(IO_DRAIN_TIMEOUT_MS)).await;
        let _ = stdio_timeout_tx.send(true);
    });

    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    timeout_handle.abort();
    drop(delivery_relay);
    let mut undelivered = if let Some(delivery_handle) = delivery_handle {
        delivery_handle.await.unwrap_or_default()
    } else {
        UndeliveredOutput::default()
    };
    // Relay failures precede any chunks rejected by relay admission. Decode the
    // entire suffix once, including UTF-8 codepoints split across those chunks.
    undelivered.append_tail(UndeliveredOutput { stdout, stderr });
    if let Some(cleanup) = terminal_cleanup {
        cleanup.sessions.lock().await.remove(&cleanup.process_key);
    }

    outgoing
        .send_response(
            request_id,
            CommandExecResponse {
                exit_code,
                stdout: bytes_to_string_smart(&undelivered.stdout),
                stderr: bytes_to_string_smart(&undelivered.stderr),
            },
        )
        .await;
}

fn spawn_output_delivery_relay(
    outgoing: Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    cancellation: CancellationToken,
) -> (
    OutputDeliveryRelay,
    tokio::task::JoinHandle<UndeliveredOutput>,
) {
    let (tx, mut rx) = mpsc::channel::<QueuedOutputDelivery>(OUTPUT_DELIVERY_QUEUE_ITEMS);
    let relay = OutputDeliveryRelay {
        tx,
        byte_budget: Arc::new(Semaphore::new(OUTPUT_DELIVERY_MAX_QUEUED_BYTES)),
    };
    let handle = tokio::spawn(async move {
        let mut undelivered = UndeliveredOutput::default();
        let mut delivery_failed = false;
        while let Some(queued) = rx.recv().await {
            if !delivery_failed {
                delivery_failed = !outgoing
                    .send_server_notification_to_connection_bounded(
                        connection_id,
                        ServerNotification::CommandExecOutputDelta(queued.notification.clone()),
                        &cancellation,
                    )
                    .await;
            }
            if delivery_failed {
                let bytes = STANDARD
                    .decode(&queued.notification.delta_base64)
                    .expect("command output base64 is generated internally");
                undelivered.append(queued.notification.stream, &bytes);
            }
            // After a delivery failure retain every later chunk in order. A
            // successful relay admission alone is not a delivered delta.
        }
        undelivered
    });
    (relay, handle)
}

impl OutputDeliveryRelay {
    fn enqueue(
        &self,
        notification: CommandExecOutputDeltaNotification,
        accounted_payload_bytes: usize,
    ) -> Result<(), ()> {
        if accounted_payload_bytes > OUTPUT_DELIVERY_MAX_QUEUED_BYTES {
            return Err(());
        }
        let accounted_payload_bytes: u32 = accounted_payload_bytes.try_into().map_err(|_| ())?;
        // A full delivery relay switches the collector to final-response capture.
        // Waiting here would let transport backpressure consume its I/O drain grace.
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

fn spawn_process_output(params: SpawnProcessOutputParams) -> tokio::task::JoinHandle<Vec<u8>> {
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
        let mut cap = OutputByteCap::new(output_bytes_cap);
        loop {
            let mut chunk = tokio::select! {
                chunk = output_rx.recv() => match chunk {
                    Some(chunk) => chunk,
                    None => break,
                },
                _ = stdio_timeout_rx.wait_for(|&v| v) => break,
            };
            // Individual chunks are at most 8KiB, so overshooting a bit is acceptable.
            while chunk.len() < OUTPUT_CHUNK_SIZE_HINT
                && let Ok(next_chunk) = output_rx.try_recv()
            {
                chunk.extend_from_slice(&next_chunk);
            }
            let (capped_chunk, cap_reached) = cap.accept(&chunk);
            if let (true, Some(process_id)) = (stream_output, process_id.as_ref()) {
                if capped_chunk.is_empty() && !cap_reached {
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
                    delivery_relay = None;
                    stream_output = false;
                    buffer.extend_from_slice(capped_chunk);
                }
            } else if !stream_output {
                buffer.extend_from_slice(capped_chunk);
            }
        }
        buffer
    })
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
        session.close_stdin();
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::error_code::INVALID_REQUEST_ERROR_CODE;
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_protocol::models::PermissionProfile;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tokio::time::Duration;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use codex_utils_pty::ProcessDriver;
    use codex_utils_pty::spawn_from_driver;

    #[test]
    fn output_byte_cap_requires_observed_excess() {
        let mut cap = OutputByteCap::new(Some(3));
        assert_eq!(cap.accept(b"abc"), (&b"abc"[..], false));
        assert!(!cap.truncated());

        assert_eq!(cap.accept(b"d"), (&b""[..], true));
        assert!(cap.truncated());
        assert_eq!(cap.accept(b"e"), (&b""[..], false));
    }

    #[tokio::test]
    async fn output_delivery_relay_finishes_without_writer_ack_and_preserves_fifo() {
        let connection_id = ConnectionId(31);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let (relay, delivery_handle) =
            spawn_output_delivery_relay(outgoing, connection_id, CancellationToken::new());

        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            let delta_base64 = STANDARD.encode(bytes);
            relay
                .enqueue(
                    CommandExecOutputDeltaNotification {
                        process_id: "fifo".to_string(),
                        stream: CommandExecOutputStream::Stdout,
                        delta_base64: delta_base64.clone(),
                        cap_reached: false,
                    },
                    delta_base64.len(),
                )
                .expect("queue output delivery");
        }
        drop(relay);

        let undelivered = timeout(Duration::from_secs(1), delivery_handle)
            .await
            .expect("delivery relay should not wait for writer acknowledgement")
            .expect("delivery relay task should not panic");
        assert!(
            undelivered.stdout.is_empty(),
            "delivered stdout is not repeated in fallback"
        );
        assert!(undelivered.stderr.is_empty());

        let mut delivered = Vec::new();
        for _ in 0..2 {
            let envelope = outgoing_rx.recv().await.expect("queued notification");
            let OutgoingEnvelope::ToConnection {
                connection_id: delivered_connection_id,
                message:
                    OutgoingMessage::AppServerNotification(ServerNotification::CommandExecOutputDelta(
                        notification,
                    )),
                write_complete_tx,
            } = envelope
            else {
                panic!("expected targeted command output notification");
            };
            assert_eq!(delivered_connection_id, connection_id);
            assert!(write_complete_tx.is_none());
            delivered.push(
                STANDARD
                    .decode(notification.delta_base64)
                    .expect("valid output base64"),
            );
        }
        assert_eq!(delivered, [b"first".to_vec(), b"second".to_vec()]);
    }

    #[tokio::test]
    async fn output_fallback_joins_queued_and_rejected_chunks_before_utf8_conversion() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (relay, delivery_handle) =
            spawn_output_delivery_relay(outgoing, ConnectionId(31), cancellation);
        // The first one-byte delta fits the real relay budget; the next delta
        // does not. Their concatenation is one UTF-8 character, split between
        // relay-owned undelivered bytes and the collector's local fallback.
        let process_id =
            "p".repeat(OUTPUT_DELIVERY_MAX_QUEUED_BYTES - OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES - 4);
        let (output_tx, output_rx) = mpsc::channel(1);
        let (_stdio_tx, stdio_timeout_rx) = watch::channel(false);
        let collector = spawn_process_output(SpawnProcessOutputParams {
            process_id: Some(process_id),
            output_rx,
            stdio_timeout_rx,
            delivery_relay: Some(relay.clone()),
            stream: CommandExecOutputStream::Stdout,
            stream_output: true,
            output_bytes_cap: Some(5),
        });
        output_tx.send(vec![0xe2]).await.expect("first fragment");
        // With a one-item pipe, this reservation waits until the collector has
        // consumed the first fragment and completed its nonblocking admission.
        output_tx
            .reserve()
            .await
            .expect("next pipe slot")
            .send(vec![0x82, 0xac, b'!', b'?', b'x']);
        drop(output_tx);
        let local_tail = collector
            .await
            .expect("collector finishes without delivery");
        assert_eq!(local_tail, vec![0x82, 0xac, b'!', b'?']);
        drop(relay);
        let mut undelivered = delivery_handle.await.expect("relay finishes");
        assert_eq!(undelivered.stdout, vec![0xe2]);
        undelivered.append_tail(UndeliveredOutput {
            stdout: local_tail,
            stderr: Vec::new(),
        });
        assert_eq!(bytes_to_string_smart(&undelivered.stdout), "€!?");
        assert!(undelivered.stderr.is_empty());
    }

    fn windows_sandbox_exec_request() -> ExecRequest {
        let cwd = AbsolutePathBuf::current_dir().expect("current dir");
        ExecRequest::new(
            vec![
                "cmd".to_string(),
                "/c".to_string(),
                "exit".to_string(),
                "0".to_string(),
            ],
            cwd.clone(),
            cwd.clone(),
            HashMap::new(),
            /*network*/ None,
            /*network_environment_id*/ None,
            ExecExpiration::DefaultTimeout,
            codex_core::exec::ExecCapturePolicy::ShellTool,
            SandboxType::WindowsRestrictedToken,
            vec![cwd],
            WindowsSandboxLevel::Disabled,
            /*windows_sandbox_private_desktop*/ false,
            PermissionProfile::read_only(),
            /*arg0*/ None,
        )
    }

    #[test]
    fn command_argv_validation_preserves_the_rpc_contract() {
        let error = validate_command_argv(&[]).expect_err("empty argv should be rejected");
        assert_eq!(error.code, INVALID_REQUEST_ERROR_CODE);
        assert_eq!(error.message, "command must not be empty");
        assert!(validate_command_argv(&["codex".to_string()]).is_ok());
    }

    #[test]
    fn terminal_size_conversion_rejects_zero_dimensions() {
        let size = PtyTerminalSize {
            rows: 40,
            cols: 120,
        };
        let converted =
            terminal_size_from_protocol(size, "command/exec").expect("valid terminal size");
        assert_eq!(converted.rows, 40);
        assert_eq!(converted.cols, 120);

        let invalid = PtyTerminalSize { rows: 0, cols: 1 };
        let error = terminal_size_from_protocol(invalid, "process")
            .expect_err("zero rows should be rejected");
        assert_eq!(
            error.message,
            "process size rows and cols must be greater than 0"
        );
    }

    #[tokio::test]
    async fn windows_sandbox_streaming_exec_failure_is_delivered_and_releases_process_id() {
        let (tx, mut rx) = mpsc::channel(1);
        let temp = tempfile::tempdir().expect("temporary execution directory");
        let missing_cwd = temp.path().join("missing-working-directory");
        let mut exec_request = windows_sandbox_exec_request();
        exec_request.cwd = AbsolutePathBuf::try_from(missing_cwd.clone())
            .expect("absolute missing cwd")
            .into();
        let manager = CommandExecManager::default();
        manager
            .start(StartCommandExecParams {
                outgoing: Arc::new(OutgoingMessageSender::new(
                    tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                )),
                request_id: ConnectionRequestId {
                    connection_id: ConnectionId(1),
                    request_id: codex_app_server_protocol::RequestId::Integer(42),
                },
                process_id: Some("proc-42".to_string()),
                exec_request,
                started_network_proxy: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: true,
                output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
                size: None,
            })
            .await
            .expect("streaming sandbox request is admitted before execution");
        let envelope = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("failed execution must produce a terminal RPC reply")
            .expect("outgoing response channel remains open");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            panic!("execution failure must target the requesting connection");
        };
        assert_eq!(connection_id, ConnectionId(1));
        let OutgoingMessage::Error(error) = message else {
            panic!("an invalid execution cwd must not report success");
        };
        assert_eq!(error.id, codex_app_server_protocol::RequestId::Integer(42));
        assert_eq!(error.error.code, crate::error_code::INTERNAL_ERROR_CODE);
        assert!(error.error.message.starts_with("exec failed: "));
        assert!(
            manager.sessions.lock().await.is_empty(),
            "terminal failure must release the accepted process ID before its reply"
        );
        assert!(
            !missing_cwd.exists(),
            "failed execution must not create the requested cwd"
        );
    }

    #[tokio::test]
    async fn windows_sandbox_exec_inherits_connection_cancellation() {
        let connection_cancellation = CancellationToken::new();
        let mut request = windows_sandbox_exec_request();
        attach_connection_cancellation(&mut request, connection_cancellation.clone());

        connection_cancellation.cancel();

        assert_eq!(
            request.expiration.wait_with_outcome().await,
            ExecExpirationOutcome::Cancelled
        );
    }

    #[tokio::test]
    async fn backpressured_stdin_does_not_block_termination_control() {
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        writer_tx
            .try_send(vec![b'x'])
            .expect("pre-fill driver stdin queue");
        let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(1);
        let (stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        drop(stdout_tx);
        drop(stderr_tx);
        let (exit_tx, exit_rx) = oneshot::channel();
        let terminated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let terminator_flag = Arc::clone(&terminated);
        let spawned = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_rx.into(),
            stderr_rx: Some(stderr_rx.into()),
            exit_rx,
            terminator: Some(Box::new(move || {
                terminator_flag.store(true, Ordering::SeqCst);
                Ok(())
            })),
            writer_handle: None,
            resizer: None,
        });
        let (control_tx, control_rx) = mpsc::channel(2);
        let (write_tx, write_rx) = mpsc::channel(2);
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
        let run_handle = tokio::spawn(run_command(RunCommandParams {
            outgoing: Arc::new(OutgoingMessageSender::new(
                outgoing_tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            request_id: ConnectionRequestId {
                connection_id: ConnectionId(21),
                request_id: codex_app_server_protocol::RequestId::Integer(21),
            },
            process_id: Some("backpressured".to_string()),
            spawned,
            control_rx,
            write_rx,
            stream_stdin: true,
            stream_stdout_stderr: false,
            expiration: ExecExpiration::Cancellation(CancellationToken::new()),
            output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
            connection_cancellation: CancellationToken::new(),
            terminal_cleanup: None,
        }));

        let (write_response_tx, mut write_response_rx) = oneshot::channel();
        write_tx
            .send(StdinWriteRequest {
                delta: vec![b'y'],
                close_stdin: false,
                response_tx: Some(write_response_tx),
            })
            .await
            .expect("queue write control");
        let (terminate_response_tx, terminate_response_rx) = oneshot::channel();
        control_tx
            .send(CommandControlRequest {
                control: CommandControl::Terminate,
                response_tx: Some(terminate_response_tx),
            })
            .await
            .expect("queue terminate control");

        assert!(
            timeout(Duration::from_millis(100), &mut write_response_rx)
                .await
                .is_err(),
            "backpressured write should remain pending",
        );
        timeout(Duration::from_secs(1), terminate_response_rx)
            .await
            .expect("terminate response timed out")
            .expect("terminate response sender dropped")
            .expect("terminate should succeed");
        assert!(terminated.load(Ordering::SeqCst));

        assert_eq!(writer_rx.recv().await, Some(vec![b'x']));
        timeout(Duration::from_secs(1), write_response_rx)
            .await
            .expect("write should complete after backpressure clears")
            .expect("write response sender dropped")
            .expect("backpressured write should eventually succeed");
        assert_eq!(writer_rx.recv().await, Some(vec![b'y']));

        exit_tx.send(1).expect("publish driver exit");
        timeout(Duration::from_secs(1), run_handle)
            .await
            .expect("run command did not finish")
            .expect("run command task panicked");
    }

    #[tokio::test]
    async fn windows_sandbox_process_ids_reject_write_requests() {
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(11),
            request_id: codex_app_server_protocol::RequestId::Integer(1),
        };
        let process_id = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: InternalProcessId::Client("proc-11".to_string()),
        };
        manager
            .sessions
            .lock()
            .await
            .insert(process_id, CommandExecSession::UnsupportedWindowsSandbox);

        let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
        let err = manager
            .write_with_gate(
                Arc::new(OutgoingMessageSender::new(
                    outgoing_tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                )),
                request_id,
                CommandExecWriteParams {
                    process_id: "proc-11".to_string(),
                    delta_base64: Some(STANDARD.encode("hello")),
                    close_stdin: false,
                },
                &ConnectionRpcGate::new(),
            )
            .await
            .expect_err("windows sandbox process ids should reject command/exec/write");

        assert_eq!(err.code, INVALID_REQUEST_ERROR_CODE);
        assert_eq!(
            err.message,
            "command/exec/write, command/exec/terminate, and command/exec/resize are not supported for windows sandbox processes"
        );
    }

    #[tokio::test]
    async fn windows_sandbox_process_ids_reject_terminate_requests() {
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(12),
            request_id: codex_app_server_protocol::RequestId::Integer(2),
        };
        let process_id = ConnectionProcessId {
            connection_id: request_id.connection_id,
            process_id: InternalProcessId::Client("proc-12".to_string()),
        };
        manager
            .sessions
            .lock()
            .await
            .insert(process_id, CommandExecSession::UnsupportedWindowsSandbox);

        let err = manager
            .terminate(
                request_id,
                CommandExecTerminateParams {
                    process_id: "proc-12".to_string(),
                },
            )
            .await
            .expect_err("windows sandbox process ids should reject command/exec/terminate");

        assert_eq!(err.code, INVALID_REQUEST_ERROR_CODE);
        assert_eq!(
            err.message,
            "command/exec/write, command/exec/terminate, and command/exec/resize are not supported for windows sandbox processes"
        );
    }

    #[tokio::test]
    async fn dropped_control_request_is_reported_as_not_running() {
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(13),
            request_id: codex_app_server_protocol::RequestId::Integer(3),
        };
        let process_id = InternalProcessId::Client("proc-13".to_string());
        let (control_tx, mut control_rx) = mpsc::channel(1);
        let (write_tx, _write_rx) = mpsc::channel(1);
        manager.sessions.lock().await.insert(
            ConnectionProcessId {
                connection_id: request_id.connection_id,
                process_id: process_id.clone(),
            },
            CommandExecSession::Active {
                control_tx,
                write_tx,
                write_slots: Arc::new(Semaphore::new(MAX_PENDING_STDIN_WRITES)),
            },
        );

        tokio::spawn(async move {
            let _request = control_rx
                .recv()
                .await
                .expect("expected queued control request");
        });

        let err = manager
            .terminate(
                request_id,
                CommandExecTerminateParams {
                    process_id: "proc-13".to_string(),
                },
            )
            .await
            .expect_err("dropped control request should be treated as not running");

        assert_eq!(err.code, INVALID_REQUEST_ERROR_CODE);
        assert_eq!(err.message, "command/exec \"proc-13\" is no longer running");
    }
}
