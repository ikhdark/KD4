#![allow(clippy::module_inception)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::exec::is_likely_sandbox_denied;
use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::command_output_artifact::RawOutputArtifactWriter;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::OutputGap;
use codex_exec_server::ProcessSignal as ExecServerProcessSignal;
use codex_exec_server::ReadResponse as ExecReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteStatus;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::TruncationPolicy;
use codex_sandboxing::SandboxType;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use codex_utils_pty::SpawnedPty;

use super::UNIFIED_EXEC_OUTPUT_MAX_TOKENS;
use super::UnifiedExecError;
use super::head_tail_buffer::HeadTailBuffer;
use super::head_tail_buffer::SharedOutputRecoveryEvidence;
use super::process_state::ProcessState;

const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);
const RECOVERY_PAGE_MAX_BYTES: usize = 64 * 1024;
const RECOVERY_PAGE_MAX_CHUNKS: usize = 128;
const RECOVERY_PAGE_LIMIT: usize = 32;
const RECOVERY_GAP_LIMIT: usize = 32;
const RECOVERY_MARKER_MAX_BYTES: usize = 256;
pub(crate) trait SpawnLifecycle: std::fmt::Debug + Send + Sync {
    /// Returns file descriptors that must stay open across the child `exec()`.
    ///
    /// The returned descriptors must already be valid in the parent process and
    /// stay valid until `after_spawn()` runs, which is the first point where
    /// the parent may release its copies.
    fn inherited_fds(&self) -> Vec<i32> {
        Vec::new()
    }

    fn after_spawn(&mut self) {}
}

pub(crate) type SpawnLifecycleHandle = Box<dyn SpawnLifecycle>;

#[derive(Debug, Default)]
/// Spawn lifecycle that performs no extra setup around process launch.
pub(crate) struct NoopSpawnLifecycle;

impl SpawnLifecycle for NoopSpawnLifecycle {}

pub(crate) type OutputBuffer = Arc<Mutex<HeadTailBuffer>>;
/// Shared output state exposed to polling and streaming consumers.
pub(crate) struct OutputHandles {
    pub(crate) output_buffer: OutputBuffer,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) output_closed: Arc<AtomicBool>,
    pub(crate) output_closed_notify: Arc<Notify>,
    pub(crate) cancellation_token: CancellationToken,
    pub(crate) recovery_evidence: SharedOutputRecoveryEvidence,
}

/// Transport-specific process handle used by unified exec.
enum ProcessHandle {
    Local(Box<ExecCommandSession>),
    ExecServer(Arc<dyn ExecProcess>),
}

/// Unified wrapper over directly spawned PTY sessions and exec-server-backed
/// processes.
pub(crate) struct UnifiedExecProcess {
    process_handle: ProcessHandle,
    output_tx: broadcast::Sender<Vec<u8>>,
    output_buffer: OutputBuffer,
    output_notify: Arc<Notify>,
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
    cancellation_token: CancellationToken,
    recovery_evidence: SharedOutputRecoveryEvidence,
    output_drained: Arc<Notify>,
    state_tx: watch::Sender<ProcessState>,
    state_rx: watch::Receiver<ProcessState>,
    output_task: Option<JoinHandle<()>>,
    raw_output_artifact: Option<Arc<Mutex<RawOutputArtifact>>>,
    sandbox_type: SandboxType,
    _spawn_lifecycle: Option<SpawnLifecycleHandle>,
}

impl std::fmt::Debug for UnifiedExecProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecProcess")
            .field("has_exited", &self.has_exited())
            .field("exit_code", &self.exit_code())
            .field("sandbox_type", &self.sandbox_type)
            .finish_non_exhaustive()
    }
}

pub(super) fn merge_recovery_gap(
    gaps: &mut Vec<OutputGap>,
    mut gap: OutputGap,
    recovered: &BTreeSet<u64>,
) -> bool {
    if gap.first_missing_seq > gap.last_missing_seq
        || recovered
            .range(gap.first_missing_seq..=gap.last_missing_seq)
            .next()
            .is_some()
    {
        return false;
    }
    let mut merged = Vec::with_capacity(gaps.len().saturating_add(1));
    let mut inserted = false;
    for current in gaps.iter().copied() {
        if current.last_missing_seq.saturating_add(1) < gap.first_missing_seq {
            merged.push(current);
        } else if gap.last_missing_seq.saturating_add(1) < current.first_missing_seq {
            if !inserted {
                merged.push(gap);
                inserted = true;
            }
            merged.push(current);
        } else {
            gap.first_missing_seq = gap.first_missing_seq.min(current.first_missing_seq);
            gap.last_missing_seq = gap.last_missing_seq.max(current.last_missing_seq);
        }
    }
    if !inserted {
        merged.push(gap);
    }
    if merged.len() > RECOVERY_GAP_LIMIT {
        return false;
    }
    *gaps = merged;
    true
}

fn range_is_declared_missing(gaps: &[OutputGap], first: u64, last: u64) -> bool {
    if first > last {
        return true;
    }
    let mut cursor = first;
    for gap in gaps {
        if gap.last_missing_seq < cursor {
            continue;
        }
        if gap.first_missing_seq > cursor {
            return false;
        }
        cursor = gap.last_missing_seq.saturating_add(1);
        if cursor > last {
            return true;
        }
    }
    false
}

fn advance_through_declared_gaps(mut cursor: u64, gaps: &[OutputGap]) -> u64 {
    loop {
        let next = cursor.saturating_add(1);
        let Some(gap) = gaps
            .iter()
            .find(|gap| gap.first_missing_seq <= next && next <= gap.last_missing_seq)
        else {
            return cursor;
        };
        if gap.last_missing_seq <= cursor {
            return cursor;
        }
        cursor = gap.last_missing_seq;
    }
}

pub(super) fn recovery_incomplete_detail(gaps: &[OutputGap], reasons: &BTreeSet<String>) -> String {
    let mut details = Vec::new();
    let mut gap_detail = String::new();
    if !gaps.is_empty() {
        gap_detail.push_str("missing sequence");
        if gaps.len() != 1 || gaps[0].first_missing_seq != gaps[0].last_missing_seq {
            gap_detail.push('s');
        }
        gap_detail.push(' ');
    }
    for (index, gap) in gaps.iter().enumerate() {
        if index > 0 {
            gap_detail.push_str(", ");
        }
        if gap.first_missing_seq == gap.last_missing_seq {
            gap_detail.push_str(&gap.first_missing_seq.to_string());
        } else {
            gap_detail.push_str(&format!(
                "{}-{}",
                gap.first_missing_seq, gap.last_missing_seq
            ));
        }
    }
    if !gap_detail.is_empty() {
        details.push(gap_detail);
    }
    details.extend(reasons.iter().cloned());
    if details.is_empty() {
        details.push("recovery completeness unknown".to_string());
    }
    let detailed = details.join("; ");
    if detailed
        .len()
        .saturating_add("\n[output incomplete: ]\n".len())
        > RECOVERY_MARKER_MAX_BYTES
    {
        if gaps.len() > 1 {
            "multiple disjoint recovery gaps".to_string()
        } else if gaps.len() == 1 {
            format!(
                "missing sequence range {}-{}",
                gaps[0].first_missing_seq, gaps[0].last_missing_seq
            )
        } else {
            "recovery completeness unknown".to_string()
        }
    } else {
        detailed
    }
}

fn recovery_incomplete_marker(gaps: &[OutputGap], reasons: &BTreeSet<String>) -> Vec<u8> {
    format!(
        "\n[output incomplete: {}]\n",
        recovery_incomplete_detail(gaps, reasons)
    )
    .into_bytes()
}

impl UnifiedExecProcess {
    fn new(
        process_handle: ProcessHandle,
        sandbox_type: SandboxType,
        spawn_lifecycle: Option<SpawnLifecycleHandle>,
        raw_output_artifact: Option<RawOutputArtifact>,
    ) -> Self {
        let recovery_evidence = Arc::new(std::sync::Mutex::new(Default::default()));
        let output_buffer = Arc::new(Mutex::new(HeadTailBuffer::new_with_recovery_evidence(
            super::UNIFIED_EXEC_OUTPUT_MAX_BYTES,
            Arc::clone(&recovery_evidence),
        )));
        let output_notify = Arc::new(Notify::new());
        let output_closed = Arc::new(AtomicBool::new(false));
        let output_closed_notify = Arc::new(Notify::new());
        let cancellation_token = CancellationToken::new();
        let output_drained = Arc::new(Notify::new());
        let (output_tx, _) = broadcast::channel(64);
        let (state_tx, state_rx) = watch::channel(ProcessState::default());

        Self {
            process_handle,
            output_tx,
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            recovery_evidence,
            output_drained,
            state_tx,
            state_rx,
            output_task: None,
            raw_output_artifact: raw_output_artifact.map(|artifact| Arc::new(Mutex::new(artifact))),
            sandbox_type,
            _spawn_lifecycle: spawn_lifecycle,
        }
    }

    pub(super) async fn write(&self, data: &[u8]) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .writer_sender()
                .send(data.to_vec())
                .await
                .map_err(|_| UnifiedExecError::WriteToStdin),
            ProcessHandle::ExecServer(process_handle) => {
                match process_handle.write(data.to_vec()).await {
                    Ok(response) => match response.status {
                        WriteStatus::Accepted => Ok(()),
                        WriteStatus::UnknownProcess | WriteStatus::StdinClosed => {
                            let state = self.state_rx.borrow().clone();
                            let _ = self.state_tx.send_replace(state.exited(state.exit_code));
                            self.cancellation_token.cancel();
                            Err(UnifiedExecError::WriteToStdin)
                        }
                        WriteStatus::Starting => Err(UnifiedExecError::WriteToStdin),
                    },
                    Err(err) => Err(UnifiedExecError::process_failed(err.to_string())),
                }
            }
        }
    }

    pub(super) fn output_handles(&self) -> OutputHandles {
        OutputHandles {
            output_buffer: Arc::clone(&self.output_buffer),
            output_notify: Arc::clone(&self.output_notify),
            output_closed: Arc::clone(&self.output_closed),
            output_closed_notify: Arc::clone(&self.output_closed_notify),
            cancellation_token: self.cancellation_token.clone(),
            recovery_evidence: Arc::clone(&self.recovery_evidence),
        }
    }

    pub(super) fn recovery_evidence(&self) -> SharedOutputRecoveryEvidence {
        Arc::clone(&self.recovery_evidence)
    }

    pub(super) fn output_receiver(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    pub(super) fn output_drained_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.output_drained)
    }

    pub(super) async fn raw_output_artifact(&self) -> Option<RawOutputArtifact> {
        match &self.raw_output_artifact {
            Some(artifact) => Some(artifact.lock().await.clone()),
            None => None,
        }
    }

    pub(super) fn has_exited(&self) -> bool {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => state.has_exited || process_handle.has_exited(),
            ProcessHandle::ExecServer(_) => state.has_exited,
        }
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                state.exit_code.or_else(|| process_handle.exit_code())
            }
            ProcessHandle::ExecServer(_) => state.exit_code,
        }
    }

    fn finish_termination(&self) {
        self.output_closed.store(true, Ordering::Release);
        self.output_closed_notify.notify_waiters();
        self.cancellation_token.cancel();
        if let Some(output_task) = &self.output_task {
            output_task.abort();
        }
    }

    pub(super) fn terminate(&self) {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                tokio::spawn(async move {
                    let _ = process_handle.terminate().await;
                });
            }
        }
        self.finish_termination();
    }

    pub(super) async fn terminate_confirmed(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                process_handle
                    .terminate()
                    .await
                    .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
            }
        }
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(super) async fn interrupt(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .signal(PtyProcessSignal::Interrupt)
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
            ProcessHandle::ExecServer(process_handle) => process_handle
                .signal(ExecServerProcessSignal::Interrupt)
                .await
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
        }
    }

    pub(super) fn fail_and_terminate(&self, message: String) {
        let state = self.state_rx.borrow().clone();
        if state.failure_message.is_none() {
            let _ = self.state_tx.send_replace(state.failed(message));
        }
        self.terminate();
    }

    async fn snapshot_output(&self) -> Vec<Vec<u8>> {
        let guard = self.output_buffer.lock().await;
        guard.snapshot_chunks()
    }

    pub(crate) fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    pub(super) fn failure_message(&self) -> Option<String> {
        self.state_rx.borrow().failure_message.clone()
    }

    pub(super) async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        let _ =
            tokio::time::timeout(Duration::from_millis(20), self.output_notify.notified()).await;

        let collected_chunks = self.snapshot_output().await;
        let mut aggregated: Vec<u8> = Vec::new();
        for chunk in collected_chunks {
            aggregated.extend_from_slice(&chunk);
        }
        let aggregated_text = String::from_utf8_lossy(&aggregated).to_string();
        self.check_for_sandbox_denial_with_text(&aggregated_text)
            .await?;

        Ok(())
    }

    pub(super) async fn check_for_sandbox_denial_with_text(
        &self,
        text: &str,
    ) -> Result<(), UnifiedExecError> {
        let executor_reported_denial = self.state_rx.borrow().sandbox_denied;
        let sandbox_type = self.sandbox_type();
        if !self.has_exited() || (!executor_reported_denial && sandbox_type == SandboxType::None) {
            return Ok(());
        }

        let exit_code = self.exit_code().unwrap_or(-1);
        let exec_output = ExecToolCallOutput {
            exit_code,
            stderr: StreamOutput::new(text.to_string()),
            aggregated_output: StreamOutput::new(text.to_string()),
            ..Default::default()
        };
        if executor_reported_denial || is_likely_sandbox_denied(sandbox_type, &exec_output) {
            let snippet = formatted_truncate_text(
                text,
                TruncationPolicy::Tokens(UNIFIED_EXEC_OUTPUT_MAX_TOKENS),
            );
            let message = if snippet.is_empty() {
                format!("Process exited with code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message, exec_output));
        }
        Ok(())
    }

    pub(super) async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
        spawn_lifecycle: SpawnLifecycleHandle,
        raw_output_artifact: Option<RawOutputArtifact>,
    ) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session: process_handle,
            stdout_rx,
            stderr_rx,
            mut exit_rx,
        } = spawned;
        let mut managed = Self::new(
            ProcessHandle::Local(Box::new(process_handle)),
            sandbox_type,
            Some(spawn_lifecycle),
            raw_output_artifact,
        );
        let output_handles = managed.output_handles();
        managed.output_task = Some(Self::spawn_local_output_task(
            stdout_rx,
            stderr_rx,
            output_handles,
            managed.output_tx.clone(),
            managed.raw_output_artifact.clone(),
        ));

        match exit_rx.try_recv() {
            Ok(exit_code) => {
                managed.signal_exit(Some(exit_code));
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Closed) => {
                managed.signal_exit(/*exit_code*/ None);
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Empty) => {}
        }

        if let Ok(exit_result) = tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, &mut exit_rx).await {
            managed.signal_exit(exit_result.ok());
            managed.check_for_sandbox_denial().await?;
            return Ok(managed);
        }

        tokio::spawn({
            let state_tx = managed.state_tx.clone();
            let cancellation_token = managed.cancellation_token.clone();
            async move {
                let exit_code = exit_rx.await.ok();
                let state = state_tx.borrow().clone();
                let _ = state_tx.send_replace(state.exited(exit_code));
                cancellation_token.cancel();
            }
        });

        Ok(managed)
    }

    pub(super) async fn from_exec_server_started(
        started: StartedExecProcess,
        raw_output_artifact: Option<RawOutputArtifact>,
    ) -> Result<Self, UnifiedExecError> {
        let process_handle = ProcessHandle::ExecServer(Arc::clone(&started.process));
        let mut managed = Self::new(
            process_handle,
            SandboxType::None,
            /*spawn_lifecycle*/ None,
            raw_output_artifact,
        );
        let output_handles = managed.output_handles();
        managed.output_task = Some(Self::spawn_exec_server_output_task(
            started,
            output_handles,
            managed.output_tx.clone(),
            managed.state_tx.clone(),
            managed.raw_output_artifact.clone(),
        ));

        let mut state_rx = managed.state_rx.clone();
        if tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, async {
            loop {
                let state = state_rx.borrow().clone();
                if state.has_exited || state.failure_message.is_some() {
                    break;
                }
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
        {
            managed.check_for_sandbox_denial().await?;
        }

        Ok(managed)
    }

    fn spawn_exec_server_output_task(
        started: StartedExecProcess,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        state_tx: watch::Sender<ProcessState>,
        raw_output_artifact: Option<Arc<Mutex<RawOutputArtifact>>>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            recovery_evidence,
        } = output_handles;
        let process = started.process;
        let mut events = process.subscribe_events();
        let mut wake_rx = process.subscribe_wake();
        tokio::spawn(async move {
            let mut artifact_writer =
                RawOutputArtifactWriter::open(raw_output_artifact.as_ref()).await;
            let mut last_seq: u64 = 0;
            let mut recovery_gaps = Vec::new();
            let mut recovered_sequences = BTreeSet::new();
            let mut recovery_reasons = BTreeSet::new();
            let mut wake_open = true;
            loop {
                let (event, event_delivery_lost) = tokio::select! {
                    event = events.recv() => match event {
                        Ok(event) => (Some(event), false),
                        Err(broadcast::error::RecvError::Lagged(_)) => (None, true),
                        Err(broadcast::error::RecvError::Closed) => {
                            let state = state_tx.borrow().clone();
                            let _ = state_tx.send_replace(
                                state.failed("exec-server process event stream closed".to_string()),
                            );
                            output_closed.store(true, Ordering::Release);
                            output_closed_notify.notify_waiters();
                            cancellation_token.cancel();
                            break;
                        }
                    },
                    changed = wake_rx.changed(), if wake_open => {
                        if changed.is_err() {
                            wake_open = false;
                            continue;
                        }
                        let wake_seq = *wake_rx.borrow_and_update();
                        if wake_seq <= last_seq {
                            continue;
                        }
                        match events.try_recv() {
                            Ok(event) => (Some(event), false),
                            Err(broadcast::error::TryRecvError::Lagged(_)) => (None, true),
                            Err(broadcast::error::TryRecvError::Empty) => (None, false),
                            Err(broadcast::error::TryRecvError::Closed) => {
                                let state = state_tx.borrow().clone();
                                let _ = state_tx.send_replace(
                                    state.failed("exec-server process event stream closed".to_string()),
                                );
                                output_closed.store(true, Ordering::Release);
                                output_closed_notify.notify_waiters();
                                cancellation_token.cancel();
                                break;
                            }
                        }
                    }
                };
                let event_seq = event.as_ref().and_then(|event| match event {
                    ExecProcessEvent::Output(chunk) => Some(chunk.seq),
                    ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq } => {
                        Some(*seq)
                    }
                    ExecProcessEvent::Failed(_) => None,
                });
                let missing_sandbox_denial = matches!(
                    event.as_ref(),
                    Some(ExecProcessEvent::Exited {
                        sandbox_denied: None,
                        ..
                    })
                );
                if event.is_none()
                    || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1))
                    || missing_sandbox_denial
                {
                    let recovery_was_required = event_delivery_lost
                        || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1));
                    let mut saw_gap_metadata = false;
                    let mut incomplete_reason: Option<String> = None;
                    let mut terminal_exited = false;
                    let mut terminal_exit_code = None;
                    let mut terminal_closed = false;
                    let mut terminal_failure = None;
                    let mut terminal_sandbox_denied = false;

                    for page_index in 0..RECOVERY_PAGE_LIMIT {
                        let page_start_seq = last_seq;
                        let response = match process
                            .read_with_limits(
                                Some(last_seq),
                                Some(RECOVERY_PAGE_MAX_BYTES),
                                Some(RECOVERY_PAGE_MAX_CHUNKS),
                                Some(0),
                            )
                            .await
                        {
                            Ok(response) => response,
                            Err(err) => {
                                terminal_failure = Some(err.to_string());
                                break;
                            }
                        };
                        let ExecReadResponse {
                            chunks,
                            output_gaps,
                            next_seq,
                            exited,
                            exit_code,
                            closed,
                            failure,
                            sandbox_denied,
                        } = response;
                        terminal_exited |= exited;
                        if terminal_exit_code.is_some()
                            && exit_code.is_some()
                            && terminal_exit_code != exit_code
                        {
                            incomplete_reason =
                                Some("recovery returned conflicting exit codes".to_string());
                            break;
                        }
                        terminal_exit_code = terminal_exit_code.or(exit_code);
                        terminal_closed |= closed;
                        terminal_failure = terminal_failure.or(failure);
                        terminal_sandbox_denied |= sandbox_denied;

                        saw_gap_metadata |= !output_gaps.is_empty();
                        for gap in output_gaps {
                            if !merge_recovery_gap(&mut recovery_gaps, gap, &recovered_sequences) {
                                incomplete_reason =
                                    Some("invalid or excessive disjoint recovery gaps".to_string());
                                break;
                            }
                        }
                        if incomplete_reason.is_some() {
                            break;
                        }

                        let mut page_chunk_count = 0_usize;
                        let mut page_bytes = 0_usize;
                        let mut previous_seq = last_seq;
                        let mut response_previous_seq = None;
                        let mut local_page_limit_reached = false;
                        for chunk in chunks {
                            if response_previous_seq.is_some_and(|seq| chunk.seq <= seq) {
                                incomplete_reason =
                                    Some("recovery output ordering was invalid".to_string());
                                break;
                            }
                            response_previous_seq = Some(chunk.seq);
                            if recovery_gaps.iter().any(|gap| {
                                gap.first_missing_seq <= chunk.seq
                                    && chunk.seq <= gap.last_missing_seq
                            }) {
                                incomplete_reason =
                                    Some("recovery gap overlapped recovered output".to_string());
                                break;
                            }
                            if chunk.seq <= page_start_seq {
                                continue;
                            }
                            if page_chunk_count >= RECOVERY_PAGE_MAX_CHUNKS {
                                local_page_limit_reached = true;
                                break;
                            }
                            let chunk_len = chunk.chunk.0.len();
                            if page_bytes.saturating_add(chunk_len) > RECOVERY_PAGE_MAX_BYTES {
                                if page_chunk_count == 0 {
                                    incomplete_reason = Some(
                                        "recovery chunk exceeded the recovery byte limit"
                                            .to_string(),
                                    );
                                } else {
                                    local_page_limit_reached = true;
                                }
                                break;
                            }
                            if recovered_sequences.contains(&chunk.seq) {
                                incomplete_reason =
                                    Some("recovery gap overlapped recovered output".to_string());
                                break;
                            }
                            if chunk.seq > previous_seq.saturating_add(1)
                                && !range_is_declared_missing(
                                    &recovery_gaps,
                                    previous_seq.saturating_add(1),
                                    chunk.seq.saturating_sub(1),
                                )
                            {
                                incomplete_reason = Some(
                                    "recovery contained an unexplained sequence gap".to_string(),
                                );
                                break;
                            }
                            recovered_sequences.insert(chunk.seq);
                            previous_seq = chunk.seq;
                            last_seq = chunk.seq;
                            page_chunk_count = page_chunk_count.saturating_add(1);
                            page_bytes = page_bytes.saturating_add(chunk_len);
                            let bytes = chunk.chunk.into_inner();
                            if let Some(writer) = artifact_writer.as_mut() {
                                writer
                                    .write_chunk(raw_output_artifact.as_ref(), &bytes)
                                    .await;
                            }
                            let mut guard = output_buffer.lock().await;
                            guard.push_chunk(bytes.clone());
                            drop(guard);
                            let _ = output_tx.send(bytes);
                            output_notify.notify_waiters();
                        }
                        last_seq = advance_through_declared_gaps(last_seq, &recovery_gaps);

                        if incomplete_reason.is_some() {
                            break;
                        }

                        let page_was_limited = local_page_limit_reached
                            || page_chunk_count >= RECOVERY_PAGE_MAX_CHUNKS
                            || page_bytes >= RECOVERY_PAGE_MAX_BYTES;
                        if page_chunk_count == 0
                            && last_seq <= page_start_seq
                            && next_seq <= page_start_seq.saturating_add(1)
                            && !terminal_exited
                            && !terminal_closed
                            && terminal_failure.is_none()
                        {
                            incomplete_reason =
                                Some("recovery stalled without cursor progress".to_string());
                            break;
                        }
                        if !page_was_limited {
                            let target_seq = next_seq.saturating_sub(1);
                            last_seq = advance_through_declared_gaps(last_seq, &recovery_gaps);
                            if target_seq > last_seq {
                                let current_lifecycle_seq =
                                    event.as_ref().and_then(|event| match event {
                                        ExecProcessEvent::Exited { seq, .. }
                                        | ExecProcessEvent::Closed { seq } => Some(*seq),
                                        _ => None,
                                    });
                                if current_lifecycle_seq != Some(target_seq)
                                    || target_seq != last_seq.saturating_add(1)
                                {
                                    incomplete_reason.get_or_insert_with(|| {
                                        "recovery terminal continuity was ambiguous".to_string()
                                    });
                                }
                                recovered_sequences.insert(target_seq);
                                last_seq = target_seq;
                            }
                            break;
                        }
                        if last_seq <= page_start_seq {
                            incomplete_reason =
                                Some("recovery stalled without cursor progress".to_string());
                            break;
                        }
                        if page_index + 1 == RECOVERY_PAGE_LIMIT {
                            incomplete_reason =
                                Some("recovery page limit was exhausted".to_string());
                            break;
                        }
                    }

                    if recovery_was_required && !saw_gap_metadata {
                        incomplete_reason.get_or_insert_with(|| {
                            "peer supplied no recovery gap metadata".to_string()
                        });
                    }
                    if terminal_closed && !terminal_exited && terminal_failure.is_none() {
                        incomplete_reason.get_or_insert_with(|| {
                            "recovery returned closed without a conclusive exit".to_string()
                        });
                    }
                    if let Some(reason) = incomplete_reason {
                        recovery_reasons.insert(reason);
                    }
                    if !recovery_gaps.is_empty() || !recovery_reasons.is_empty() {
                        let detail = recovery_incomplete_detail(&recovery_gaps, &recovery_reasons);
                        HeadTailBuffer::record_shared_recovery_detail(&recovery_evidence, detail);
                        output_notify.notify_waiters();
                    }
                    if let Some(message) = terminal_failure {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    if terminal_sandbox_denied || terminal_exited {
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= terminal_sandbox_denied;
                        let _ = state_tx.send_replace(if terminal_exited {
                            state.exited(terminal_exit_code)
                        } else {
                            state
                        });
                    }
                    if terminal_closed {
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    continue;
                }

                let Some(event) = event else {
                    continue;
                };
                match event {
                    ExecProcessEvent::Output(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        recovered_sequences.insert(chunk.seq);
                        last_seq = chunk.seq;
                        let bytes = chunk.chunk.into_inner();
                        if let Some(writer) = artifact_writer.as_mut() {
                            writer
                                .write_chunk(raw_output_artifact.as_ref(), &bytes)
                                .await;
                        }
                        let mut guard = output_buffer.lock().await;
                        guard.push_chunk(bytes.clone());
                        drop(guard);
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    ExecProcessEvent::Exited {
                        seq,
                        exit_code,
                        sandbox_denied,
                    } => {
                        if seq <= last_seq {
                            continue;
                        }
                        recovered_sequences.insert(seq);
                        last_seq = seq;
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= sandbox_denied.unwrap_or(false);
                        let _ = state_tx.send_replace(state.exited(Some(exit_code)));
                    }
                    ExecProcessEvent::Closed { seq } => {
                        if seq <= last_seq {
                            continue;
                        }
                        recovered_sequences.insert(seq);
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                    ExecProcessEvent::Failed(message) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        output_closed.store(true, Ordering::Release);
                        output_closed_notify.notify_waiters();
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
            if let Some(writer) = artifact_writer.as_mut() {
                if !recovery_gaps.is_empty() || !recovery_reasons.is_empty() {
                    let marker = recovery_incomplete_marker(&recovery_gaps, &recovery_reasons);
                    writer
                        .write_chunk(raw_output_artifact.as_ref(), &marker)
                        .await;
                }
                writer.finish(raw_output_artifact.as_ref()).await;
            }
        })
    }

    fn spawn_local_output_task(
        mut stdout_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
        mut stderr_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        raw_output_artifact: Option<Arc<Mutex<RawOutputArtifact>>>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token: _,
            recovery_evidence: _,
        } = output_handles;
        tokio::spawn(async move {
            let mut artifact_writer =
                RawOutputArtifactWriter::open(raw_output_artifact.as_ref()).await;
            let mut stdout_open = true;
            let mut stderr_open = true;
            loop {
                let chunk = tokio::select! {
                    chunk = stdout_rx.recv(), if stdout_open => match chunk {
                        Some(chunk) => Some(chunk),
                        None => {
                            stdout_open = false;
                            None
                        }
                    },
                    chunk = stderr_rx.recv(), if stderr_open => match chunk {
                        Some(chunk) => Some(chunk),
                        None => {
                            stderr_open = false;
                            None
                        }
                    },
                    else => break,
                };
                if let Some(chunk) = chunk {
                    if let Some(writer) = artifact_writer.as_mut() {
                        writer
                            .write_chunk(raw_output_artifact.as_ref(), &chunk)
                            .await;
                    }
                    let mut guard = output_buffer.lock().await;
                    guard.push_chunk(chunk.clone());
                    drop(guard);
                    let _ = output_tx.send(chunk);
                    output_notify.notify_waiters();
                }
            }
            output_closed.store(true, Ordering::Release);
            output_closed_notify.notify_waiters();
            if let Some(writer) = artifact_writer.as_mut() {
                writer.finish(raw_output_artifact.as_ref()).await;
            }
        })
    }

    fn signal_exit(&self, exit_code: Option<i32>) {
        let state = self.state_rx.borrow().clone();
        let _ = self.state_tx.send_replace(state.exited(exit_code));
        self.cancellation_token.cancel();
    }
}

impl Drop for UnifiedExecProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}
