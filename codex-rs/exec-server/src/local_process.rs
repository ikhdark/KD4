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
#[cfg(test)]
use tokio::sync::mpsc;
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
#[cfg(test)]
const NOTIFICATION_CHANNEL_CAPACITY: usize = 256;
const PROCESS_EVENT_CHANNEL_CAPACITY: usize = 256;
const RETAINED_STDIN_WRITE_IDS_PER_PROCESS: usize = 4096;
// Together with the existing 128-item writer queue, cap accepted stdin at
// 129 MiB including the currently blocked write. IDs retain at most 2 MiB.
const MAX_STDIN_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_STDIN_WRITE_ID_BYTES: usize = 256;
const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_LOCAL_STDIN_WRITE_ID: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
const EXITED_PROCESS_RETENTION: Duration = Duration::from_millis(25);
#[cfg(not(test))]
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
    #[cfg(test)]
    read_wait_started: tokio::sync::Notify,
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
                #[cfg(test)]
                read_wait_started: tokio::sync::Notify::new(),
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

    #[cfg(test)]
    pub(crate) async fn exec_read(
        &self,
        params: ReadParams,
    ) -> Result<ReadResponse, JSONRPCErrorError> {
        self.exec_read_bound(params, None).await
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

            // Exercise updates between snapshotting and polling the waiter in tests.
            #[cfg(test)]
            tokio::task::yield_now().await;

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
            #[cfg(test)]
            self.inner.read_wait_started.notify_one();
            let _ = tokio::time::timeout(remaining, output_notify).await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_read_wait(&self) {
        self.inner.read_wait_started.notified().await;
    }

    #[cfg(test)]
    pub(crate) async fn exec_write(
        &self,
        params: WriteParams,
    ) -> Result<WriteResponse, JSONRPCErrorError> {
        self.exec_write_bound(params, None).await
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

    #[cfg(test)]
    pub(crate) async fn terminate_process(
        &self,
        params: TerminateParams,
    ) -> Result<TerminateResponse, JSONRPCErrorError> {
        self.terminate_admitted(params, None).await
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

// Unbound by-id helpers for tests. Production handles go through
// `LocalExecProcess`, which binds every operation to its start identity.
#[cfg(test)]
impl LocalProcess {
    async fn write(
        &self,
        process_id: &ProcessId,
        chunk: Vec<u8>,
    ) -> Result<WriteResponse, ExecServerError> {
        self.exec_write(WriteParams {
            process_id: process_id.clone(),
            chunk: chunk.into(),
            write_id: format!(
                "local-{}",
                NEXT_LOCAL_STDIN_WRITE_ID.fetch_add(1, Ordering::Relaxed)
            ),
        })
        .await
        .map_err(map_handler_error)
    }

    async fn terminate(&self, process_id: &ProcessId) -> Result<(), ExecServerError> {
        tokio::time::timeout(PROCESS_TERMINATION_TIMEOUT, async {
            loop {
                // Subscribe before checking/requesting termination so an exit between
                // that check and the await cannot be missed.
                let wake_rx = {
                    let processes = self.inner.processes.lock().await;
                    match processes.get(process_id) {
                        Some(ProcessEntry::Running(process)) => Some(process.wake_tx.subscribe()),
                        Some(ProcessEntry::Starting(_)) | None => None,
                    }
                };
                let response = self
                    .terminate_process(TerminateParams {
                        process_id: process_id.clone(),
                    })
                    .await
                    .map_err(map_handler_error)?;
                if !response.running {
                    return Ok(());
                }
                if let Some(mut wake_rx) = wake_rx {
                    let _ = wake_rx.changed().await;
                }
            }
        })
        .await
        .map_err(|_| {
            ExecServerError::Protocol(format!(
                "timed out confirming local process termination after {PROCESS_TERMINATION_TIMEOUT:?}"
            ))
        })?
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_otel::MetricsConfig;
    use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
    use codex_utils_path_uri::PathUri;
    use codex_utils_pty::ProcessDriver;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    use pretty_assertions::assert_eq;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    #[tokio::test]
    async fn lagged_notification_gap_does_not_discard_retained_exit() {
        let (notification_tx, mut notification_rx) = mpsc::channel(1);
        let backend = LocalProcess::with_runtime_paths(
            Some(RpcNotificationSender::new(notification_tx)),
            ExecServerTelemetry::default(),
            None,
        );
        let mut process = spawn_test_process(&backend, "lagged-notification-exit").await;
        let process_id = process.process_id.clone();
        timeout(Duration::from_secs(3), async {
            for _ in 0..1000 {
                process.stdout_tx.send(vec![b'x']).await.unwrap();
            }
            loop {
                let processes = backend.inner.processes.lock().await;
                if let Some(ProcessEntry::Running(process)) = processes.get(&process_id)
                    && process.output.len() == 1000
                {
                    break;
                }
                drop(processes);
                tokio::task::yield_now().await;
            }
            process.exit_tx.take().unwrap().send(7).unwrap();
            loop {
                let processes = backend.inner.processes.lock().await;
                if let Some(ProcessEntry::Running(process)) = processes.get(&process_id)
                    && process.exit_seq.is_some()
                {
                    break;
                }
                drop(processes);
                tokio::task::yield_now().await;
            }
            // The bounded suffix now starts immediately after the exit.
            for _ in 0..PROCESS_EVENT_CHANNEL_CAPACITY {
                process.stdout_tx.send(vec![b'y']).await.unwrap();
            }
            drop(process.stdout_tx);
            drop(process.stderr_tx);
            loop {
                let processes = backend.inner.processes.lock().await;
                if let Some(ProcessEntry::Running(process)) = processes.get(&process_id)
                    && process.closed
                {
                    break;
                }
                drop(processes);
                tokio::task::yield_now().await;
            }
            let mut exit_seen = false;
            while let Some(message) = notification_rx.recv().await {
                let crate::rpc::RpcServerOutboundMessage::Notification(notification) = message else {
                    continue;
                };
                if notification.method == crate::protocol::EXEC_EXITED_METHOD {
                    assert!(!exit_seen, "exit must be delivered exactly once");
                    assert_eq!(notification.params.as_ref().unwrap()["exitCode"], 7);
                    exit_seen = true;
                }
                if notification.method == EXEC_CLOSED_METHOD {
                    assert!(exit_seen, "a replay gap must not swallow the retained exit");
                    break;
                }
            }
            assert!(exit_seen);
        }).await.expect("lag replay must settle after the sink resumes");
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn full_notification_sink_does_not_hold_output_or_native_closure() {
        let (notification_tx, _notification_rx) = mpsc::channel(1);
        let backend = LocalProcess::with_runtime_paths(
            Some(RpcNotificationSender::new(notification_tx)),
            ExecServerTelemetry::default(),
            None,
        );
        let mut process = spawn_test_process(&backend, "blocked-notification-sink").await;
        let process_id = process.process_id.clone();
        timeout(Duration::from_secs(2), async {
            for _ in 0..1000 {
                process.stdout_tx.send(vec![b'x']).await.unwrap();
            }
            process.exit_tx.take().unwrap().send(0).unwrap();
            drop(process.stdout_tx);
            drop(process.stderr_tx);
            loop {
                let processes = backend.inner.processes.lock().await;
                if let Some(ProcessEntry::Running(process)) = processes.get(&process_id)
                    && process.closed
                {
                    assert_eq!(process.exit_code, Some(0));
                    assert_eq!(process.retained_bytes, 1000);
                    break;
                }
                drop(processes);
                tokio::task::yield_now().await;
            }
        }).await.expect("notification backpressure must not block local custody");
        backend.shutdown().await;
    }

    #[test]
    fn stdin_retry_window_retires_sequences_but_preserves_opaque_compatibility() {
        let mut ids = AcceptedStdinWriteIds::default();
        ids.remember("10".to_string());
        ids.remember("local-20".to_string());
        ids.remember("opaque-id".to_string());
        for sequence in 11..=RETAINED_STDIN_WRITE_IDS_PER_PROCESS + 11 {
            ids.remember(sequence.to_string());
        }
        assert_eq!(ids.ids.len(), RETAINED_STDIN_WRITE_IDS_PER_PROCESS);
        assert_eq!(ids.order.len(), RETAINED_STDIN_WRITE_IDS_PER_PROCESS);
        assert!(ids.already_accepted("10").is_err());
        assert!(ids.already_accepted("9").is_err());
        assert!(ids.already_accepted("local-20").is_err());
        assert!(!ids.already_accepted("local-21").unwrap());
        assert!(!ids.already_accepted("opaque-id").unwrap());
        // Noncanonical numbers are opaque, not aliases of sequenced IDs.
        for opaque in ["010", "+10", "local-020", "18446744073709551616"] {
            assert!(!ids.already_accepted(opaque).unwrap());
        }

        let mut out_of_order = AcceptedStdinWriteIds::default();
        out_of_order.remember("100".to_string());
        out_of_order.remember("1".to_string());
        for sequence in 101..100 + RETAINED_STDIN_WRITE_IDS_PER_PROCESS {
            out_of_order.remember(sequence.to_string());
        }
        assert!(out_of_order.already_accepted("100").is_err());
        assert!(out_of_order.already_accepted("1").unwrap());
        assert!(out_of_order.already_accepted("2").is_err());
    }

    #[tokio::test]
    async fn oversized_stdin_or_id_is_rejected_before_writer_or_dedup_mutation() {
        let backend = LocalProcess::default();
        let process = spawn_test_process(&backend, "stdin-bounds").await;
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        let accepted = {
            let mut processes = backend.inner.processes.lock().await;
            let ProcessEntry::Running(running) = processes.get_mut(&process.process_id).unwrap()
            else {
                panic!("running");
            };
            running.pipe_stdin = true;
            running.session = dummy_session_with_writer(writer_tx);
            running.accepted_stdin_write_ids.clone()
        };
        for (bytes, id) in [
            (MAX_STDIN_CHUNK_BYTES + 1, "legal".to_string()),
            (1, "x".repeat(MAX_STDIN_WRITE_ID_BYTES + 1)),
        ] {
            let error = backend
                .exec_write(WriteParams {
                    process_id: process.process_id.clone(),
                    chunk: vec![0; bytes].into(),
                    write_id: id,
                })
                .await
                .unwrap_err();
            assert!(error.message.contains("not accepted"));
            assert!(writer_rx.try_recv().is_err());
            assert!(accepted.lock().await.ids.is_empty());
        }
        let result = backend
            .exec_write(WriteParams {
                process_id: process.process_id.clone(),
                chunk: b"exact".to_vec().into(),
                write_id: "legal".into(),
            })
            .await
            .unwrap();
        assert_eq!(result.status, WriteStatus::Accepted);
        assert_eq!(writer_rx.recv().await.unwrap(), b"exact");
        drop(process);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn expired_stdin_retry_never_reaches_the_writer() {
        for prefix in ["", "local-"] {
            let backend = LocalProcess::default();
            let process = spawn_test_process(&backend, "stdin-retirement").await;
            let (writer_tx, mut writer_rx) = mpsc::channel(1);
            {
                let mut processes = backend.inner.processes.lock().await;
                let ProcessEntry::Running(running) =
                    processes.get_mut(&process.process_id).unwrap()
                else {
                    panic!("running process");
                };
                running.pipe_stdin = true;
                running.session = dummy_session_with_writer(writer_tx);
            }
            let request = |sequence| WriteParams {
                process_id: process.process_id.clone(),
                chunk: b"input".to_vec().into(),
                write_id: format!("{prefix}{sequence}"),
            };
            for sequence in 1..=RETAINED_STDIN_WRITE_IDS_PER_PROCESS + 1 {
                assert_eq!(
                    backend.exec_write(request(sequence)).await.unwrap().status,
                    WriteStatus::Accepted
                );
                assert_eq!(writer_rx.recv().await.unwrap(), b"input");
            }
            let error = backend.exec_write(request(1)).await.unwrap_err();
            assert!(error.message.contains("expired"));
            assert!(matches!(
                writer_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            // Recent retries still acknowledge without writing again.
            assert_eq!(
                backend.exec_write(request(2)).await.unwrap().status,
                WriteStatus::Accepted
            );
            assert!(matches!(
                writer_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            drop(process);
            backend.shutdown().await;
        }
    }

    #[tokio::test]
    async fn stdin_retry_rechecks_retirement_after_waiting_for_writer_capacity() {
        let backend = LocalProcess::default();
        let process = spawn_test_process(&backend, "stdin-retirement-race").await;
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        writer_tx.send(b"occupied".to_vec()).await.unwrap();
        let accepted = {
            let mut processes = backend.inner.processes.lock().await;
            let ProcessEntry::Running(running) = processes.get_mut(&process.process_id).unwrap()
            else {
                panic!("running process");
            };
            running.pipe_stdin = true;
            running.session = dummy_session_with_writer(writer_tx);
            Arc::clone(&running.accepted_stdin_write_ids)
        };
        let pending = backend.exec_write(WriteParams {
            process_id: process.process_id.clone(),
            chunk: b"must not be written".to_vec().into(),
            write_id: "1".to_string(),
        });
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        {
            let mut ids = accepted.lock().await;
            for sequence in 1..=RETAINED_STDIN_WRITE_IDS_PER_PROCESS + 1 {
                ids.remember(sequence.to_string());
            }
        }
        assert_eq!(writer_rx.recv().await.unwrap(), b"occupied");
        assert!(pending.await.unwrap_err().message.contains("expired"));
        assert!(matches!(
            writer_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(process);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_stdin_write_on_a_full_queue_is_never_enqueued() {
        let backend = LocalProcess::default();
        let process = spawn_test_process(&backend, "stdin-cancelled-full-queue").await;
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        writer_tx.send(b"occupied".to_vec()).await.unwrap();
        {
            let mut processes = backend.inner.processes.lock().await;
            let ProcessEntry::Running(running) = processes.get_mut(&process.process_id).unwrap()
            else {
                panic!("running process");
            };
            running.tty = true;
            running.session = dummy_session_with_writer(writer_tx);
        }
        let handle = LocalExecProcess {
            identity: backend
                .process_start_identity(&process.process_id)
                .await
                .unwrap(),
            process_id: process.process_id.clone(),
            backend: backend.clone(),
            wake_tx: watch::channel(0).0,
            events: ExecProcessEventLog::new(
                PROCESS_EVENT_CHANNEL_CAPACITY,
                RETAINED_OUTPUT_BYTES_PER_PROCESS,
            ),
        };
        assert!(handle.cancelled_write_is_unsent());

        let mut cancelled = ExecProcess::write(&handle, b"cancelled".to_vec());
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        // Freed capacity cannot let the abandoned write land later.
        assert_eq!(writer_rx.recv().await.unwrap(), b"occupied");
        assert!(matches!(
            writer_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let retried = ExecProcess::write(&handle, b"retried".to_vec())
            .await
            .unwrap();
        assert_eq!(retried.status, WriteStatus::Accepted);
        assert_eq!(writer_rx.recv().await.unwrap(), b"retried");
        assert!(matches!(
            writer_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(process);
        backend.shutdown().await;
    }

    fn test_exec_params(env: HashMap<String, String>) -> ExecParams {
        ExecParams {
            process_id: ProcessId::from("env-test"),
            argv: vec!["true".to_string()],
            cwd: PathUri::from_host_native_path(std::env::current_dir().expect("cwd"))
                .expect("cwd URI"),
            env_policy: None,
            env,
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retained_handle_never_reads_or_controls_a_reused_id() {
        let backend = LocalProcess::default();
        let mut old = spawn_test_process(&backend, "reused-handle").await;
        let id = old.process_id.clone();
        let handle = {
            let map = backend.inner.processes.lock().await;
            let ProcessEntry::Running(process) = map.get(&id).unwrap() else {
                panic!("running");
            };
            LocalExecProcess {
                process_id: id.clone(),
                backend: backend.clone(),
                identity: ProcessStartIdentity(Arc::clone(&process.start_generation)),
                wake_tx: process.wake_tx.clone(),
                events: process.events.clone(),
            }
        };
        old.exit(7);
        drop(old.stdout_tx);
        drop(old.stderr_tx);
        old.exit_watch.await.unwrap();
        loop {
            if matches!(backend.inner.processes.lock().await.get(&id), Some(ProcessEntry::Running(process)) if process.closed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::advance(EXITED_PROCESS_RETENTION + Duration::from_millis(1)).await;
        while backend.inner.processes.lock().await.contains_key(&id) {
            tokio::task::yield_now().await;
        }
        let replacement = spawn_test_process(&backend, id.as_str()).await;
        let (writer_tx, mut writer_rx) = mpsc::channel(1);
        {
            let mut map = backend.inner.processes.lock().await;
            let ProcessEntry::Running(process) = map.get_mut(&id).unwrap() else {
                panic!("running");
            };
            process.pipe_stdin = true;
            process.session = dummy_session_with_writer(writer_tx);
        }
        assert!(handle.read(None, None, None).await.is_err());
        assert!(handle.write(b"stale".to_vec()).await.is_err());
        assert!(handle.signal(ProcessSignal::Interrupt).await.is_err());
        handle.terminate().await.unwrap();
        assert!(writer_rx.try_recv().is_err());
        {
            let map = backend.inner.processes.lock().await;
            let ProcessEntry::Running(process) = map.get(&id).unwrap() else {
                panic!("running");
            };
            assert!(!process.termination_requested.load(Ordering::Acquire));
            assert!(process.exit_code.is_none());
        }
        assert_eq!(
            backend
                .write(&id, b"current".to_vec())
                .await
                .unwrap()
                .status,
            WriteStatus::Accepted
        );
        assert_eq!(writer_rx.recv().await.unwrap(), b"current");
        drop(replacement);
        backend.shutdown().await;
    }

    fn telemetry_backend() -> (
        LocalProcess,
        codex_otel::MetricsClient,
        InMemoryMetricExporter,
    ) {
        let exporter = InMemoryMetricExporter::default();
        let metrics = codex_otel::MetricsClient::new(
            MetricsConfig::in_memory(
                "test",
                "codex-exec-server",
                env!("CARGO_PKG_VERSION"),
                exporter.clone(),
            )
            .with_runtime_reader(),
        )
        .expect("metrics");
        let telemetry = ExecServerTelemetry::new(metrics.clone());
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(NOTIFICATION_CHANNEL_CAPACITY);
        tokio::spawn(async move { while outgoing_rx.recv().await.is_some() {} });
        (
            LocalProcess::with_runtime_paths(
                Some(RpcNotificationSender::new(outgoing_tx)),
                telemetry,
                /*runtime_paths*/ None,
            ),
            metrics,
            exporter,
        )
    }

    fn assert_finished_process_result(
        metrics: codex_otel::MetricsClient,
        exporter: &InMemoryMetricExporter,
        expected: &str,
    ) {
        metrics.shutdown().expect("shutdown metrics");
        let resource_metrics = exporter
            .get_finished_metrics()
            .expect("finished metrics")
            .into_iter()
            .last()
            .expect("metrics export");
        let finished_processes = resource_metrics
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == "exec_server_processes_finished_total")
            .expect("finished process metric");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = finished_processes.data() else {
            panic!("finished process metric should be a u64 sum");
        };
        assert_eq!(
            sum.data_points()
                .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                .collect::<Vec<_>>(),
            vec![1],
        );
        let results = sum
            .data_points()
            .flat_map(opentelemetry_sdk::metrics::data::SumDataPoint::attributes)
            .filter(|attribute| attribute.key.as_str() == "result")
            .map(|attribute| attribute.value.as_str().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(results, vec![expected.to_string()]);
    }

    #[test]
    fn cancelled_start_cleanup_preserves_replacement_outside_runtime_context() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let backend = runtime.block_on(async { LocalProcess::default() });
        for replace in [false, true] {
            let process_id = ProcessId::from("replacement-reservation");
            let original = Arc::new(ProcessStart::default());
            let replacement = Arc::new(ProcessStart::default());
            let guard = ProcessStartReservation {
                inner: Arc::clone(&backend.inner),
                process_id: process_id.clone(),
                start: Arc::clone(&original),
                runtime: runtime.handle().clone(),
                active: true,
            };
            let cleanup_finished = Arc::downgrade(&original);
            let mut held = runtime.block_on(backend.inner.processes.lock());
            held.insert(process_id.clone(), ProcessEntry::Starting(original));
            // No runtime is entered here. Drop uses the admission-time handle.
            drop(guard);
            if replace {
                // An explicit cancellation and new admission can reuse this ID
                // while the old cleanup is queued behind the real map lock.
                held.insert(
                    process_id.clone(),
                    ProcessEntry::Starting(Arc::clone(&replacement)),
                );
            }
            drop(held);
            runtime.block_on(async {
                // The old identity is retained by cleanup until its real map
                // operation ends, so this also proves that cleanup actually ran.
                timeout(Duration::from_secs(2), async {
                    while cleanup_finished.upgrade().is_some() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("old reservation cleanup finished");
                let processes = backend.inner.processes.lock().await;
                if replace {
                    assert!(matches!(
                        processes.get(&process_id),
                        Some(ProcessEntry::Starting(current)) if Arc::ptr_eq(current, &replacement)
                    ));
                } else {
                    assert!(!processes.contains_key(&process_id));
                }
            });
        }
    }

    #[test]
    fn failed_preparation_releases_process_id_before_retry() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let backend = LocalProcess::default();
            let cwd = PathUri::parse("file:///usr/local/checkout").expect("foreign cwd");
            let source = cwd.to_abs_path().expect_err("non-native cwd");
            let expected = invalid_params(format!(
                "cwd URI `{cwd}` is not valid on this exec-server host: {source}"
            ));
            for _ in 0..2 {
                let mut params = test_exec_params(HashMap::new());
                params.cwd = cwd.clone();
                let process_id = params.process_id.clone();
                assert_eq!(backend.exec(params).await, Err(expected.clone()));
                assert!(
                    !backend
                        .inner
                        .processes
                        .lock()
                        .await
                        .contains_key(&process_id)
                );
            }
        });
    }

    #[test]
    fn preparation_yields_and_allows_termination_before_launch() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (occupied_tx, occupied_rx) = oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                occupied_tx.send(()).expect("blocking pool occupied");
                match release_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        panic!("blocking pool was not released");
                    }
                }
            });
            occupied_rx.await.expect("blocking worker started");
            let backend = LocalProcess::default();
            let mut params = test_exec_params(HashMap::new());
            let temp = tempfile::tempdir().expect("temp directory");
            // Preparation does not stat the cwd; only the spawn step does. A start
            // that reaches it fails with a different error than the cancellation.
            params.cwd = PathUri::from_host_native_path(temp.path().join("missing"))
                .expect("missing cwd URI");
            let process_id = params.process_id.clone();
            let expected = invalid_request(format!("process {process_id} start was cancelled"));
            let mut start = Box::pin(backend.exec(params));

            // With the blocking pool occupied, preparation must yield instead.
            assert!(futures::poll!(start.as_mut()).is_pending());
            assert!(matches!(
                backend.inner.processes.lock().await.get(&process_id),
                Some(ProcessEntry::Starting(_))
            ));
            let terminated = backend
                .terminate_process(TerminateParams {
                    process_id: process_id.clone(),
                })
                .await
                .expect("terminate queued preparation");
            release_tx.send(()).expect("release preparation");
            assert!(terminated.running);
            assert!(
                !backend
                    .inner
                    .processes
                    .lock()
                    .await
                    .contains_key(&process_id)
            );
            assert!(
                !backend
                    .terminate_process(TerminateParams {
                        process_id: process_id.clone(),
                    })
                    .await
                    .expect("queued preparation is already removed")
                    .running
            );
            let result = timeout(Duration::from_secs(2), start)
                .await
                .expect("preparation finishes");
            assert_eq!(result, Err(expected));
            assert!(
                !backend
                    .inner
                    .processes
                    .lock()
                    .await
                    .contains_key(&process_id)
            );
            blocker.await.expect("blocking pool released");
            tokio::task::spawn_blocking(|| ())
                .await
                .expect("drain preparation");
        });
    }

    #[test]
    fn child_env_defaults_to_exact_env() {
        let params = test_exec_params(HashMap::from([("ONLY_THIS".to_string(), "1".to_string())]));

        assert_eq!(
            child_env(&params),
            HashMap::from([("ONLY_THIS".to_string(), "1".to_string())])
        );
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn child_env_aliases_reach_pipe_and_pty_with_identical_precedence() {
        let cmd = std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("cmd.exe");
        for with_policy in [false, true] {
            let mut expected_output = None;
            for overlay_key in ["PATH", "Path", "path"] {
                for reverse in [false, true] {
                    for (tty, pipe_stdin) in [(false, false), (false, true), (true, false)] {
                        let temp = tempfile::tempdir().expect("tempdir");
                        let mut pairs = vec![
                            (overlay_key.to_string(), "overlay-path".to_string()),
                            ("CODEX_ENV_PROBE".to_string(), "present".to_string()),
                        ];
                        if reverse {
                            pairs.reverse();
                        }
                        let mut params = test_exec_params(pairs.into_iter().collect());
                        if with_policy {
                            params.env_policy = Some(ExecEnvPolicy {
                                inherit: ShellEnvironmentPolicyInherit::None,
                                ignore_default_excludes: true,
                                exclude: Vec::new(),
                                r#set: HashMap::from([
                                    ("Path".into(), "policy-path".into()),
                                    ("PATH".into(), "other-policy-path".into()),
                                ]),
                                include_only: Vec::new(),
                            });
                        }
                        let env = child_env(&params);
                        assert_eq!(
                            env.iter()
                                .filter(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                                .map(|(_, value)| value.as_str())
                                .collect::<Vec<_>>(),
                            ["overlay-path"],
                        );
                        params.argv = vec![
                            cmd.to_string_lossy().into_owned(),
                            "/d".into(),
                            "/c".into(),
                            "set > environment.txt".into(),
                        ];
                        params.cwd = PathUri::from_host_native_path(temp.path()).expect("cwd URI");
                        params.tty = tty;
                        params.pipe_stdin = pipe_stdin;
                        let (backend, _, _) = telemetry_backend();
                        let process_id = backend
                            .exec(params)
                            .await
                            .expect("spawn environment probe")
                            .process_id;
                        let response = timeout(
                            Duration::from_secs(15),
                            read_process_until_closed(&backend, &process_id),
                        )
                        .await
                        .expect("probe completes");
                        let captured = backend
                            .exec_read(ReadParams {
                                process_id,
                                after_seq: None,
                                max_bytes: None,
                                wait_ms: None,
                            })
                            .await
                            .expect("probe output");
                        backend.shutdown().await;
                        assert_eq!(
                            response.exit_code,
                            Some(0),
                            "with_policy={with_policy}, tty={tty}, pipe_stdin={pipe_stdin}: {captured:?}"
                        );
                        // Compare the complete child environment, not terminal
                        // framing (ConPTY adds control sequences to stdout).
                        let output = std::fs::read_to_string(temp.path().join("environment.txt"))
                            .expect("child environment");
                        let mut entries = output
                            .lines()
                            .map(|line| {
                                let (key, value) = line.split_once('=').expect("environment entry");
                                (key.to_ascii_uppercase(), value.to_string())
                            })
                            .collect::<Vec<_>>();
                        entries.sort();
                        assert!(entries.contains(&("PATH".into(), "overlay-path".into())));
                        if let Some(expected) = &expected_output {
                            assert_eq!(&entries, expected);
                        } else {
                            expected_output = Some(entries);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn child_env_applies_policy_then_overlay() {
        let mut params = test_exec_params(HashMap::from([
            ("OVERLAY".to_string(), "overlay".to_string()),
            ("POLICY_SET".to_string(), "overlay-wins".to_string()),
        ]));
        params.env_policy = Some(ExecEnvPolicy {
            inherit: ShellEnvironmentPolicyInherit::None,
            ignore_default_excludes: true,
            exclude: Vec::new(),
            r#set: HashMap::from([("POLICY_SET".to_string(), "policy".to_string())]),
            include_only: Vec::new(),
        });

        let mut expected = HashMap::from([
            ("OVERLAY".to_string(), "overlay".to_string()),
            ("POLICY_SET".to_string(), "overlay-wins".to_string()),
        ]);
        {
            expected.insert("PATHEXT".to_string(), ".COM;.EXE;.BAT;.CMD".to_string());
        }

        assert_eq!(child_env(&params), expected);
    }

    #[tokio::test]
    async fn exit_before_shutdown_records_success() {
        let (backend, metrics, exporter) = telemetry_backend();
        let mut process = spawn_test_process(&backend, "exit-before-shutdown").await;

        process.exit(/*exit_code*/ 0);
        let _ = read_process_until_change(&backend, &process.process_id, /*after_seq*/ None).await;
        backend.shutdown().await;

        assert_finished_process_result(metrics, &exporter, "success");
    }

    #[tokio::test(start_paused = true)]
    async fn termination_request_before_exit_records_terminated() {
        let (backend, metrics, exporter) = telemetry_backend();
        let mut process = spawn_test_process(&backend, "terminate-before-exit").await;

        assert_eq!(
            backend
                .terminate_process(TerminateParams {
                    process_id: process.process_id.clone(),
                })
                .await
                .expect("terminate process"),
            TerminateResponse { running: true },
        );
        let process_id = process.process_id.clone();
        let mut confirmed_termination = Box::pin(backend.terminate(&process_id));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut confirmed_termination)
                .await
                .is_err(),
            "termination should not be confirmed while the process is still running"
        );
        let exit_observed_at = tokio::time::Instant::now();
        process.exit(/*exit_code*/ 0);
        tokio::time::timeout(Duration::from_secs(1), confirmed_termination)
            .await
            .expect("termination should be confirmed after exit")
            .expect("confirmed termination should succeed");
        assert_eq!(
            tokio::time::Instant::now(),
            exit_observed_at,
            "exit notification should confirm termination without a polling delay"
        );
        let _ = read_process_until_change(&backend, &process.process_id, /*after_seq*/ None).await;
        backend.shutdown().await;

        assert_finished_process_result(metrics, &exporter, "terminated");
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_termination_has_a_total_deadline() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "termination-deadline").await;

        let error = backend
            .terminate(&process.process_id)
            .await
            .expect_err("a process that never exits should time out");
        assert!(
            error
                .to_string()
                .contains("timed out confirming local process termination"),
            "unexpected error: {error}"
        );

        process.exit(/*exit_code*/ 0);
        let _ = read_process_until_change(&backend, &process.process_id, /*after_seq*/ None).await;
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn audit_observability_lost_exit_is_not_a_confirmed_completion() {
        let (backend, metrics, _exporter) = telemetry_backend();
        let mut process = spawn_test_process(&backend, "lost-exit").await;
        drop(process.exit_tx.take());
        process.exit_watch.await.expect("exit observer");
        let response = backend
            .exec_read(ReadParams {
                process_id: process.process_id,
                after_seq: None,
                max_bytes: None,
                wait_ms: None,
            })
            .await
            .expect("observation failure remains readable");
        assert!(!response.exited);
        assert_eq!(response.exit_code, None);
        assert_eq!(
            response.failure.as_deref(),
            Some("process exit observation lost")
        );
        let snapshot = metrics.snapshot().expect("observation metrics");
        assert!(
            !snapshot
                .scope_metrics()
                .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
                .any(|metric| metric.name() == "exec_server_processes_finished_total")
        );
        let lost = snapshot
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == "exec_server_process_observation_lost_total")
            .unwrap();
        let AggregatedMetrics::U64(MetricData::Sum(lost)) = lost.data() else {
            panic!("lost counter");
        };
        assert_eq!(
            lost.data_points()
                .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                .collect::<Vec<_>>(),
            vec![1]
        );
        backend.shutdown().await;
        metrics.shutdown().expect("shutdown metrics");
    }

    #[tokio::test]
    async fn shutdown_before_exit_records_terminated() {
        let (backend, metrics, exporter) = telemetry_backend();
        let mut process = spawn_test_process(&backend, "shutdown-before-exit").await;

        backend.shutdown().await;
        let snapshot = metrics.snapshot().expect("snapshot pending exit");
        assert!(
            !snapshot
                .scope_metrics()
                .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
                .any(|metric| metric.name() == "exec_server_processes_finished_total")
        );
        let active = snapshot
            .scope_metrics()
            .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
            .find(|metric| metric.name() == "exec_server_processes_active")
            .unwrap();
        let AggregatedMetrics::I64(MetricData::Gauge(active)) = active.data() else {
            panic!("active gauge");
        };
        assert_eq!(
            active
                .data_points()
                .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value)
                .collect::<Vec<_>>(),
            vec![1]
        );
        process.exit(/*exit_code*/ 0);
        process.exit_watch.await.expect("exit observer");

        assert_finished_process_result(metrics, &exporter, "terminated");
    }

    #[tokio::test]
    async fn exited_process_retains_late_output_past_retention() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "proc-late-output").await;

        process.exit(/*exit_code*/ 0);
        let exit_response =
            read_process_until_change(&backend, &process.process_id, /*after_seq*/ None).await;
        assert_eq!(
            exit_response,
            ReadResponse {
                chunks: Vec::new(),
                next_seq: 2,
                exited: true,
                exit_code: Some(0),
                closed: false,
                failure: None,
                output_gap: None,
                sandbox_denied: false,
            }
        );

        tokio::time::sleep(EXITED_PROCESS_RETENTION + Duration::from_millis(10)).await;
        process
            .stdout_tx
            .send(b"late output after retention\n".to_vec())
            .await
            .expect("send late stdout");

        let late_response =
            read_process_until_change(&backend, &process.process_id, /*after_seq*/ Some(1)).await;
        assert_eq!(
            late_response.chunks,
            vec![ProcessOutputChunk {
                seq: 2,
                stream: ExecOutputStream::Stdout,
                chunk: b"late output after retention\n".to_vec().into(),
            }]
        );
        assert_eq!(late_response.exit_code, Some(0));
        assert!(!late_response.closed);

        drop(process.stdout_tx);
        drop(process.stderr_tx);
        let _closed_response = timeout(
            Duration::from_secs(1),
            read_process_until_closed(&backend, &process.process_id),
        )
        .await
        .expect("process should close");
        let replay_after_exit = backend
            .exec_read(ReadParams {
                process_id: process.process_id.clone(),
                after_seq: Some(1),
                max_bytes: None,
                wait_ms: Some(0),
            })
            .await
            .expect("closed process should remain readable");
        assert_eq!(replay_after_exit.next_seq, 4);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn closed_process_is_evicted_after_retention() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "proc-closed-eviction").await;
        let process_id = process.process_id.clone();

        process.exit(/*exit_code*/ 0);
        drop(process.stdout_tx);
        drop(process.stderr_tx);

        let closed_response = timeout(
            Duration::from_secs(1),
            read_process_until_closed(&backend, &process_id),
        )
        .await
        .expect("process should close");
        assert!(closed_response.closed);

        timeout(Duration::from_secs(1), async {
            loop {
                {
                    let processes = backend.inner.processes.lock().await;
                    if !processes.contains_key(&process_id) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("closed process should be evicted");
        backend.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn closed_record_releases_native_custody_before_replay_expiry() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "released-custody").await;
        let owner = Arc::new(());
        let custody = Arc::downgrade(&owner);
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(1);
        let (stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        let (exit_tx, exit_rx) = oneshot::channel();
        let session = codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_rx.into(),
            stderr_rx: Some(stderr_rx.into()),
            exit_rx,
            terminator: Some(Box::new(move || {
                let _retained = &owner;
                Ok(())
            })),
            writer_handle: None,
            resizer: None,
        })
        .session;
        drop(stdout_tx);
        drop(stderr_tx);
        exit_tx.send(0).unwrap();
        while !session.output_capture().complete() {
            tokio::task::yield_now().await;
        }
        {
            let mut processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(record)) = processes.get_mut(&process.process_id) else {
                panic!("running record");
            };
            record.session = session;
        }
        assert!(custody.upgrade().is_some());
        process
            .stdout_tx
            .send(b"replay evidence".to_vec())
            .await
            .unwrap();
        process.exit(0);
        drop(process.stdout_tx);
        drop(process.stderr_tx);
        let response = read_process_until_closed(&backend, &process.process_id).await;
        assert!(response.closed);
        assert!(
            custody.upgrade().is_none(),
            "replay retained the native terminator"
        );
        assert!(
            backend
                .inner
                .processes
                .lock()
                .await
                .contains_key(&process.process_id)
        );
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn read_observes_output_notified_after_snapshot_before_wait() {
        let backend = LocalProcess::default();
        assert!(notification_sender(&backend.inner).is_none());
        let process = spawn_test_process(&backend, "read-race").await;
        let mut read = Box::pin(backend.exec_read(ReadParams {
            process_id: process.process_id.clone(),
            after_seq: None,
            max_bytes: None,
            wait_ms: Some(30_000),
        }));
        // Stop at the yield after snapshotting, before polling the Notify future.
        assert!(futures::poll!(read.as_mut()).is_pending());
        process
            .stdout_tx
            .send(b"ready".to_vec())
            .await
            .expect("send output");
        timeout(Duration::from_secs(1), async {
            loop {
                {
                    let processes = backend.inner.processes.lock().await;
                    if matches!(processes.get(&process.process_id), Some(ProcessEntry::Running(running)) if !running.output.is_empty()) {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("output retained before resuming read");
        let response = timeout(Duration::from_secs(1), read)
            .await
            .expect("prompt wakeup")
            .expect("read");
        assert_eq!(response.chunks.len(), 1);
        assert_eq!(response.chunks[0].chunk.0, b"ready");
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_read_cursors_preserve_real_process_output() {
        let backend = LocalProcess::default();
        let mut params = test_exec_params(std::env::vars().collect());
        params.argv = vec![
            "cmd.exe".into(),
            "/d".into(),
            "/c".into(),
            "echo retained".into(),
        ];
        let started = backend.start(params).await.expect("start real process");
        let process = started.process;
        let mut events = process.subscribe_events();
        timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    events.recv().await.expect("process event"),
                    ExecProcessEvent::Closed { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("real process closes");

        let original = process
            .read(None, None, None)
            .await
            .expect("retained output");
        assert!(original.closed);
        assert_eq!(original.exit_code, Some(0));
        assert_eq!(original.failure, None);
        assert_eq!(
            original
                .chunks
                .iter()
                .flat_map(|chunk| chunk.chunk.0.iter().copied())
                .collect::<Vec<_>>(),
            b"retained\r\n"
        );
        for after_seq in [original.next_seq, u64::MAX] {
            let error = process
                .read(Some(after_seq), None, Some(0))
                .await
                .expect_err("a future cursor is not a successful empty read");
            assert!(
                matches!(error, ExecServerError::Server { code: -32602, message, .. }
                if message.contains("afterSeq") && message.contains("last available sequence"))
            );
        }
        assert_eq!(
            process
                .read(None, None, Some(0))
                .await
                .expect("replay after errors"),
            original
        );
        let drained = process
            .read(Some(original.next_seq - 1), Some(1), Some(30_000))
            .await
            .expect("last observed sequence remains a valid cursor");
        assert!(drained.closed && drained.chunks.is_empty());
        assert_eq!(drained.exit_code, Some(0));
        assert_eq!(drained.failure, None);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_read_cursor_does_not_wait_for_running_process() {
        let backend = LocalProcess::default();
        let process = spawn_test_process(&backend, "invalid-cursor").await;
        let error = timeout(
            Duration::from_secs(1),
            backend.exec_read(ReadParams {
                process_id: process.process_id.clone(),
                after_seq: Some(1),
                max_bytes: None,
                wait_ms: Some(30_000),
            }),
        )
        .await
        .expect("invalid cursor must not enter the long poll")
        .expect_err("no events have been emitted yet");
        assert_eq!(
            error,
            invalid_params(
                "afterSeq 1 exceeds the last available sequence 0 for process invalid-cursor"
                    .to_string()
            )
        );
        process
            .stdout_tx
            .send(b"still running".to_vec())
            .await
            .expect("send output");
        let response = read_process_until_change(&backend, &process.process_id, Some(0)).await;
        assert_eq!(response.chunks[0].chunk.0, b"still running");
        assert!(!response.exited && !response.closed);
        assert_eq!(response.failure, None);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn exec_rejects_managed_network_without_sandbox() {
        let backend = LocalProcess::default();
        let mut params = test_exec_params(HashMap::new());
        params.argv = vec!["cmd.exe".into(), "/d".into(), "/c".into(), "exit 0".into()];
        params.enforce_managed_network = true;
        let error = backend
            .exec(params)
            .await
            .expect_err("unenforceable network policy");
        assert_eq!(
            error.message,
            "managed network enforcement requires a sandbox context"
        );
        assert!(backend.inner.processes.lock().await.is_empty());
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn evicted_output_is_reported_only_to_cursors_missing_bytes() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "retention-loss").await;
        process
            .stdout_tx
            .send(vec![b'x'; RETAINED_OUTPUT_BYTES_PER_PROCESS])
            .await
            .unwrap();
        read_process_until_change(&backend, &process.process_id, None).await;
        process.stdout_tx.send(b"tail".to_vec()).await.unwrap();
        let tail = read_process_until_change(&backend, &process.process_id, Some(1)).await;
        assert_eq!(tail.chunks[0].chunk.0, b"tail");
        assert!(tail.output_gap.is_none());
        let lost = backend
            .exec_read(ReadParams {
                process_id: process.process_id.clone(),
                after_seq: Some(0),
                max_bytes: None,
                wait_ms: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(lost.failure, None);
        assert_eq!(lost.output_gap.unwrap().through_seq, 1);
        assert!(!lost.exited);
        assert_eq!(lost.chunks, tail.chunks);
        process.exit(0);
        drop(process);
        backend.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unsandboxed_close_preserves_retained_output_without_classification() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "unsandboxed-close").await;
        let mut output = vec![b'x'; RETAINED_OUTPUT_BYTES_PER_PROCESS];
        output[..17].copy_from_slice(b"Access is denied.");
        process.stdout_tx.send(output.clone()).await.unwrap();
        read_process_until_change(&backend, &process.process_id, None).await;
        let process_id = process.process_id.clone();
        process.exit(1);
        drop(process);

        let response = read_process_until_closed(&backend, &process_id).await;
        assert!(response.closed);
        assert_eq!(response.exit_code, Some(1));
        assert!(!response.sandbox_denied);
        let replay = backend
            .exec_read(ReadParams {
                process_id,
                after_seq: None,
                max_bytes: None,
                wait_ms: None,
            })
            .await
            .expect("closed output stays available for replay");
        assert_eq!(replay.failure, None);
        assert_eq!(replay.chunks.len(), 1);
        assert_eq!(replay.chunks[0].chunk.0, output);
        assert!(replay.closed);
        assert!(!replay.sandbox_denied);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn sandbox_assessment_waits_for_late_stderr_after_prompt_exit() {
        let backend = LocalProcess::default();
        let mut process = spawn_test_process(&backend, "late-sandbox-diagnostic").await;
        {
            let mut processes = backend.inner.processes.lock().await;
            let ProcessEntry::Running(running) = processes.get_mut(&process.process_id).unwrap()
            else {
                panic!("running");
            };
            running.sandbox = SandboxType::WindowsRestrictedToken;
        }
        let mut events = {
            let processes = backend.inner.processes.lock().await;
            let ProcessEntry::Running(running) = processes.get(&process.process_id).unwrap() else {
                panic!("running");
            };
            running.events.subscribe()
        };
        process.exit(1);
        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            ExecProcessEvent::Exited {
                sandbox_denied: None,
                ..
            }
        ));
        process
            .stdout_tx
            .send(b"early stdout".to_vec())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        process
            .stderr_tx
            .send(b"Access is denied.".to_vec())
            .await
            .unwrap();
        let id = process.process_id.clone();
        drop(process);
        timeout(Duration::from_secs(1), async {
            loop {
                if let ExecProcessEvent::Closed { sandbox_denied, .. } =
                    events.recv().await.unwrap()
                {
                    assert_eq!(sandbox_denied, Some(true));
                    break;
                }
            }
        })
        .await
        .unwrap();
        let read = backend
            .exec_read(ReadParams {
                process_id: id,
                after_seq: None,
                max_bytes: None,
                wait_ms: None,
            })
            .await
            .unwrap();
        assert!(read.closed && read.sandbox_denied);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn blocked_notification_sink_does_not_hold_native_output_or_closure() {
        let (outgoing_tx, _blocked_receiver) = mpsc::channel(1);
        let backend = LocalProcess::with_runtime_paths(
            Some(RpcNotificationSender::new(outgoing_tx)), ExecServerTelemetry::default(), None,
        );
        let fixture = spawn_test_process(&backend, "blocked-notification-sink").await;
        let mut observed = {
            let processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(process)) = processes.get(&fixture.process_id) else { panic!("running fixture"); };
            process.events.subscribe()
        };
        let TestProcess { stdout_tx, stderr_tx, exit_tx, exit_watch, .. } = fixture;
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..64 { stdout_tx.send(b"data".to_vec()).await.unwrap(); }
            exit_tx.unwrap().send(0).unwrap();
            drop(stdout_tx); drop(stderr_tx);
            exit_watch.await.unwrap();
            let mut bytes = 0;
            loop {
                match observed.recv().await.unwrap() {
                    ExecProcessEvent::Output(chunk) => bytes += chunk.chunk.0.len(),
                    ExecProcessEvent::Closed { .. } => break,
                    _ => {}
                }
            }
            assert_eq!(bytes, 64 * 4);
        }).await.expect("local drain and closure must not await a full notification sink");
        backend.shutdown().await;
    }

    struct TestProcess {
        process_id: ProcessId,
        stdout_tx: mpsc::Sender<Vec<u8>>,
        stderr_tx: mpsc::Sender<Vec<u8>>,
        exit_tx: Option<oneshot::Sender<i32>>,
        exit_watch: tokio::task::JoinHandle<()>,
    }

    impl TestProcess {
        fn exit(&mut self, exit_code: i32) {
            self.exit_tx
                .take()
                .expect("process should not have exited")
                .send(exit_code)
                .expect("send process exit");
        }
    }

    async fn spawn_test_process(backend: &LocalProcess, process_id: &str) -> TestProcess {
        let process_id = ProcessId::from(process_id);
        let (stdout_tx, stdout_rx) = mpsc::channel(16);
        let (stderr_tx, stderr_rx) = mpsc::channel(16);
        let (exit_tx, exit_rx) = oneshot::channel();
        let output_notify = Arc::new(Notify::new());
        let (wake_tx, _wake_rx) = watch::channel(0);
        let events = ExecProcessEventLog::new(
            PROCESS_EVENT_CHANNEL_CAPACITY,
            RETAINED_OUTPUT_BYTES_PER_PROCESS,
        );

        let session = dummy_session();
        // This fixture forwards its own synthetic streams. Settle the dummy
        // driver's genuine empty EOF receipts before using its native handle.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.output_capture().complete() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut processes = backend.inner.processes.lock().await;
        let termination_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let metrics = backend.inner.telemetry.process_started();
        let previous = processes.insert(
            process_id.clone(),
            ProcessEntry::Running(Box::new(RunningProcess {
                session,
                start_generation: Arc::new(super::ProcessStart::default()),
                tty: false,
                pipe_stdin: false,
                accepted_stdin_write_ids: Arc::new(Mutex::new(AcceptedStdinWriteIds::default())),
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
                sandbox: SandboxType::None,
                sandbox_denied: false,
            })),
        );
        assert!(previous.is_none());
        drop(processes);

        tokio::spawn(forward_process_notifications(
            process_id.clone(), events.clone(), events.subscribe(), Arc::downgrade(&backend.inner),
            backend.inner.notification_shutdown.clone(),
        ));
        tokio::spawn(stream_output(
            process_id.clone(),
            ExecOutputStream::Stdout,
            stdout_rx,
            Arc::clone(&backend.inner),
            Arc::clone(&output_notify),
        ));
        tokio::spawn(stream_output(
            process_id.clone(),
            ExecOutputStream::Stderr,
            stderr_rx,
            Arc::clone(&backend.inner),
            Arc::clone(&output_notify),
        ));
        let exit_watch = tokio::spawn(watch_exit(
            process_id.clone(),
            exit_rx,
            Arc::clone(&backend.inner),
            output_notify,
            metrics,
            termination_requested,
        ));

        TestProcess {
            process_id,
            stdout_tx,
            stderr_tx,
            exit_tx: Some(exit_tx),
            exit_watch,
        }
    }

    #[tokio::test]
    async fn native_reader_cutoff_is_an_ordered_gap_in_live_and_retained_replay() {
        let backend = LocalProcess::default();
        let mut fixture = spawn_test_process(&backend, "capture-cutoff").await;
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (_native_stdout, native_stdout_rx) = mpsc::channel(1);
        let (_native_exit, native_exit_rx) = oneshot::channel();
        let native = codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: native_stdout_rx.into(),
            stderr_rx: None,
            exit_rx: native_exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        native.session.finish(); // Abort before native EOF, independently of the process exit.
        {
            let mut processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(process)) = processes.get_mut(&fixture.process_id)
            else {
                panic!("running fixture");
            };
            process.session = native.session;
        }
        fixture
            .stdout_tx
            .send(b"known prefix".to_vec())
            .await
            .unwrap();
        let prefix = read_process_until_change(&backend, &fixture.process_id, Some(0)).await;
        assert_eq!(prefix.chunks[0].chunk.0, b"known prefix");
        fixture.exit(0);
        let TestProcess {
            process_id,
            stdout_tx,
            stderr_tx,
            exit_watch,
            ..
        } = fixture;
        drop(stdout_tx);
        drop(stderr_tx);
        exit_watch.await.unwrap();
        let response = timeout(Duration::from_secs(2), async {
            loop {
                let response = backend
                    .exec_read(ReadParams {
                        process_id: process_id.clone(),
                        after_seq: Some(0),
                        max_bytes: None,
                        wait_ms: Some(0),
                    })
                    .await
                    .unwrap();
                if response.closed {
                    break response;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(response.exit_code, Some(0));
        assert!(response.output_gap.as_ref().unwrap().through_seq > prefix.chunks[0].seq);
        let replay =
            crate::client::recovery::replay_process_events(0, false, response, None).unwrap();
        assert!(replay.iter().any(
            |event| matches!(event, ExecProcessEvent::OutputGap { through_seq } if *through_seq > 0)
        ));
        assert!(replay.iter().any(|event| matches!(event, ExecProcessEvent::Output(chunk) if chunk.chunk.0 == b"known prefix")));
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn pty_placeholder_close_is_not_capture_loss_while_the_terminal_drains() {
        let backend = LocalProcess::default();
        let fixture = spawn_test_process(&backend, "pty-placeholder").await;
        // A live terminal reader keeps its capture incomplete; PTY spawns have
        // no separate stderr stream, so that half of the capture is preset.
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (native_stdout_tx, native_stdout_rx) = mpsc::channel(1);
        let (_native_exit, native_exit_rx) = oneshot::channel();
        let native = codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: native_stdout_rx.into(),
            stderr_rx: None,
            exit_rx: native_exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        {
            let mut processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(process)) = processes.get_mut(&fixture.process_id)
            else {
                panic!("running fixture");
            };
            process.tty = true;
            process.session = native.session;
        }
        let read_params = ReadParams {
            process_id: fixture.process_id.clone(),
            after_seq: None,
            max_bytes: None,
            wait_ms: Some(0),
        };
        // Both terminal receivers report `Pty`; the placeholder closes first.
        finish_output_stream(
            fixture.process_id.clone(),
            ExecOutputStream::Pty,
            Arc::clone(&backend.inner),
        )
        .await;
        let response = backend.exec_read(read_params.clone()).await.unwrap();
        assert!(
            response.output_gap.is_none(),
            "placeholder EOF while the terminal is live is not output loss"
        );
        // The terminal receiver itself draining without native EOF is loss.
        finish_output_stream(
            fixture.process_id.clone(),
            ExecOutputStream::Pty,
            Arc::clone(&backend.inner),
        )
        .await;
        let response = backend.exec_read(read_params).await.unwrap();
        assert!(response.output_gap.is_some());
        drop(native_stdout_tx);
        drop(fixture);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn eviction_after_capture_loss_keeps_the_marker_inside_the_gap() {
        let backend = LocalProcess::default();
        let fixture = spawn_test_process(&backend, "capture-loss-then-eviction").await;
        // A live native stdout reader keeps that half of the capture incomplete.
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (native_stdout_tx, native_stdout_rx) = mpsc::channel(1);
        let (_native_exit, native_exit_rx) = oneshot::channel();
        let native = codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: native_stdout_rx.into(),
            stderr_rx: None,
            exit_rx: native_exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        {
            let mut processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(process)) = processes.get_mut(&fixture.process_id)
            else {
                panic!("running fixture");
            };
            process.session = native.session;
        }
        fixture
            .stderr_tx
            .send(vec![b'x'; RETAINED_OUTPUT_BYTES_PER_PROCESS])
            .await
            .unwrap();
        read_process_until_change(&backend, &fixture.process_id, None).await;
        // Stdout drains without native EOF: its loss marker takes sequence 2.
        finish_output_stream(
            fixture.process_id.clone(),
            ExecOutputStream::Stdout,
            Arc::clone(&backend.inner),
        )
        .await;
        // Later stderr output evicts sequence 1 from the front of retention.
        fixture.stderr_tx.send(b"tail".to_vec()).await.unwrap();
        let tail = read_process_until_change(&backend, &fixture.process_id, Some(2)).await;
        assert_eq!(tail.chunks[0].chunk.0, b"tail");
        let between = backend
            .exec_read(ReadParams {
                process_id: fixture.process_id.clone(),
                after_seq: Some(1),
                max_bytes: None,
                wait_ms: Some(0),
            })
            .await
            .unwrap();
        assert_eq!(between.output_gap.map(|gap| gap.through_seq), Some(2));
        let retained = backend
            .exec_read(ReadParams {
                process_id: fixture.process_id.clone(),
                after_seq: Some(0),
                max_bytes: None,
                wait_ms: Some(0),
            })
            .await
            .unwrap();
        let replay = crate::client::recovery::replay_process_events(0, false, retained, None)
            .expect("the retained gap declares the capture-loss hole");
        assert_eq!(replay[0], ExecProcessEvent::OutputGap { through_seq: 2 });
        assert!(matches!(&replay[1], ExecProcessEvent::Output(chunk) if chunk.chunk.0 == b"tail"));
        drop(native_stdout_tx);
        drop(fixture);
        backend.shutdown().await;
    }

    #[tokio::test]
    async fn capture_loss_marker_is_notified_to_remote_sessions() {
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(NOTIFICATION_CHANNEL_CAPACITY);
        let backend = LocalProcess::with_runtime_paths(
            Some(RpcNotificationSender::new(outgoing_tx)),
            ExecServerTelemetry::default(),
            /*runtime_paths*/ None,
        );
        let fixture = spawn_test_process(&backend, "capture-loss-notification").await;
        // A live native stdout reader keeps that half of the capture incomplete.
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        let (native_stdout_tx, native_stdout_rx) = mpsc::channel(1);
        let (_native_exit, native_exit_rx) = oneshot::channel();
        let native = codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: native_stdout_rx.into(),
            stderr_rx: None,
            exit_rx: native_exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        });
        {
            let mut processes = backend.inner.processes.lock().await;
            let Some(ProcessEntry::Running(process)) = processes.get_mut(&fixture.process_id)
            else {
                panic!("running fixture");
            };
            process.session = native.session;
        }
        finish_output_stream(
            fixture.process_id.clone(),
            ExecOutputStream::Stdout,
            Arc::clone(&backend.inner),
        )
        .await;
        // Remote sessions publish in sequence order, so the marker's sequence
        // must reach them like every other sequenced process event.
        let message = timeout(Duration::from_secs(1), outgoing_rx.recv())
            .await
            .expect("capture loss should be notified")
            .expect("notification channel stays open");
        let crate::rpc::RpcServerOutboundMessage::Notification(notification) = message else {
            panic!("expected a notification");
        };
        assert_eq!(notification.method, crate::protocol::EXEC_OUTPUT_GAP_METHOD);
        let params: crate::protocol::ExecOutputGapNotification =
            serde_json::from_value(notification.params.expect("gap params")).unwrap();
        assert_eq!(
            params,
            crate::protocol::ExecOutputGapNotification {
                process_id: fixture.process_id.clone(),
                through_seq: 1,
            }
        );
        drop(native_stdout_tx);
        drop(fixture);
        backend.shutdown().await;
    }

    fn dummy_session() -> ExecCommandSession {
        let (writer_tx, _writer_rx) = mpsc::channel(1);
        dummy_session_with_writer(writer_tx)
    }

    fn dummy_session_with_writer(writer_tx: mpsc::Sender<Vec<u8>>) -> ExecCommandSession {
        let (_stdout_tx, stdout_rx) = tokio::sync::broadcast::channel(1);
        let (_stderr_tx, stderr_rx) = tokio::sync::broadcast::channel(1);
        let (_exit_tx, exit_rx) = oneshot::channel();

        codex_utils_pty::spawn_from_driver(ProcessDriver {
            writer_tx,
            stdout_rx: stdout_rx.into(),
            stderr_rx: Some(stderr_rx.into()),
            exit_rx,
            terminator: None,
            writer_handle: None,
            resizer: None,
        })
        .session
    }

    async fn read_process_until_change(
        backend: &LocalProcess,
        process_id: &ProcessId,
        after_seq: Option<u64>,
    ) -> ReadResponse {
        timeout(
            Duration::from_secs(1),
            backend.exec_read(ReadParams {
                process_id: process_id.clone(),
                after_seq,
                max_bytes: None,
                wait_ms: Some(1_000),
            }),
        )
        .await
        .expect("process read should finish")
        .expect("process read")
    }

    async fn read_process_until_closed(
        backend: &LocalProcess,
        process_id: &ProcessId,
    ) -> ReadResponse {
        let mut after_seq = None;
        loop {
            let response = read_process_until_change(backend, process_id, after_seq).await;
            if response.closed {
                return response;
            }
            for chunk in &response.chunks {
                after_seq = Some(chunk.seq);
            }
            after_seq = response.next_seq.checked_sub(1).or(after_seq);
        }
    }
}
