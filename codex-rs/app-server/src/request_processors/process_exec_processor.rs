use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ProcessExitedNotification;
use codex_app_server_protocol::ProcessKillParams;
use codex_app_server_protocol::ProcessKillResponse;
use codex_app_server_protocol::ProcessOutputDeltaNotification;
use codex_app_server_protocol::ProcessOutputStream;
use codex_app_server_protocol::ProcessResizePtyParams;
use codex_app_server_protocol::ProcessResizePtyResponse;
use codex_app_server_protocol::ProcessSpawnParams;
use codex_app_server_protocol::ProcessSpawnResponse;
use codex_app_server_protocol::ProcessWriteStdinParams;
use codex_app_server_protocol::ProcessWriteStdinResponse;
use codex_app_server_protocol::ServerNotification;
use codex_core::exec::ExecExpiration;
use codex_core::exec::ExecExpirationOutcome;
use codex_core::exec::IO_DRAIN_TIMEOUT_MS;
use codex_exec_server::EnvironmentManager;
use codex_protocol::exec_output::bytes_to_string_smart;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use codex_utils_pty::ProcessHandle;
use codex_utils_pty::SpawnedProcess;
use codex_utils_pty::TerminalSize;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::command_exec::OutputByteCap;
use crate::command_exec::StdinWriteRequest;
use crate::command_exec::spawn_stdin_writer;
use crate::command_exec::terminal_size_from_protocol;
use crate::command_exec::validate_command_argv;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessSpawnLogMetadata {
    argv_count: usize,
    env_key_count: usize,
    tty: bool,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    output_cap_present: bool,
    timeout_present: bool,
    size_present: bool,
}

fn process_spawn_log_metadata(params: &ProcessSpawnParams) -> ProcessSpawnLogMetadata {
    ProcessSpawnLogMetadata {
        argv_count: params.command.len(),
        env_key_count: params.env.as_ref().map_or(0, HashMap::len),
        tty: params.tty,
        stream_stdin: params.stream_stdin,
        stream_stdout_stderr: params.stream_stdout_stderr,
        output_cap_present: params.output_bytes_cap.is_some(),
        timeout_present: params.timeout_ms.is_some(),
        size_present: params.size.is_some(),
    }
}

fn log_process_spawn_request(params: &ProcessSpawnParams) {
    let metadata = process_spawn_log_metadata(params);
    tracing::debug!(
        rpc.method = "process/spawn",
        argv_count = metadata.argv_count,
        env_key_count = metadata.env_key_count,
        tty = metadata.tty,
        stream_stdin = metadata.stream_stdin,
        stream_stdout_stderr = metadata.stream_stdout_stderr,
        output_cap_present = metadata.output_cap_present,
        timeout_present = metadata.timeout_present,
        size_present = metadata.size_present,
        "command request"
    );
}

#[derive(Clone)]
pub(crate) struct ProcessExecRequestProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    environment_manager: Arc<EnvironmentManager>,
    sessions: Arc<Mutex<HashMap<ConnectionProcessHandle, ProcessSession>>>,
    run_tasks: tokio_util::task::TaskTracker,
}

impl ProcessExecRequestProcessor {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        environment_manager: Arc<EnvironmentManager>,
    ) -> Self {
        Self {
            outgoing,
            environment_manager,
            sessions: Arc::default(),
            run_tasks: tokio_util::task::TaskTracker::new(),
        }
    }

    pub(crate) async fn process_spawn(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessSpawnParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        self.require_local_environment()?;
        log_process_spawn_request(&params);
        let ProcessSpawnParams {
            command,
            process_handle,
            cwd,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            timeout_ms,
            env: env_overrides,
            size,
        } = params;
        let method_name = "process/spawn";
        validate_command_argv(&command)?;
        if process_handle.is_empty() {
            return Err(invalid_request("processHandle must not be empty"));
        }
        if size.is_some() && !tty {
            return Err(invalid_params("process/spawn size requires tty: true"));
        }
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        if let Some(env_overrides) = env_overrides {
            #[cfg(windows)]
            validate_environment_override_keys(&env_overrides)?;
            for (key, value) in env_overrides {
                // Windows treats environment names as case-insensitive. Remove
                // the inherited spelling before applying either an unset or a
                // replacement, so duplicate spellings cannot win during spawn.
                #[cfg(windows)]
                env.retain(|inherited_key, _| !inherited_key.eq_ignore_ascii_case(&key));
                match value {
                    Some(value) => {
                        env.insert(key, value);
                    }
                    None => {
                        env.remove(&key);
                    }
                }
            }
        }
        let expiration = match timeout_ms {
            Some(Some(timeout_ms)) => match u64::try_from(timeout_ms) {
                Ok(timeout_ms) => timeout_ms.into(),
                Err(_) => {
                    return Err(invalid_params(format!(
                        "{method_name} timeoutMs must be non-negative, got {timeout_ms}"
                    )));
                }
            },
            Some(None) => ExecExpiration::Cancellation(CancellationToken::new()),
            None => ExecExpiration::DefaultTimeout,
        };
        let output_bytes_cap = output_bytes_cap.unwrap_or(Some(DEFAULT_OUTPUT_BYTES_CAP));
        let size = size
            .map(|size| terminal_size_from_protocol(size.into_inner(), "process"))
            .transpose()?;

        self.start(
            StartProcessParams {
                outgoing: self.outgoing.clone(),
                request_id,
                process_handle,
                command,
                cwd,
                env,
                expiration,
                tty,
                stream_stdin,
                stream_stdout_stderr,
                output_bytes_cap,
                size,
            },
            rpc_gate,
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn process_write_stdin(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessWriteStdinParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.write_stdin(request_id, params, rpc_gate)
            .await
            .map(|()| None)
    }

    pub(crate) async fn process_resize_pty(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessResizePtyParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.resize_pty(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn process_kill(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessKillParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.kill(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        self.close_connection_processes(connection_id).await;
    }

    pub(crate) async fn drain_run_tasks(&self) {
        self.run_tasks.close();
        self.run_tasks.wait().await;
    }

    fn require_local_environment(&self) -> Result<(), JSONRPCErrorError> {
        self.environment_manager
            .try_local_environment()
            .is_some()
            .then_some(())
            .ok_or_else(|| internal_error("local environment is not configured"))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ConnectionProcessHandle {
    connection_id: ConnectionId,
    process_handle: String,
}

#[derive(Clone)]
struct ProcessSession {
    control_tx: mpsc::Sender<ProcessControlRequest>,
    write_tx: mpsc::Sender<StdinWriteRequest>,
    write_slots: Arc<Semaphore>,
}

enum ProcessControl {
    Resize { size: TerminalSize },
    Kill,
}

struct ProcessControlRequest {
    control: ProcessControl,
    response_tx: Option<oneshot::Sender<Result<(), JSONRPCErrorError>>>,
}

struct StartProcessParams {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: ConnectionRequestId,
    process_handle: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    env: HashMap<String, String>,
    expiration: ExecExpiration,
    tty: bool,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    output_bytes_cap: Option<usize>,
    size: Option<TerminalSize>,
}

struct RunProcessParams {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: ConnectionRequestId,
    process_handle: String,
    spawned: SpawnedProcess,
    control_rx: mpsc::Receiver<ProcessControlRequest>,
    write_rx: mpsc::Receiver<StdinWriteRequest>,
    stream_stdin: bool,
    stream_stdout_stderr: bool,
    expiration: ExecExpiration,
    output_bytes_cap: Option<usize>,
    connection_cancellation: CancellationToken,
    terminal_cleanup: Option<ProcessTerminalCleanup>,
    response_enqueued: Option<oneshot::Receiver<()>>,
}

struct ProcessTerminalCleanup {
    sessions: Arc<Mutex<HashMap<ConnectionProcessHandle, ProcessSession>>>,
    process_key: ConnectionProcessHandle,
}

struct SpawnProcessOutputParams {
    connection_id: ConnectionId,
    process_handle: String,
    output_rx: mpsc::Receiver<Vec<u8>>,
    stdio_timeout_rx: watch::Receiver<bool>,
    outgoing: Arc<OutgoingMessageSender>,
    stream: ProcessOutputStream,
    stream_output: bool,
    output_bytes_cap: Option<usize>,
    connection_cancellation: CancellationToken,
    response_ready: CancellationToken,
}

#[derive(Default)]
struct ProcessOutputCapture {
    text: String,
    cap_reached: bool,
    base64: Option<String>,
    drain_timed_out: bool,
    reached_eof: bool,
}

impl ProcessExecRequestProcessor {
    async fn start(
        &self,
        params: StartProcessParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        let StartProcessParams {
            outgoing,
            request_id,
            process_handle,
            command,
            cwd,
            env,
            expiration,
            tty,
            stream_stdin,
            stream_stdout_stderr,
            output_bytes_cap,
            size,
        } = params;
        let connection_cancellation = rpc_gate.cancellation_token().child_token();
        let run_owner = rpc_gate
            .try_commit(|| self.run_tasks.token())
            .ok_or_else(|| invalid_request("connection is closed"))?;

        let Some((program, args)) = command.split_first() else {
            return Err(internal_error(
                "validated process command unexpectedly empty",
            ));
        };
        let stream_stdin = tty || stream_stdin;
        let stream_stdout_stderr = tty || stream_stdout_stderr;
        let arg0 = None;
        let (control_tx, control_rx) = mpsc::channel(32);
        // Stdin writes preserve ordered backpressure on a dedicated worker so a
        // slow child cannot block kill, resize, expiration, or exit handling.
        let (write_tx, write_rx) = mpsc::channel(MAX_PENDING_STDIN_WRITES);
        let write_slots = Arc::new(Semaphore::new(MAX_PENDING_STDIN_WRITES));
        let process_key = ConnectionProcessHandle {
            connection_id: request_id.connection_id,
            process_handle: process_handle.clone(),
        };

        {
            let mut sessions = self.sessions.lock().await;
            rpc_gate
                .try_commit(|| match sessions.entry(process_key.clone()) {
                    Entry::Occupied(_) => Err(invalid_request(format!(
                        "duplicate active process handle: {process_handle:?}",
                    ))),
                    Entry::Vacant(entry) => {
                        entry.insert(ProcessSession {
                            control_tx,
                            write_tx,
                            write_slots,
                        });
                        Ok(())
                    }
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
                return Err(internal_error(format!("failed to spawn process: {err}")));
            }
        };

        let sessions = Arc::clone(&self.sessions);
        let response_outgoing = Arc::clone(&outgoing);
        let response_request_id = request_id.clone();
        let (response_enqueued_tx, response_enqueued_rx) = oneshot::channel();
        tokio::spawn(async move {
            let _run_owner = run_owner;
            run_process(RunProcessParams {
                outgoing,
                request_id,
                process_handle,
                spawned,
                control_rx,
                write_rx,
                stream_stdin,
                stream_stdout_stderr,
                expiration,
                output_bytes_cap,
                connection_cancellation,
                terminal_cleanup: Some(ProcessTerminalCleanup {
                    sessions,
                    process_key,
                }),
                response_enqueued: Some(response_enqueued_rx),
            })
            .await;
        });

        response_outgoing
            .send_response(response_request_id, ProcessSpawnResponse {})
            .await;
        let _ = response_enqueued_tx.send(());

        Ok(())
    }

    async fn write_stdin(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessWriteStdinParams,
        rpc_gate: &ConnectionRpcGate,
    ) -> Result<(), JSONRPCErrorError> {
        if params.delta_base64.is_none() && !params.close_stdin {
            return Err(invalid_params(
                "process/writeStdin requires deltaBase64 or closeStdin",
            ));
        }

        let delta = match params.delta_base64 {
            Some(delta_base64) => STANDARD
                .decode(delta_base64)
                .map_err(|err| invalid_params(format!("invalid deltaBase64: {err}")))?,
            None => Vec::new(),
        };

        let process_key = ConnectionProcessHandle {
            connection_id: request_id.connection_id,
            process_handle: params.process_handle,
        };
        let session = self
            .sessions
            .lock()
            .await
            .get(&process_key)
            .cloned()
            .ok_or_else(|| no_active_process_error(&process_key.process_handle))?;
        let outgoing = Arc::clone(&self.outgoing);
        let cancellation = rpc_gate.cancellation_token();
        // Keep FIFO admission in the process lane, then transfer the potentially
        // backpressured acknowledgement to the existing connection task owner.
        drop(rpc_gate.spawn_with_commit(|| {
            let busy = || invalid_request("process stdin write queue is full; retry after a pending write completes");
            let slot = session.write_slots.try_acquire_owned().map_err(|_| busy())?;
            let (response_tx, response_rx) = oneshot::channel();
            session.write_tx.try_send(StdinWriteRequest {
                delta,
                close_stdin: params.close_stdin,
                response_tx: Some(response_tx),
            }).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => busy(),
                mpsc::error::TrySendError::Closed(_) => process_no_longer_running_error(&process_key.process_handle),
            })?;
            Ok::<_, JSONRPCErrorError>(async move {
                // Waiting response deliveries count against the same finite limit.
                let _slot = slot;
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = response_rx => result.unwrap_or_else(|_| {
                        Err(process_no_longer_running_error(&process_key.process_handle))
                    }),
                };
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {},
                    _ = async {
                        match result {
                            Ok(()) => outgoing.send_response(request_id, ProcessWriteStdinResponse {}).await,
                            Err(error) => outgoing.send_error(request_id, error).await,
                        }
                    } => {},
                }
            })
        })?.ok_or_else(|| invalid_request("connection is closed"))?);
        Ok(())
    }

    async fn kill(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessKillParams,
    ) -> Result<ProcessKillResponse, JSONRPCErrorError> {
        self.send_control(
            request_id.connection_id,
            params.process_handle,
            ProcessControl::Kill,
        )
        .await?;
        Ok(ProcessKillResponse {})
    }

    async fn resize_pty(
        &self,
        request_id: ConnectionRequestId,
        params: ProcessResizePtyParams,
    ) -> Result<ProcessResizePtyResponse, JSONRPCErrorError> {
        self.send_control(
            request_id.connection_id,
            params.process_handle,
            ProcessControl::Resize {
                size: terminal_size_from_protocol(params.size.into_inner(), "process")?,
            },
        )
        .await?;
        Ok(ProcessResizePtyResponse {})
    }

    async fn close_connection_processes(&self, connection_id: ConnectionId) {
        let controls = {
            let mut sessions = self.sessions.lock().await;
            let process_handles = sessions
                .keys()
                .filter(|process_handle| process_handle.connection_id == connection_id)
                .cloned()
                .collect::<Vec<_>>();
            let mut controls = Vec::with_capacity(process_handles.len());
            for process_handle in process_handles {
                if let Some(control) = sessions.remove(&process_handle) {
                    controls.push(control);
                }
            }
            controls
        };

        futures::future::join_all(controls.into_iter().map(|control| async move {
            let _ = control
                .control_tx
                .send(ProcessControlRequest {
                    control: ProcessControl::Kill,
                    response_tx: None,
                })
                .await;
        })).await;
    }

    async fn send_control(
        &self,
        connection_id: ConnectionId,
        process_handle: String,
        control: ProcessControl,
    ) -> Result<(), JSONRPCErrorError> {
        let process_key = ConnectionProcessHandle {
            connection_id,
            process_handle,
        };
        let session = self
            .sessions
            .lock()
            .await
            .get(&process_key)
            .cloned()
            .ok_or_else(|| no_active_process_error(&process_key.process_handle))?;
        let (response_tx, response_rx) = oneshot::channel();
        session
            .control_tx
            .send(ProcessControlRequest {
                control,
                response_tx: Some(response_tx),
            })
            .await
            .map_err(|_| process_no_longer_running_error(&process_key.process_handle))?;
        response_rx
            .await
            .map_err(|_| process_no_longer_running_error(&process_key.process_handle))?
    }
}

async fn run_process(params: RunProcessParams) {
    let RunProcessParams {
        outgoing,
        request_id,
        process_handle,
        spawned,
        control_rx,
        write_rx,
        stream_stdin,
        stream_stdout_stderr,
        expiration,
        output_bytes_cap,
        connection_cancellation,
        terminal_cleanup,
        response_enqueued,
    } = params;
    let response_ready = CancellationToken::new();
    if response_enqueued.is_none() {
        response_ready.cancel();
    }
    let response_barrier = async {
        match response_enqueued {
            Some(response) => response.await.is_ok(),
            None => true,
        }
    };
    tokio::pin!(response_barrier);
    let mut response_processed = false;
    let mut control_rx = control_rx;
    let mut control_open = true;
    let mut connection_cancelled = false;
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
        "stdin streaming is not enabled for this process",
    );

    let stdout_handle = collect_spawn_process_output(SpawnProcessOutputParams {
        connection_id: request_id.connection_id,
        process_handle: process_handle.clone(),
        output_rx: stdout_rx,
        stdio_timeout_rx: stdio_timeout_rx.clone(),
        outgoing: Arc::clone(&outgoing),
        stream: ProcessOutputStream::Stdout,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
        connection_cancellation: connection_cancellation.clone(),
        response_ready: response_ready.clone(),
    });
    let stderr_handle = collect_spawn_process_output(SpawnProcessOutputParams {
        connection_id: request_id.connection_id,
        process_handle: process_handle.clone(),
        output_rx: stderr_rx,
        stdio_timeout_rx,
        outgoing: Arc::clone(&outgoing),
        stream: ProcessOutputStream::Stderr,
        stream_output: stream_stdout_stderr,
        output_bytes_cap,
        connection_cancellation: connection_cancellation.clone(),
        response_ready: response_ready.clone(),
    });

    let exit_code = loop {
        tokio::select! {
            delivered = &mut response_barrier, if !response_processed => {
                response_processed = true;
                if delivered {
                    response_ready.cancel();
                } else {
                    connection_cancellation.cancel();
                }
            }
            _ = connection_cancellation.cancelled(), if !connection_cancelled => {
                connection_cancelled = true;
                if let Err(error) = session.request_terminate() {
                    tracing::warn!(
                        process_handle = %process_handle,
                        %error,
                        "failed to terminate process after connection cancellation"
                    );
                }
            }
            control = control_rx.recv(), if control_open => {
                match control {
                    Some(ProcessControlRequest { control, response_tx }) => {
                        let result = match control {
                            ProcessControl::Resize { size } => {
                                handle_process_resize(&session, size)
                            }
                            ProcessControl::Kill => session
                                .request_terminate()
                                .map_err(|error| internal_error(error.to_string())),
                        };
                        if let Some(response_tx) = response_tx
                            && response_tx.send(result).is_err()
                        {
                            tracing::debug!(
                                process_handle = %process_handle,
                                "process control response receiver dropped"
                            );
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

    // Give stdout/stderr readers a bounded grace period to drain after process exit.
    let timeout_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(IO_DRAIN_TIMEOUT_MS)).await;
        let _ = stdio_timeout_tx.send(true);
    });

    let stdout = stdout_handle.await.unwrap_or_default();
    let stderr = stderr_handle.await.unwrap_or_default();
    timeout_handle.abort();

    // Supervision and capped pipe draining must not wait for response delivery.
    // Only notification publication is ordered behind the spawn response.
    if !response_processed {
        let delivered = tokio::select! {
            delivered = &mut response_barrier => delivered,
            _ = connection_cancellation.cancelled() => false,
        };
        if !delivered {
            connection_cancellation.cancel();
        }
    }

    // Keep the client handle reserved until the terminal is accepted or its
    // connection explicitly fails; a successor must not overtake this result.
    let delivered = outgoing
        .send_server_notification_to_connection_bounded(
            request_id.connection_id,
            ServerNotification::ProcessExited(ProcessExitedNotification {
                process_handle,
                exit_code,
                stdout_base64: stdout.base64,
                stderr_base64: stderr.base64,
                stdout_capture: Some(codex_app_server_protocol::ProcessOutputCaptureState {
                    reached_eof: stdout.reached_eof && session.output_capture().stdout_complete(),
                    cap_reached: stdout.cap_reached,
                    drain_timed_out: stdout.drain_timed_out,
                }),
                stderr_capture: Some(codex_app_server_protocol::ProcessOutputCaptureState {
                    reached_eof: stderr.reached_eof && session.output_capture().stderr_complete(),
                    cap_reached: stderr.cap_reached,
                    drain_timed_out: stderr.drain_timed_out,
                }),
                stdout: stdout.text,
                stdout_cap_reached: stdout.cap_reached,
                stderr: stderr.text,
                stderr_cap_reached: stderr.cap_reached,
            }),
            &connection_cancellation,
        )
        .await;
    if !delivered {
        outgoing
            .fail_connection_delivery(request_id.connection_id)
            .await;
    }
    if let Some(cleanup) = terminal_cleanup {
        cleanup.sessions.lock().await.remove(&cleanup.process_key);
    }
}

fn collect_spawn_process_output(
    params: SpawnProcessOutputParams,
) -> tokio::task::JoinHandle<ProcessOutputCapture> {
    let SpawnProcessOutputParams {
        connection_id,
        process_handle,
        mut output_rx,
        mut stdio_timeout_rx,
        outgoing,
        stream,
        mut stream_output,
        output_bytes_cap,
        connection_cancellation,
        response_ready,
    } = params;
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut cap = OutputByteCap::new(output_bytes_cap);
        let streaming_requested = stream_output;
        let mut drain_timed_out = false;
        // Every loop exit decides whether the pipe reached EOF.
        let reached_eof;
        loop {
            if *stdio_timeout_rx.borrow() {
                reached_eof = output_rx.is_closed() && output_rx.is_empty();
                drain_timed_out = !reached_eof;
                break;
            }
            let mut chunk = tokio::select! {
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
            while chunk.len() < OUTPUT_CHUNK_SIZE_HINT
                && let Ok(next_chunk) = output_rx.try_recv()
            {
                chunk.extend_from_slice(&next_chunk);
            }
            let (capped_chunk, cap_reached) = cap.accept(&chunk);
            if stream_output {
                if capped_chunk.is_empty() && !cap_reached {
                    continue;
                }
                if !response_ready.is_cancelled() || !outgoing.try_send_server_notification_to_connection(
                    connection_id,
                    ServerNotification::ProcessOutputDelta(ProcessOutputDeltaNotification {
                        process_handle: process_handle.clone(),
                        stream,
                        delta_base64: STANDARD.encode(capped_chunk),
                        cap_reached,
                    }),
                    &connection_cancellation,
                ) {
                    // Preserve the rejected delta and every later byte in the
                    // exit notification; transport waits must not consume the
                    // pipe reader's post-exit drain window.
                    stream_output = false;
                    buffer.extend_from_slice(capped_chunk);
                }
            } else {
                buffer.extend_from_slice(capped_chunk);
            }
        }
        ProcessOutputCapture {
            reached_eof,
            base64: (streaming_requested && !buffer.is_empty()).then(|| STANDARD.encode(&buffer)),
            drain_timed_out,
            text: bytes_to_string_smart(&buffer),
            cap_reached: cap.truncated(),
        }
    })
}

fn handle_process_resize(
    session: &ProcessHandle,
    size: TerminalSize,
) -> Result<(), JSONRPCErrorError> {
    session
        .resize(size)
        .map_err(|err| invalid_request(format!("failed to resize PTY: {err}")))
}

#[cfg(windows)]
fn validate_environment_override_keys(
    overrides: &HashMap<String, Option<String>>,
) -> Result<(), JSONRPCErrorError> {
    let mut names = std::collections::HashSet::new();
    for key in overrides.keys() {
        if !names.insert(key.to_ascii_lowercase()) {
            return Err(invalid_params(format!(
                "process/spawn env contains conflicting case-insensitive key {key:?}"
            )));
        }
    }
    Ok(())
}

fn no_active_process_error(process_handle: &str) -> JSONRPCErrorError {
    invalid_request(format!(
        "no active process for process handle {process_handle:?}"
    ))
}

fn process_no_longer_running_error(process_handle: &str) -> JSONRPCErrorError {
    invalid_request(format!("process {process_handle:?} is no longer running"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    use codex_utils_pty::ProcessDriver;
    use codex_utils_pty::spawn_from_driver;
    use tokio::time::timeout;

    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;

    #[cfg(windows)]
    #[test]
    fn conflicting_environment_aliases_are_rejected_before_spawn() {
        for first in [None, Some(String::new()), Some("requested".into())] {
            for second in [None, Some(String::new()), Some("other".into())] {
                for keys in [["PATH", "Path"], ["Path", "PATH"]] {
                    let overrides = HashMap::from([
                        (keys[0].to_string(), first.clone()),
                        (keys[1].to_string(), second.clone()),
                    ]);
                    assert!(validate_environment_override_keys(&overrides).is_err());
                }
            }
        }
        assert!(
            validate_environment_override_keys(&HashMap::from([
                ("PATH".into(), None),
                ("OTHER".into(), Some(String::new())),
            ]))
            .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_backpressure_explicitly_fails_the_connection() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let failed = CancellationToken::new();
        let initialized = Arc::new(AtomicBool::new(true));
        outgoing
            .connection_opened_with_runtime(
                ConnectionId(42),
                Arc::clone(&initialized),
                failed.clone(),
            )
            .await;
        assert!(
            outgoing.try_send_server_notification(ServerNotification::ThreadClosed(
                codex_app_server_protocol::ThreadClosedNotification {
                    thread_id: "full".into()
                },
            ))
        );
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(1);
        let (stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        stdout_tx.send(b"retained fallback".to_vec()).unwrap();
        drop(stdout_tx);
        drop(stderr_tx);
        let (exit_tx, exit_rx) = oneshot::channel();
        let spawned = spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_rx.into(),
            stderr_rx: Some(stderr_rx.into()),
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        let (_control_tx, control_rx) = mpsc::channel(1);
        let (_write_tx, write_rx) = mpsc::channel(1);
        exit_tx.send(7).unwrap();
        run_process(RunProcessParams {
            outgoing,
            request_id: ConnectionRequestId {
                connection_id: ConnectionId(42),
                request_id: codex_app_server_protocol::RequestId::Integer(42),
            },
            process_handle: "terminal".into(),
            spawned,
            control_rx,
            write_rx,
            stream_stdin: false,
            stream_stdout_stderr: true,
            expiration: ExecExpiration::Cancellation(CancellationToken::new()),
            output_bytes_cap: Some(1024),
            connection_cancellation: CancellationToken::new(),
            terminal_cleanup: None,
            response_enqueued: None,
        })
        .await;
        assert!(
            failed.is_cancelled(),
            "a forgotten terminal must not leave a live connection"
        );
        assert!(!initialized.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn connection_cleanup_admits_ready_kills_before_full_channels() {
        let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
        let processor = ProcessExecRequestProcessor::new(
            Arc::new(OutgoingMessageSender::new(outgoing_tx, codex_analytics::AnalyticsEventsClient::disabled())),
            Arc::new(EnvironmentManager::default_for_tests()),
        );
        let mut receivers = HashMap::new();
        for (connection_id, handle) in [(ConnectionId(1), "a"), (ConnectionId(1), "b"), (ConnectionId(2), "foreign")] {
            let (control_tx, control_rx) = mpsc::channel(1);
            let (write_tx, _write_rx) = mpsc::channel(1);
            processor.sessions.lock().await.insert(ConnectionProcessHandle { connection_id, process_handle: handle.into() }, ProcessSession { control_tx, write_tx, write_slots: Arc::new(Semaphore::new(1)) });
            receivers.insert(handle.to_string(), control_rx);
        }
        let blocked = {
            let sessions = processor.sessions.lock().await;
            let key = sessions.keys().find(|key| key.connection_id == ConnectionId(1)).unwrap();
            sessions[key].control_tx.try_send(ProcessControlRequest { control: ProcessControl::Kill, response_tx: None }).unwrap_or_else(|_| panic!("fill control channel"));
            key.process_handle.clone()
        };
        let ready = if blocked == "a" { "b" } else { "a" };
        let owner = processor.clone();
        let cleanup = tokio::spawn(async move { owner.close_connection_processes(ConnectionId(1)).await });
        assert!(matches!(timeout(Duration::from_secs(2), receivers.get_mut(ready).unwrap().recv()).await.unwrap().unwrap().control, ProcessControl::Kill));
        assert!(!cleanup.is_finished(), "cleanup still owns the blocked admission");
        receivers.get_mut(&blocked).unwrap().recv().await.unwrap();
        timeout(Duration::from_secs(2), cleanup).await.unwrap().unwrap();
        assert_eq!(processor.sessions.lock().await.len(), 1);
        assert!(receivers.get_mut("foreign").unwrap().try_recv().is_err());
    }

    #[tokio::test]
    async fn spawn_response_barrier_does_not_block_supervision_or_capped_collection() {
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(2);
        let (stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        stdout_tx.send(b"abcdef".to_vec()).unwrap();
        drop(stdout_tx);
        drop(stderr_tx);
        let (exit_tx, exit_rx) = oneshot::channel();
        let terminated = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&terminated);
        let spawned = spawn_from_driver(ProcessDriver { writer_tx, stdout_rx: stdout_rx.into(), stderr_rx: Some(stderr_rx.into()), exit_rx, terminator: Some(Box::new(move || { flag.store(true, Ordering::SeqCst); Ok(()) })), writer_handle: None, resizer: None });
        let (control_tx, control_rx) = mpsc::channel(1);
        let (_write_tx, write_rx) = mpsc::channel(1);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(4);
        let (response_tx, response_rx) = oneshot::channel();
        let owner = tokio::spawn(run_process(RunProcessParams {
            outgoing: Arc::new(OutgoingMessageSender::new(outgoing_tx, codex_analytics::AnalyticsEventsClient::disabled())),
            request_id: ConnectionRequestId { connection_id: ConnectionId(42), request_id: codex_app_server_protocol::RequestId::Integer(1) }, process_handle: "barrier".into(), spawned, control_rx, write_rx,
            stream_stdin: false, stream_stdout_stderr: true, expiration: ExecExpiration::Cancellation(CancellationToken::new()), output_bytes_cap: Some(3), connection_cancellation: CancellationToken::new(), terminal_cleanup: None, response_enqueued: Some(response_rx),
        }));
        let (ack_tx, ack_rx) = oneshot::channel();
        control_tx.send(ProcessControlRequest { control: ProcessControl::Kill, response_tx: Some(ack_tx) }).await.unwrap_or_else(|_| panic!("send kill"));
        timeout(Duration::from_secs(2), ack_rx).await.unwrap().unwrap().unwrap();
        assert!(terminated.load(Ordering::SeqCst));
        assert!(outgoing_rx.try_recv().is_err(), "no notification may precede spawn response");
        exit_tx.send(0).unwrap();
        response_tx.send(()).unwrap();
        timeout(Duration::from_secs(2), owner).await.unwrap().unwrap();
        let mut bytes = Vec::new();
        while let Ok(envelope) = outgoing_rx.try_recv() {
            if let OutgoingEnvelope::ToConnection { message: OutgoingMessage::AppServerNotification(notification), .. } = envelope {
                match notification {
                    ServerNotification::ProcessOutputDelta(delta) => bytes.extend(STANDARD.decode(delta.delta_base64).unwrap()),
                    ServerNotification::ProcessExited(exit) => {
                        if let Some(base64) = exit.stdout_base64 { bytes.extend(STANDARD.decode(base64).unwrap()); }
                        assert!(exit.stdout_cap_reached);
                    }
                    _ => panic!("unexpected notification"),
                }
            }
        }
        assert_eq!(bytes, b"abc");
    }

    #[tokio::test(start_paused = true)]
    async fn output_backpressure_preserves_capped_bytes_without_blocking_pipe_drain() {
        for (saturated, cancelled) in [(true, false), (false, false), (false, true)] {
            let (outgoing_tx, mut outgoing_rx) = mpsc::channel(2);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                outgoing_tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            ));
            if saturated {
                for _ in 0..2 {
                    assert!(outgoing.try_send_server_notification(
                        ServerNotification::ThreadClosed(
                            codex_app_server_protocol::ThreadClosedNotification {
                                thread_id: "blocker".to_string(),
                            },
                        ),
                    ));
                }
            }
            let (output_tx, output_rx) = mpsc::channel(2);
            output_tx
                .send(vec![b'x'; OUTPUT_CHUNK_SIZE_HINT])
                .await
                .unwrap();
            output_tx.send("€!extra".as_bytes().to_vec()).await.unwrap();
            drop(output_tx);
            let (_drain_tx, stdio_timeout_rx) = watch::channel(false);
            let connection_cancellation = CancellationToken::new();
            if cancelled {
                connection_cancellation.cancel();
            }
            let response_ready = CancellationToken::new();
            response_ready.cancel();
            let started = tokio::time::Instant::now();
            let capture = collect_spawn_process_output(SpawnProcessOutputParams {
                connection_id: ConnectionId(42),
                process_handle: "output".to_string(),
                output_rx,
                stdio_timeout_rx,
                outgoing,
                stream: ProcessOutputStream::Stdout,
                stream_output: true,
                output_bytes_cap: Some(OUTPUT_CHUNK_SIZE_HINT + "€!".len()),
                connection_cancellation,
                response_ready,
            })
            .await
            .unwrap();
            assert_eq!(
                started.elapsed(),
                Duration::ZERO,
                "pipe drain must not wait for transport capacity"
            );
            let expected = format!("{}€!", "x".repeat(OUTPUT_CHUNK_SIZE_HINT));
            assert!(capture.cap_reached);
            if saturated || cancelled {
                assert_eq!(capture.text, expected);
                if saturated {
                    for _ in 0..2 {
                        assert!(matches!(
                            outgoing_rx.try_recv().unwrap(),
                            OutgoingEnvelope::Broadcast { .. }
                        ));
                    }
                }
            } else {
                assert!(
                    capture.text.is_empty(),
                    "delivered bytes must not repeat in the final response"
                );
                let mut delivered = Vec::new();
                for capped in [false, true] {
                    let OutgoingEnvelope::ToConnection {
                        connection_id,
                        message:
                            OutgoingMessage::AppServerNotification(
                                ServerNotification::ProcessOutputDelta(delta),
                            ),
                        write_complete_tx,
                    } = outgoing_rx.try_recv().unwrap()
                    else {
                        panic!("expected targeted output");
                    };
                    assert_eq!(connection_id, ConnectionId(42));
                    assert!(write_complete_tx.is_none());
                    assert_eq!(delta.process_handle, "output");
                    assert_eq!(delta.stream, ProcessOutputStream::Stdout);
                    assert_eq!(delta.cap_reached, capped);
                    delivered.extend(STANDARD.decode(delta.delta_base64).unwrap());
                }
                assert_eq!(String::from_utf8(delivered).unwrap(), expected);
            }
            assert!(outgoing_rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn delivered_prefix_and_raw_fallback_reconstruct_every_utf8_split() {
        let bytes = "é€🙂".as_bytes();
        for stream in [ProcessOutputStream::Stdout, ProcessOutputStream::Stderr] {
            for split in 1..bytes.len() {
                let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
                let outgoing = Arc::new(OutgoingMessageSender::new(
                    outgoing_tx,
                    codex_analytics::AnalyticsEventsClient::disabled(),
                ));
                let (output_tx, output_rx) = mpsc::channel(1);
                let (_drain_tx, stdio_timeout_rx) = watch::channel(false);
                let collector = collect_spawn_process_output(SpawnProcessOutputParams {
                    connection_id: ConnectionId(42),
                    process_handle: "split".into(),
                    output_rx,
                    stdio_timeout_rx,
                    outgoing: Arc::clone(&outgoing),
                    stream,
                    stream_output: true,
                    output_bytes_cap: None,
                    connection_cancellation: CancellationToken::new(),
                    response_ready: {
                        let ready = CancellationToken::new();
                        ready.cancel();
                        ready
                    },
                });
                output_tx.send(bytes[..split].to_vec()).await.unwrap();
                let OutgoingEnvelope::ToConnection {
                    message:
                        OutgoingMessage::AppServerNotification(ServerNotification::ProcessOutputDelta(
                            delta,
                        )),
                    ..
                } = timeout(Duration::from_secs(1), outgoing_rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
                else {
                    panic!("delivered prefix");
                };
                assert_eq!(delta.stream, stream);
                let mut received = STANDARD.decode(delta.delta_base64).unwrap();
                // Fail admission only after the prefix was delivered.
                assert!(
                    outgoing.try_send_server_notification(ServerNotification::ThreadClosed(
                        codex_app_server_protocol::ThreadClosedNotification {
                            thread_id: "full".into()
                        }
                    ))
                );
                output_tx.send(bytes[split..].to_vec()).await.unwrap();
                drop(output_tx);
                let capture = collector.await.unwrap();
                received.extend(STANDARD.decode(capture.base64.unwrap()).unwrap());
                assert_eq!(received, bytes, "split {split} on {stream:?}");
                assert!(!capture.cap_reached);
                assert!(!capture.drain_timed_out);
            }
        }
    }

    #[tokio::test]
    async fn post_exit_drain_expiry_is_not_eof_or_a_byte_cap() {
        for keep_open in [false, true] {
            let (outgoing_tx, _outgoing_rx) = mpsc::channel(1);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                outgoing_tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            ));
            let (output_tx, output_rx) = mpsc::channel(1);
            output_tx.send(b"prefix".to_vec()).await.unwrap();
            let (drain_tx, stdio_timeout_rx) = watch::channel(false);
            let collector = collect_spawn_process_output(SpawnProcessOutputParams {
                connection_id: ConnectionId(42),
                process_handle: "drain".into(),
                output_rx,
                stdio_timeout_rx,
                outgoing,
                stream: ProcessOutputStream::Stdout,
                stream_output: false,
                output_bytes_cap: None,
                connection_cancellation: CancellationToken::new(),
                response_ready: CancellationToken::new(),
            });
            let slot = output_tx.reserve().await.unwrap();
            drop(slot); // The prefix was collected before expiry/EOF.
            if keep_open {
                drain_tx.send_replace(true);
            } else {
                drop(output_tx);
            }
            let capture = timeout(Duration::from_secs(1), collector)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(capture.text, "prefix");
            assert_eq!(capture.drain_timed_out, keep_open);
            assert!(!capture.cap_reached);
        }
    }

    #[test]
    fn logging_contract_process_spawn_metadata_omits_argv_and_env_values() {
        let params = ProcessSpawnParams {
            command: vec![
                "powershell".to_string(),
                "-Command".to_string(),
                "command secret".to_string(),
            ],
            process_handle: "process secret".to_string(),
            cwd: AbsolutePathBuf::from_absolute_path(std::env::temp_dir())
                .expect("absolute temp dir"),
            tty: true,
            stream_stdin: true,
            stream_stdout_stderr: true,
            output_bytes_cap: Some(Some(1024)),
            timeout_ms: Some(Some(1000)),
            env: Some(HashMap::from([(
                "SECRET_KEY".to_string(),
                Some("secret value".to_string()),
            )])),
            size: None,
        };

        let metadata = process_spawn_log_metadata(&params);

        assert_eq!(
            metadata,
            ProcessSpawnLogMetadata {
                argv_count: 3,
                env_key_count: 1,
                tty: true,
                stream_stdin: true,
                stream_stdout_stderr: true,
                output_cap_present: true,
                timeout_present: true,
                size_present: false,
            }
        );
        let rendered = format!("{metadata:?}");
        assert!(!rendered.contains("command secret"));
        assert!(!rendered.contains("secret value"));
        assert!(!rendered.contains("process secret"));
    }

    #[tokio::test]
    async fn backpressured_stdin_does_not_block_kill_control() {
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        writer_tx
            .try_send(vec![b'x'])
            .expect("pre-fill driver stdin queue");
        let writer_probe = writer_tx.clone();
        let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(1);
        let (stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        drop(stdout_tx);
        drop(stderr_tx);
        let (exit_tx, exit_rx) = oneshot::channel();
        let terminated = Arc::new(AtomicBool::new(false));
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
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(1);
        let run_handle = tokio::spawn(run_process(RunProcessParams {
            outgoing: Arc::new(OutgoingMessageSender::new(
                outgoing_tx,
                codex_analytics::AnalyticsEventsClient::disabled(),
            )),
            request_id: ConnectionRequestId {
                connection_id: ConnectionId(22),
                request_id: codex_app_server_protocol::RequestId::Integer(22),
            },
            process_handle: "backpressured".to_string(),
            spawned,
            control_rx,
            write_rx,
            stream_stdin: true,
            stream_stdout_stderr: false,
            expiration: ExecExpiration::Cancellation(CancellationToken::new()),
            output_bytes_cap: Some(DEFAULT_OUTPUT_BYTES_CAP),
            connection_cancellation: CancellationToken::new(),
            terminal_cleanup: None,
            response_enqueued: None,
        }));

        let (write_response_tx, mut write_response_rx) = oneshot::channel();
        write_tx
            .send(StdinWriteRequest {
                delta: vec![b'y'],
                close_stdin: false,
                response_tx: Some(write_response_tx),
            })
            .await
            .expect("queue stdin write");
        timeout(Duration::from_secs(1), async {
            // ProcessHandle and this probe own two senders. The third belongs
            // to handle_process_write while its send waits on the full driver.
            while writer_probe.strong_count() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stdin worker must reach the blocked driver before kill");
        assert!(matches!(
            write_response_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let (kill_response_tx, kill_response_rx) = oneshot::channel();
        control_tx
            .send(ProcessControlRequest {
                control: ProcessControl::Kill,
                response_tx: Some(kill_response_tx),
            })
            .await
            .expect("queue kill control");

        timeout(Duration::from_secs(1), kill_response_rx)
            .await
            .expect("kill response timed out")
            .expect("kill response sender dropped")
            .expect("kill should succeed");
        assert!(terminated.load(Ordering::SeqCst));

        assert_eq!(writer_rx.recv().await, Some(vec![b'x']));
        timeout(Duration::from_secs(1), write_response_rx)
            .await
            .expect("write should complete after backpressure clears")
            .expect("write response sender dropped")
            .expect("backpressured write should eventually succeed");
        assert_eq!(writer_rx.recv().await, Some(vec![b'y']));

        exit_tx.send(1).expect("publish driver exit");
        let envelope = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("process exit notification timed out")
            .expect("outgoing channel closed");
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            write_complete_tx,
        } = envelope
        else {
            panic!("process exit should use a targeted notification");
        };
        assert_eq!(connection_id, ConnectionId(22));
        let OutgoingMessage::AppServerNotification(ServerNotification::ProcessExited(exited)) =
            message
        else {
            panic!("expected process exit notification");
        };
        assert_eq!(exited.process_handle, "backpressured");
        assert_eq!(exited.exit_code, 1);
        assert_eq!(exited.stdout, "");
        assert_eq!(exited.stderr, "");
        assert!(!exited.stdout_cap_reached);
        assert!(!exited.stderr_cap_reached);
        assert!(write_complete_tx.is_none());
        timeout(Duration::from_secs(1), run_handle)
            .await
            .expect("run process did not finish")
            .expect("run process task panicked");
    }
}
