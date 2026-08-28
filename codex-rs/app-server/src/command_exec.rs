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
    },
    UnsupportedWindowsSandbox,
}

enum CommandControl {
    Write { delta: Vec<u8>, close_stdin: bool },
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
    notification: ServerNotification,
    _byte_permit: OwnedSemaphorePermit,
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
                                    if delivery_relay
                                        .enqueue(
                                            ServerNotification::CommandExecOutputDelta(
                                                CommandExecOutputDeltaNotification {
                                                    process_id: process_id.clone(),
                                                    stream,
                                                    delta_base64,
                                                    cap_reached,
                                                },
                                            ),
                                            accounted_payload_bytes,
                                        )
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
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
                if let Some(handle) = event_relay_handle {
                    let _ = handle.await;
                }
                drop(delivery_relay);
                if let Some(handle) = delivery_handle {
                    let _ = handle.await;
                }
                sessions.lock().await.remove(&process_key);
                match output {
                    Ok(output) => {
                        outgoing
                            .send_response(
                                request_id,
                                CommandExecResponse {
                                    exit_code: output.exit_code,
                                    stdout: final_response_output(
                                        stream_stdout_stderr,
                                        output.stdout.text,
                                    ),
                                    stderr: final_response_output(
                                        stream_stdout_stderr,
                                        output.stderr.text,
                                    ),
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
        let (write_tx, write_rx) = mpsc::channel(32);
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

    pub(crate) async fn write(
        &self,
        request_id: ConnectionRequestId,
        params: CommandExecWriteParams,
    ) -> Result<CommandExecWriteResponse, JSONRPCErrorError> {
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
        self.send_control(
            target_process_id,
            CommandControl::Write {
                delta,
                close_stdin: params.close_stdin,
            },
        )
        .await?;

        Ok(CommandExecWriteResponse {})
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
        let CommandExecSession::Active {
            control_tx,
            write_tx,
        } = session
        else {
            return Err(invalid_request(
                "command/exec/write, command/exec/terminate, and command/exec/resize are not supported for windows sandbox processes",
            ));
        };
        let (response_tx, response_rx) = oneshot::channel();
        let send_result = match control {
            CommandControl::Write { delta, close_stdin } => write_tx
                .send(StdinWriteRequest {
                    delta,
                    close_stdin,
                    response_tx: Some(response_tx),
                })
                .await
                .map_err(|_| ()),
            control => control_tx
                .send(CommandControlRequest {
                    control,
                    response_tx: Some(response_tx),
                })
                .await
                .map_err(|_| ()),
        };
        send_result.map_err(|_| command_no_longer_running_error(&process_id.process_id))?;
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
                            CommandControl::Write { .. } => Err(internal_error(
                                "stdin write was routed to the command control queue",
                            )),
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
    if let Some(delivery_handle) = delivery_handle {
        let _ = delivery_handle.await;
    }
    if let Some(cleanup) = terminal_cleanup {
        cleanup.sessions.lock().await.remove(&cleanup.process_key);
    }

    outgoing
        .send_response(
            request_id,
            CommandExecResponse {
                exit_code,
                stdout,
                stderr,
            },
        )
        .await;
}

fn spawn_output_delivery_relay(
    outgoing: Arc<OutgoingMessageSender>,
    connection_id: ConnectionId,
    cancellation: CancellationToken,
) -> (OutputDeliveryRelay, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<QueuedOutputDelivery>(OUTPUT_DELIVERY_QUEUE_ITEMS);
    let relay = OutputDeliveryRelay {
        tx,
        byte_budget: Arc::new(Semaphore::new(OUTPUT_DELIVERY_MAX_QUEUED_BYTES)),
    };
    let handle = tokio::spawn(async move {
        while let Some(queued) = rx.recv().await {
            if !outgoing
                .send_server_notification_to_connection_bounded(
                    connection_id,
                    queued.notification,
                    &cancellation,
                )
                .await
            {
                break;
            }
        }
    });
    (relay, handle)
}

impl OutputDeliveryRelay {
    async fn enqueue(
        &self,
        notification: ServerNotification,
        accounted_payload_bytes: usize,
    ) -> Result<(), ()> {
        if accounted_payload_bytes > OUTPUT_DELIVERY_MAX_QUEUED_BYTES {
            return Err(());
        }
        let accounted_payload_bytes: u32 = accounted_payload_bytes.try_into().map_err(|_| ())?;
        let permit = Arc::clone(&self.byte_budget)
            .acquire_many_owned(accounted_payload_bytes)
            .await
            .map_err(|_| ())?;
        self.tx
            .send(QueuedOutputDelivery {
                notification,
                _byte_permit: permit,
            })
            .await
            .map_err(|_| ())
    }
}

fn accounted_output_delivery_bytes(delta_base64: &str, process_id: &str) -> usize {
    delta_base64
        .len()
        .saturating_add(process_id.len())
        .saturating_add(OUTPUT_DELIVERY_EVENT_OVERHEAD_BYTES)
}

fn final_response_output(streamed: bool, output: String) -> String {
    if streamed { String::new() } else { output }
}

fn spawn_process_output(params: SpawnProcessOutputParams) -> tokio::task::JoinHandle<String> {
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
                            ServerNotification::CommandExecOutputDelta(
                                CommandExecOutputDeltaNotification {
                                    process_id: process_id.clone(),
                                    stream,
                                    delta_base64,
                                    cap_reached,
                                },
                            ),
                            accounted_payload_bytes,
                        )
                        .await
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
        bytes_to_string_smart(&buffer)
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

    #[test]
    fn streamed_windows_output_is_not_repeated_in_final_response() {
        assert_eq!(
            final_response_output(true, "already streamed".to_string()),
            ""
        );
        assert_eq!(
            final_response_output(false, "not streamed".to_string()),
            "not streamed"
        );
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

        for delta_base64 in ["first", "second"] {
            relay
                .enqueue(
                    ServerNotification::CommandExecOutputDelta(
                        CommandExecOutputDeltaNotification {
                            process_id: "fifo".to_string(),
                            stream: CommandExecOutputStream::Stdout,
                            delta_base64: delta_base64.to_string(),
                            cap_reached: false,
                        },
                    ),
                    delta_base64.len(),
                )
                .await
                .expect("queue output delivery");
        }
        drop(relay);

        timeout(Duration::from_secs(1), delivery_handle)
            .await
            .expect("delivery relay should not wait for writer acknowledgement")
            .expect("delivery relay task should not panic");

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
            delivered.push(notification.delta_base64);
        }
        assert_eq!(delivered, ["first", "second"]);
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
    fn command_argv_validation_is_centralized_in_request_processors() {
        for source in [
            include_str!("request_processors/command_exec_processor.rs"),
            include_str!("request_processors/process_exec_processor.rs"),
        ] {
            assert_eq!(source.matches("validate_command_argv(").count(), 1);
            assert!(!source.contains("command must not be empty"));
        }

        let command_exec_source = include_str!("command_exec.rs");
        let process_exec_source = include_str!("request_processors/process_exec_processor.rs");
        assert!(!command_exec_source.contains(&["trait InternalProcessId", "Ext"].concat()));
        assert!(!process_exec_source.contains(&["struct ProcessExec", "Manager"].concat()));
    }

    #[test]
    fn terminal_size_validation_is_shared_by_process_apis() {
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
        assert!(
            !include_str!("request_processors/process_exec_processor.rs")
                .contains("fn terminal_size_from_protocol")
        );
    }

    #[tokio::test]
    async fn windows_sandbox_streaming_exec_uses_execution_path() {
        let (tx, _rx) = mpsc::channel(1);
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
                exec_request: windows_sandbox_exec_request(),
                started_network_proxy: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: true,
                output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
                size: None,
            })
            .await
            .expect("streaming windows sandbox exec should start");
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

        let err = manager
            .write(
                request_id,
                CommandExecWriteParams {
                    process_id: "proc-11".to_string(),
                    delta_base64: Some(STANDARD.encode("hello")),
                    close_stdin: false,
                },
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
