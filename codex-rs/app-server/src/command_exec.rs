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
use codex_app_server_protocol::CommandExecTerminalSize;
use codex_app_server_protocol::CommandExecTerminateParams;
use codex_app_server_protocol::CommandExecTerminateResponse;
use codex_app_server_protocol::CommandExecWriteParams;
use codex_app_server_protocol::CommandExecWriteResponse;
use codex_app_server_protocol::JSONRPCErrorError;
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

trait InternalProcessIdExt {
    fn error_repr(&self) -> String;
}

impl InternalProcessIdExt for InternalProcessId {
    fn error_repr(&self) -> String {
        match self {
            Self::Generated(id) => id.to_string(),
            Self::Client(id) => serde_json::to_string(id).unwrap_or_else(|_| format!("{id:?}")),
        }
    }
}

impl CommandExecManager {
    pub(crate) async fn start(
        &self,
        params: StartCommandExecParams,
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
            }
            let sessions = Arc::clone(&self.sessions);
            tokio::spawn(async move {
                let _started_network_proxy = started_network_proxy;
                let (delivery_relay, delivery_handle) = if stream_stdout_stderr {
                    let (relay, handle) = spawn_output_delivery_relay(
                        Arc::clone(&outgoing),
                        request_id.connection_id,
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
                                while let Ok(event) = rx_event.recv().await {
                                    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
                                        continue;
                                    };
                                    let stream = match delta.stream {
                                        ExecOutputStream::Stdout => CommandExecOutputStream::Stdout,
                                        ExecOutputStream::Stderr => CommandExecOutputStream::Stderr,
                                    };
                                    let delta_base64 = STANDARD.encode(delta.chunk);
                                    let accounted_payload_bytes =
                                        accounted_output_delivery_bytes(&delta_base64, &process_id);
                                    if delivery_relay
                                        .enqueue(
                                            ServerNotification::CommandExecOutputDelta(
                                                CommandExecOutputDeltaNotification {
                                                    process_id: process_id.clone(),
                                                    stream,
                                                    delta_base64,
                                                    cap_reached: false,
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
                match output {
                    Ok(output) => {
                        outgoing
                            .send_response(
                                request_id,
                                CommandExecResponse {
                                    exit_code: output.exit_code,
                                    stdout: output.stdout.text,
                                    stderr: output.stderr.text,
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
                sessions.lock().await.remove(&process_key);
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
        let (program, args) = command
            .split_first()
            .ok_or_else(|| invalid_request("command must not be empty"))?;
        {
            let mut sessions = self.sessions.lock().await;
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
            })
            .await;
            sessions.lock().await.remove(&process_key);
        });
        Ok(())
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
                size: terminal_size_from_protocol(params.size)?,
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
        let (relay, handle) =
            spawn_output_delivery_relay(Arc::clone(&outgoing), request_id.connection_id);
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
                            CommandControl::Terminate => {
                                session.request_terminate();
                                Ok(())
                            }
                        };
                        if let Some(response_tx) = response_tx {
                            let _ = response_tx.send(result);
                        }
                    },
                    None => {
                        control_open = false;
                        session.request_terminate();
                    }
                }
            }
            outcome = &mut expiration, if expiration_outcome.is_none() => {
                expiration_outcome = Some(outcome);
                session.request_terminate();
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
) -> (OutputDeliveryRelay, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<QueuedOutputDelivery>(OUTPUT_DELIVERY_QUEUE_ITEMS);
    let relay = OutputDeliveryRelay {
        tx,
        byte_budget: Arc::new(Semaphore::new(OUTPUT_DELIVERY_MAX_QUEUED_BYTES)),
    };
    let handle = tokio::spawn(async move {
        while let Some(queued) = rx.recv().await {
            outgoing
                .send_server_notification_to_connection_and_wait(connection_id, queued.notification)
                .await;
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

fn spawn_process_output(params: SpawnProcessOutputParams) -> tokio::task::JoinHandle<String> {
    let SpawnProcessOutputParams {
        process_id,
        mut output_rx,
        mut stdio_timeout_rx,
        mut delivery_relay,
        stream,
        stream_output,
        output_bytes_cap,
    } = params;
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut observed_num_bytes = 0usize;
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
            let capped_chunk = match output_bytes_cap {
                Some(output_bytes_cap) => {
                    let capped_chunk_len = output_bytes_cap
                        .saturating_sub(observed_num_bytes)
                        .min(chunk.len());
                    observed_num_bytes += capped_chunk_len;
                    &chunk[0..capped_chunk_len]
                }
                None => chunk.as_slice(),
            };
            let cap_reached = Some(observed_num_bytes) == output_bytes_cap;
            if let (true, Some(process_id)) = (stream_output, process_id.as_ref()) {
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
                }
            } else if !stream_output {
                buffer.extend_from_slice(capped_chunk);
            }
            if cap_reached {
                break;
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
    size: CommandExecTerminalSize,
) -> Result<TerminalSize, JSONRPCErrorError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(invalid_params(
            "command/exec size rows and cols must be greater than 0",
        ));
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
    #[cfg(not(target_os = "windows"))]
    use crate::outgoing_message::OutgoingEnvelope;
    #[cfg(not(target_os = "windows"))]
    use crate::outgoing_message::OutgoingMessage;
    use codex_utils_pty::ProcessDriver;
    use codex_utils_pty::spawn_from_driver;

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

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn windows_sandbox_non_streaming_exec_uses_execution_path() {
        let (tx, mut rx) = mpsc::channel(1);
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(7),
            request_id: codex_app_server_protocol::RequestId::Integer(99),
        };

        manager
            .start(StartCommandExecParams {
                outgoing: Arc::new(OutgoingMessageSender::new(
                    tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                )),
                request_id: request_id.clone(),
                process_id: Some("proc-99".to_string()),
                exec_request: windows_sandbox_exec_request(),
                started_network_proxy: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
                size: None,
            })
            .await
            .expect("non-streaming windows sandbox exec should start");

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for outgoing message")
            .expect("channel closed before outgoing message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            panic!("expected connection-scoped outgoing message");
        };
        assert_eq!(connection_id, request_id.connection_id);
        let OutgoingMessage::Error(error) = message else {
            panic!("expected execution failure to be reported as an error");
        };
        assert_eq!(error.id, request_id.request_id);
        assert!(error.error.message.starts_with("exec failed:"));
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn cancellation_expiration_keeps_process_alive_until_terminated() {
        let (tx, mut rx) = mpsc::channel(4);
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(8),
            request_id: codex_app_server_protocol::RequestId::Integer(100),
        };
        let cwd = AbsolutePathBuf::current_dir().expect("current dir");

        manager
            .start(StartCommandExecParams {
                outgoing: Arc::new(OutgoingMessageSender::new(
                    tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                )),
                request_id: request_id.clone(),
                process_id: Some("proc-100".to_string()),
                exec_request: ExecRequest::new(
                    vec!["sh".to_string(), "-lc".to_string(), "sleep 30".to_string()],
                    cwd.clone(),
                    HashMap::new(),
                    /*network*/ None,
                    /*network_environment_id*/ None,
                    ExecExpiration::Cancellation(CancellationToken::new()),
                    codex_core::exec::ExecCapturePolicy::ShellTool,
                    SandboxType::None,
                    vec![cwd.clone()],
                    WindowsSandboxLevel::Disabled,
                    /*windows_sandbox_private_desktop*/ false,
                    PermissionProfile::read_only(),
                    /*arg0*/ None,
                ),
                started_network_proxy: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
                size: None,
            })
            .await
            .expect("cancellation-based exec should start");

        assert!(
            timeout(Duration::from_millis(250), rx.recv())
                .await
                .is_err(),
            "command/exec should remain active until explicit termination",
        );

        manager
            .terminate(
                request_id.clone(),
                CommandExecTerminateParams {
                    process_id: "proc-100".to_string(),
                },
            )
            .await
            .expect("terminate should succeed");

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for outgoing message")
            .expect("channel closed before outgoing message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            panic!("expected connection-scoped outgoing message");
        };
        assert_eq!(connection_id, request_id.connection_id);
        let OutgoingMessage::Response(response) = message else {
            panic!("expected execution response after termination");
        };
        assert_eq!(response.id, request_id.request_id);
        let response: CommandExecResponse =
            serde_json::from_value(response.result).expect("deserialize command/exec response");
        assert_ne!(response.exit_code, 0);
        assert_eq!(response.stdout, "");
        // The deferred response now drains any already-emitted stderr before
        // replying, so shell startup noise is allowed here.
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
            stdout_rx,
            stderr_rx: Some(stderr_rx),
            exit_rx,
            terminator: Some(Box::new(move || {
                terminator_flag.store(true, Ordering::SeqCst);
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

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn timeout_or_cancellation_reports_cancellation_without_timeout_exit_code() {
        let (tx, mut rx) = mpsc::channel(4);
        let manager = CommandExecManager::default();
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: codex_app_server_protocol::RequestId::Integer(101),
        };
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        let cwd = AbsolutePathBuf::current_dir().expect("current dir");

        manager
            .start(StartCommandExecParams {
                outgoing: Arc::new(OutgoingMessageSender::new(
                    tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                )),
                request_id: request_id.clone(),
                process_id: Some("proc-101".to_string()),
                exec_request: ExecRequest::new(
                    vec!["sh".to_string(), "-lc".to_string(), "sleep 30".to_string()],
                    cwd.clone(),
                    HashMap::new(),
                    /*network*/ None,
                    /*network_environment_id*/ None,
                    ExecExpiration::TimeoutOrCancellation {
                        timeout: Duration::from_secs(30),
                        cancellation,
                    },
                    codex_core::exec::ExecCapturePolicy::ShellTool,
                    SandboxType::None,
                    vec![cwd],
                    WindowsSandboxLevel::Disabled,
                    /*windows_sandbox_private_desktop*/ false,
                    PermissionProfile::read_only(),
                    /*arg0*/ None,
                ),
                started_network_proxy: None,
                tty: false,
                stream_stdin: false,
                stream_stdout_stderr: false,
                output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
                size: None,
            })
            .await
            .expect("timeout-or-cancellation exec should start");

        cancel.cancel();

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for outgoing message")
            .expect("channel closed before outgoing message");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            panic!("expected connection-scoped outgoing message");
        };
        assert_eq!(connection_id, request_id.connection_id);
        let OutgoingMessage::Response(response) = message else {
            panic!("expected execution response after cancellation");
        };
        assert_eq!(response.id, request_id.request_id);
        let response: CommandExecResponse =
            serde_json::from_value(response.result).expect("deserialize command/exec response");
        assert_ne!(response.exit_code, EXEC_TIMEOUT_EXIT_CODE);
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
