use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_protocol::config_types::EnvironmentVariablePattern;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::shell_environment;
use codex_sandboxing::SandboxType;
use codex_sandboxing::is_likely_sandbox_denied;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;

use crate::ExecBackend;
use crate::ExecBackendFuture;
use crate::ExecProcess;
use crate::ExecProcessEvent;
use crate::ExecProcessEventReceiver;
use crate::ExecProcessFuture;
use crate::ExecServerError;
use crate::ExecServerRuntimePaths;
use crate::ProcessId;
use crate::StartedExecProcess;
use crate::process::ExecProcessEventLog;
use crate::process_sandbox::prepare_exec_request_cancellable;
use crate::protocol::EXEC_CLOSED_METHOD;
use crate::protocol::ExecClosedNotification;
use crate::protocol::ExecEnvPolicy;
use crate::protocol::ExecExitedNotification;
use crate::protocol::ExecOutputStream;
use crate::protocol::ExecParams;
use crate::protocol::ExecResponse;
use crate::protocol::ProcessOutputChunk;
use crate::protocol::ProcessSignal;
use crate::protocol::ReadParams;
use crate::protocol::ReadResponse;
use crate::protocol::SignalParams;
use crate::protocol::SignalResponse;
use crate::protocol::TerminateParams;
use crate::protocol::TerminateResponse;
use crate::protocol::WriteParams;
use crate::protocol::WriteResponse;
use crate::protocol::WriteStatus;
use crate::rpc::RpcNotificationSender;
use crate::rpc::internal_error;
use crate::rpc::invalid_params;
use crate::rpc::invalid_request;
use crate::telemetry::ExecServerTelemetry;
use crate::telemetry::ProcessMetricGuard;

const RETAINED_OUTPUT_BYTES_PER_PROCESS: usize = 1024 * 1024;
const PROCESS_EVENT_CHANNEL_CAPACITY: usize = 256;
const RETAINED_STDIN_WRITE_IDS_PER_PROCESS: usize = 4096;
// Together with the existing 128-item writer queue, cap accepted stdin at
// 129 MiB including the currently blocked write. IDs retain at most 2 MiB.
const MAX_STDIN_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_STDIN_WRITE_ID_BYTES: usize = 256;
const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_LOCAL_STDIN_WRITE_ID: AtomicU64 = AtomicU64::new(1);
const EXITED_PROCESS_RETENTION: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct RetainedOutputChunk {
    seq: u64,
    stream: ExecOutputStream,
    event: Arc<ExecProcessEvent>,
}

impl RetainedOutputChunk {
    fn bytes(&self) -> &[u8] {
        let ExecProcessEvent::Output(output) = self.event.as_ref() else {
            unreachable!("retained output contains only output events");
        };
        &output.chunk.0
    }
}

struct RunningProcess {
    start_generation: Arc<ProcessStart>,
    session: ExecCommandSession,
    tty: bool,
    pipe_stdin: bool,
    accepted_stdin_write_ids: Arc<Mutex<AcceptedStdinWriteIds>>,
    output: VecDeque<RetainedOutputChunk>,
    retained_bytes: usize,
    last_evicted_seq: u64,
    next_seq: u64,
    exit_seq: Option<u64>,
    exit_code: Option<i32>,
    wake_tx: watch::Sender<u64>,
    events: ExecProcessEventLog,
    output_notify: Arc<Notify>,
    open_streams: usize,
    closed: bool,
    finalizing: bool,
    termination_requested: Arc<std::sync::atomic::AtomicBool>,
    failure: Option<String>,
    sandbox: SandboxType,
    sandbox_denied: bool,
}

/// Bounded cache of stdin write ids that have already been accepted for one process.
///
/// A remote client can retry `process/write` after reconnecting. Remembering accepted
/// ids lets the server acknowledge the retried request without writing the same bytes
/// to child stdin twice.
/// Sequenced IDs (canonical decimal for remote writers, `local-<decimal>` for
/// local writers) remain retired after eviction. Opaque IDs retain the legacy
/// bounded deduplication window; they cannot provide an ordered retirement floor.
#[derive(Default)]
struct AcceptedStdinWriteIds {
    ids: HashSet<String>,
    order: VecDeque<String>,
    retired_through: [Option<u64>; 2],
}

impl AcceptedStdinWriteIds {
    fn sequence(write_id: &str) -> Option<(usize, u64)> {
        let (writer, digits) = match write_id.strip_prefix("local-") {
            Some(digits) => (1, digits),
            None => (0, write_id),
        };
        let sequence = digits.parse::<u64>().ok()?;
        (digits == sequence.to_string()).then_some((writer, sequence))
    }

    fn already_accepted(&self, write_id: &str) -> Result<bool, JSONRPCErrorError> {
        if self.ids.contains(write_id) {
            return Ok(true);
        }
        if let Some((writer, sequence)) = Self::sequence(write_id)
            && self.retired_through[writer].is_some_and(|floor| sequence <= floor)
        {
            // Do not acknowledge an unknown gap or turn an expired retry into
            // a new write. The caller must reconcile, not retry with a new ID.
            return Err(invalid_params(
                "writeId has expired from the stdin retry window; input was not written"
                    .to_string(),
            ));
        }
        Ok(false)
    }

    fn remember(&mut self, write_id: String) {
        if !self.ids.insert(write_id.clone()) {
            return;
        }

        self.order.push_back(write_id);
        while self.order.len() > RETAINED_STDIN_WRITE_IDS_PER_PROCESS {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            self.ids.remove(&evicted);
            if let Some((writer, sequence)) = Self::sequence(&evicted) {
                let floor = &mut self.retired_through[writer];
                *floor = Some(floor.map_or(sequence, |previous| previous.max(sequence)));
            }
        }
    }
}

#[derive(Default)]
struct ProcessStart {
    incarnation: Option<String>,
    preparation_cancelled: tokio_util::sync::CancellationToken,
}

// Release the exact provisional entry if a connection disappears during spawn.
// Capture the runtime at admission: an already-polled start future can be
// dropped outside an entered runtime. Cleanup still requires that runtime alive.
pub(crate) struct ProcessStartReservation {
    inner: Arc<Inner>,
    process_id: ProcessId,
    start: Arc<ProcessStart>,
    runtime: tokio::runtime::Handle,
    active: bool,
}

#[derive(Clone)]
pub(crate) struct ProcessStartIdentity(Arc<ProcessStart>);

impl ProcessStartIdentity {
    fn matches(&self, entry: &ProcessEntry) -> bool {
        let current = match entry {
            ProcessEntry::Starting(start) => start,
            ProcessEntry::Running(process) => &process.start_generation,
        };
        Arc::ptr_eq(current, &self.0)
    }

    pub(crate) fn has_incarnation(&self, incarnation: &str) -> bool {
        self.0.incarnation.as_deref() == Some(incarnation)
    }
}

impl ProcessStartReservation {
    pub(crate) fn identity(&self) -> ProcessStartIdentity {
        ProcessStartIdentity(Arc::clone(&self.start))
    }
}

impl Drop for ProcessStartReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.start.preparation_cancelled.cancel();
        let inner = Arc::clone(&self.inner);
        let process_id = self.process_id.clone();
        let start = Arc::clone(&self.start);
        self.runtime.spawn(async move {
            let mut processes = inner.processes.lock().await;
            if matches!(
                processes.get(&process_id),
                Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &start)
            ) {
                processes.remove(&process_id);
            }
        });
    }
}

enum ProcessEntry {
    Starting(Arc<ProcessStart>),
    Running(Box<RunningProcess>),
}

struct Inner {
    notifications: std::sync::RwLock<Option<RpcNotificationSender>>,
    forwards_notifications: bool,
    notification_shutdown: tokio_util::sync::CancellationToken,
    processes: Mutex<HashMap<ProcessId, ProcessEntry>>,
    telemetry: ExecServerTelemetry,
}

#[derive(Clone)]
pub(crate) struct LocalProcess {
    inner: Arc<Inner>,
    runtime_paths: Option<ExecServerRuntimePaths>,
}

struct LocalExecProcess {
    identity: ProcessStartIdentity,
    process_id: ProcessId,
    backend: LocalProcess,
    wake_tx: watch::Sender<u64>,
    events: ExecProcessEventLog,
}

impl Default for LocalProcess {
    fn default() -> Self {
        Self::with_runtime_paths(None, ExecServerTelemetry::default(), None)
    }
}

impl LocalProcess {
    pub(crate) fn with_local_runtime_paths(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self::with_runtime_paths(None, ExecServerTelemetry::default(), Some(runtime_paths))
    }

    pub(crate) fn new(
        notifications: RpcNotificationSender,
        telemetry: ExecServerTelemetry,
        runtime_paths: ExecServerRuntimePaths,
    ) -> Self {
        Self::with_runtime_paths(Some(notifications), telemetry, Some(runtime_paths))
    }

    fn with_runtime_paths(
        notifications: Option<RpcNotificationSender>,
        telemetry: ExecServerTelemetry,
        runtime_paths: Option<ExecServerRuntimePaths>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                forwards_notifications: notifications.is_some(),
                notifications: std::sync::RwLock::new(notifications),
                notification_shutdown: tokio_util::sync::CancellationToken::new(),
                processes: Mutex::new(HashMap::new()),
                telemetry,
            }),
            runtime_paths,
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.inner.notification_shutdown.cancel();
        let remaining = {
            let mut processes = self.inner.processes.lock().await;
            processes
                .drain()
                .filter_map(|(process_id, process)| match process {
                    ProcessEntry::Starting(start) => {
                        start.preparation_cancelled.cancel();
                        None
                    }
                    ProcessEntry::Running(process) => {
                        process.termination_requested.store(true, Ordering::Release);
                        Some((process_id, process))
                    }
                })
                .collect::<Vec<_>>()
        };
        for (process_id, process) in remaining {
            match process.session.terminate() {
                Ok(()) => tracing::info!(%process_id, cleanup_outcome = "termination_requested", "process shutdown cleanup"),
                Err(error) => {
                    tracing::error!(%process_id, %error, cleanup_outcome = "unresolved", "process shutdown cleanup failed");
                    // Retain the failed identity for inspection or a later
                    // cleanup attempt, without replacing a newer incarnation.
                    self.inner.processes.lock().await.entry(process_id)
                        .or_insert(ProcessEntry::Running(process));
                }
            }
        }
    }

    pub(crate) fn set_notification_sender(&self, notifications: Option<RpcNotificationSender>) {
        let mut notification_sender = self
            .inner
            .notifications
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *notification_sender = notifications;
    }

    pub(crate) async fn reserve_start(
        &self,
        process_id: &ProcessId,
    ) -> Result<ProcessStartReservation, JSONRPCErrorError> {
        self.reserve_start_owned(process_id, None).await
    }

    pub(crate) async fn reserve_start_owned(
        &self,
        process_id: &ProcessId,
        incarnation: Option<String>,
    ) -> Result<ProcessStartReservation, JSONRPCErrorError> {
        if incarnation
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 256)
        {
            return Err(invalid_params(
                "incarnation must contain 1..=256 bytes".into(),
            ));
        }
        let process_id = process_id.clone();
        let start = Arc::new(ProcessStart { incarnation, ..Default::default() });
        let mut reservation = ProcessStartReservation {
            inner: Arc::clone(&self.inner),
            process_id: process_id.clone(),
            start: Arc::clone(&start),
            runtime: tokio::runtime::Handle::current(),
            active: false,
        };
        {
            let mut process_map = self.inner.processes.lock().await;
            if self.inner.notification_shutdown.is_cancelled() {
                return Err(invalid_request("process backend is shutting down".into()));
            }
            if process_map.contains_key(&process_id) {
                return Err(crate::rpc::process_start_refused(format!(
                    "process {process_id} already exists"
                )));
            }
            process_map.insert(
                process_id.clone(),
                ProcessEntry::Starting(Arc::clone(&start)),
            );
            reservation.active = true;
        }

        Ok(reservation)
    }

    pub(crate) async fn process_start_identity(
        &self,
        process_id: &ProcessId,
    ) -> Option<ProcessStartIdentity> {
        self.inner
            .processes
            .lock()
            .await
            .get(process_id)
            .map(|entry| match entry {
                ProcessEntry::Starting(start) => ProcessStartIdentity(Arc::clone(start)),
                ProcessEntry::Running(process) => {
                    ProcessStartIdentity(Arc::clone(&process.start_generation))
                }
            })
    }

    pub(crate) async fn owned_identity(
        &self,
        process_id: &ProcessId,
        incarnation: Option<&str>,
    ) -> Result<Option<ProcessStartIdentity>, JSONRPCErrorError> {
        let Some(incarnation) = incarnation else {
            return Ok(None);
        };
        let identity = self
            .process_start_identity(process_id)
            .await
            .filter(|identity| identity.0.incarnation.as_deref() == Some(incarnation))
            .ok_or_else(|| invalid_request("process handle has expired or was replaced".into()))?;
        Ok(Some(identity))
    }

    async fn start_process(
        &self,
        params: ExecParams,
    ) -> Result<(ExecResponse, watch::Sender<u64>, ExecProcessEventLog), JSONRPCErrorError> {
        let reservation = self.reserve_start(&params.process_id).await?;
        self.start_process_reserved(params, reservation).await
    }

    async fn start_process_reserved(
        &self,
        params: ExecParams,
        mut reservation: ProcessStartReservation,
    ) -> Result<(ExecResponse, watch::Sender<u64>, ExecProcessEventLog), JSONRPCErrorError> {
        let process_id = params.process_id.clone();
        let start = Arc::clone(&reservation.start);
        if process_id != reservation.process_id || !Arc::ptr_eq(&self.inner, &reservation.inner) {
            return Err(invalid_request(
                "process start reservation does not match its owner".into(),
            ));
        }
        // Sandbox preparation can walk filesystem trees to resolve deny globs.
        // Reserve the process ID first so termination can cancel queued preparation.
        let runtime_paths = self.runtime_paths.clone();
        let preparation_cancelled = start.preparation_cancelled.clone();
        let preparation = tokio::task::spawn_blocking(move || {
            if preparation_cancelled.is_cancelled() {
                return Err(invalid_request(format!("process {} start was cancelled", params.process_id)));
            }
            prepare_exec_request_cancellable(&params, child_env(&params), runtime_paths.as_ref(), &preparation_cancelled)
                .map(|prepared| (prepared, params.tty, params.pipe_stdin))
        })
        .await
        .map_err(|err| internal_error(format!("process preparation task failed: {err}")))
        .and_then(std::convert::identity);
        let (prepared, tty, pipe_stdin) = match preparation {
            Ok(prepared) => prepared,
            Err(err) => {
                let mut process_map = self.inner.processes.lock().await;
                if matches!(
                    process_map.get(&process_id),
                    Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &start)
                ) {
                    process_map.remove(&process_id);
                }
                reservation.active = false;
                return Err(err);
            }
        };
        let sandbox = prepared.sandbox;
        {
            let process_map = self.inner.processes.lock().await;
            if !matches!(
                process_map.get(&process_id),
                Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &start)
            ) {
                return Err(invalid_request(format!(
                    "process {process_id} start was cancelled"
                )));
            }
        }

        let spawned_result = prepared.spawn(tty, pipe_stdin).await;
        let spawned = match spawned_result {
            Ok(spawned) => spawned,
            Err(err) => {
                let mut process_map = self.inner.processes.lock().await;
                if matches!(
                    process_map.get(&process_id),
                    Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &start)
                ) {
                    process_map.remove(&process_id);
                }
                reservation.active = false;
                return Err(internal_error(err.to_string()));
            }
        };

        let output_notify = Arc::new(Notify::new());
        let (wake_tx, _wake_rx) = watch::channel(0);
        let events = ExecProcessEventLog::new(
            PROCESS_EVENT_CHANNEL_CAPACITY,
            RETAINED_OUTPUT_BYTES_PER_PROCESS,
        );
        let termination_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let metrics;
        {
            let mut process_map = self.inner.processes.lock().await;
            if !matches!(
                process_map.get(&process_id),
                Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &start)
            ) {
                drop(process_map);
                let _ = spawned.session.terminate();
                reservation.active = false;
                return Err(invalid_request(format!(
                    "process {process_id} start was cancelled"
                )));
            }
            metrics = self.inner.telemetry.process_started();
            process_map.insert(
                process_id.clone(),
                ProcessEntry::Running(Box::new(RunningProcess {
                    start_generation: Arc::clone(&start),
                    session: spawned.session,
                    tty,
                    pipe_stdin,
                    accepted_stdin_write_ids: Arc::new(
                        Mutex::new(AcceptedStdinWriteIds::default()),
                    ),
                    output: VecDeque::new(),
                    retained_bytes: 0,
                    last_evicted_seq: 0,
                    next_seq: 1,
                    exit_seq: None,
                    exit_code: None,
                    wake_tx: wake_tx.clone(),
                    events: events.clone(),
                    output_notify: Arc::clone(&output_notify),
                    open_streams: 2,
                    closed: false,
                    finalizing: false,
                    termination_requested: Arc::clone(&termination_requested),
                    failure: None,
                    sandbox,
                    sandbox_denied: false,
                })),
            );
            reservation.active = false;
        }
        if self.inner.forwards_notifications {
            tokio::spawn(forward_process_notifications(
                process_id.clone(), events.clone(), events.subscribe(), Arc::downgrade(&self.inner),
                self.inner.notification_shutdown.clone(),
            ));
        }
        tokio::spawn(stream_output(
            process_id.clone(),
            if tty {
                ExecOutputStream::Pty
            } else {
                ExecOutputStream::Stdout
            },
            spawned.stdout_rx,
            Arc::clone(&self.inner),
            Arc::clone(&output_notify),
        ));
        tokio::spawn(stream_output(
            process_id.clone(),
            if tty {
                ExecOutputStream::Pty
            } else {
                ExecOutputStream::Stderr
            },
            spawned.stderr_rx,
            Arc::clone(&self.inner),
            Arc::clone(&output_notify),
        ));
        tokio::spawn(watch_exit(
            process_id.clone(),
            spawned.exit_rx,
            Arc::clone(&self.inner),
            output_notify,
            metrics,
            termination_requested,
        ));

        Ok((ExecResponse { process_id }, wake_tx, events))
    }

    pub(crate) async fn exec(&self, params: ExecParams) -> Result<ExecResponse, JSONRPCErrorError> {
        self.start_process(params)
            .await
            .map(|(response, _, _)| response)
    }

    pub(crate) async fn exec_reserved(
        &self,
        params: ExecParams,
        reservation: ProcessStartReservation,
    ) -> Result<ExecResponse, JSONRPCErrorError> {
        self.start_process_reserved(params, reservation)
            .await
            .map(|(response, _, _)| response)
    }


    pub(crate) async fn exec_read_bound(
        &self,
        params: ReadParams,
        identity: Option<&ProcessStartIdentity>,
    ) -> Result<ReadResponse, JSONRPCErrorError> {
        let after_seq = params.after_seq.unwrap_or(0);
        let max_bytes = params.max_bytes.unwrap_or(usize::MAX);
        let wait = Duration::from_millis(params.wait_ms.unwrap_or(0));
        let deadline = tokio::time::Instant::now() + wait;

        loop {
            let (mut response, output_notify, chunks) = {
                let process_map = self.inner.processes.lock().await;
                let process = process_map.get(&params.process_id).ok_or_else(|| {
                    invalid_request(format!("unknown process id {}", params.process_id))
                })?;
                if identity.is_some_and(|identity| !identity.matches(process)) {
                    return Err(invalid_request(
                        "process handle has expired or was replaced".into(),
                    ));
                }
                let ProcessEntry::Running(process) = process else {
                    return Err(invalid_request(format!(
                        "process id {} is starting",
                        params.process_id
                    )));
                };
                if after_seq >= process.next_seq {
                    return Err(invalid_params(format!(
                        "afterSeq {after_seq} exceeds the last available sequence {} for process {}",
                        process.next_seq - 1,
                        params.process_id
                    )));
                }

                let mut chunks = Vec::new();
                let mut total_bytes = 0;
                let mut next_seq = process.next_seq;
                let start = process
                    .output
                    .partition_point(|chunk| chunk.seq <= after_seq);
                for retained in process.output.range(start..) {
                    let chunk_len = retained.bytes().len();
                    if !chunks.is_empty() && total_bytes + chunk_len > max_bytes {
                        break;
                    }
                    total_bytes += chunk_len;
                    chunks.push(retained.clone());
                    next_seq = retained.seq + 1;
                    if total_bytes >= max_bytes {
                        break;
                    }
                }
                if params.max_bytes.is_none() {
                    next_seq = process.next_seq;
                }
                (
                    ReadResponse {
                        chunks: Vec::new(),
                        next_seq,
                        exited: process.exit_code.is_some(),
                        exit_code: process.exit_code,
                        closed: process.closed,
                        failure: process.failure.clone(),
                        output_gap: (after_seq < process.last_evicted_seq).then_some(
                            crate::protocol::ProcessOutputGap {
                                through_seq: process.last_evicted_seq,
                                exit_seq: process.exit_seq,
                            },
                        ),
                        sandbox_denied: process.sandbox_denied,
                    },
                    // Register before releasing the state lock: notify_waiters does
                    // not retain a permit for a future created after the update.
                    Arc::clone(&process.output_notify).notified_owned(),
                    chunks,
                )
            };
            // Retain an immutable sequence-consistent snapshot under the map
            // lock; copy payload bytes only after unrelated process controls can run.
            response.chunks = chunks.into_iter().map(|retained| {
                let ExecProcessEvent::Output(output) = Arc::unwrap_or_clone(retained.event) else {
                    unreachable!("retained output contains only output events");
                };
                output
            }).collect();


            let has_new_terminal_event =
                response.exited && after_seq < response.next_seq.saturating_sub(1);
            if !response.chunks.is_empty()
                || response.output_gap.is_some()
                || response.closed
                || response.failure.is_some()
                || has_new_terminal_event
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(response);
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(response);
            }
            let _ = tokio::time::timeout(remaining, output_notify).await;
        }
    }



    pub(crate) async fn exec_write_bound(
        &self,
        params: WriteParams,
        identity: Option<&ProcessStartIdentity>,
    ) -> Result<WriteResponse, JSONRPCErrorError> {
        if params.write_id.is_empty() || params.write_id.len() > MAX_STDIN_WRITE_ID_BYTES {
            return Err(invalid_params(
                "writeId must contain 1..=256 bytes; input was not accepted".to_string(),
            ));
        }
        if params.chunk.0.len() > MAX_STDIN_CHUNK_BYTES {
            return Err(invalid_params(
                "stdin chunk exceeds the 1 MiB limit; input was not accepted".to_string(),
            ));
        }

        let (writer_tx, accepted_stdin_write_ids) = {
            let process_map = self.inner.processes.lock().await;
            let Some(process) = process_map.get(&params.process_id) else {
                return Ok(WriteResponse {
                    status: WriteStatus::UnknownProcess,
                });
            };
            if identity.is_some_and(|identity| !identity.matches(process)) {
                return Err(invalid_request(
                    "process handle has expired or was replaced".into(),
                ));
            }
            let ProcessEntry::Running(process) = process else {
                return Ok(WriteResponse {
                    status: WriteStatus::Starting,
                });
            };
            if !process.tty && !process.pipe_stdin {
                return Ok(WriteResponse {
                    status: WriteStatus::StdinClosed,
                });
            }
            (
                process.session.writer_sender(),
                Arc::clone(&process.accepted_stdin_write_ids),
            )
        };

        if accepted_stdin_write_ids
            .lock()
            .await
            .already_accepted(&params.write_id)?
        {
            return Ok(WriteResponse {
                status: WriteStatus::Accepted,
            });
        }

        let permit = writer_tx
            .reserve()
            .await
            .map_err(|_| internal_error("failed to write to process stdin".to_string()))?;
        let mut accepted_stdin_write_ids = accepted_stdin_write_ids.lock().await;
        if accepted_stdin_write_ids.already_accepted(&params.write_id)? {
            return Ok(WriteResponse {
                status: WriteStatus::Accepted,
            });
        }

        // After this synchronous send, record the write id before any further await.
        // Otherwise a cancelled RPC handler could retry and write the same bytes again.
        // Typed local callers can supply a short Vec with an arbitrarily large
        // spare capacity. The accepted queue owns only the bounded payload.
        permit.send(params.chunk.into_inner().into_boxed_slice().into_vec());
        accepted_stdin_write_ids.remember(params.write_id.into_boxed_str().into_string());

        Ok(WriteResponse {
            status: WriteStatus::Accepted,
        })
    }

    pub(crate) async fn signal_process_bound(
        &self,
        params: SignalParams,
        identity: Option<&ProcessStartIdentity>,
    ) -> Result<SignalResponse, JSONRPCErrorError> {
        {
            let process_map = self.inner.processes.lock().await;
            if identity.is_some_and(|identity| {
                !process_map
                    .get(&params.process_id)
                    .is_some_and(|entry| identity.matches(entry))
            }) {
                return Err(invalid_request(
                    "process handle has expired or was replaced".into(),
                ));
            }
            match process_map.get(&params.process_id) {
                Some(ProcessEntry::Running(process)) => {
                    if process.exit_code.is_some() {
                        return Ok(SignalResponse {});
                    }
                    process
                        .session
                        .signal(pty_process_signal(params.signal))
                        .map_err(|err| internal_error(format!("failed to signal process: {err}")))?
                }
                Some(ProcessEntry::Starting(_)) | None => {}
            }
        }

        Ok(SignalResponse {})
    }


    pub(crate) async fn terminate_admitted(
        &self,
        params: TerminateParams,
        identity: Option<ProcessStartIdentity>,
    ) -> Result<TerminateResponse, JSONRPCErrorError> {
        let running = {
            let mut process_map = self.inner.processes.lock().await;
            if let Some(identity) = identity {
                let matches = match process_map.get(&params.process_id) {
                    Some(ProcessEntry::Starting(start)) => Arc::ptr_eq(start, &identity.0),
                    Some(ProcessEntry::Running(process)) => {
                        Arc::ptr_eq(&process.start_generation, &identity.0)
                    }
                    None => false,
                };
                if !matches {
                    return Ok(TerminateResponse { running: false });
                }
            }
            match process_map.get_mut(&params.process_id) {
                Some(ProcessEntry::Running(process)) => {
                    if process.exit_code.is_some() {
                        if !process.closed {
                            process.termination_requested.store(true, Ordering::Release);
                            process.session.finish();
                        }
                        return Ok(TerminateResponse { running: false });
                    }
                    if !process.termination_requested.load(Ordering::Acquire) {
                        process.session.request_terminate().map_err(|err| {
                            internal_error(format!("failed to terminate process: {err}"))
                        })?;
                        process.termination_requested.store(true, Ordering::Release);
                    }
                    true
                }
                Some(ProcessEntry::Starting(start)) => {
                    start.preparation_cancelled.cancel();
                    process_map.remove(&params.process_id);
                    true
                }
                None => false,
            }
        };

        Ok(TerminateResponse { running })
    }
}

fn child_env(params: &ExecParams) -> HashMap<String, String> {
    let mut env = params
        .env_policy
        .as_ref()
        .map_or_else(HashMap::new, |env_policy| {
            shell_environment::create_env(
                &shell_environment_policy(env_policy),
                /*thread_id*/ None,
            )
        });
    shell_environment::apply_env_overlay(&mut env, params.env.clone());
    env
}

fn shell_environment_policy(env_policy: &ExecEnvPolicy) -> ShellEnvironmentPolicy {
    ShellEnvironmentPolicy {
        inherit: env_policy.inherit.clone(),
        ignore_default_excludes: env_policy.ignore_default_excludes,
        exclude: env_policy
            .exclude
            .iter()
            .map(|pattern| EnvironmentVariablePattern::new_case_insensitive(pattern))
            .collect(),
        r#set: env_policy.r#set.clone(),
        include_only: env_policy
            .include_only
            .iter()
            .map(|pattern| EnvironmentVariablePattern::new_case_insensitive(pattern))
            .collect(),
        use_profile: false,
    }
}

impl LocalProcess {
    async fn start(&self, params: ExecParams) -> Result<StartedExecProcess, ExecServerError> {
        let reservation = self
            .reserve_start(&params.process_id)
            .await
            .map_err(map_handler_error)?;
        let identity = reservation.identity();
        let (response, wake_tx, events) = self
            .start_process_reserved(params, reservation)
            .await
            .map_err(map_handler_error)?;
        Ok(StartedExecProcess {
            process: Arc::new(LocalExecProcess {
                identity,
                process_id: response.process_id,
                backend: self.clone(),
                wake_tx,
                events,
            }),
        })
    }
}

impl ExecBackend for LocalProcess {
    fn start(&self, params: ExecParams) -> ExecBackendFuture<'_> {
        Box::pin(LocalProcess::start(self, params))
    }
}

impl LocalExecProcess {
    async fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> Result<ReadResponse, ExecServerError> {
        self.backend
            .exec_read_bound(
                ReadParams {
                    process_id: self.process_id.clone(),
                    after_seq,
                    max_bytes,
                    wait_ms,
                },
                Some(&self.identity),
            )
            .await
            .map_err(map_handler_error)
    }

    async fn write(&self, chunk: Vec<u8>) -> Result<WriteResponse, ExecServerError> {
        self.backend
            .exec_write_bound(
                WriteParams {
                    process_id: self.process_id.clone(),
                    chunk: chunk.into(),
                    write_id: format!(
                        "local-{}",
                        NEXT_LOCAL_STDIN_WRITE_ID.fetch_add(1, Ordering::Relaxed)
                    ),
                },
                Some(&self.identity),
            )
            .await
            .map_err(map_handler_error)
    }

    async fn signal(&self, signal: ProcessSignal) -> Result<(), ExecServerError> {
        self.backend
            .signal_process_bound(
                SignalParams {
                    process_id: self.process_id.clone(),
                    signal,
                },
                Some(&self.identity),
            )
            .await
            .map_err(map_handler_error)?;
        Ok(())
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        tokio::time::timeout(PROCESS_TERMINATION_TIMEOUT, async {
            let mut wake = self.wake_tx.subscribe();
            loop {
                let response = self
                    .backend
                    .terminate_admitted(
                        TerminateParams {
                            process_id: self.process_id.clone(),
                        },
                        Some(self.identity.clone()),
                    )
                    .await
                    .map_err(map_handler_error)?;
                if !response.running {
                    return Ok(());
                }
                let _ = wake.changed().await;
            }
        })
        .await
        .map_err(|_| {
            ExecServerError::Protocol("timed out confirming local process termination".into())
        })?
    }
}

impl ExecProcess for LocalExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        self.events.subscribe()
    }

    fn read(
        &self,
        after_seq: Option<u64>,
        max_bytes: Option<usize>,
        wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(LocalExecProcess::read(self, after_seq, max_bytes, wait_ms))
    }

    fn write(&self, chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(LocalExecProcess::write(self, chunk))
    }

    // `exec_write` finishes every await before its synchronous send, so a
    // dropped write never reaches the stdin queue.
    fn cancelled_write_is_unsent(&self) -> bool {
        true
    }

    fn signal(&self, signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(LocalExecProcess::signal(self, signal))
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(LocalExecProcess::terminate(self))
    }
}


fn pty_process_signal(signal: ProcessSignal) -> PtyProcessSignal {
    match signal {
        ProcessSignal::Interrupt => PtyProcessSignal::Interrupt,
    }
}

fn map_handler_error(error: JSONRPCErrorError) -> ExecServerError {
    ExecServerError::Server {
        code: error.code,
        message: error.message,
        data: error.data,
    }
}

async fn stream_output(
    process_id: ProcessId,
    stream: ExecOutputStream,
    mut receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
    inner: Arc<Inner>,
    output_notify: Arc<Notify>,
) {
    while let Some(chunk) = receiver.recv().await {
        let chunk_bytes = chunk.len();
        {
            let mut processes = inner.processes.lock().await;
            let Some(entry) = processes.get_mut(&process_id) else {
                break;
            };
            let ProcessEntry::Running(process) = entry else {
                break;
            };
            let seq = process.next_seq;
            process.next_seq += 1;
            let event = Arc::new(ExecProcessEvent::Output(ProcessOutputChunk {
                seq, stream, chunk: chunk.into(),
            }));
            process.retained_bytes += chunk_bytes;
            process.output.push_back(RetainedOutputChunk {
                seq,
                stream,
                event: Arc::clone(&event),
            });
            while process.retained_bytes > RETAINED_OUTPUT_BYTES_PER_PROCESS {
                let Some(evicted) = process.output.pop_front() else {
                    break;
                };
                // A capture-loss marker may already declare a later hole; front
                // eviction must never move the declared gap backwards past it.
                process.last_evicted_seq = process.last_evicted_seq.max(evicted.seq);
                process.retained_bytes = process.retained_bytes.saturating_sub(evicted.bytes().len());
            }
            let _ = process.wake_tx.send(seq);
            process
                .events
                .publish_shared(event);
        }
        output_notify.notify_waiters();
    }

    finish_output_stream(process_id, stream, inner).await;
}

async fn watch_exit(
    process_id: ProcessId,
    exit_rx: tokio::sync::oneshot::Receiver<i32>,
    inner: Arc<Inner>,
    output_notify: Arc<Notify>,
    metrics: ProcessMetricGuard,
    termination_requested: Arc<std::sync::atomic::AtomicBool>,
) {
    let exit = exit_rx.await;
    let observed_at = std::time::Instant::now();
    let exit_code = match exit {
        Ok(exit_code) => exit_code,
        Err(_) => {
            let message = "process exit observation lost".to_string();
            {
                let mut processes = inner.processes.lock().await;
                if let Some(ProcessEntry::Running(process)) = processes.get_mut(&process_id) {
                    process.failure = Some(message.clone());
                    process
                        .events
                        .publish(ExecProcessEvent::Failed(message.clone()));
                    let _ = process.wake_tx.send(process.next_seq);
                }
            }
            output_notify.notify_waiters();
            // Do not let a blocked notification sink extend the observation lease.
            drop(metrics);
            // Dropping the observation lease is unknown, not a confirmed exit.
            return;
        }
    };
    let (tty, requested) = {
        let mut processes = inner.processes.lock().await;
        let tty = match processes.get_mut(&process_id) {
            Some(ProcessEntry::Running(process)) => process.tty,
            Some(ProcessEntry::Starting(_)) | None => false,
        };
        (tty, termination_requested.load(Ordering::Acquire))
    };
    metrics.finish(
        if requested {
            "terminated"
        } else if exit_code == 0 {
            "success"
        } else {
            "error"
        },
        observed_at,
    );
    if tty {
        let processes = inner.processes.lock().await;
        if let Some(ProcessEntry::Running(process)) = processes.get(&process_id) {
            process.session.release_pty_after_exit();
        }
    }
    {
        let mut processes = inner.processes.lock().await;
        if let Some(ProcessEntry::Running(process)) = processes.get_mut(&process_id) {
            let seq = process.next_seq;
            process.next_seq += 1;
            process.exit_code = Some(exit_code);
            process.exit_seq = Some(seq);
            let _ = process.wake_tx.send(seq);
            process.events.publish(ExecProcessEvent::Exited {
                seq,
                exit_code,
                sandbox_denied: (process.sandbox == SandboxType::None).then_some(false),
            });
        }
    }
    output_notify.notify_waiters();

    maybe_emit_closed(process_id, Arc::clone(&inner)).await;
}

async fn finish_output_stream(process_id: ProcessId, stream: ExecOutputStream, inner: Arc<Inner>) {
    let gap = {
        let mut processes = inner.processes.lock().await;
        let Some(ProcessEntry::Running(process)) = processes.get_mut(&process_id) else {
            return;
        };
        let capture = process.session.output_capture();
        let complete = match stream {
            ExecOutputStream::Stdout => capture.stdout_complete(),
            ExecOutputStream::Stderr => capture.stderr_complete(),
            // Both terminal receivers report `Pty`, and one is a placeholder
            // that closes at spawn while the terminal is still producing output.
            // Judge the shared capture only once the last receiver has drained.
            ExecOutputStream::Pty => process.open_streams > 1 || capture.complete(),
        };
        let mut gap = None;
        if !complete {
            // Capture loss has an ordered position of its own, even if no
            // bytes were received. Reuse the existing live/read gap contract.
            let through_seq = process.next_seq;
            process.next_seq += 1;
            process.last_evicted_seq = through_seq;
            process
                .events
                .publish(ExecProcessEvent::OutputGap { through_seq });
            let _ = process.wake_tx.send(through_seq);
            gap = Some((through_seq, Arc::clone(&process.output_notify)));
        }
        if process.open_streams > 0 {
            process.open_streams -= 1;
        }
        gap
    };
    if let Some((through_seq, output_notify)) = gap {
        // Remote sessions publish strictly in sequence order. Without this
        // notification every later event, including close, waits forever
        // behind a sequence that was never sent.
        output_notify.notify_waiters();
        let _ = through_seq;
    }

    maybe_emit_closed(process_id, inner).await;
}

async fn maybe_emit_closed(process_id: ProcessId, inner: Arc<Inner>) {
    let (sandbox, exit_code, output, identity, complete_output) = {
        let mut processes = inner.processes.lock().await;
        let Some(ProcessEntry::Running(process)) = processes.get_mut(&process_id) else {
            return;
        };
        if process.closed || process.finalizing || process.open_streams != 0 {
            return;
        }
        let Some(exit_code) = process.exit_code else {
            return;
        };
        process.finalizing = true;
        (
            process.sandbox,
            exit_code,
            // Unsandboxed and successful exits never classify output. Avoid copying up
            // to the retention limit while holding the shared process-map lock.
            if process.sandbox == SandboxType::None || exit_code == 0 {
                VecDeque::new()
            } else {
                process.output.clone()
            },
            Arc::clone(&process.output_notify),
            process.last_evicted_seq == 0,
        )
    };
    // Output-dependent classification belongs after both streams drain, outside
    // the process-map mutex used by unrelated reads and process controls.
    let assessment = if sandbox == SandboxType::None {
        Some(false)
    } else if exit_code == 0 {
        complete_output.then_some(false)
    } else {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut aggregated = Vec::new();
        for chunk in output {
            match chunk.stream {
                ExecOutputStream::Stdout | ExecOutputStream::Pty => {
                    stdout.extend_from_slice(chunk.bytes())
                }
                ExecOutputStream::Stderr => stderr.extend_from_slice(chunk.bytes()),
            }
            aggregated.extend_from_slice(chunk.bytes());
        }
        let denied = is_likely_sandbox_denied(
            sandbox,
            &ExecToolCallOutput {
                exit_code,
                stdout: StreamOutput::new(String::from_utf8_lossy(&stdout).into_owned()),
                stderr: StreamOutput::new(String::from_utf8_lossy(&stderr).into_owned()),
                aggregated_output: StreamOutput::new(
                    String::from_utf8_lossy(&aggregated).into_owned(),
                ),
                ..Default::default()
            },
        );
        (denied || complete_output).then_some(denied)
    };
    let output_notify = {
        let mut processes = inner.processes.lock().await;
        let Some(ProcessEntry::Running(process)) = processes.get_mut(&process_id) else {
            return;
        };

        if process.closed || !Arc::ptr_eq(&process.output_notify, &identity) {
            return;
        }
        process.sandbox_denied = assessment.unwrap_or(false);
        // Native custody ends at confirmed root exit plus drained readers, not
        // at replay expiry. The record retains only replayable evidence.
        process.session.finish();
        process.closed = true;
        let seq = process.next_seq;
        process.next_seq += 1;
        let _ = process.wake_tx.send(seq);
        process.events.publish(ExecProcessEvent::Closed {
            seq,
            sandbox_denied: assessment,
        });
        Arc::clone(&process.output_notify)
    };

    output_notify.notify_waiters();
    let cleanup_process_id = process_id.clone();
    let cleanup_inner = Arc::clone(&inner);
    tokio::spawn(async move {
        tokio::time::sleep(EXITED_PROCESS_RETENTION).await;
        let mut processes = cleanup_inner.processes.lock().await;
        match processes.entry(cleanup_process_id) {
            Entry::Occupied(entry) => {
                if matches!(entry.get(), ProcessEntry::Running(process) if process.closed) {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => {}
        }
    });

}

// A blocked transport owns only bounded replay, never native draining/closure.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedOutputDelta<'a> {
    process_id: &'a ProcessId,
    seq: u64,
    stream: ExecOutputStream,
    chunk: &'a crate::protocol::ByteChunk,
}

async fn forward_process_notifications(
    process_id: ProcessId,
    events: ExecProcessEventLog,
    mut receiver: ExecProcessEventReceiver,
    inner: std::sync::Weak<Inner>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    // Local draining/closure never waits for this sink. Bound the independent
    // delivery task too, so a live but non-draining RPC connection cannot retain
    // a finished process's replay log indefinitely.
    const SEND_TIMEOUT: Duration = Duration::from_secs(30);
    let mut last_seq = 0;
    let mut pending = VecDeque::new();
    loop {
        let event = if let Some(event) = pending.pop_front() {
            event
        } else {
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                event = receiver.recv_shared() => event,
            };
            match event {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let Some(owner) = inner.upgrade() else { return; };
                    let processes = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return,
                        processes = owner.processes.lock() => processes,
                    };
                    let Some(ProcessEntry::Running(process)) = processes.get(&process_id) else { return; };
                    if !events.same_instance(&process.events) {
                        return; // Never mix a reused process ID into this stream.
                    }
                    receiver = events.subscribe();
                    // The output replay is byte-bounded. Root exit is retained
                    // separately because later output can evict it from the
                    // event-count-bounded live log. Restore both in seq order.
                    let retained = process.output.iter().rev()
                        .filter(|chunk| chunk.seq > last_seq.max(process.last_evicted_seq))
                        .take(PROCESS_EVENT_CHANNEL_CAPACITY)
                        .cloned()
                        .collect::<Vec<_>>();
                    let notification_gap = retained.iter().map(|chunk| chunk.seq.saturating_sub(1))
                        .min().unwrap_or(0).max(process.last_evicted_seq);
                    let mut replay = Vec::new();
                    // A retained exit can be the sequence immediately before
                    // the replay suffix. Do not advance the gap cursor over it:
                    // the normal duplicate fence would then discard the exit.
                    if let (Some(seq), Some(_)) = (process.exit_seq, process.exit_code)
                        && seq > last_seq && seq <= notification_gap
                    {
                        let through_seq = seq.saturating_sub(1);
                        if through_seq > last_seq {
                            replay.push((through_seq, ExecProcessEvent::OutputGap { through_seq }));
                        }
                    }
                    if notification_gap > last_seq && process.exit_seq != Some(notification_gap) {
                        replay.push((notification_gap, ExecProcessEvent::OutputGap {
                            through_seq: notification_gap,
                        }));
                    }
                    if let (Some(seq), Some(exit_code)) = (process.exit_seq, process.exit_code)
                        && seq > last_seq
                    {
                        replay.push((seq, ExecProcessEvent::Exited {
                            seq, exit_code,
                            sandbox_denied: (process.sandbox == SandboxType::None).then_some(false),
                        }));
                    }
                    if process.closed {
                        let seq = process.next_seq.saturating_sub(1);
                        let assessment = if process.sandbox_denied { Some(true) }
                            else if process.sandbox == SandboxType::None { Some(false) }
                            else { None };
                        replay.push((seq, ExecProcessEvent::Closed { seq, sandbox_denied: assessment }));
                    } else if let Some(failure) = &process.failure {
                        replay.push((process.next_seq, ExecProcessEvent::Failed(failure.clone())));
                    }
                    drop(processes);
                    let mut replay = replay.into_iter().map(|(seq, event)| (seq, Arc::new(event))).collect::<Vec<_>>();
                    replay.extend(retained.into_iter().map(|chunk| (chunk.seq, chunk.event)));
                    replay.sort_by_key(|(seq, _)| *seq);
                    pending.extend(replay.into_iter().map(|(_, event)| event));
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        };
        let seq = event.seq();
        if seq.is_some_and(|seq| seq <= last_seq) { continue; }
        let terminal = matches!(event.as_ref(), ExecProcessEvent::Closed { .. } | ExecProcessEvent::Failed(_));
        let Some(owner) = inner.upgrade() else { return; };
        let notifications = notification_sender(&owner);
        drop(owner);
        let Some(notifications) = notifications else {
            if let Some(seq) = seq { last_seq = seq; }
            if terminal { return; }
            continue;
        };
        let (method, params) = match event.as_ref() {
            ExecProcessEvent::Output(chunk) => (crate::protocol::EXEC_OUTPUT_DELTA_METHOD,
                serde_json::to_value(BorrowedOutputDelta { process_id: &process_id,
                    seq: chunk.seq, stream: chunk.stream, chunk: &chunk.chunk })),
            ExecProcessEvent::OutputGap { through_seq } => (crate::protocol::EXEC_OUTPUT_GAP_METHOD,
                serde_json::to_value(crate::protocol::ExecOutputGapNotification { process_id: process_id.clone(), through_seq: *through_seq })),
            ExecProcessEvent::Exited { seq, exit_code, sandbox_denied } => (crate::protocol::EXEC_EXITED_METHOD,
                serde_json::to_value(ExecExitedNotification { process_id: process_id.clone(), seq: *seq, exit_code: *exit_code, sandbox_denied: *sandbox_denied })),
            ExecProcessEvent::Closed { seq, sandbox_denied } => (EXEC_CLOSED_METHOD,
                serde_json::to_value(ExecClosedNotification { process_id: process_id.clone(), seq: *seq, sandbox_denied: *sandbox_denied })),
            ExecProcessEvent::Failed(message) => (crate::protocol::EXEC_FAILED_METHOD,
                serde_json::to_value(crate::protocol::ExecFailedNotification { process_id: process_id.clone(), message: message.clone() })),
        };
        let Ok(params) = params else { return; };
        {
            let result = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                result = tokio::time::timeout(SEND_TIMEOUT, notifications.notify_value(method, params)) => result,
            };
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!(%process_id, ?result, last_seq, replay_required = true,
                    "process notification delivery failed; retained reads remain authoritative");
                return;
            }
        }
        if let Some(seq) = seq { last_seq = seq; }
        if terminal { return; }
    }
}

fn notification_sender(inner: &Inner) -> Option<RpcNotificationSender> {
    inner
        .notifications
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

