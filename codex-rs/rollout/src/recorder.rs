//! Persist Codex session rollouts (.jsonl) so sessions can be replayed or inspected later.

use std::borrow::Cow;
use std::fs;
use std::fs::File;
use std::future::Future;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::BaseInstructions;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use super::SESSIONS_SUBDIR;
use super::compression;
use crate::config::RolloutConfigView;
use crate::payload_artifact::AppendPayloadTiming;
use crate::payload_artifact::time_append_stage;
use codex_git_utils::RepositoryContext;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::DiagnosticOrigin;
use codex_protocol::protocol::DiagnosticOrigins;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;

const RECONSTRUCTION_NOTICE_ID_BASE: &str = "msg_rollout_gap";

mod listing;

/// Reserved loader-owned identity; provider output must not claim this namespace.
pub fn is_rollout_reconstruction_notice_id(id: &str) -> bool {
    id.starts_with("msg_rollout_gap_")
}

/// Reconstruction integrity is global source evidence, not a turn-local message.
/// Text alone (including a quoted gap tag) never grants this lifecycle.
pub fn is_rollout_reconstruction_notice(item: &codex_protocol::models::ResponseItem) -> bool {
    matches!(item, codex_protocol::models::ResponseItem::Message { id: Some(id), role, .. }
        if role == "developer" && is_rollout_reconstruction_notice_id(id))
}

/// Consumers that cannot represent partial source integrity must fail explicitly,
/// not project a surviving terminal boundary as a complete history.
pub fn ensure_rollout_source_integrity(items: &[RolloutItem]) -> std::io::Result<()> {
    let partial = items.iter().any(|item| match item {
        RolloutItem::ResponseItem(item) => is_rollout_reconstruction_notice(item),
        RolloutItem::Compacted(item) => item
            .replacement_history
            .as_ref()
            .is_some_and(|items| items.iter().any(is_rollout_reconstruction_notice)),
        _ => false,
    });
    if partial {
        Err(IoError::new(
            ErrorKind::InvalidData,
            "rollout history has known reconstruction gaps; partial source integrity cannot be represented by this API view",
        ))
    } else {
        Ok(())
    }
}

/// Writes canonical session rollout items to JSONL.
///
/// Rollouts are recorded as JSONL and can be inspected with tools such as:
///
/// ```text
/// $ jq -C . ~/.codex/sessions/rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl
/// $ fx ~/.codex/sessions/rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl
/// ```
#[derive(Clone)]
pub struct RolloutRecorder {
    tx: Sender<RolloutCmd>,
    writer_task: Arc<RolloutWriterTask>,
    pub(crate) rollout_path: PathBuf,
}

#[derive(Clone)]
pub enum RolloutRecorderParams {
    Create {
        session_id: SessionId,
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
        source: Box<SessionSource>,
        thread_source: Option<ThreadSource>,
        originator: String,
        base_instructions: BaseInstructions,
        dynamic_tools: Vec<DynamicToolSpec>,
        selected_capability_roots: Vec<SelectedCapabilityRoot>,
        multi_agent_version: Option<MultiAgentVersion>,
        history_mode: ThreadHistoryMode,
        initial_window_id: Option<String>,
    },
    Resume {
        path: PathBuf,
    },
}

/// A native append can establish origin; copied (including unattributed)
/// history must not acquire a new origin at its destination.
pub enum DiagnosticOriginSource {
    Unknown,
    Fresh(ThreadId),
    Copied(DiagnosticOrigins),
}

impl DiagnosticOriginSource {
    pub fn origin_for(
        &self,
        index: usize,
        item: &RolloutItem,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Option<DiagnosticOrigin> {
        if !item.is_origin_diagnostic() {
            return None;
        }
        match self {
            Self::Unknown => None,
            Self::Fresh(thread_id) => Some(DiagnosticOrigin::new(*thread_id, timestamp)),
            Self::Copied(origins) => origins.get(&index).cloned(),
        }
    }
}

/// A rollout item paired with the instant it was handed to the recorder.
///
/// Records are coalesced and flushed in batches, and a batch can be rebased or
/// retried before it lands. Stamping at write time gave every record in a batch
/// the flush time instead of its own, so a record's timestamp described when
/// the writer got around to it rather than when it happened.
#[derive(Clone, Debug)]
struct CapturedRolloutItem {
    captured_at: OffsetDateTime,
    item: CapturedRolloutPayload,
    diagnostic_origin: Option<Box<DiagnosticOrigin>>,
    // Shared batch admission follows canonical records from enqueue through
    // persistence, including retries and in-place manifest compaction.
    admission: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}

#[derive(Clone, Debug)]
enum CapturedRolloutPayload {
    Canonical(RolloutItem),
    Artifact(Arc<CapturedRolloutArtifact>),
}

#[derive(Debug)]
struct CapturedRolloutArtifact {
    line: Vec<u8>,
    // Custody covers publication, queueing, retries and terminal durability.
    _retention: File,
    _sync_owner: Arc<crate::payload_artifact::PayloadSyncOwner>,
}

impl CapturedRolloutItem {
    fn new(item: RolloutItem, captured_at: OffsetDateTime) -> Self {
        Self {
            captured_at,
            item: CapturedRolloutPayload::Canonical(item),
            diagnostic_origin: None,
            admission: None,
        }
    }

    fn canonical_item(&self) -> Option<&RolloutItem> {
        match &self.item {
            CapturedRolloutPayload::Canonical(item) => Some(item),
            CapturedRolloutPayload::Artifact(_) => None,
        }
    }

    /// Keeps the original capture time while replacing the payload, for the
    /// manifest encoding that rewrites an item in place.
    fn with_item(&self, item: RolloutItem) -> Self {
        Self {
            captured_at: self.captured_at,
            item: CapturedRolloutPayload::Canonical(item),
            diagnostic_origin: self.diagnostic_origin.clone(),
            admission: self.admission.clone(),
        }
    }
}

enum RolloutCmd {
    AddItems {
        items: Vec<CapturedRolloutItem>,
    },
    Persist {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    /// Ensure all prior writes are processed; respond when flushed. A durable
    /// flush also waits for the written bytes to reach stable storage.
    Flush {
        ack: oneshot::Sender<std::io::Result<()>>,
        durable: bool,
    },
    Shutdown {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    Discard {
        ack: oneshot::Sender<()>,
    },
    Maintenance {
        ack: oneshot::Sender<std::io::Result<()>>,
        decision: oneshot::Receiver<MaintenanceDecision>,
        preserve_bytes: bool,
        publication_order: Option<Arc<tokio::sync::OwnedMutexGuard<()>>>,
    },
}

/// Observable state for the background rollout writer task.
struct RolloutWriterTask {
    handle: Mutex<Option<JoinHandle<()>>>,
    terminal_failure: Mutex<Option<Arc<IoError>>>,
    // Clones share custody, not independent lease references. A stopped handle
    // must not keep a terminated writer's logical ownership alive.
    writer_lease: Mutex<Option<Arc<std::fs::File>>>,
    lease_custody_released: tokio::sync::watch::Sender<bool>,
    lifecycle: AtomicU8,
    shutdown_confirmed: std::sync::atomic::AtomicBool,
    /// Serializes the lifecycle check with command enqueue across recorder
    /// clones so no command can be accepted behind Shutdown.
    enqueue_gate: Arc<tokio::sync::Semaphore>,
    pending_bytes: Arc<tokio::sync::Semaphore>,
}

/// The writer future, not its observers, owns release on cancellation or panic.
struct RolloutLeaseCustody(Arc<RolloutWriterTask>);

impl Drop for RolloutLeaseCustody {
    fn drop(&mut self) {
        if !matches!(
            self.0.lifecycle.load(Ordering::Acquire),
            WRITER_SHUT_DOWN | WRITER_FAILED
        ) {
            self.0.mark_failed(
                IoError::other("rollout writer stopped before terminal settlement").into(),
            );
        }
        // Close the file after the guard, still before signalling release.
        let writer_lease = self
            .0
            .writer_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(writer_lease);
        self.0.lease_custody_released.send_replace(true);
    }
}

const MAX_PENDING_ROLLOUT_BYTES: usize = 64 * 1024 * 1024;

fn rollout_admission_bytes(items: &[RolloutItem]) -> std::io::Result<u32> {
    serialized_admission_bytes(items, items.len())
}

fn serialized_admission_bytes(
    items: &(impl serde::Serialize + ?Sized),
    count: usize,
) -> std::io::Result<u32> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len().saturating_mul(4));
            if self.0 > MAX_PENDING_ROLLOUT_BYTES {
                return Err(IoError::other(
                    "rollout batch exceeds pending byte budget; no items accepted",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Account for decoded structure as well as payload bytes. Count before
    // cloning, without allocating a second serialized copy of the batch.
    let mut counter =
        Counter(count.saturating_mul(std::mem::size_of::<CapturedRolloutItem>() + 256));
    serde_json::to_writer(&mut counter, items).map_err(IoError::other)?;
    Ok(counter.0 as u32)
}

const WRITER_ACTIVE: u8 = 0;
const WRITER_SHUTTING_DOWN: u8 = 1;
const WRITER_SHUT_DOWN: u8 = 2;
const WRITER_FAILED: u8 = 3;

enum MaintenanceDecision {
    Resume,
    Commit,
    RestoreFailed(Arc<IoError>),
}

/// Temporarily closes the physical writer while retaining ordered admission and
/// logical ownership. Restore files and release maintenance locks before dropping
/// this guard. Cancellation then resumes the same writer, not a replacement task.
pub struct RolloutMaintenanceGuard {
    decision: Option<oneshot::Sender<MaintenanceDecision>>,
    writer_task: Arc<RolloutWriterTask>,
    _enqueue_guard: tokio::sync::OwnedSemaphorePermit,
    _writer_lease: Option<Arc<std::fs::File>>,
}

impl std::fmt::Debug for RolloutMaintenanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RolloutMaintenanceGuard")
            .finish_non_exhaustive()
    }
}

impl RolloutMaintenanceGuard {
    /// Reject later admission before releasing the enqueue gate.
    pub fn commit(mut self) {
        self.writer_task.finish_shutdown(true);
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(MaintenanceDecision::Commit);
        }
    }

    /// A failed restore must never reopen/create a replacement rollout.
    pub fn restore_failed(mut self) {
        let error = Arc::new(IoError::other("rollout maintenance restoration failed"));
        self.writer_task.mark_failed(Arc::clone(&error));
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(MaintenanceDecision::RestoreFailed(error));
        }
    }
}

impl Drop for RolloutMaintenanceGuard {
    fn drop(&mut self) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(MaintenanceDecision::Resume);
        }
    }
}

impl RolloutWriterTask {
    /// Create task observability state before spawning the writer.
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            terminal_failure: Mutex::new(None),
            writer_lease: Mutex::new(None),
            lease_custody_released: tokio::sync::watch::Sender::new(false),
            lifecycle: AtomicU8::new(WRITER_ACTIVE),
            shutdown_confirmed: std::sync::atomic::AtomicBool::new(false),
            enqueue_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            pending_bytes: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_ROLLOUT_BYTES)),
        }
    }

    /// Store the spawned task handle so it remains owned for the lifetime of recorder clones.
    fn set_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(handle);
    }

    /// Remember a terminal task failure for future recorder API calls.
    fn mark_failed(&self, err: Arc<IoError>) {
        let mut guard = self
            .terminal_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A maintenance guard can publish the failure before the writer exits.
        guard.get_or_insert(err);
        self.lifecycle.store(WRITER_FAILED, Ordering::Release);
    }

    /// Return the terminal writer-task failure, if the task exited with an error.
    fn terminal_failure(&self) -> Option<IoError> {
        let guard = self
            .terminal_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map(share_io_error)
    }

    fn ensure_active(&self, operation: &str) -> std::io::Result<()> {
        match self.lifecycle.load(Ordering::Acquire) {
            WRITER_ACTIVE => Ok(()),
            WRITER_SHUTTING_DOWN => Err(IoError::other(format!(
                "cannot {operation} while rollout shutdown is in progress"
            ))),
            WRITER_SHUT_DOWN => Err(IoError::other(format!(
                "cannot {operation} after rollout shutdown completed"
            ))),
            _ => Err(self.writer_task_failure(operation)),
        }
    }

    fn writer_task_failure(&self, operation: &str) -> IoError {
        self.terminal_failure().unwrap_or_else(|| {
            IoError::other(format!(
                "cannot {operation} after the rollout writer task failed"
            ))
        })
    }

    fn begin_shutdown(&self) -> std::io::Result<()> {
        self.lifecycle
            .compare_exchange(
                WRITER_ACTIVE,
                WRITER_SHUTTING_DOWN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| {
                if state == WRITER_FAILED {
                    return self.writer_task_failure("shut down the rollout");
                }
                let phase = if state == WRITER_SHUTTING_DOWN {
                    "already in progress"
                } else {
                    "already completed"
                };
                IoError::other(format!("rollout shutdown is {phase}"))
            })
    }

    fn finish_shutdown(&self, succeeded: bool) {
        if succeeded {
            let writer_lease = self.writer_lease
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(writer_lease);
        }
        self.lifecycle.store(
            if succeeded {
                WRITER_SHUT_DOWN
            } else {
                WRITER_ACTIVE
            },
            Ordering::Release,
        );
    }
}

fn share_io_error(err: &Arc<IoError>) -> IoError {
    if let Some(code) = err.raw_os_error() {
        IoError::from_raw_os_error(code)
    } else {
        // Keep the original error rather than flattening it to text.
        IoError::new(err.kind(), Arc::clone(err))
    }
}

impl RolloutRecorderParams {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
        source: SessionSource,
        thread_source: Option<ThreadSource>,
        originator: String,
        base_instructions: BaseInstructions,
        dynamic_tools: Vec<DynamicToolSpec>,
    ) -> Self {
        Self::Create {
            session_id: conversation_id.into(),
            conversation_id,
            forked_from_id,
            parent_thread_id,
            source: Box::new(source),
            thread_source,
            originator,
            base_instructions,
            dynamic_tools,
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: Default::default(),
            initial_window_id: None,
        }
    }

    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        if let Self::Create { session_id: id, .. } = &mut self {
            *id = session_id;
        }
        self
    }

    pub fn with_selected_capability_roots(
        mut self,
        selected_capability_roots: Vec<SelectedCapabilityRoot>,
    ) -> Self {
        if let Self::Create {
            selected_capability_roots: roots,
            ..
        } = &mut self
        {
            *roots = selected_capability_roots;
        }
        self
    }

    pub fn with_multi_agent_version(
        mut self,
        multi_agent_version: Option<MultiAgentVersion>,
    ) -> Self {
        if let Self::Create {
            multi_agent_version: version,
            ..
        } = &mut self
        {
            *version = multi_agent_version;
        }
        self
    }

    pub fn with_history_mode(mut self, history_mode: ThreadHistoryMode) -> Self {
        if let Self::Create {
            history_mode: mode, ..
        } = &mut self
        {
            *mode = history_mode;
        }
        self
    }

    pub fn with_initial_window_id(mut self, initial_window_id: String) -> Self {
        if let Self::Create {
            initial_window_id: window_id,
            ..
        } = &mut self
        {
            *window_id = Some(initial_window_id);
        }
        self
    }

    pub fn resume(path: PathBuf) -> Self {
        Self::Resume { path }
    }
}


fn initial_harness_build_info() -> codex_protocol::protocol::SessionBuildInfo {
    let build = codex_utils_build_info::BuildInfo::current();
    codex_protocol::protocol::SessionBuildInfo {
        version: build.version.to_string(),
        commit: build.commit.to_string(),
        dirty: build.dirty.to_string(),
        profile: build.profile.to_string(),
        built: build.built.to_string(),
        executable_sha256: None,
        token_accounting: [
            (
                "local_input_estimate",
                "utf8_bytes_div4_ceil_v1",
                "complete_serialized_logical_prompt",
            ),
            (
                "logical_category_tokens",
                "utf8_bytes_div4_ceil_v1",
                "sum_of_independently_rounded_category_fragments",
            ),
            (
                "canonical_tool_tokens",
                "utf8_bytes_div4_ceil_v1",
                "canonical_tool_text_plus_nontext_estimates",
            ),
            (
                "model_facing_tool_tokens",
                "utf8_bytes_div4_ceil_v1",
                "rendered_model_packet_plus_nontext_estimates",
            ),
            (
                "projection_text_budget",
                "o200k_base_count_ordinary_v1",
                "rendered_text",
            ),
            (
                "provider_input_tokens",
                "provider_reported_tokenizer_unknown",
                "provider_request_input",
            ),
        ]
        .into_iter()
        .map(|(name, estimator, scope)| {
            (
                name.to_string(),
                codex_protocol::protocol::SessionTokenMeasurement {
                    estimator: estimator.to_string(),
                    scope: scope.to_string(),
                },
            )
        })
        .collect(),
    }
}

/// Current process identity, not the process that originally created a resumed
/// rollout. First use can hash the executable; call from a blocking worker.
pub fn current_harness_build_info() -> codex_protocol::protocol::SessionBuildInfo {
    static CURRENT: std::sync::LazyLock<codex_protocol::protocol::SessionBuildInfo> =
        std::sync::LazyLock::new(|| {
            let mut build = initial_harness_build_info();
            build.executable_sha256 =
                codex_utils_build_info::executable_identity_sha256().map(str::to_owned);
            build
        });
    CURRENT.clone()
}

impl RolloutRecorder {
    /// Preflight a complete response batch before image decode, cloning, or
    /// acceptance. Borrow payloads while using the writer's exact wire shape
    /// and pending-memory charge; this does not reserve queue capacity.
    pub fn validate_response_items(
        items: &[codex_protocol::models::ResponseItem],
    ) -> std::io::Result<()> {
        #[derive(serde::Serialize)]
        #[serde(tag = "type", content = "payload", rename_all = "snake_case")]
        enum BorrowedItem<'a> {
            ResponseItem(&'a codex_protocol::models::ResponseItem),
        }
        let borrowed = items
            .iter()
            .map(BorrowedItem::ResponseItem)
            .collect::<Vec<_>>();
        serialized_admission_bytes(&borrowed, items.len()).map(|_| ()).map_err(|error| {
            IoError::new(std::io::ErrorKind::InvalidInput, format!(
                "Input batch exceeds the 64 MiB rollout admission budget (four times serialized bytes plus record overhead); no items accepted. Use fewer or smaller images/text. {error}"
            ))
        })
    }


    /// Attempt to create a new [`RolloutRecorder`].
    ///
    /// For newly created sessions, this precomputes path/metadata and defers
    /// file creation/open until an explicit `persist()` call.
    ///
    /// For resumed sessions, this immediately opens the existing rollout file.
    pub async fn new(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
    ) -> std::io::Result<Self> {
        Self::new_with_repository_context(config, params, None).await
    }

    /// Creates a recorder while optionally reusing repository context discovered by the caller.
    ///
    /// `None` means the caller has no observation and preserves the normal discovery path.
    /// `Some(None)` records a known absence without probing again.
    pub async fn new_with_repository_context(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
        known_repository_context: Option<Option<RepositoryContext>>,
    ) -> std::io::Result<Self> {
        Self::new_with_resume_state(config, params, known_repository_context, None, None).await
    }

    /// Thread-store entrypoint; the background writer retains ownership through shutdown.
    pub async fn new_with_writer_lease(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
        known_repository_context: Option<Option<RepositoryContext>>,
        lease: Arc<std::fs::File>,
    ) -> std::io::Result<Self> {
        Self::new_with_resume_state(config, params, known_repository_context, None, Some(lease))
            .await
    }

    pub fn writer_lease(&self) -> Option<Arc<std::fs::File>> {
        self.writer_task
            .writer_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// On a terminal/closing writer, wait until its lease custody has unwound.
    /// An active writer remains eligible for retry and must not be retired.
    pub async fn wait_for_writer_retirement(&self) -> bool {
        let phase = self.writer_task.lifecycle.load(Ordering::Acquire);
        if phase == WRITER_ACTIVE {
            return false;
        }
        if phase == WRITER_SHUT_DOWN {
            // Successful shutdown releases the lease before publishing this phase.
            return true;
        }
        let mut released = self.writer_task.lease_custody_released.subscribe();
        if phase == WRITER_FAILED {
            return released.wait_for(|released| *released).await.is_ok();
        }
        // A concurrent normal shutdown can fail and return to Active.
        // Do not wait indefinitely for a writer that can still be reused.
        *released.borrow_and_update()
    }

    /// Identity of the append owner, independent of path reuse or recorder clones.
    pub fn same_writer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.writer_task, &other.writer_task)
    }

    /// Quiesce this recorder before acquiring rollout-move or maintenance locks.
    /// Bounded admission prevents blocked storage from stranding a delete caller.
    pub async fn pause_for_maintenance(&self) -> std::io::Result<RolloutMaintenanceGuard> {
        self.pause_for_maintenance_inner(false, None).await
    }

    /// Quiesce under the caller's per-thread publication order. Keep a share
    /// until the writer completes rollback, including after caller cancellation.
    /// An outstanding intent owns its exact bytes: do not flush or repair them.
    pub async fn pause_for_maintenance_ordered(
        &self,
        publication_order: Arc<tokio::sync::OwnedMutexGuard<()>>,
        preserve_bytes: bool,
    ) -> std::io::Result<RolloutMaintenanceGuard> {
        self.pause_for_maintenance_inner(preserve_bytes, Some(publication_order))
            .await
    }

    async fn pause_for_maintenance_inner(
        &self,
        preserve_bytes: bool,
        publication_order: Option<Arc<tokio::sync::OwnedMutexGuard<()>>>,
    ) -> std::io::Result<RolloutMaintenanceGuard> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let enqueue_guard = Arc::clone(&self.writer_task.enqueue_gate)
                .acquire_owned()
                .await
                .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
            self.writer_task.ensure_active("pause for maintenance")?;
            let permit = self.tx.reserve().await.map_err(|_| {
                self.writer_task
                    .writer_task_failure("pause for maintenance")
            })?;
            let (decision, receiver) = oneshot::channel();
            let guard = RolloutMaintenanceGuard {
                decision: Some(decision),
                writer_task: Arc::clone(&self.writer_task),
                _enqueue_guard: enqueue_guard,
                _writer_lease: self.writer_lease(),
            };
            let (ack, ready) = oneshot::channel();
            permit.send(RolloutCmd::Maintenance {
                ack,
                decision: receiver,
                preserve_bytes,
                publication_order,
            });
            ready.await.map_err(|_| {
                self.writer_task
                    .writer_task_failure("pause for maintenance")
            })??;
            Ok(guard)
        })
        .await
        .map_err(|_| {
            IoError::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for rollout maintenance",
            )
        })?
    }


    pub async fn resume_and_load_with_writer_lease(
        config: &impl RolloutConfigView,
        path: PathBuf,
        expected_thread: ThreadId,
        lease: Option<Arc<std::fs::File>>,
    ) -> std::io::Result<(Self, Vec<RolloutItem>)> {
        let mut items = Vec::new();
        let mut manifests = crate::ToolManifestDictionary::default();
        let (path, writer, integrity) =
            open_rollout_for_append_with_replay(&path, |item, _| {
                if let RolloutItem::ToolManifest(manifest) = &item
                    && let Err(error) = manifests.apply(manifest)
                {
                    warn!(%error, "failed to reconstruct persisted tool manifest");
                }
                // Append preflight still verifies and enrolls every payload, but
                // diagnostic snapshots are not part of the resumed history.
                if !matches!(item, RolloutItem::ContextReplay { .. }) {
                    items.push(item);
                }
            })
            .await?;
        let (thread_id, errors, partial_tail, unresolved_payloads) = integrity
            .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "empty session file"))?;
        if thread_id != Some(expected_thread) {
            return Err(IoError::other(
                "resume rollout belongs to a different thread",
            ));
        }
        Self::append_reconstruction_notice(&mut items, errors, partial_tail, unresolved_payloads);
        let recorder = Self::new_with_resume_state(
            config,
            RolloutRecorderParams::resume(path),
            None,
            Some((writer, manifests)),
            lease,
        )
        .await?;
        Ok((recorder, items))
    }

    async fn new_with_resume_state(
        config: &impl RolloutConfigView,
        params: RolloutRecorderParams,
        known_repository_context: Option<Option<RepositoryContext>>,
        resumed: Option<(JsonlWriter, crate::ToolManifestDictionary)>,
        writer_lease: Option<Arc<std::fs::File>>,
    ) -> std::io::Result<Self> {
        // Session metadata and turn identity both record the executable hash;
        // start it now rather than on the first turn.
        codex_utils_build_info::prehash_executable();
        let _ = codex_protocol::timing::observations::prepare_terminal_lane();
        let (writer, deferred_log_file_info, rollout_path, meta, tool_manifests) = match params {
            RolloutRecorderParams::Create {
                session_id,
                conversation_id,
                forked_from_id,
                parent_thread_id,
                source,
                thread_source,
                originator,
                base_instructions,
                dynamic_tools,
                selected_capability_roots,
                multi_agent_version,
                history_mode,
                initial_window_id,
            } => {
                let log_file_info = precompute_log_file_info(config, conversation_id)?;
                let path = log_file_info.path.clone();
                let thread_id = log_file_info.conversation_id;
                let started_at = log_file_info.timestamp;

                let timestamp_format: &[FormatItem] = format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
                );
                let timestamp = started_at
                    .to_offset(time::UtcOffset::UTC)
                    .format(timestamp_format)
                    .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

                let session_meta = SessionMeta {
                    session_id,
                    id: thread_id,
                    forked_from_id,
                    parent_thread_id,
                    timestamp,
                    cwd: config.cwd().to_path_buf(),
                    originator,
                    cli_version: env!("CARGO_PKG_VERSION").to_string(),
                    // The first metadata write fills in the executable hash,
                    // which recorder creation started computing.
                    harness_build: Some(initial_harness_build_info()),
                    agent_nickname: source.get_nickname(),
                    agent_role: source.get_agent_role(),
                    agent_path: source.get_agent_path().map(Into::into),
                    source: *source,
                    thread_source,
                    model_provider: Some(config.model_provider_id().to_string()),
                    base_instructions: Some(base_instructions),
                    dynamic_tools: if dynamic_tools.is_empty() {
                        None
                    } else {
                        Some(dynamic_tools)
                    },
                    selected_capability_roots,
                    history_mode,
                    multi_agent_version,
                    context_window: initial_window_id.map(SessionContextWindow::new),
                };

                (
                    None,
                    Some(log_file_info),
                    path,
                    Some(session_meta),
                    crate::ToolManifestDictionary::default(),
                )
            }
            RolloutRecorderParams::Resume { path } => {
                let (path, writer, tool_manifests) = if let Some((writer, manifests)) = resumed {
                    (path, writer, manifests)
                } else {
                    let mut tool_manifests = crate::ToolManifestDictionary::default();
                    let (path, writer, _) = open_rollout_for_append_with_replay(
                        path.as_path(), |item, _| {
                            if let RolloutItem::ToolManifest(manifest) = item
                                && let Err(err) = tool_manifests.apply(&manifest)
                            {
                                warn!(%err, "failed to reconstruct persisted tool manifest");
                            }
                        },
                    ).await?;
                    (path, writer, tool_manifests)
                };
                (Some(writer), None, path, None, tool_manifests)
            }
        };

        // Clone the cwd for the spawned task to collect git info asynchronously
        let cwd = config.cwd().to_path_buf();

        // A reasonably-sized bounded channel. If the buffer fills up the send
        // future will yield, which is fine â€“ we only need to ensure we do not
        // perform *blocking* I/O on the caller's thread.
        let (tx, rx) = mpsc::channel::<RolloutCmd>(256);
        // Spawn a Tokio task that owns the file handle and performs async
        // writes. Using `tokio::fs::File` keeps everything on the async I/O
        // driver instead of blocking the runtime.
        let writer_task = Arc::new(RolloutWriterTask::new());
        *writer_task
            .writer_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = writer_lease;
        let writer_task_for_spawn = Arc::clone(&writer_task);
        let rollout_path_for_spawn = rollout_path.clone();
        let lease_custody = RolloutLeaseCustody(Arc::clone(&writer_task));
        let handle = tokio::task::spawn(async move {
            let _lease_custody = lease_custody;
            // Keep queued commands and their replies alive until any terminal
            // error is observable, even when the writer loop exits via `?`.
            let mut rx = rx;
            let result = rollout_writer(
                writer,
                deferred_log_file_info,
                &mut rx,
                meta,
                cwd,
                known_repository_context,
                rollout_path_for_spawn.clone(),
                tool_manifests,
                Arc::clone(&writer_task_for_spawn),
            )
            .await;
            if let Err(err) = result {
                // This is the terminal background-task failure path. Normal I/O failures stay inside
                // `rollout_writer`, are reported through command acks, and leave items buffered for retry.
                writer_task_for_spawn.mark_failed(Arc::clone(&err));
                drop(rx);
                error!(
                    "rollout writer task failed for {}: {err}; error_kind={:?}; raw_os_error={:?}",
                    rollout_path_for_spawn.display(),
                    err.kind(),
                    err.raw_os_error()
                );
            }
        });
        writer_task.set_handle(handle);

        Ok(Self {
            tx,
            writer_task,
            rollout_path,
        })
    }

    pub fn rollout_path(&self) -> &Path {
        self.rollout_path.as_path()
    }

    pub async fn record_canonical_items(&self, items: &[RolloutItem]) -> std::io::Result<()> {
        self.record_canonical_items_with_admission(items, None, &DiagnosticOriginSource::Unknown, 0)
            .await
    }

    /// Queue canonical items in writer order and return once the bounded queue owns them.
    /// A materialized rollout is appended to promptly afterwards, off the caller's path, so a
    /// turn that never reaches its terminal barrier still leaves its items on disk. A later
    /// persist/flush/shutdown command remains the write-completion barrier for the queued
    /// prefix; an unmaterialized rollout keeps waiting for that barrier.
    pub async fn record_canonical_items_ordered(
        &self,
        items: &[RolloutItem],
    ) -> std::io::Result<()> {
        self.record_canonical_items_ordered_with_origins(items, &DiagnosticOriginSource::Unknown)
            .await
    }

    pub async fn record_canonical_items_ordered_with_origins(
        &self,
        items: &[RolloutItem],
        origins: &DiagnosticOriginSource,
    ) -> std::io::Result<()> {
        let admission_bytes = rollout_admission_bytes(items);
        if let Err(batch_error) = admission_bytes {
            // Validate every individual item before accepting any prefix. Bulk
            // inheritance may exceed admission even though each record fits.
            let individual_charge = |item, admission| match admission {
                Ok(charge) => Ok(charge),
                Err(_) if crate::payload_artifact::is_spooled_before_admission(item) => {
                    crate::payload_artifact::validate_replay_size(item)?;
                    Ok(0) // Spool this record instead of cloning its body into the queue.
                }
                Err(error) => Err(error),
            };
            let charges = if let [item] = items {
                // The failed batch check already measured this exact singleton.
                vec![individual_charge(item, Err(batch_error))?]
            } else {
                items
                    .iter()
                    .map(|item| {
                        individual_charge(item, rollout_admission_bytes(std::slice::from_ref(item)))
                    })
                    .collect::<std::io::Result<Vec<_>>>()?
            };
            let mut start = 0;
            let mut charge = 0usize;
            for (index, next) in charges.into_iter().enumerate() {
                if next == 0 {
                    if index > start {
                        self.record_canonical_items_with_admission(
                            &items[start..index],
                            Some(charge as u32),
                            origins,
                            start,
                        )
                        .await?;
                    }
                    // Release preceding lazy/failed-write admission before staging.
                    self.persist().await?;
                    self.record_replay_artifact(&items[index]).await?;
                    self.persist().await?;
                    start = index + 1;
                    charge = 0;
                    continue;
                }
                let next = next as usize;
                if index > start && charge.saturating_add(next) > MAX_PENDING_ROLLOUT_BYTES / 2 {
                    self.record_canonical_items_with_admission(
                        &items[start..index],
                        Some(charge as u32),
                        origins,
                        start,
                    )
                    .await?;
                    // Lazy pending items release admission only after materialization.
                    // No enqueue gate is held across this barrier.
                    self.persist().await?;
                    start = index;
                    charge = 0;
                }
                charge = charge.saturating_add(next);
            }
            if start < items.len() {
                self.record_canonical_items_with_admission(&items[start..], Some(charge as u32), origins, start)
                    .await?;
            }
            return self.persist().await;
        }
        // Queue admission transfers ownership and preserves ordering with
        // barriers/shutdown under the enqueue gate. Do not wait for preceding
        // disk writes here; durability still requires an explicit barrier.
        self.record_canonical_items_with_admission(items, Some(admission_bytes?), origins, 0)
            .await
    }

    async fn record_replay_artifact(&self, item: &RolloutItem) -> std::io::Result<()> {
        let enqueue_guard = Arc::clone(&self.writer_task.enqueue_gate)
            .acquire_owned()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        self.writer_task.ensure_active("record context replay")?;
        // Only the small reference is retained in pending memory. The enqueue
        // gate bounds staging to one streaming publisher per writer, including
        // cancellation while its blocking filesystem operation is still live.
        let reference_charge = (crate::payload_artifact::INLINE_BYTES * 4
            + std::mem::size_of::<CapturedRolloutItem>()
            + 256) as u32;
        let admission = Arc::new(
            Arc::clone(&self.writer_task.pending_bytes)
                .try_acquire_many_owned(reference_charge)
                .map_err(|_| {
                    IoError::other("rollout pending byte budget exhausted; no items accepted")
                })?,
        );
        let path = self.rollout_path.clone();
        // ContextReplay shares its immutable capture; this clone does not copy
        // the large snapshot while moving filesystem work off the async runtime.
        let item = item.clone();
        let captured_at = OffsetDateTime::now_utc();
        let tx = self.tx.clone();
        let (captured, enqueue_guard, tx) = tokio::task::spawn_blocking(move || {
            let retention = crate::payload_artifact::lock_publication(&path)?;
            let sync_owner = crate::payload_artifact::PayloadSyncOwner::new(&path);
            let line = RolloutLineRef::new(&item, captured_at)?;
            let reference = crate::payload_artifact::store_replay(
                &path,
                &item,
                line.timestamp,
                line.format_version,
            )?;
            Ok::<_, IoError>((
                CapturedRolloutItem {
                    captured_at,
                    item: CapturedRolloutPayload::Artifact(Arc::new(CapturedRolloutArtifact {
                        line: reference,
                        _retention: retention,
                        _sync_owner: sync_owner,
                    })),
                    diagnostic_origin: None,
                    admission: Some(admission),
                },
                enqueue_guard,
                tx,
            ))
        })
        .await
        .map_err(IoError::other)??;
        let result = tx
            .send(RolloutCmd::AddItems { items: vec![captured] })
            .await
            .map_err(|error| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue context replay: {error}"))
                })
            });
        drop(enqueue_guard);
        result
    }

    async fn record_canonical_items_with_admission(
        &self,
        items: &[RolloutItem],
        admission_bytes: Option<u32>,
        origins: &DiagnosticOriginSource,
        offset: usize,
    ) -> std::io::Result<()> {
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        self.writer_task.ensure_active("record rollout items")?;
        if items.is_empty() {
            return Ok(());
        }
        // Ordered admission already sized this exact immutable slice. Other
        // callers still count here, before cloning or accepting any records.
        let admission_bytes = match admission_bytes {
            Some(bytes) => bytes,
            None => rollout_admission_bytes(items)?,
        };
        let admission = Arc::new(Arc::clone(&self.writer_task.pending_bytes)
            .try_acquire_many_owned(admission_bytes)
            .map_err(|_| IoError::other("rollout pending byte budget exhausted; no items accepted; use an explicit persistence barrier to retry retained records"))?);
        let result = self
            .tx
            .send(RolloutCmd::AddItems {
                // Capture once, here: this is when the caller produced these
                // records, and it is the only point that still knows.
                items: {
                    let captured_at = OffsetDateTime::now_utc();
                    let origin_timestamp = chrono::DateTime::from_timestamp_millis(
                        captured_at.unix_timestamp() * 1_000 + i64::from(captured_at.millisecond()),
                    );
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let mut captured = CapturedRolloutItem::new(item.clone(), captured_at);
                            captured.diagnostic_origin = origin_timestamp
                                .and_then(|timestamp| origins.origin_for(offset + index, item, timestamp))
                                .map(Box::new);
                            captured.admission = Some(Arc::clone(&admission));
                            captured
                        })
                        .collect()
                },
            })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout items: {e}"))
                })
            });
        drop(enqueue_guard);
        result?;
        Ok(())
    }

    /// Materialize the rollout file and persist all buffered items.
    ///
    /// This is idempotent. If materialization fails, the recorder keeps all pending items in memory
    /// and a later `persist()` or `flush()` can retry opening and writing the rollout file.
    pub async fn persist(&self) -> std::io::Result<()> {
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        self.writer_task.ensure_active("persist the rollout")?;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Persist { ack: tx })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout persist: {e}"))
                })
            })?;
        drop(enqueue_guard);
        rx.await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed waiting for rollout persist: {e}"))
            })
        })?
    }

    /// Flush all queued writes and wait until they are committed by the writer task.
    ///
    /// If the first writer attempt fails, the writer drops and reopens the file handle before
    /// retrying. This returns an error only when that retry also fails or the writer task is gone.
    pub async fn flush(&self) -> std::io::Result<()> {
        self.flush_with_durability(/*durable*/ false).await
    }

    /// Like [`Self::flush`], then waits for the rollout bytes to reach stable storage so
    /// they survive an operating-system crash, not only a process crash. Use it at terminal
    /// checkpoints; ordinary flushes stay cheap.
    pub async fn flush_durable(&self) -> std::io::Result<()> {
        self.flush_with_durability(/*durable*/ true).await
    }

    async fn flush_with_durability(&self, durable: bool) -> std::io::Result<()> {
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        self.writer_task.ensure_active("flush the rollout")?;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Flush { ack: tx, durable })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout flush: {e}"))
                })
            })?;
        drop(enqueue_guard);
        rx.await.map_err(|e| {
            self.writer_task
                .terminal_failure()
                .unwrap_or_else(|| IoError::other(format!("failed waiting for rollout flush: {e}")))
        })?
    }

    /// Load valid records plus a bounded, synthetic reconstruction notice when
    /// operational records were skipped. Diagnostic ContextReplay snapshots are
    /// excluded (including their referenced bodies); use for_each_rollout_item
    /// for supported diagnostic records and payload-integrity inspection.
    /// Keeping the notice in the existing history carrier
    /// makes gaps visible even to consumers that discard the diagnostic count.
    /// It is not written back to the source rollout by this read.
    pub async fn load_rollout_items(
        path: &Path,
    ) -> std::io::Result<(Vec<RolloutItem>, Option<ThreadId>, usize)> {
        let (items, thread_id, errors, _, _) = Self::load_rollout_items_inner(path, false, false).await?;
        Ok((items, thread_id, errors))
    }

    /// Load canonical history and dependency versions observed before hydration,
    /// without a separate discovery/decompression pass.
    pub async fn load_rollout_items_with_payload_versions(
        path: &Path,
    ) -> std::io::Result<(
        Vec<RolloutItem>,
        Option<ThreadId>,
        usize,
        Vec<crate::PayloadDependencyVersion>,
    )> {
        let (items, thread_id, errors, versions, _) =
            Self::load_rollout_items_inner(path, true, false).await?;
        Ok((items, thread_id, errors, versions))
    }

    /// Operational history and exact diagnostic origins observed by one reader.
    pub async fn load_rollout_items_with_origins(
        path: &Path,
        capture_payload_versions: bool,
    ) -> std::io::Result<(
        Vec<RolloutItem>,
        Option<ThreadId>,
        usize,
        Vec<crate::PayloadDependencyVersion>,
        DiagnosticOrigins,
    )> {
        Self::load_rollout_items_inner(path, capture_payload_versions, true).await
    }

    async fn load_rollout_items_inner(
        path: &Path,
        capture_payload_versions: bool,
        capture_origins: bool,
    ) -> std::io::Result<(
        Vec<RolloutItem>,
        Option<ThreadId>,
        usize,
        Vec<crate::PayloadDependencyVersion>,
        DiagnosticOrigins,
    )> {
        let mut items = Vec::new();
        let mut origins = DiagnosticOrigins::new();
        let (thread_id, parse_errors, trailing_partial_records, unresolved_payloads, versions) =
            Self::visit_rollout_with_origins(
                path,
                false,
                capture_payload_versions,
                true,
                capture_origins,
                None,
                |item, _, origin| {
                    if !matches!(item, RolloutItem::ContextReplay { .. }) {
                        if let Some(origin) = origin {
                            origins.insert(items.len(), origin);
                        }
                        items.push(item);
                    }
                },
            )
            .await?;
        tracing::debug!(
            "Resumed rollout with {} items, thread ID: {:?}, parse errors: {}",
            items.len(),
            thread_id,
            parse_errors,
        );
        Self::append_reconstruction_notice(
            &mut items,
            parse_errors,
            trailing_partial_records,
            unresolved_payloads,
        );
        Ok((items, thread_id, parse_errors, versions, origins))
    }

    fn append_reconstruction_notice(
        items: &mut Vec<RolloutItem>,
        parse_errors: usize,
        trailing_partial_records: usize,
        unresolved_payloads: usize,
    ) {
        if parse_errors > 0 {
            // A reference whose payload body is unreadable keeps its slot as a parse
            // error, but restoring that body restores the record: it is not malformed.
            let malformed_records = parse_errors
                .saturating_sub(trailing_partial_records)
                .saturating_sub(unresolved_payloads);
            let (skipped, payload_integrity, payload_guidance) = if unresolved_payloads == 0 {
                ("malformed", String::new(), "")
            } else {
                (
                    "unreadable",
                    format!(", unresolved_payload_artifacts={unresolved_payloads}"),
                    " Unresolved payload artifacts are record bodies under rollout-payloads/ that are missing or failed verification; restoring those exact files restores the records.",
                )
            };
            items.push(RolloutItem::ResponseItem(
                codex_protocol::models::ResponseItem::Message {
                    id: Some(codex_protocol::ResponseItemId::new(RECONSTRUCTION_NOTICE_ID_BASE)),
                    role: "developer".to_string(),
                    content: vec![codex_protocol::models::ContentItem::InputText {
                        text: format!(
                            "<rollout_reconstruction_gap>\nHistory is incomplete: {parse_errors} {skipped} rollout records were skipped. Integrity: malformed_records={malformed_records}, trailing_partial_records={trailing_partial_records}{payload_integrity}, complete=false.{payload_guidance} A trailing partial record indicates an interrupted append; other gaps may omit interior tool results or rollback markers. Missing records may include user constraints, context, rollback markers, or tool results. Do not treat this reconstruction as complete evidence or assume external effects were undone; reconcile affected work before relying on it.\n</rollout_reconstruction_gap>"
                        ),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            ));
        }
    }

    /// Visit each valid typed rollout item without retaining the full rollout in memory.
    /// Retired diagnostics remain in the source but have no typed item to visit.
    pub async fn for_each_rollout_item<F>(
        path: &Path,
        mut visit: F,
    ) -> std::io::Result<(Option<ThreadId>, usize)>
    where
        F: FnMut(RolloutItem),
    {
        Self::for_each_rollout_item_with_record_number(path, |item, _record_number| visit(item))
            .await
    }

    /// Visit each valid rollout item together with its one-based non-empty record number.
    pub(crate) async fn for_each_rollout_item_with_record_number<F>(
        path: &Path,
        visit: F,
    ) -> std::io::Result<(Option<ThreadId>, usize)>
    where
        F: FnMut(RolloutItem, usize),
    {
        let (thread_id, errors, _, _) =
            Self::for_each_rollout_item_with_integrity(path, false, visit).await?;
        Ok((thread_id, errors))
    }

    pub(crate) async fn for_each_rollout_item_with_integrity<F>(
        path: &Path,
        enroll_dependencies: bool,
        visit: F,
    ) -> std::io::Result<(Option<ThreadId>, usize, usize, usize)>
    where
        F: FnMut(RolloutItem, usize),
    {
        let (id, errors, partial, unresolved, _) =
            Self::for_each_rollout_item_with_versions(path, enroll_dependencies, false, visit).await?;
        Ok((id, errors, partial, unresolved))
    }

    pub(crate) async fn for_each_rollout_item_with_versions<F>(
        path: &Path,
        enroll_dependencies: bool,
        capture_payload_versions: bool,
        visit: F,
    ) -> std::io::Result<(
        Option<ThreadId>, usize, usize, usize, Vec<crate::PayloadDependencyVersion>,
    )>
    where
        F: FnMut(RolloutItem, usize),
    {
        Self::for_each_rollout_item_with_versions_inner(
            path, enroll_dependencies, capture_payload_versions, false, visit,
        ).await
    }

    pub(crate) async fn for_each_rollout_item_with_versions_inner<F>(
        path: &Path,
        enroll_dependencies: bool,
        capture_payload_versions: bool,
        exclude_context_replay: bool,
        mut visit: F,
    ) -> std::io::Result<(
        Option<ThreadId>,
        usize,
        usize,
        usize,
        Vec<crate::PayloadDependencyVersion>,
    )>
    where
        F: FnMut(RolloutItem, usize),
    {
        Self::visit_rollout_with_origins(
            path, enroll_dependencies, capture_payload_versions, exclude_context_replay, false,
            None, |item, record, _| visit(item, record),
        ).await
    }

    async fn visit_rollout_with_origins<F>(
        path: &Path,
        enroll_dependencies: bool,
        capture_payload_versions: bool,
        exclude_context_replay: bool,
        capture_origins: bool,
        mut timing: Option<&mut AppendWriterTiming>,
        mut visit: F,
    ) -> std::io::Result<(
        Option<ThreadId>,
        usize,
        usize,
        usize,
        Vec<crate::PayloadDependencyVersion>,
    )>
    where
        F: FnMut(RolloutItem, usize, Option<DiagnosticOrigin>),
    {
        trace!("Resuming rollout from {path:?}");
        let mut decoder = RolloutRecordDecoder { capture_origins, ..Default::default() };
        let mut reader = compression::open_rollout_line_reader(path).await?;
        if exclude_context_replay {
            reader.exclude_context_replay();
        }
        if codex_protocol::timing::observations::prepare_runtime_async_lane() {
            reader.capture_source_revision(false);
        }
        if capture_payload_versions {
            reader.capture_payload_versions();
        }
        if enroll_dependencies {
            reader.enroll_payload_dependencies();
        }
        use crate::payload_artifact::HydratedRecord;
        let measure_decode = timing.is_some();
        while let Some(record) = reader
            .next_record(enroll_dependencies, timing.as_mut().map(|t| &mut t.payload))
            .await?
        {
            if matches!(&record, HydratedRecord::Text(line) if line.trim().is_empty()) {
                continue;
            }
            // Excluded (operational load) and size-checked (append preflight) replay
            // references keep their record slot without decoding a diagnostic body.
            if (exclude_context_replay || enroll_dependencies)
                && matches!(&record, HydratedRecord::Text(line)
                    if crate::payload_artifact::is_context_replay_reference(line))
            {
                decoder.note_undecoded_record();
                continue;
            }
            // Keep small records on the fast path, but avoid monopolizing an
            // async worker while parsing a large hydrated payload. Decode in
            // sequence and move the decoder/string rather than cloning them.
            const BLOCKING_DECODE_THRESHOLD_BYTES: usize = 256 * 1024;
            let terminated = reader.last_line_terminated();
            let decoded = if let HydratedRecord::Text(line) = &record
                && line.len() < BLOCKING_DECODE_THRESHOLD_BYTES
            {
                time_append_stage(timing.as_mut().map(|t| &mut t.decode), || {
                    decoder.decode(line, terminated)
                })
            } else {
                let (returned_decoder, result, measured) = tokio::task::spawn_blocking(move || {
                    let mut decoder = decoder;
                    let mut measured = measure_decode.then(<(Duration, Duration)>::default);
                    let result = match record {
                        HydratedRecord::Text(line) => {
                            time_append_stage(measured.as_mut().map(|t| &mut t.0), || {
                                decoder.decode(&line, terminated)
                            })
                        }
                        HydratedRecord::Parsed(mut value) => {
                            decoder.begin_record();
                            // Enrolling Parsed reads are append preflight. Its visitors
                            // use session/tool metadata but never a replay capture.
                            if enroll_dependencies {
                                time_append_stage(measured.as_mut().map(|t| &mut t.1), || {
                                    discard_verified_replay_capture(&mut value);
                                });
                            }
                            time_append_stage(measured.as_mut().map(|t| &mut t.0), || {
                                decoder.decode_value(value)
                            })
                        }
                    };
                    (decoder, result, measured)
                })
                .await
                .map_err(|error| IoError::other(format!("rollout decode task failed: {error}")))?;
                if let (Some(timing), Some((decode, discard))) = (timing.as_mut(), measured) {
                    timing.decode += decode;
                    timing.replay_discard += discard;
                }
                decoder = returned_decoder;
                result
            };
            if let Some(item) = decoded? {
                visit(item, decoder.record_number, decoder.diagnostic_origin.take());
            }
        }
        let thread_id = decoder.finish(path)?;
        if let Some(revision) = reader.source_revision() {
            let _ = codex_protocol::timing::observations::enqueue_runtime("reload_rollout_observed",
                &thread_id.map(|id| id.to_string()).unwrap_or_default(), "", "source_reconstruction",
                serde_json::json!({"store_id":crate::diagnostic_revision::store_id(path),
                    "revision":revision, "parse_errors":decoder.parse_errors,
                    "partial_tail":decoder.last_record_partial,
                    "unresolved_payloads":reader.unresolved_payloads(),
                    "atomic_snapshot":false,
                    "scope":"exact_index_record_stream_read; concurrent_append_may_extend_source"}));
        }
        Ok((
            thread_id,
            decoder.parse_errors,
            usize::from(decoder.last_record_partial),
            reader.unresolved_payloads(),
            reader.take_payload_versions(),
        ))
    }


    async fn existing_rollout_state(
        path: &Path,
    ) -> std::io::Result<(bool, crate::ToolManifestDictionary)> {
        let mut has_session_meta = false;
        let mut manifests = crate::ToolManifestDictionary::default();
        let mut parse_errors = 0usize;
        let mut reader = compression::open_rollout_line_reader(path).await?;
        reader.enroll_payload_dependencies();
        while let Some(line) = reader.next_line().await? {
            // Enrolled replay references arrive size-checked but undecoded; they
            // carry neither session metadata nor tool manifests.
            if line.trim().is_empty() || crate::payload_artifact::is_context_replay_reference(&line) {
                continue;
            }
            let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(line.trim()) else {
                if !serde_json::from_str::<Value>(line.trim())
                    .is_ok_and(|value| is_retired_terminalization_receipt(&value))
                {
                    parse_errors = parse_errors.saturating_add(1);
                }
                continue;
            };
            match rollout_line.item {
                RolloutItem::SessionMeta(_) => has_session_meta = true,
                RolloutItem::ToolManifest(manifest) => {
                    if let Err(err) = manifests.apply(&manifest) {
                        tracing::warn!(%err, "failed to reconstruct persisted tool manifest");
                    }
                }
                _ => {}
            }
        }
        if parse_errors > 0 {
            warn!(
                ?path,
                parse_errors,
                "skipped malformed rollout records while reconstructing persisted state"
            );
        }
        Ok((has_session_meta, manifests))
    }

    pub async fn get_rollout_history(path: &Path) -> std::io::Result<InitialHistory> {
        let (items, thread_id, parse_errors, _, diagnostic_origins) =
            Self::load_rollout_items_with_origins(path, false).await?;
        if parse_errors > 0 {
            warn!(
                ?path,
                parse_errors, "rollout history is incomplete: malformed records were skipped"
            );
        }
        let conversation_id = thread_id
            .ok_or_else(|| IoError::other("failed to parse thread ID from rollout file"))?;

        if items.is_empty() {
            return Ok(InitialHistory::New);
        }

        info!("Resumed rollout successfully from {path:?}");
        Ok(InitialHistory::Resumed(ResumedHistory {
            conversation_id,
            history: Arc::new(items),
            rollout_path: Some(compression::plain_rollout_path(path)),
            diagnostic_origins,
        }))
    }

    /// Stop without materializing buffered records or deleting already-written data.
    /// A queued discard remains owned by the writer if the caller is cancelled.
    /// Earlier commands may be blocked on storage; do not wait indefinitely for them.
    pub async fn discard(&self) -> std::io::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(30), self.discard_owned())
            .await
            .map_err(|_| {
                IoError::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for rollout discard",
                )
            })?
    }

    /// Await actual terminal settlement from an independently owned lifecycle
    /// task. Its observer may time out, but must not cancel registry cleanup.
    pub async fn discard_owned(&self) -> std::io::Result<()> {
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        if self.writer_task.lifecycle.load(Ordering::Acquire) == WRITER_SHUT_DOWN {
            return Ok(());
        }
        let permit = match self.tx.reserve().await {
            Ok(permit) => permit,
            Err(error) => {
                drop(enqueue_guard);
                self.wait_for_discard_custody_release().await;
                return Err(self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to reserve rollout discard command: {error}"))
                }));
            }
        };
        if let Err(error) = self.writer_task.begin_shutdown() {
            drop(permit);
            drop(enqueue_guard);
            if self.writer_task.lifecycle.load(Ordering::Acquire) == WRITER_FAILED {
                self.wait_for_discard_custody_release().await;
            }
            return Err(error);
        }
        let (ack, done) = oneshot::channel();
        permit.send(RolloutCmd::Discard { ack });
        drop(enqueue_guard);
        match done.await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.wait_for_discard_custody_release().await;
                Err(self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed waiting for rollout discard: {error}"))
                }))
            }
        }
    }

    async fn wait_for_discard_custody_release(&self) {
        // A published failure or a dropped receiver/reply is not lease release.
        // The writer's actual unwind signals custody release.
        let mut released = self.writer_task.lease_custody_released.subscribe();
        let _ = released.wait_for(|released| *released).await;
    }

    /// Drain pending items before stopping the writer task.
    ///
    /// If draining fails, the writer stays alive so callers can continue retrying flush/shutdown.
    pub async fn shutdown(&self) -> std::io::Result<()> {
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        if self.writer_task.shutdown_confirmed.load(Ordering::Acquire) {
            return Ok(());
        }
        let permit = self.tx.reserve().await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed to reserve rollout shutdown command: {e}"))
            })
        })?;
        self.writer_task.begin_shutdown()?;
        let (tx_done, rx_done) = oneshot::channel();
        permit.send(RolloutCmd::Shutdown { ack: tx_done });
        drop(enqueue_guard);
        rx_done.await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed waiting for rollout shutdown: {e}"))
            })
        })?
    }
}

/// Canonical decoding of one rollout JSONL record at a time, with its compatibility
/// migrations and unsupported-format errors. Stateful so every reader, async or
/// blocking, reports the same record numbers and thread identity.
#[derive(Default)]
pub(crate) struct RolloutRecordDecoder {
    thread_id: Option<ThreadId>,
    parse_errors: usize,
    last_record_partial: bool,
    saw_non_empty_line: bool,
    record_number: usize,
    capture_origins: bool,
    diagnostic_origin: Option<DiagnosticOrigin>,
}

// The retired receipt has no live or typed replay representation. Match its
// historical v1 wire shape here; additional fields were ignored by its decoder.
fn is_retired_terminalization_receipt(value: &Value) -> bool {
    if value.get("format_version").and_then(Value::as_u64) != Some(1)
        || value.get("timestamp").and_then(Value::as_str).is_none()
        || value.get("type").and_then(Value::as_str) != Some("event_msg")
    {
        return false;
    }
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return false;
    };
    if payload.get("type").and_then(Value::as_str) != Some("turn_terminalization_complete")
        || payload.get("turn_id").and_then(Value::as_str).is_none()
    {
        return false;
    }
    let Some(receipt) = payload.get("receipt").and_then(Value::as_object) else {
        return false;
    };
    if receipt.get("terminalIdentity").and_then(Value::as_str).is_none()
        || !matches!(
            receipt.get("deliveryState").and_then(Value::as_str),
            Some("not_attempted" | "claimed" | "delivered" | "delivery_failed")
        )
        || !matches!(
            receipt.get("recoveryState").and_then(Value::as_str),
            Some("none" | "pending" | "recovered")
        )
        || receipt.get("activeTurnDetached").and_then(Value::as_bool).is_none()
        || receipt.get("terminalInteractionReleased").and_then(Value::as_bool).is_none()
        || !matches!(
            receipt.get("deadlineExhaustedPhase"),
            None | Some(Value::Null | Value::String(_))
        )
    {
        return false;
    }
    let Some(timing) = receipt.get("terminalization").and_then(Value::as_object) else {
        return false;
    };
    ["validationProcessNs", "completionGateNs"]
        .iter()
        .all(|field| timing.get(*field).and_then(Value::as_u64).is_some())
        && [
            "finalMutationToSealNs",
            "validationAggregateNs",
            "reviewPreflightNs",
            "reviewNs",
            "checkpointTokens",
            "preparationNs",
            "hooksQuiescenceNs",
            "fenceNs",
            "freshnessNs",
            "gateNs",
            "durableCommitNs",
            "deliveryAttemptNs",
            "interactionReleaseNs",
            "postCleanupNs",
            "unclassifiedNs",
        ]
        .iter()
        .all(|field| timing.get(*field).is_none_or(|value| value.as_u64().is_some()))
        && [
            "validationLaunchCount",
            "validationAggregateCount",
            "terminalMemoHitCount",
            "diffReuseCount",
            "diffRefreshCount",
            "reviewerInfrastructureMemoHitCount",
            "reviewsPreventedByCorrectnessCount",
        ]
        .iter()
        .all(|field| {
            timing.get(*field).is_none_or(|value| {
                value.as_u64().is_some_and(|count| u32::try_from(count).is_ok())
            })
        })
}

fn discard_verified_replay_capture(value: &mut Value) {
    if value["format_version"].as_u64()
        != Some(u64::from(codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION))
        || value["type"].as_str() != Some("context_replay")
    {
        return;
    }
    let Some(payload) = value["payload"].as_object_mut() else {
        return;
    };
    if payload.len() != 5
        || !["version", "turn_id", "sampling_request_id", "physical_attempt_id"]
            .iter()
            .all(|key| payload.contains_key(*key))
    {
        return;
    }
    let Some(capture) = payload.get_mut("capture") else {
        return;
    };
    // The artifact reader has verified length/hash/type and fully parsed the
    // capture as Value, the exact unconstrained type of this field. Avoid
    // rematerializing that discarded tree through RolloutLine and the tagged
    // enum. The canonical decoder still validates the whole typed envelope.
    // Reserved serde_json map keys can change meaning during re-deserialization;
    // leave those and unusual numeric representations on the original path.
    if is_plain_replay_json(capture) {
        *capture = Value::Null;
    }
}

fn is_plain_replay_json(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().all(|(key, child)| {
            !key.starts_with("$serde_json::private::") && is_plain_replay_json(child)
        }),
        Value::Array(items) => items.iter().all(is_plain_replay_json),
        Value::Number(number) => number.is_i64() || number.is_u64() || number.as_f64().is_some(),
        _ => true,
    }
}

impl RolloutRecordDecoder {
    fn begin_record(&mut self) {
        self.saw_non_empty_line = true;
        self.last_record_partial = false;
        self.record_number = self.record_number.saturating_add(1);
    }

    /// A record the caller classified as irrelevant still occupies its record slot.
    pub(crate) fn note_undecoded_record(&mut self) {
        self.begin_record();
    }

    /// A referenced body the caller could not read counts as a malformed record.
    pub(crate) fn note_unreadable_record(&mut self) {
        self.begin_record();
        self.parse_errors = self.parse_errors.saturating_add(1);
    }

    pub(crate) fn parse_errors(&self) -> usize {
        self.parse_errors
    }

    pub(crate) fn decode(
        &mut self,
        line: &str,
        terminated: bool,
    ) -> std::io::Result<Option<RolloutItem>> {
        self.begin_record();
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    record = self.record_number,
                    bytes = line.len(),
                    "failed to parse rollout record as JSON: {e}"
                );
                self.parse_errors = self.parse_errors.saturating_add(1);
                self.last_record_partial = !terminated && e.is_eof();
                return Ok(None);
            }
        };
        self.decode_value(v)
    }

    fn decode_value(&mut self, mut v: Value) -> std::io::Result<Option<RolloutItem>> {
        let record_number = self.record_number;
        let is_v0 = match v.get("format_version") {
            None => true,
            Some(Value::Number(version)) => version.as_u64() == Some(0),
            Some(_) => false,
        };
        if let Some(version) = v.get("format_version")
            && version.as_u64() != Some(0)
            && version.as_u64()
                != Some(u64::from(
                    codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION,
                ))
        {
            return Err(IoError::new(
                ErrorKind::Unsupported,
                format!("unsupported rollout format version {version} at record {record_number}"),
            ));
        }
        if is_retired_terminalization_receipt(&v) {
            // Keep the physical ordinal; only the typed projection omits this
            // diagnostic. It is not a missing turn boundary or a history gap.
            return Ok(None);
        }
        if is_v0 && migrate_v0_ghost_snapshot_rollout_line(&mut v) {
            trace!("skipping legacy ghost_snapshot rollout line");
            return Ok(None);
        }

        if self.thread_id.is_none() {
            // Validate the small compatibility discriminator before consuming the JSON value.
            // This preserves the unknown-mode guard without cloning the whole rollout line.
            reject_unknown_thread_history_mode(&v)?;
        }

        // Secondary session metadata is provenance for a fork source, not
        // the canonical thread's schema/operational history. Preserve the
        // existing recoverable-gap treatment for that optional provenance.
        let secondary_metadata =
            self.thread_id.is_some() && v.get("type").and_then(Value::as_str) == Some("session_meta");
        // This metadata is optional and never participates in canonical payload
        // decoding or legacy migration. Malformed provenance isn't a history gap.
        let origin = if self.capture_origins && !is_v0 {
            v.as_object_mut().and_then(|fields| fields.remove("diagnostic_origin"))
        } else {
            None
        };
        match serde_json::from_value::<RolloutLine>(v) {
            Ok(rollout_line) => {
                let item = rollout_line.item;
                // Use the FIRST SessionMeta encountered in the file as the canonical
                // thread id and main session information. Keep all items intact.
                if self.thread_id.is_none()
                    && let RolloutItem::SessionMeta(session_meta_line) = &item
                {
                    self.thread_id = Some(session_meta_line.meta.id);
                }
                if item.is_origin_diagnostic() {
                    self.diagnostic_origin = origin
                        .and_then(|value| serde_json::from_value::<DiagnosticOrigin>(value).ok())
                        .filter(|origin| !origin.id.is_nil());
                }
                Ok(Some(item))
            }
            Err(e) => {
                // Match the Serde diagnostic itself, not quoted record data inside it.
                if !secondary_metadata && e.to_string().starts_with("unknown variant `") {
                    return Err(IoError::new(
                        ErrorKind::Unsupported,
                        format!("unsupported rollout representation at record {record_number}: {e}"),
                    ));
                }
                trace!("failed to parse rollout line: {e}");
                self.parse_errors = self.parse_errors.saturating_add(1);
                Ok(None)
            }
        }
    }

    pub(crate) fn finish(&self, path: &Path) -> std::io::Result<Option<ThreadId>> {
        if !self.saw_non_empty_line {
            return Err(IoError::new(ErrorKind::InvalidData, "empty session file"));
        }

        // A damaged metadata payload must not hide an otherwise usable rollout.
        Ok(self.thread_id.or_else(|| {
            let (_, id) =
                crate::list::parse_timestamp_uuid_from_filename(path.file_name()?.to_str()?)?;
            ThreadId::from_string(&id.to_string()).ok()
        }))
    }
}

pub(crate) fn reject_unknown_thread_history_mode(value: &Value) -> std::io::Result<()> {
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return Ok(());
    }
    let Some(history_mode) = value
        .get("payload")
        .and_then(|payload| payload.get("history_mode"))
    else {
        return Ok(());
    };
    serde_json::from_value::<ThreadHistoryMode>(history_mode.clone())
        .map(|_| ())
        .map_err(|err| {
            IoError::new(
                ErrorKind::InvalidData,
                format!("invalid session metadata history_mode: {err}"),
            )
        })
}

fn migrate_v0_ghost_snapshot_rollout_line(value: &mut Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some("response_item") => value
            .get("payload")
            .is_some_and(is_legacy_ghost_snapshot_response_item),
        Some("compacted") => {
            if let Some(replacement_history) = value
                .get_mut("payload")
                .and_then(|payload| payload.get_mut("replacement_history"))
                .and_then(Value::as_array_mut)
            {
                replacement_history.retain(|item| !is_legacy_ghost_snapshot_response_item(item));
            }
            false
        }
        _ => false,
    }
}

fn is_legacy_ghost_snapshot_response_item(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("ghost_snapshot")
}


struct LogFileInfo {
    /// Full path to the rollout file.
    path: PathBuf,

    /// Session ID (also embedded in filename).
    conversation_id: ThreadId,

    /// Timestamp for the start of the session.
    timestamp: OffsetDateTime,
}

fn precompute_log_file_info(
    config: &impl RolloutConfigView,
    conversation_id: ThreadId,
) -> std::io::Result<LogFileInfo> {
    // Resolve ~/.codex/sessions/YYYY/MM/DD path.
    let timestamp = OffsetDateTime::now_local()
        .map_err(|e| IoError::other(format!("failed to get local time: {e}")))?;
    let mut dir = config.codex_home().to_path_buf();
    dir.push(SESSIONS_SUBDIR);
    dir.push(timestamp.year().to_string());
    dir.push(format!("{:02}", u8::from(timestamp.month())));
    dir.push(format!("{:02}", timestamp.day()));

    // Custom format for YYYY-MM-DDThh-mm-ss. Use `-` instead of `:` for
    // compatibility with filesystems that do not allow colons in filenames.
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let date_str = timestamp
        .format(format)
        .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

    let filename = format!("rollout-{date_str}-{conversation_id}.jsonl");

    let path = dir.join(filename);

    Ok(LogFileInfo {
        path,
        conversation_id,
        timestamp,
    })
}

struct LockedRolloutFile {
    path: PathBuf,
    file: File,
    payload_sync_owner: Arc<crate::payload_artifact::PayloadSyncOwner>,
    append_lock: compression::RolloutAppendLock,
    payload_retention: std::fs::File,
}

impl LockedRolloutFile {
    fn into_jsonl_writer(self) -> JsonlWriter {
        JsonlWriter {
            path: self.path,
            file: tokio::fs::File::from_std(self.file),
            _payload_sync_owner: self.payload_sync_owner,
            _append_lock: Arc::new(self.append_lock),
            _payload_retention: Arc::new(self.payload_retention),
        }
    }
}

fn open_log_file(path: &Path) -> std::io::Result<LockedRolloutFile> {
    open_log_file_with_options(path, /*create*/ true, /*repair_tail*/ true)
        .map(|(_, file)| file)
}

fn open_log_file_with_options(
    path: &Path,
    create: bool,
    repair_tail: bool,
) -> std::io::Result<(PathBuf, LockedRolloutFile)> {
    let payload_retention = crate::payload_artifact::lock_publication(path)?;
    let (path, append_lock) = compression::lock_rollout_for_append_blocking(path)?;
    let Some(parent) = path.parent() else {
        return Err(IoError::other(format!(
            "rollout path has no parent: {}",
            path.display()
        )));
    };
    fs::create_dir_all(parent)?;

    let _write_lock = compression::lock_rollout_for_write_blocking(&path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).append(true).create(create);

    let mut file = options.open(&path)?;

    if repair_tail {
        ensure_rollout_is_newline_terminated(&mut file)?;
    }
    let locked_file = LockedRolloutFile {
        path: path.clone(),
        file,
        payload_sync_owner: crate::payload_artifact::PayloadSyncOwner::new(&path),
        append_lock,
        payload_retention,
    };
    Ok((path, locked_file))
}

/// Mutable state owned by the background rollout writer.
///
/// Items are first appended to `pending_items`; persist/flush/shutdown remove each item from that
/// queue only after it is written successfully. I/O failures drop the file handle but keep the
/// unwritten suffix so the next barrier can reopen the file and retry.
struct RolloutWriterState {
    writer: Option<JsonlWriter>,
    payload_sync_owner: Arc<crate::payload_artifact::PayloadSyncOwner>,
    residency: Option<(Arc<compression::RolloutAppendLock>, Arc<std::fs::File>)>,
    deferred_log_file_info: Option<LogFileInfo>,
    pending_items: Vec<CapturedRolloutItem>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    known_repository_context: Option<Option<RepositoryContext>>,
    rollout_path: PathBuf,
    last_logged_error: Option<String>,
    retry_blocked_error: Option<String>,
    tool_manifests: crate::ToolManifestDictionary,
    last_settings: Option<serde_json::Value>,
    settings_prefix_len: Option<u64>,
    namespace_sync_pending: bool,
    pending_agent_message_mirrors:
        std::collections::VecDeque<codex_protocol::protocol::AgentMessageEvent>,
}

impl RolloutWriterState {
    fn new(
        writer: Option<JsonlWriter>,
        deferred_log_file_info: Option<LogFileInfo>,
        meta: Option<SessionMeta>,
        cwd: PathBuf,
        known_repository_context: Option<Option<RepositoryContext>>,
        rollout_path: PathBuf,
        tool_manifests: crate::ToolManifestDictionary,
    ) -> Self {
        let residency = writer.as_ref().map(|writer| {
            (
                Arc::clone(&writer._append_lock),
                Arc::clone(&writer._payload_retention),
            )
        });
        let payload_sync_owner = writer.as_ref()
            .map(|writer| Arc::clone(&writer._payload_sync_owner))
            .unwrap_or_else(|| crate::payload_artifact::PayloadSyncOwner::new(&rollout_path));
        Self {
            writer,
            payload_sync_owner,
            residency,
            deferred_log_file_info,
            pending_items: Vec::new(),
            meta,
            cwd,
            known_repository_context,
            rollout_path,
            last_logged_error: None,
            retry_blocked_error: None,
            tool_manifests,
            last_settings: None,
            settings_prefix_len: None,
            namespace_sync_pending: true,
            pending_agent_message_mirrors: Default::default(),
        }
    }

    fn add_items(&mut self, items: Vec<CapturedRolloutItem>) {
        for captured in items {
            if let Some(expected) = self.pending_agent_message_mirrors.pop_front() {
                if let Some(RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::AgentMessage(
                    actual,
                ))) = captured.canonical_item()
                    && expected.message == actual.message
                    && expected.phase == actual.phase
                {
                    continue;
                }
                self.pending_agent_message_mirrors.clear();
            }
            match captured.canonical_item() {
                // The recorder owns the single canonical metadata slot. Inherited history must
                // never append another session_meta record.
                Some(RolloutItem::SessionMeta(_)) => {
                    trace!(
                        "ignoring inherited session metadata; recorder owns the canonical record"
                    );
                }
                Some(RolloutItem::EventMsg(
                    codex_protocol::protocol::EventMsg::ItemCompleted(event),
                )) => {
                    if let codex_protocol::items::TurnItem::AgentMessage(message) = &event.item {
                        self.pending_agent_message_mirrors = message
                            .as_legacy_events()
                            .into_iter()
                            .filter_map(|event| match event {
                                codex_protocol::protocol::EventMsg::AgentMessage(message) => {
                                    Some(message)
                                }
                                _ => None,
                            })
                            .collect();
                    }
                    self.pending_items.push(captured);
                }
                Some(RolloutItem::ToolManifest(manifest)) => {
                    // Encoding rewrites the payload, never when it happened.
                    let encoded = match self.tool_manifests.encode_item(manifest) {
                        Ok(encoded) => encoded,
                        Err(err) => {
                            tracing::warn!(%err, "failed to encode tool manifest; preserving input record");
                            manifest.clone()
                        }
                    };
                    self.pending_items
                        .push(captured.with_item(RolloutItem::ToolManifest(encoded)));
                }
                // Every sampling request's usage is retained. One record per
                // turn cannot show which request grew the prefix, what each
                // request's cache hit rate was, or where context was spent, so
                // coalescing them removes the primary signal this fork is
                // optimized against.
                _ => self.pending_items.push(captured),
            }
        }
    }

    async fn flush_if_materialized(&mut self) {
        // One automatic recovery episode per degradation. New additions may
        // consume bounded admission, but only an explicit barrier retries I/O.
        if self.is_deferred() || self.last_logged_error.is_some() {
            return;
        }
        if let Err(err) = self.write_pending_with_recovery("record").await {
            self.enter_recovery_mode(&err);
        }
    }

    async fn persist(&mut self) -> std::io::Result<()> {
        self.write_pending_with_recovery("persist").await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        if self.is_deferred() && self.pending_items.is_empty() {
            return Ok(());
        }
        self.write_pending_with_recovery("flush").await
    }

    async fn flush_durable(&mut self) -> std::io::Result<()> {
        use codex_protocol::timing::observations;

        let observation = if observations::terminal_enabled() {
            observations::TerminalSpan::start_durable_rollout(
                &self.meta.as_ref().map(|meta| meta.id.to_string()).unwrap_or_default(),
            )
        } else {
            None
        };
        self.flush().await?;
        crate::payload_artifact::sync_payload_artifacts(&self.rollout_path).await?;
        if let Some(writer) = self.writer.as_ref() {
            writer.file.sync_data().await?;
            if self.namespace_sync_pending {
                let path = self.rollout_path.clone();
                tokio::task::spawn_blocking(move || sync_rollout_namespace(&path))
                    .await
                    .map_err(IoError::other)??;
                // Failure or cancellation leaves the namespace charged for retry.
                self.namespace_sync_pending = false;
            }
        }
        if let Some(observation) = observation {
            observation.finish(
                self.writer.is_some(),
                self.pending_items.len(),
                self.namespace_sync_pending,
            );
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        if self.is_deferred() && self.pending_items.is_empty() {
            return Ok(());
        }
        self.write_pending_with_recovery("shutdown").await
    }

    async fn write_pending_with_recovery(&mut self, operation: &str) -> std::io::Result<()> {
        if let Some(err) = self.retry_blocked_error.as_ref() {
            return Err(IoError::other(err.clone()));
        }

        match self.write_pending_once().await {
            Ok(()) => {
                self.last_logged_error = None;
                Ok(())
            }
            Err(first_err) => {
                if is_unrecoverable_rollout_append_error(&first_err) {
                    self.block_unsafe_retry(&first_err);
                    return Err(first_err);
                }
                self.enter_recovery_mode(&first_err);
                warn!("failed to {operation} rollout writer; reopening and retrying: {first_err}");
                match self.write_pending_once().await {
                    Ok(()) => {
                        self.last_logged_error = None;
                        Ok(())
                    }
                    Err(second_err) => {
                        if is_unrecoverable_rollout_append_error(&second_err) {
                            self.block_unsafe_retry(&second_err);
                        } else {
                            self.enter_recovery_mode(&second_err);
                        }
                        warn!(
                            "retrying rollout writer {operation} failed; first error: \
                             {first_err}; final error: {second_err}"
                        );
                        Err(second_err)
                    }
                }
            }
        }
    }

    fn is_deferred(&self) -> bool {
        self.writer.is_none() && self.deferred_log_file_info.is_some()
    }

    fn enter_recovery_mode(&mut self, err: &IoError) {
        if self.retry_blocked_error.is_some() {
            return;
        }
        let message = err.to_string();
        if self.last_logged_error.as_ref() != Some(&message) {
            error!(
                "rollout writer failed for {}; buffered rollout items will be retried: {err}; \
                 error_kind={:?}; raw_os_error={:?}",
                self.rollout_path.display(),
                err.kind(),
                err.raw_os_error()
            );
        }
        self.last_logged_error = Some(message);
        self.writer = None;
    }

    fn block_unsafe_retry(&mut self, err: &IoError) {
        let message = err.to_string();
        error!(
            "rollout writer failed for {}; retry is blocked because the last record could not be \
             proven committed or rolled back: {err}",
            self.rollout_path.display()
        );
        self.last_logged_error = Some(message.clone());
        self.retry_blocked_error = Some(message);
        self.writer = None;
    }

    async fn ensure_writer_open(&mut self) -> std::io::Result<()> {
        if self.writer.is_some() {
            return Ok(());
        }

        let path = self
            .deferred_log_file_info
            .as_ref()
            .map(|info| info.path.as_path())
            .unwrap_or(self.rollout_path.as_path());
        let path = path.to_path_buf();
        let writer = if let Some((append_lock, payload_retention)) = &self.residency {
            // Recovery replaces only the data handle. Residency still excludes
            // relocation, and a vanished materialized file must not be recreated.
            let file = tokio::task::spawn_blocking(move || {
                let _write = compression::lock_rollout_for_write_blocking(&path)?;
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .append(true)
                    .open(&path)?;
                ensure_rollout_is_newline_terminated(&mut file)?;
                Ok::<_, IoError>(file)
            })
            .await
            .map_err(IoError::other)??;
            JsonlWriter {
                path: self.rollout_path.clone(),
                file: tokio::fs::File::from_std(file),
                _payload_sync_owner: Arc::clone(&self.payload_sync_owner),
                _append_lock: Arc::clone(append_lock),
                _payload_retention: Arc::clone(payload_retention),
            }
        } else {
            tokio::task::spawn_blocking(move || open_log_file(path.as_path()))
                .await
                .map_err(IoError::other)??
                .into_jsonl_writer()
        };
        self.residency = Some((
            Arc::clone(&writer._append_lock),
            Arc::clone(&writer._payload_retention),
        ));
        self.writer = Some(writer);
        self.deferred_log_file_info = None;
        Ok(())
    }

    // Called while holding the write lock through the initial append.
    async fn reconcile_initial_state(&mut self) -> std::io::Result<()> {
        let existing_rollout_state = if self.meta.is_some()
            && let Some(writer) = self.writer.as_ref()
            && writer.file.metadata().await?.len() > 0
        {
            Some(RolloutRecorder::existing_rollout_state(&writer.path).await?)
        } else {
            None
        };
        if let Some((has_session_meta, mut persisted_tool_manifests)) = existing_rollout_state {
            let known_manifests = self.tool_manifests.clone();
            // Rebasing rewrites manifest payloads against the persisted
            // dictionary; each record keeps the capture time it arrived with.
            let mut rebased_items = self.pending_items.clone();
            for captured in &mut rebased_items {
                let CapturedRolloutPayload::Canonical(RolloutItem::ToolManifest(manifest)) =
                    &mut captured.item
                else {
                    continue;
                };
                let Some(full) = known_manifests.manifest(&manifest.hash).cloned() else {
                    continue;
                };
                *manifest = persisted_tool_manifests
                    .encode(manifest.hash.clone(), full)
                    .map_err(IoError::other)?;
            }
            if has_session_meta {
                self.meta = None;
            }
            self.pending_items = rebased_items;
            self.tool_manifests = persisted_tool_manifests;
        }
        Ok(())
    }

    async fn session_meta_item_if_needed(&self) -> std::io::Result<Option<CapturedRolloutItem>> {
        let Some(mut session_meta) = self.meta.as_ref().cloned() else {
            return Ok(None);
        };
        if let Some(build) = session_meta.harness_build.as_mut()
            && build.executable_sha256.is_none()
        {
            build.executable_sha256 =
                tokio::task::spawn_blocking(codex_utils_build_info::executable_identity_sha256)
                    .await
                    .ok()
                    .flatten()
                    .map(str::to_owned);
        }
        let git_info = match self.known_repository_context.clone() {
            Some(repository_context) => repository_context.map(|context| context.git_info),
            None if get_git_repo_root(&self.cwd).is_some() => collect_git_info(&self.cwd).await,
            None => None,
        };
        // Session creation predates queued records, even when materialization
        // and Git enrichment happen later.
        let captured_at = OffsetDateTime::parse(
            &session_meta.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(IoError::other)?;
        Ok(Some(CapturedRolloutItem::new(
            RolloutItem::SessionMeta(SessionMetaLine {
                meta: session_meta,
                git: git_info,
            }),
            captured_at,
        )))
    }

    async fn write_pending_once(&mut self) -> std::io::Result<()> {
        // AddItems already flushes a materialized writer. Its following barrier
        // must retain ordering and error checks, but has no append transaction
        // to serialize when neither records nor the initial header remain.
        // Keep recovery/opening and durable sync on their existing paths.
        if self.writer.is_some() && self.meta.is_none() && self.pending_items.is_empty() {
            return Ok(());
        }
        self.ensure_writer_open().await?;
        let path = self.rollout_path.clone();
        let write_lock = tokio::task::spawn_blocking(move || {
            compression::lock_rollout_for_write_blocking(&path)
        })
        .await
        .map_err(IoError::other)??;
        self.reconcile_initial_state().await?;
        let session_meta_item = self.session_meta_item_if_needed().await?;
        let Some(writer) = self.writer.as_mut() else {
            return Err(IoError::other("rollout writer is not open"));
        };
        // Only the immediately preceding committed prefix authorizes delta
        // compaction. Independent append owners invalidate our private cache.
        let prefix_len = writer.file.metadata().await?.len();
        let mut last_settings = if self.settings_prefix_len == Some(prefix_len) {
            self.last_settings.clone()
        } else {
            None
        };
        let mut compacted = Vec::with_capacity(self.pending_items.len());
        for captured in &self.pending_items {
            if let Some(RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::ThreadSettingsApplied(settings),
            )) = captured.canonical_item()
            {
                let value = serde_json::to_value(settings).map_err(IoError::other)?;
                if last_settings.as_ref() == Some(&value) {
                    continue;
                }
                let mut settings = settings.clone();
                if last_settings.as_ref().is_some_and(|previous| {
                    previous["thread_settings"].get("developer_instructions")
                        == value["thread_settings"].get("developer_instructions")
                }) {
                    settings.thread_settings.developer_instructions = None;
                }
                last_settings = Some(value);
                compacted.push(Cow::Owned(captured.with_item(RolloutItem::EventMsg(
                    codex_protocol::protocol::EventMsg::ThreadSettingsApplied(settings),
                ))));
            } else {
                compacted.push(Cow::Borrowed(captured));
            }
        }
        let items = session_meta_item
            .iter()
            .chain(compacted.iter().map(AsRef::as_ref))
            .collect::<Vec<_>>();
        if items.is_empty() {
            drop(items);
            drop(compacted);
            self.pending_items.clear();
            return Ok(());
        }
        let committed_end = writer
            .write_rollout_items_locked(&items, &write_lock, Some(prefix_len))
            .await?;
        // End the borrowed write view before releasing the retained originals.
        drop(items);
        drop(compacted);
        self.last_settings = last_settings;
        self.settings_prefix_len = Some(committed_end);
        if session_meta_item.is_some() {
            self.meta = None;
        }
        self.pending_items.clear();
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn rollout_writer(
    writer: Option<JsonlWriter>,
    deferred_log_file_info: Option<LogFileInfo>,
    rx: &mut mpsc::Receiver<RolloutCmd>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    known_repository_context: Option<Option<RepositoryContext>>,
    rollout_path: PathBuf,
    tool_manifests: crate::ToolManifestDictionary,
    writer_task: Arc<RolloutWriterTask>,
) -> Result<(), Arc<IoError>> {
    let mut state = RolloutWriterState::new(
        writer,
        deferred_log_file_info,
        meta,
        cwd,
        known_repository_context,
        rollout_path,
        tool_manifests,
    );

    // Only adjacent, immediately ready additions may share an append. Bound
    // each drain so producers cannot postpone incremental persistence forever.
    let mut next_command = None;
    loop {
        let cmd = match next_command.take() {
            Some(cmd) => cmd,
            None => match rx.recv().await {
                Some(cmd) => cmd,
                None => break,
            },
        };
        match cmd {
            RolloutCmd::AddItems { items } => {
                state.add_items(items);
                for _ in 1..32 {
                    match rx.try_recv() {
                        Ok(RolloutCmd::AddItems { items }) => state.add_items(items),
                        Ok(barrier) => {
                            next_command = Some(barrier);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                state.flush_if_materialized().await;
            }
            RolloutCmd::Persist { ack } => {
                let _ = ack.send(state.persist().await);
            }
            RolloutCmd::Flush { ack, durable } => {
                let result = if durable {
                    state.flush_durable().await
                } else {
                    state.flush().await
                };
                let _ = ack.send(result);
            }
            RolloutCmd::Shutdown { ack } => match state.shutdown().await {
                Ok(()) => {
                    drop(state);
                    writer_task.finish_shutdown(true);
                    writer_task.shutdown_confirmed.store(true, Ordering::Release);
                    let _ = ack.send(Ok(()));
                    return Ok(());
                }
                Err(err) => {
                    writer_task.finish_shutdown(false);
                    let _ = ack.send(Err(err));
                }
            },
            RolloutCmd::Discard { ack } => {
                // Release pending payload permits and the file before acknowledging.
                // In particular, do not fall through to the channel-close drain.
                drop(state);
                writer_task.finish_shutdown(true);
                let _ = ack.send(());
                return Ok(());
            }
            RolloutCmd::Maintenance {
                ack,
                decision,
                preserve_bytes,
                publication_order: _publication_order,
            } => {
                let materialized = !state.is_deferred();
                if !preserve_bytes && materialized && let Err(error) = state.flush().await {
                    if state.retry_blocked_error.is_some() {
                        let error = Arc::new(error);
                        writer_task.mark_failed(Arc::clone(&error));
                        let _ = ack.send(Err(share_io_error(&error)));
                        return Err(error);
                    }
                    let _ = ack.send(Err(error));
                    continue;
                }
                // Keep lazy records, dictionaries and metadata in this owner.
                state.writer.take();
                state.residency.take();
                let _ = ack.send(Ok(()));
                match decision.await.unwrap_or(MaintenanceDecision::Resume) {
                    MaintenanceDecision::Commit => {
                        drop(state);
                        writer_task.finish_shutdown(true);
                        return Ok(());
                    }
                    MaintenanceDecision::RestoreFailed(error) => return Err(error),
                    MaintenanceDecision::Resume if materialized => {
                        // Restore must have succeeded: never create an empty rollout.
                        let writer = if preserve_bytes {
                            // Do not parse or repair the checkpoint's unowned tail.
                            // Keep the retained writer state and lossless representation
                            // handling, even if compression ran while it was paused.
                            let path = state.rollout_path.clone();
                            tokio::task::spawn_blocking(move || {
                                open_log_file_with_options(
                                    &path,
                                    /*create*/ false,
                                    /*repair_tail*/ false,
                                )
                            })
                            .await
                            .map_err(IoError::other)??
                            .1
                            .into_jsonl_writer()
                        } else {
                            open_rollout_for_append(&state.rollout_path).await?.1
                        };
                        state.residency = Some((
                            Arc::clone(&writer._append_lock),
                            Arc::clone(&writer._payload_retention),
                        ));
                        state.writer = Some(writer);
                    }
                    MaintenanceDecision::Resume => {}
                }
            }
        }
        if let Some(message) = state.retry_blocked_error.as_ref() {
            let pending_records = state.pending_items.len();
            let err = Arc::new(IoError::other(format!(
                "{message}; {pending_records} buffered rollout records could not be confirmed persisted"
            )));
            error!(pending_records, path = %state.rollout_path.display(), "rollout writer stopped with unconfirmed buffered records");
            // Batching may already have removed a pending barrier from rx.
            writer_task.mark_failed(Arc::clone(&err));
            return Err(err);
        }
    }

    // Closing the last sender still must finish writes for accepted records.
    state.shutdown().await?;
    drop(state);
    writer_task.finish_shutdown(true);
    writer_task.shutdown_confirmed.store(true, Ordering::Release);
    Ok(())
}

/// Append one already-filtered rollout item to an existing rollout JSONL file.
///
/// This is for metadata updates to unloaded threads. Live sessions should use
/// `RolloutRecorder::record_canonical_items` so rollout writes remain ordered
/// with the rest of the session stream.
pub async fn append_rollout_item_to_path(
    rollout_path: &Path,
    item: &RolloutItem,
) -> std::io::Result<()> {
    let rollout_path = rollout_path.to_path_buf();
    let item = item.clone();
    // The task, not the cancellable caller, owns the append locks and any
    // partial-write reconciliation until all filesystem work has settled.
    tokio::spawn(async move {
        let (_rollout_path, mut writer) = open_rollout_for_append(&rollout_path).await?;
        writer.write_rollout_item(&item).await
    })
    .await
    .map_err(IoError::other)?
}

/// Opaque, versioned custody for a single physical SessionMeta append. Serialize
/// only into the caller's durable private intent store; this contains rollout
/// bytes and deliberately does not implement Debug.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub struct RolloutAppendCheckpoint {
    version: u8,
    recipient: ThreadId,
    source_session: SessionId,
    rollout_name: String,
    offset: u64,
    preceding_sha256: [u8; 32],
    record_sha256: [u8; 32],
    record: Vec<u8>,
}

#[derive(Debug)]
pub enum DurableRolloutAppendOutcome {
    /// This call appended, including a failed write proven complete in place.
    Appended,
    /// A prior append was found at its checkpoint and synced without rewriting.
    AlreadyPresent,
    /// The intended record is not present and this attempt made no rollout
    /// change, or the current byte owner verified a rollback to the append point.
    Retryable(IoError),
    /// Publication cannot be ruled out, ownership is unproven, or durability
    /// was not confirmed. Keep the intent; never retry with a fresh checkpoint.
    Unknown(IoError),
    /// The input or the initial append point is unsuitable; no record was written.
    Rejected(IoError),
}

/// Append an immutable compatibility SessionMeta record. The caller owns the
/// logical thread lease and serializes these intents with live writer startup
/// and other updates across retries. This routine owns representation and byte
/// locks (also across the callback), not application-level ordering.
///
/// The callback must durably save this checkpoint before returning success.
/// It must not reenter the rollout writer, nor wait for work that needs its
/// locks. Any newly externalized payload is secured first, but no bytes have
/// been appended to the rollout. Dropping the caller does not cancel the owner.
pub async fn append_checkpointed_session_meta<F, Fut>(
    rollout_path: &Path,
    session_meta: SessionMetaLine,
    persist_checkpoint: F,
) -> DurableRolloutAppendOutcome
where
    F: FnOnce(RolloutAppendCheckpoint) -> Fut + Send + 'static,
    Fut: Future<Output = std::io::Result<()>> + Send + 'static,
{
    let path = rollout_path.to_path_buf();
    tokio::spawn(async move {
        append_checkpointed_session_meta_owned(&path, session_meta, persist_checkpoint).await
    })
    .await
    .unwrap_or_else(|_| DurableRolloutAppendOutcome::Unknown(IoError::other(
        "rollout append owner did not confirm its result",
    )))
}

/// Query or complete the exact persisted append; never selects an alternate
/// recipient or re-encodes the record. Supply the same immutable SessionMeta
/// from the intent so a collected, unreferenced external body can be restored.
/// Resolve a unique current active/archive target and retain the logical
/// writer/order custody described by append_checkpointed_session_meta.
pub async fn replay_checkpointed_session_meta(
    rollout_path: &Path,
    session_meta: SessionMetaLine,
    checkpoint: RolloutAppendCheckpoint,
) -> DurableRolloutAppendOutcome {
    let path = rollout_path.to_path_buf();
    tokio::spawn(async move {
        replay_checkpointed_session_meta_owned(&path, session_meta, checkpoint).await
    })
    .await
    .unwrap_or_else(|_| DurableRolloutAppendOutcome::Unknown(IoError::other(
        "rollout replay owner did not confirm its result",
    )))
}

const ROLLOUT_APPEND_CHECKPOINT_VERSION: u8 = 1;
const ROLLOUT_APPEND_ANCHOR_BYTES: u64 = 4096;

fn checkpointed_rollout_name(path: &Path) -> Option<String> {
    let plain = compression::plain_rollout_path(path);
    plain.file_name()?.to_str()
        .filter(|name| name.ends_with(".jsonl"))
        .map(str::to_owned)
}

// This path intentionally does not run newline repair. A persisted checkpoint
// has first claim on interpreting an unterminated tail; repair could consume
// the reserved position before the exact-byte comparison.
async fn open_checkpointed_rollout(
    path: &Path,
    recipient: (ThreadId, SessionId),
) -> std::io::Result<(JsonlWriter, compression::RolloutWriteLock)> {
    let (_, writer, _, write_lock) =
        open_rollout_for_append_with_replay_inner(path, |_, _| {}, None, Some(recipient)).await?;
    let write_lock = write_lock.ok_or_else(|| IoError::other("missing checkpoint byte lock"))?;
    Ok((writer, write_lock))
}

async fn append_checkpointed_session_meta_owned<F, Fut>(
    path: &Path,
    session_meta: SessionMetaLine,
    persist_checkpoint: F,
) -> DurableRolloutAppendOutcome
where
    F: FnOnce(RolloutAppendCheckpoint) -> Fut,
    Fut: Future<Output = std::io::Result<()>>,
{
    use DurableRolloutAppendOutcome::{Rejected, Retryable, Unknown};

    let Some(rollout_name) = checkpointed_rollout_name(path) else {
        return Rejected(IoError::other("invalid rollout recipient path"));
    };
    let recipient = session_meta.meta.id;
    let source_session = session_meta.meta.session_id;
    let (mut writer, write_lock) = match open_checkpointed_rollout(path, (recipient, source_session)).await {
        Ok(owned) => owned,
        Err(error) if matches!(error.kind(), ErrorKind::InvalidData | ErrorKind::Unsupported) => {
            return Rejected(IoError::other("rollout recipient is invalid or ambiguous"));
        }
        Err(_) => return Retryable(IoError::other("could not open rollout append position")),
    };
    let Ok(offset) = writer.file.metadata().await.map(|metadata| metadata.len()) else {
        return Retryable(IoError::other("could not determine rollout append position"));
    };
    let preceding_sha256 = match preceding_rollout_anchor(&mut writer.file, offset).await {
        Ok(Some(anchor)) => anchor,
        Ok(None) => return Rejected(IoError::other("rollout append position is unterminated")),
        Err(_) => return Retryable(IoError::other("could not read preceding rollout anchor")),
    };
    let captured =
        CapturedRolloutItem::new(RolloutItem::SessionMeta(session_meta), OffsetDateTime::now_utc());
    let Ok(record) = writer.encode_rollout_items(&[&captured]).await else {
        return Retryable(IoError::other("could not encode rollout append"));
    };
    if offset.checked_add(record.len() as u64).is_none() {
        return Rejected(IoError::other("rollout checkpoint offset overflow"));
    }
    let checkpoint = RolloutAppendCheckpoint {
        version: ROLLOUT_APPEND_CHECKPOINT_VERSION,
        recipient,
        source_session,
        rollout_name,
        offset,
        preceding_sha256,
        record_sha256: Sha256::digest(&record).into(),
        record,
    };
    if crate::payload_artifact::sync_payload_artifacts(&writer.path).await.is_err() {
        return Retryable(IoError::other("could not secure rollout payload before checkpoint"));
    }
    if persist_checkpoint(checkpoint.clone()).await.is_err() {
        // A failed acknowledgement cannot tell us whether the caller's store
        // committed the checkpoint. No rollout write was attempted.
        return Unknown(IoError::other("could not confirm durable rollout checkpoint"));
    }
    write_checkpointed_record(&mut writer, &checkpoint, &write_lock).await
}

async fn replay_checkpointed_session_meta_owned(
    path: &Path,
    session_meta: SessionMetaLine,
    checkpoint: RolloutAppendCheckpoint,
) -> DurableRolloutAppendOutcome {
    use DurableRolloutAppendOutcome::{AlreadyPresent, Retryable, Unknown};

    let record_sha256: [u8; 32] = Sha256::digest(&checkpoint.record).into();
    if checkpoint.version != ROLLOUT_APPEND_CHECKPOINT_VERSION
        || checkpoint.recipient != session_meta.meta.id
        || checkpoint.source_session != session_meta.meta.session_id
        || checkpointed_rollout_name(path).as_deref() != Some(checkpoint.rollout_name.as_str())
        || record_sha256 != checkpoint.record_sha256
        || !checkpoint.record.strip_suffix(b"\n")
            .is_some_and(|line| !line.is_empty() && !line.contains(&b'\n'))
        || checkpoint.offset.checked_add(checkpoint.record.len() as u64).is_none()
    {
        return Unknown(IoError::other("rollout checkpoint or recipient did not match"));
    }
    let recipient = (checkpoint.recipient, checkpoint.source_session);
    let Ok((mut writer, write_lock)) = open_checkpointed_rollout(path, recipient).await else {
        return Unknown(IoError::other("could not verify checkpoint rollout recipient"));
    };
    let observed = match observe_checkpoint(&mut writer.file, &checkpoint).await {
        Ok(Some(observed)) => observed,
        _ => return Unknown(IoError::other("rollout bytes did not confirm the checkpoint position")),
    };
    if observed == CheckpointPosition::Partial {
        // A matching prefix alone is not a durable ownership witness after
        // releasing the byte lock. In-call recovery remains in write_transaction.
        return Unknown(IoError::other("rollout has an unowned short tail at the checkpoint"));
    }
    if !verify_checkpointed_metadata(&writer.path, &checkpoint, session_meta).await {
        return Unknown(IoError::other("could not verify the checkpoint payload"));
    }
    if crate::payload_artifact::sync_payload_artifacts(&writer.path).await.is_err() {
        return if observed == CheckpointPosition::Eof {
            Retryable(IoError::other("could not secure rollout payload before retry"))
        } else {
            Unknown(IoError::other("could not sync the already published rollout payload"))
        };
    }
    if observed == CheckpointPosition::Present {
        if sync_checkpointed_rollout(&writer, &write_lock).await.is_err() {
            return Unknown(IoError::other("could not sync the already published rollout record"));
        }
        return AlreadyPresent;
    }
    write_checkpointed_record(&mut writer, &checkpoint, &write_lock).await
}

#[derive(PartialEq, Eq)]
enum CheckpointPosition {
    Eof,
    Partial,
    Present,
}

async fn preceding_rollout_anchor(
    file: &mut tokio::fs::File,
    offset: u64,
) -> std::io::Result<Option<[u8; 32]>> {
    let anchor_len = offset.min(ROLLOUT_APPEND_ANCHOR_BYTES);
    let mut anchor = [0u8; ROLLOUT_APPEND_ANCHOR_BYTES as usize];
    let anchor = &mut anchor[..anchor_len as usize];
    file.seek(SeekFrom::Start(offset - anchor_len)).await?;
    file.read_exact(anchor).await?;
    Ok((anchor.last() == Some(&b'\n')).then(|| Sha256::digest(&*anchor).into()))
}

async fn observe_checkpoint(
    file: &mut tokio::fs::File,
    checkpoint: &RolloutAppendCheckpoint,
) -> std::io::Result<Option<CheckpointPosition>> {
    let file_len = file.metadata().await?.len();
    let Some(available) = file_len.checked_sub(checkpoint.offset) else {
        return Ok(None);
    };
    if preceding_rollout_anchor(file, checkpoint.offset).await? != Some(checkpoint.preceding_sha256) {
        return Ok(None);
    }
    if available == 0 {
        return Ok(Some(CheckpointPosition::Eof));
    }
    let read_len = available.min(checkpoint.record.len() as u64) as usize;
    let mut observed = [0u8; 8192];
    for expected in checkpoint.record[..read_len].chunks(observed.len()) {
        let observed = &mut observed[..expected.len()];
        file.read_exact(observed).await?;
        if &*observed != expected {
            return Ok(None);
        }
    }
    Ok(Some(if read_len == checkpoint.record.len() {
        CheckpointPosition::Present
    } else {
        CheckpointPosition::Partial
    }))
}

async fn verify_checkpointed_metadata(
    path: &Path,
    checkpoint: &RolloutAppendCheckpoint,
    session_meta: SessionMetaLine,
) -> bool {
    let path = path.to_path_buf();
    let record = checkpoint.record.clone();
    let recipient = (checkpoint.recipient, checkpoint.source_session);
    tokio::task::spawn_blocking(move || {
        use crate::payload_artifact::HydratedRecord;
        let record = String::from_utf8(record)?;
        let hydrated = match crate::payload_artifact::hydrate_record(
            &path, record.clone(), true, true, None,
        ) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A not-yet-referenced body may have been reclaimed while the
                // process was down. Recreate from the immutable intent, not
                // SQLite's current Git info, and demand the same reference.
                let reference: Value = serde_json::from_str(&record)?;
                let timestamp = reference["timestamp"].as_str()
                    .ok_or_else(|| IoError::other("invalid checkpoint payload timestamp"))?;
                let item = RolloutItem::SessionMeta(session_meta);
                let line = RolloutLineRef {
                    timestamp: timestamp.to_owned(),
                    format_version: CURRENT_ROLLOUT_FORMAT_VERSION,
                    diagnostic_origin: None,
                    item: &item,
                };
                if crate::payload_artifact::store_value(&path, serde_json::to_value(&line)?)?
                    .as_deref() != Some(record.as_bytes())
                {
                    return Ok(false);
                }
                crate::payload_artifact::hydrate_record(&path, record, true, true, None)?
            }
            result => result?,
        };
        let line: RolloutLine = match hydrated {
            HydratedRecord::Text(line) => serde_json::from_str(&line)?,
            HydratedRecord::Parsed(value) => serde_json::from_value(value)?,
        };
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
            matches!(line.item, RolloutItem::SessionMeta(meta)
                if (meta.meta.id, meta.meta.session_id) == recipient),
        )
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

async fn write_checkpointed_record(
    writer: &mut JsonlWriter,
    checkpoint: &RolloutAppendCheckpoint,
    write_lock: &compression::RolloutWriteLock,
) -> DurableRolloutAppendOutcome {
    use DurableRolloutAppendOutcome::{Appended, Retryable, Unknown};

    match writer.write_transaction(&checkpoint.record, checkpoint.offset).await {
        Ok(_) => match sync_checkpointed_rollout(writer, write_lock).await {
            Ok(()) => Appended,
            Err(_) => Unknown(IoError::other("could not confirm rollout append durability")),
        },
        Err(error) if is_unrecoverable_rollout_append_error(&error) => {
            Unknown(IoError::other("rollout append recovery did not establish ownership"))
        }
        Err(_) => Retryable(IoError::other("rollout append did not commit")),
    }
}

async fn sync_checkpointed_rollout(
    writer: &JsonlWriter,
    _write_lock: &compression::RolloutWriteLock,
) -> std::io::Result<()> {
    writer.file.sync_data().await?;
    let path = writer.path.clone();
    tokio::task::spawn_blocking(move || sync_rollout_namespace(&path))
        .await
        .map_err(IoError::other)?
}

async fn open_rollout_for_append(path: &Path) -> std::io::Result<(PathBuf, JsonlWriter)> {
    let (path, writer, _) = open_rollout_for_append_with_replay(path, |_, _| {}).await?;
    Ok((path, writer))
}

type AppendReplayIntegrity = (Option<ThreadId>, usize, usize, usize);

#[derive(Default)]
struct AppendWriterTiming {
    payload: AppendPayloadTiming,
    lock: Duration,
    preflight: Duration,
    decode: Duration,
    replay_discard: Duration,
    writer_materialize: Duration,
}

async fn open_rollout_for_append_with_replay<F>(
    path: &Path,
    visit: F,
) -> std::io::Result<(PathBuf, JsonlWriter, Option<AppendReplayIntegrity>)>
where
    F: FnMut(RolloutItem, usize),
{
    use codex_protocol::timing::observations;

    static ENABLED: OnceLock<bool> = OnceLock::new();
    let started = (*ENABLED.get_or_init(|| {
        std::env::var_os("CODEX_ROLLOUT_WRITER_OPEN_TIMING").is_some_and(|value| value == "1")
    }) && observations::prepare_runtime_async_lane())
    .then(Instant::now);
    let mut timing = started.map(|_| AppendWriterTiming::default());
    let result = open_rollout_for_append_with_replay_inner(path, visit, timing.as_mut(), None)
        .await
        .map(|(path, writer, integrity, _)| (path, writer, integrity));
    if let (Some(started), Some(timing)) = (started, timing) {
        let total_ns = started.elapsed().as_nanos();
        let integrity = result.as_ref().ok().and_then(|(_, _, integrity)| *integrity);
        let thread_id = integrity.and_then(|(id, ..)| id).map(|id| id.to_string());
        let store_id = crate::diagnostic_revision::store_id(path);
        // This receipt is queue admission only. The journal writer owns disk I/O.
        let _ = observations::enqueue_runtime(
            "rollout_writer_open_summary",
            thread_id.as_deref().unwrap_or_default(),
            "",
            &store_id,
            serde_json::json!({
                "outcome": if result.is_ok() { "ok" } else { "error" },
                "total_ns": total_ns,
                "lock_ns": timing.lock.as_nanos(),
                "preflight_ns": timing.preflight.as_nanos(),
                "payload_candidates": timing.payload.candidates,
                "payload_bytes_read": timing.payload.bytes_read,
                "payload_read_ns": timing.payload.read.as_nanos(),
                "payload_sha256_ns": timing.payload.sha256.as_nanos(),
                "payload_parse_ns": timing.payload.parse.as_nanos(),
                "plain_replay_projected": timing.payload.plain_replay_projected,
                "replay_size_checked": timing.payload.replay_size_checked,
                "payload_enroll_ns": timing.payload.enroll.as_nanos(),
                "payload_envelope_ns": timing.payload.envelope.as_nanos(),
                "canonical_decode_ns": timing.decode.as_nanos(),
                "replay_discard_ns": timing.replay_discard.as_nanos(),
                "writer_materialize_ns": timing.writer_materialize.as_nanos(),
            }),
        );
    }
    result
}

async fn open_rollout_for_append_with_replay_inner<F>(
    path: &Path,
    mut visit: F,
    mut timing: Option<&mut AppendWriterTiming>,
    checkpoint_recipient: Option<(ThreadId, SessionId)>,
) -> std::io::Result<(
    PathBuf,
    JsonlWriter,
    Option<AppendReplayIntegrity>,
    Option<compression::RolloutWriteLock>,
)>
where
    F: FnMut(RolloutItem, usize),
{
    let path = path.to_path_buf();
    let lock_path = path.clone();
    let lock_started = timing.as_ref().map(|_| Instant::now());
    let locks = tokio::task::spawn_blocking(move || {
        // Same order as ordinary publication: payload lease, representation,
        // then byte writer. Keep all three across preflight and materialization.
        let retention = crate::payload_artifact::lock_publication(&lock_path)?;
        let append = compression::lock_rollout_before_append_blocking(&lock_path)?;
        let snapshot = compression::lock_rollout_for_write_blocking(&lock_path)?;
        if checkpoint_recipient.is_some() {
            let plain = compression::plain_rollout_path(&lock_path);
            if plain.try_exists()? && plain.with_extension("jsonl.zst").try_exists()? {
                return Err(IoError::new(ErrorKind::InvalidData, "ambiguous rollout representations"));
            }
        }
        Ok::<_, IoError>((retention, append, snapshot))
    })
    .await
    .map_err(IoError::other);
    if let (Some(timing), Some(started)) = (timing.as_mut(), lock_started) {
        timing.lock = started.elapsed();
    }
    let (payload_retention, append_lock, snapshot) = locks??;
    let payload_sync_owner = crate::payload_artifact::PayloadSyncOwner::new(&path);
    // Collection and append preflight share this one verified observation,
    // while all publication/representation/byte-write owners remain held.
    let preflight_started = timing.as_ref().map(|_| Instant::now());
    let mut actual_recipient = None;
    let replay = RolloutRecorder::visit_rollout_with_origins(
        &path,
        true,
        false,
        false,
        false,
        timing.as_deref_mut(),
        |item, record, _| {
            if checkpoint_recipient.is_some()
                && actual_recipient.is_none()
                && let RolloutItem::SessionMeta(meta) = &item
            {
                actual_recipient = Some((meta.meta.id, meta.meta.session_id));
            }
            visit(item, record);
        },
    )
    .await;
    if let (Some(timing), Some(started)) = (timing.as_mut(), preflight_started) {
        timing.preflight = started.elapsed();
    }
    let integrity = match replay {
        // A live owner can reopen an empty file after an interrupted initial
        // write. There is no unknown semantic record to fence in that case.
        Err(error)
            if error.kind() == ErrorKind::InvalidData
                && error.to_string() == "empty session file" => None,
        result => {
            let (id, errors, partial, unresolved, _) = result?;
            Some((id, errors, partial, unresolved))
        }
    };
    // Do not use the decoder's filename fallback as recipient proof.
    if checkpoint_recipient.is_some() && checkpoint_recipient != actual_recipient {
        return Err(IoError::new(ErrorKind::InvalidData, "rollout recipient mismatch"));
    }
    let materialize_started = timing.as_ref().map(|_| Instant::now());
    let materialized = tokio::task::spawn_blocking(move || {
        let path = compression::materialize_rollout_for_append_locked(&path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)?;
        if checkpoint_recipient.is_none() {
            ensure_rollout_is_newline_terminated(&mut file)?;
        }
        let file = LockedRolloutFile {
            path: path.clone(),
            file,
            payload_sync_owner,
            append_lock,
            payload_retention,
        };
        let write_lock = checkpoint_recipient.is_some().then_some(snapshot);
        Ok::<_, IoError>((path, file.into_jsonl_writer(), write_lock))
    })
    .await
    .map_err(IoError::other);
    if let (Some(timing), Some(started)) = (timing, materialize_started) {
        timing.writer_materialize = started.elapsed();
    }
    let (path, writer, write_lock) = materialized??;
    Ok((path, writer, integrity, write_lock))
}

fn ensure_rollout_is_newline_terminated(file: &mut File) -> std::io::Result<()> {
    if file.metadata()?.len() == 0 {
        return Ok(());
    }

    file.seek(SeekFrom::End(-1))?;
    let mut final_byte = [0];
    file.read_exact(&mut final_byte)?;
    if final_byte[0] != b'\n' {
        file.write_all(b"\n")?;
        file.flush()?;
    }
    Ok(())
}

// A file sync alone does not persist its name or freshly created date directories.
// Do this once per writer lifetime, never on every append or ordinary flush.
fn sync_rollout_namespace(path: &Path) -> std::io::Result<()> {
    for directory in path
        .ancestors()
        .skip(1)
        .filter(|path| !path.as_os_str().is_empty())
    {
        #[cfg(unix)]
        let result = std::fs::File::open(directory).and_then(|file| file.sync_all());
        #[cfg(windows)]
        let result = {
            use std::os::windows::fs::OpenOptionsExt;
            // Windows directory flushing is best effort, matching artifact publication.
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(0x0200_0000)
                .open(directory)
                .and_then(|file| file.sync_all())
        };
        #[cfg(not(any(unix, windows)))]
        let result: std::io::Result<()> = Ok(());
        // The walk reaches ancestors this writer never created. An unreadable
        // one (for example an execute-only `/home`) or a filesystem without
        // directory sync cannot be fixed by retrying, so it must not fail every
        // later durable barrier. A missing directory still fails.
        if let Err(error) = result
            && !matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::InvalidInput
            )
        {
            return Err(error);
        }
    }
    Ok(())
}

struct JsonlWriter {
    path: PathBuf,
    file: tokio::fs::File,
    _payload_sync_owner: Arc<crate::payload_artifact::PayloadSyncOwner>,
    _append_lock: Arc<compression::RolloutAppendLock>,
    _payload_retention: Arc<std::fs::File>,
}

#[derive(Debug, Eq, PartialEq)]
enum FailedAppendRecovery {
    Committed,
    RolledBack,
}


#[derive(serde::Serialize)]
struct RolloutLineRef<'a> {
    timestamp: String,
    format_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_origin: Option<&'a DiagnosticOrigin>,
    #[serde(flatten)]
    item: &'a RolloutItem,
}

impl<'a> RolloutLineRef<'a> {
    fn new(item: &'a RolloutItem, captured_at: OffsetDateTime) -> std::io::Result<Self> {
        let timestamp_format: &[FormatItem] = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        let timestamp = captured_at
            .format(timestamp_format)
            .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;
        Ok(Self {
            timestamp,
            format_version: CURRENT_ROLLOUT_FORMAT_VERSION,
            diagnostic_origin: None,
            item,
        })
    }

    fn write_jsonl(&self, mut writer: impl Write) -> std::io::Result<()> {
        if let RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event)) =
            self.item
            && let codex_protocol::items::TurnItem::CommandExecution(command) = &event.item
            && command.stdout.as_ref().is_some_and(|s| s.len() >= crate::payload_artifact::INLINE_BYTES)
            && command.stdout == command.aggregated_output
        {
            // Keep the aggregate for raw readers; only the rollout projection
            // omits the redundant stdout field.
            let mut value = serde_json::to_value(self)?;
            let item = value.pointer_mut("/payload/item").and_then(serde_json::Value::as_object_mut)
                .ok_or_else(|| IoError::other("missing command completion item"))?;
            if item.remove("stdout").is_none() {
                return Err(IoError::other("command completion did not serialize stdout"));
            }
            item.insert("stdout_from_aggregated_output".into(), true.into());
            serde_json::to_writer(&mut writer, &value)?;
        } else {
            serde_json::to_writer(&mut writer, self)?;
        }
        writer.write_all(b"\n")
    }
}

/// Include the newline in the inline threshold without encoding a whole large
/// record. Small eligible records still need only one serialization.
#[derive(Default)]
struct InlineRolloutLine {
    bytes: Vec<u8>,
    limit_reached: bool,
}

impl Write for InlineRolloutLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.limit_reached
            || bytes.len() >= crate::payload_artifact::INLINE_BYTES - self.bytes.len()
        {
            self.limit_reached = true;
            return Err(IoError::new(ErrorKind::WriteZero, "rollout inline limit reached"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl JsonlWriter {
    async fn write_rollout_item(&mut self, rollout_item: &RolloutItem) -> std::io::Result<()> {
        // A direct append has no queue between capture and write.
        let captured = CapturedRolloutItem::new(rollout_item.clone(), OffsetDateTime::now_utc());
        self.write_rollout_items(&[&captured]).await
    }

    async fn write_rollout_items(
        &mut self,
        rollout_items: &[&CapturedRolloutItem],
    ) -> std::io::Result<()> {
        let path = self.path.clone();
        let write_lock = tokio::task::spawn_blocking(move || {
            compression::lock_rollout_for_write_blocking(&path)
        })
        .await
        .map_err(IoError::other)??;
        self.write_rollout_items_locked(rollout_items, &write_lock, None)
            .await.map(|_| ())
    }

    async fn write_rollout_items_locked(
        &mut self,
        rollout_items: &[&CapturedRolloutItem],
        _write_lock: &compression::RolloutWriteLock,
        append_start: Option<u64>,
    ) -> std::io::Result<u64> {
        let append_start = match append_start {
            Some(start) => start,
            None => self.file.metadata().await?.len(),
        };
        if rollout_items.is_empty() {
            return Ok(append_start);
        }
        let bytes = self.encode_rollout_items(rollout_items).await?;
        self.write_transaction(&bytes, append_start).await
    }

    async fn encode_rollout_items(
        &self,
        rollout_items: &[&CapturedRolloutItem],
    ) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for captured in rollout_items {
            let item = match &captured.item {
                CapturedRolloutPayload::Canonical(item) => item,
                CapturedRolloutPayload::Artifact(artifact) => {
                    bytes.extend_from_slice(&artifact.line);
                    continue;
                }
            };
            if !crate::payload_artifact::is_artifact_candidate(item) {
                let mut line = RolloutLineRef::new(item, captured.captured_at)?;
                line.diagnostic_origin = captured.diagnostic_origin.as_deref();
                line.write_jsonl(&mut bytes)?;
                continue;
            }
            let line = RolloutLineRef::new(item, captured.captured_at)?;
            let mut inline = InlineRolloutLine::default();
            let serialized = line.write_jsonl(&mut inline);
            if !inline.limit_reached {
                serialized?;
                bytes.extend_from_slice(&inline.bytes);
                continue;
            }
            // Preserve the line's field insertion order and the artifact owner's
            // metadata removals, but avoid encoding then parsing its full body.
            let value = match serde_json::to_value(&line) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(%error, "retaining rollout payload inline because artifact storage failed");
                    line.write_jsonl(&mut bytes)?;
                    continue;
                }
            };
            let path = self.path.clone();
            let payload_sync_owner = Arc::clone(&self._payload_sync_owner);
            let append_lock = Arc::clone(&self._append_lock);
            let stored = tokio::task::spawn_blocking(move || {
                let result = crate::payload_artifact::store_value(&path, value);
                drop(payload_sync_owner);
                drop(append_lock);
                result
            })
            .await
            .map_err(IoError::other)?;
            match stored {
                Ok(Some(stored)) => bytes.extend_from_slice(&stored),
                Ok(None) => line.write_jsonl(&mut bytes)?,
                Err(error) => {
                    tracing::warn!(%error, "retaining rollout payload inline because artifact storage failed");
                    line.write_jsonl(&mut bytes)?;
                }
            }
        }
        Ok(bytes)
    }

    async fn write_transaction(&mut self, bytes: &[u8], append_start: u64) -> std::io::Result<u64> {
        let committed_end = append_start.checked_add(bytes.len() as u64)
            .ok_or_else(|| IoError::other("rollout append offset overflow"))?;
        let write_result = self.write_line_bytes(bytes).await;
        let Err(append_error) = write_result else {
            return Ok(committed_end);
        };

        match self.recover_failed_append(append_start, bytes).await {
            Ok(FailedAppendRecovery::Committed) => Ok(committed_end),
            Ok(FailedAppendRecovery::RolledBack) => Err(append_error),
            Err(recovery_error) => Err(IoError::other(RolloutAppendRecoveryError {
                path: self.path.clone(),
                append_error,
                recovery_error,
            })),
        }
    }

    async fn write_line_bytes(&mut self, line: &[u8]) -> std::io::Result<()> {

        self.file.write_all(line).await?;
        self.file.flush().await
    }

    async fn recover_failed_append(
        &self,
        append_start: u64,
        expected_line: &[u8],
    ) -> std::io::Result<FailedAppendRecovery> {
        let mut file = tokio::fs::File::open(&self.path).await?;
        let file_len = file.metadata().await?.len();
        if file_len == append_start {
            return Ok(FailedAppendRecovery::RolledBack);
        }
        if file_len < append_start {
            return Err(IoError::other(format!(
                "rollout shrank from byte {append_start} to {file_len} during append"
            )));
        }

        let expected_len = u64::try_from(expected_line.len())
            .map_err(|_| IoError::other("rollout line length exceeds u64"))?;
        let observed_len = usize::try_from((file_len - append_start).min(expected_len))
            .map_err(|_| IoError::other("rollout append length exceeds usize"))?;
        file.seek(SeekFrom::Start(append_start)).await?;
        let mut observed = vec![0; observed_len];
        file.read_exact(&mut observed).await?;

        if observed == expected_line[..observed_len] {
            if observed_len == expected_line.len() {
                return Ok(FailedAppendRecovery::Committed);
            }

            let rollback_file = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .await?;
            rollback_file.set_len(append_start).await?;
            return Ok(FailedAppendRecovery::RolledBack);
        }

        Err(IoError::other(format!(
            "rollout contains unexpected bytes after failed append at byte {append_start}"
        )))
    }
}

#[derive(Debug)]
struct RolloutAppendRecoveryError {
    path: PathBuf,
    append_error: IoError,
    recovery_error: IoError,
}

impl std::fmt::Display for RolloutAppendRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "append to {} failed ({}) and its on-disk state could not be recovered ({}); retry is \
             unsafe",
            self.path.display(),
            self.append_error,
            self.recovery_error
        )
    }
}

impl std::error::Error for RolloutAppendRecoveryError {}

fn is_unrecoverable_rollout_append_error(err: &IoError) -> bool {
    err.get_ref()
        .and_then(|source| source.downcast_ref::<RolloutAppendRecoveryError>())
        .is_some()
}



