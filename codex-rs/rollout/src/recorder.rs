//! Persist Codex session rollouts (.jsonl) so sessions can be replayed or inspected later.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
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
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use chrono::SecondsFormat;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::BaseInstructions;
use serde_json::Value;
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

use super::ARCHIVED_SESSIONS_SUBDIR;
use super::SESSIONS_SUBDIR;
use super::compression;
use super::list::Cursor;
use super::list::SortDirection;
use super::list::ThreadItem;
use super::list::ThreadListConfig;
use super::list::ThreadListLayout;
use super::list::ThreadSortKey;
use super::list::ThreadsPage;
use super::list::get_threads;
use super::list::get_threads_ascending;
use super::list::get_threads_in_root;
use super::list::get_threads_in_root_ascending;
use super::list::thread_item_sort_key;
use super::session_index::find_thread_names_by_ids;
use crate::config::RolloutConfigView;
use crate::state_integration;
use crate::state_integration::StateDbHandle;
use codex_git_utils::RepositoryContext;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_state::StateRuntime;

const RECONSTRUCTION_NOTICE_ID_BASE: &str = "msg_rollout_gap";

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
/// ```ignore
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

/// A rollout item paired with the instant it was handed to the recorder.
///
/// Records are coalesced and flushed in batches, and a batch can be rebased or
/// retried before it lands. Stamping at write time gave every record in a batch
/// the flush time instead of its own, so a record's timestamp described when
/// the writer got around to it rather than when it happened.
#[derive(Clone, Debug)]
struct CapturedRolloutItem {
    captured_at: OffsetDateTime,
    item: RolloutItem,
    // Shared batch admission follows canonical records from enqueue through
    // persistence, including retries and in-place manifest compaction.
    admission: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}

impl CapturedRolloutItem {
    fn new(item: RolloutItem, captured_at: OffsetDateTime) -> Self {
        Self {
            captured_at,
            item,
            admission: None,
        }
    }

    /// Keeps the original capture time while replacing the payload, for the
    /// manifest encoding that rewrites an item in place.
    fn with_item(&self, item: RolloutItem) -> Self {
        Self {
            captured_at: self.captured_at,
            item,
            admission: self.admission.clone(),
        }
    }
}

enum RolloutCmd {
    AddItems {
        items: Vec<CapturedRolloutItem>,
        flush_if_materialized: bool,
        accepted: Option<oneshot::Sender<()>>,
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
    },
    #[cfg(test)]
    Pause {
        entered: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    },
}

/// Observable state for the background rollout writer task.
struct RolloutWriterTask {
    handle: Mutex<Option<JoinHandle<()>>>,
    terminal_failure: Mutex<Option<Arc<IoError>>>,
    // Clones share custody, not independent lease references. A stopped handle
    // must not keep a terminated writer's logical ownership alive.
    writer_lease: Mutex<Option<Arc<std::fs::File>>>,
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
            self.0.mark_failed(&IoError::other(
                "rollout writer stopped before terminal settlement",
            ));
        }
        self.0
            .writer_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
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
    RestoreFailed,
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
        self.writer_task
            .mark_failed(&IoError::other("rollout maintenance restoration failed"));
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(MaintenanceDecision::RestoreFailed);
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
    fn mark_failed(&self, err: &IoError) {
        {
            let mut guard = self
                .terminal_failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(Arc::new(clone_io_error(err)));
        }
        self.lifecycle.store(WRITER_FAILED, Ordering::Release);
    }

    /// Return the terminal writer-task failure, if the task exited with an error.
    fn terminal_failure(&self) -> Option<IoError> {
        let guard = self
            .terminal_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().map(|err| clone_io_error(err.as_ref()))
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
            self.writer_lease
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
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

fn clone_io_error(err: &IoError) -> IoError {
    IoError::new(err.kind(), err.to_string())
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

#[derive(Clone, Copy)]
enum ThreadListArchiveFilter {
    Active,
    Archived,
}

#[derive(Clone, Copy)]
enum ThreadListRepairMode {
    ScanAndRepair,
    StateDbOnly,
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
                codex_utils_build_info::executable_sha256().map(str::to_owned);
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

    /// List threads (rollout files) under the provided Codex home directory.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_threads(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Active,
            ThreadListRepairMode::ScanAndRepair,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_threads_from_state_db(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Active,
            ThreadListRepairMode::StateDbOnly,
            search_term,
        )
        .await
    }

    /// List archived threads (rollout files) under the archived sessions directory.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_archived_threads(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Archived,
            ThreadListRepairMode::ScanAndRepair,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_archived_threads_from_state_db(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Archived,
            ThreadListRepairMode::StateDbOnly,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_threads_with_db_fallback(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        archive_filter: ThreadListArchiveFilter,
        repair_mode: ThreadListRepairMode,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        let codex_home = config.codex_home();
        let archived = match archive_filter {
            ThreadListArchiveFilter::Active => false,
            ThreadListArchiveFilter::Archived => true,
        };
        if cwd_filters.is_some_and(<[std::path::PathBuf]>::is_empty) {
            return Ok(ThreadsPage::default());
        }

        if matches!(repair_mode, ThreadListRepairMode::StateDbOnly) {
            let page = state_integration::list_threads_db(
                state_db_ctx.as_deref(),
                codex_home,
                page_size,
                cursor,
                sort_key,
                sort_direction,
                allowed_sources,
                model_providers,
                cwd_filters,
                /*relation_filter*/ None,
                archived,
                /*project_id*/ None,
                search_term,
            )
            .await
            .ok_or_else(|| std::io::Error::other("state DB unavailable for thread listing"))?;
            return Ok(page.into());
        }

        let listing_has_metadata_filters = !allowed_sources.is_empty()
            || model_providers.is_some()
            || cwd_filters.is_some()
            || search_term.is_some();
        // Filesystem-first listing intentionally overfetches so we can repair stale/missing
        // SQLite rows while keeping the returned page and cursor filesystem-backed.
        let fs_page = match sort_direction {
            SortDirection::Asc => {
                list_threads_from_files_asc(
                    state_db_ctx.as_deref(),
                    codex_home,
                    page_size,
                    cursor,
                    sort_key,
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    archived,
                    search_term,
                )
                .await?
            }
            SortDirection::Desc => {
                list_threads_from_files_desc(
                    state_db_ctx.as_deref(),
                    codex_home,
                    if state_db_ctx.is_some() {
                        page_size.saturating_mul(2)
                    } else {
                        page_size
                    },
                    cursor,
                    sort_key,
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    archived,
                    search_term,
                )
                .await?
            }
        };

        if state_db_ctx.is_none() {
            // Keep legacy behavior when SQLite is unavailable: return filesystem results
            // at the requested page size.
            codex_state::record_fallback(
                "list_threads",
                "db_unavailable",
                /*telemetry_override*/ None,
            );
            return Ok(page_from_filesystem_scan(
                fs_page,
                sort_direction,
                page_size,
                sort_key,
            ));
        }

        // Track filesystem IDs so the later DB comparison only triggers full reconciliation for
        // DB-only hits.
        let fs_page_thread_ids = fs_page
            .items
            .iter()
            .filter_map(|item| item.thread_id)
            .collect::<HashSet<_>>();

        // Reconcile each filesystem hit once, then use SQLite as the authoritative projection.
        // Filesystem filtering already reduced the complete persisted-settings history, so the
        // same current provider/cwd facts drive both candidate selection and the database row.
        let mut reconciled_ids = HashSet::new();
        for item in &fs_page.items {
            if state_integration::reconcile_rollout(
                state_db_ctx.as_deref(),
                item.path.as_path(),
                default_provider,
                /*builder*/ None,
                &[],
                Some(archived),
            )
            .await
            {
                reconciled_ids.extend(item.thread_id);
            }
        }

        if !listing_has_metadata_filters {
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            return Ok(overlay_thread_item_metadata_from_state_db(
                state_db_ctx.as_deref(),
                page,
                &reconciled_ids,
            )
            .await);
        }

        let db_page = state_integration::list_threads_db(
            state_db_ctx.as_deref(),
            codex_home,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            /*relation_filter*/ None,
            archived,
            /*project_id*/ None,
            search_term,
        )
        .await;
        if let Some(db_page) = db_page {
            if search_term.is_some() && (!db_page.items.is_empty() || cursor.is_some()) {
                for item in &db_page.items {
                    if !Self::db_hit_needs_reconciliation(&fs_page_thread_ids, item.id) {
                        continue;
                    }
                    state_integration::reconcile_rollout(
                        state_db_ctx.as_deref(),
                        item.rollout_path.as_path(),
                        default_provider,
                        /*builder*/ None,
                        &[],
                        Some(archived),
                    )
                    .await;
                }
                let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
                return Ok(overlay_thread_item_metadata_from_state_db(
                    state_db_ctx.as_deref(),
                    page,
                    &reconciled_ids,
                )
                .await);
            }
            if listing_has_metadata_filters {
                for item in &db_page.items {
                    // Rows that also appeared in the filesystem page were just validated from the
                    // complete rollout. Rows only found by SQLite may be stale filter matches, so
                    // fully reconcile those before returning the filesystem-backed page.
                    if !Self::db_hit_needs_reconciliation(&fs_page_thread_ids, item.id) {
                        continue;
                    }
                    state_integration::reconcile_rollout(
                        state_db_ctx.as_deref(),
                        item.rollout_path.as_path(),
                        default_provider,
                        /*builder*/ None,
                        &[],
                        Some(archived),
                    )
                    .await;
                }
                if sort_key == ThreadSortKey::RecencyAt {
                    let page =
                        page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
                    return Ok(overlay_thread_item_metadata_from_state_db(
                        state_db_ctx.as_deref(),
                        page,
                        &reconciled_ids,
                    )
                    .await);
                }
                codex_state::record_fallback(
                    "list_threads",
                    "metadata_filter",
                    /*telemetry_override*/ None,
                );
                let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
                return Ok(overlay_thread_item_metadata_from_state_db(
                    state_db_ctx.as_deref(),
                    page,
                    &reconciled_ids,
                )
                .await);
            }
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            return Ok(overlay_thread_item_metadata_from_state_db(
                state_db_ctx.as_deref(),
                page,
                &reconciled_ids,
            )
            .await);
        }
        // SQLite listing failed and list_threads_db already logged why. Return the
        // filesystem page rather than failing the list.
        let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
        codex_state::record_fallback("list_threads", "db_error", /*telemetry_override*/ None);
        Ok(overlay_thread_item_metadata_from_state_db(
            state_db_ctx.as_deref(),
            page,
            &reconciled_ids,
        )
        .await)
    }

    fn db_hit_needs_reconciliation(
        filesystem_thread_ids: &HashSet<ThreadId>,
        db_thread_id: ThreadId,
    ) -> bool {
        !filesystem_thread_ids.contains(&db_thread_id)
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

    /// Identity of the append owner, independent of path reuse or recorder clones.
    pub fn same_writer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.writer_task, &other.writer_task)
    }

    /// Quiesce this recorder before acquiring rollout-move or maintenance locks.
    /// Bounded admission prevents blocked storage from stranding a delete caller.
    pub async fn pause_for_maintenance(&self) -> std::io::Result<RolloutMaintenanceGuard> {
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

    /// Load canonical history and its manifest dictionary in one pass. The
    /// existing append/write leases bind both to the same file snapshot until
    /// the recorder has taken custody; externally supplied history uses `new`.
    pub async fn resume_and_load(
        config: &impl RolloutConfigView,
        path: PathBuf,
        expected_thread: ThreadId,
    ) -> std::io::Result<(Self, Vec<RolloutItem>)> {
        Self::resume_and_load_with_writer_lease(config, path, expected_thread, None).await
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
                items.push(item);
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
                    // Hash lazily when metadata is first written.
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
            let result = rollout_writer(
                writer,
                deferred_log_file_info,
                rx,
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
                error!(
                    "rollout writer task failed for {}: {err}; error_kind={:?}; raw_os_error={:?}",
                    rollout_path_for_spawn.display(),
                    err.kind(),
                    err.raw_os_error()
                );
                writer_task_for_spawn.mark_failed(&err);
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
        self.record_canonical_items_with_flush(items, true, false)
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
        let admission_bytes = rollout_admission_bytes(items);
        if admission_bytes.is_err() {
            // Validate every individual item before accepting any prefix. Bulk
            // inheritance may exceed admission even though each record fits.
            let charges = items.iter()
                .map(|item| rollout_admission_bytes(std::slice::from_ref(item)))
                .collect::<std::io::Result<Vec<_>>>()?;
            let mut start = 0;
            let mut charge = 0usize;
            for (index, next) in charges.into_iter().enumerate() {
                let next = next as usize;
                if index > start && charge.saturating_add(next) > MAX_PENDING_ROLLOUT_BYTES / 2 {
                    self.record_canonical_items_with_admission(
                        &items[start..index], true, false, Some(charge as u32),
                    ).await?;
                    // Lazy pending items release admission only after materialization.
                    // No enqueue gate is held across this barrier.
                    self.persist().await?;
                    start = index;
                    charge = 0;
                }
                charge = charge.saturating_add(next);
            }
            self.record_canonical_items_with_admission(
                &items[start..], true, false, Some(charge as u32),
            ).await?;
            return self.persist().await;
        }
        // Waiting for the consumer's acknowledgement also waits for its preceding
        // disk write. Queue admission already transfers ownership and preserves
        // ordering with barriers/shutdown under the enqueue gate. Reuse that path
        // rather than serializing independent work behind slow storage. Neither
        // form promises durability before an explicit barrier.
        self.record_canonical_items_with_admission(items, true, false, Some(admission_bytes?))
            .await
    }

    async fn record_canonical_items_with_flush(
        &self,
        items: &[RolloutItem],
        flush_if_materialized: bool,
        wait_for_acceptance: bool,
    ) -> std::io::Result<()> {
        self.record_canonical_items_with_admission(
            items,
            flush_if_materialized,
            wait_for_acceptance,
            None,
        )
        .await
    }

    async fn record_canonical_items_with_admission(
        &self,
        items: &[RolloutItem],
        flush_if_materialized: bool,
        wait_for_acceptance: bool,
        admission_bytes: Option<u32>,
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
        let (accepted, acceptance) = if wait_for_acceptance {
            let (accepted, acceptance) = oneshot::channel();
            (Some(accepted), Some(acceptance))
        } else {
            (None, None)
        };
        let result = self
            .tx
            .send(RolloutCmd::AddItems {
                // Capture once, here: this is when the caller produced these
                // records, and it is the only point that still knows.
                items: {
                    let captured_at = OffsetDateTime::now_utc();
                    items
                        .iter()
                        .map(|item| {
                            let mut captured = CapturedRolloutItem::new(item.clone(), captured_at);
                            captured.admission = Some(Arc::clone(&admission));
                            captured
                        })
                        .collect()
                },
                flush_if_materialized,
                accepted,
            })
            .await
            .map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed to queue rollout items: {e}"))
                })
            });
        drop(enqueue_guard);
        result?;
        if let Some(acceptance) = acceptance {
            acceptance.await.map_err(|e| {
                self.writer_task.terminal_failure().unwrap_or_else(|| {
                    IoError::other(format!("failed waiting for rollout item acceptance: {e}"))
                })
            })?;
        }
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
    /// records were skipped. Keeping the notice in the existing history carrier
    /// makes gaps visible even to consumers that discard the diagnostic count.
    /// It is not written back to the source rollout by this read.
    pub async fn load_rollout_items(
        path: &Path,
    ) -> std::io::Result<(Vec<RolloutItem>, Option<ThreadId>, usize)> {
        let (items, thread_id, errors, _) = Self::load_rollout_items_inner(path, false).await?;
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
        Self::load_rollout_items_inner(path, true).await
    }

    async fn load_rollout_items_inner(
        path: &Path,
        capture_payload_versions: bool,
    ) -> std::io::Result<(
        Vec<RolloutItem>,
        Option<ThreadId>,
        usize,
        Vec<crate::PayloadDependencyVersion>,
    )> {
        let mut items = Vec::new();
        let (thread_id, parse_errors, trailing_partial_records, unresolved_payloads, versions) =
            Self::for_each_rollout_item_with_versions(
                path,
                false,
                capture_payload_versions,
                |item, _| items.push(item),
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
        Ok((items, thread_id, parse_errors, versions))
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

    /// Visit each valid rollout item without retaining the full rollout in memory.
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

    async fn for_each_rollout_item_with_versions<F>(
        path: &Path,
        enroll_dependencies: bool,
        capture_payload_versions: bool,
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
        trace!("Resuming rollout from {path:?}");
        let mut thread_id: Option<ThreadId> = None;
        let mut parse_errors = 0usize;
        let mut last_record_partial = false;
        let mut reader = compression::open_rollout_line_reader(path).await?;
        if capture_payload_versions {
            reader.capture_payload_versions();
        }
        if enroll_dependencies {
            reader.enroll_payload_dependencies();
        }
        let mut saw_non_empty_line = false;
        let mut record_number = 0usize;
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            saw_non_empty_line = true;
            last_record_partial = false;
            record_number = record_number.saturating_add(1);
            let mut v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse line as JSON: {line:?}, error: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                    last_record_partial = !reader.last_line_terminated() && e.is_eof();
                    continue;
                }
            };
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
                    format!(
                        "unsupported rollout format version {version} at record {record_number}"
                    ),
                ));
            }
            if is_v0 && migrate_v0_ghost_snapshot_rollout_line(&mut v) {
                trace!("skipping legacy ghost_snapshot rollout line");
                continue;
            }

            if thread_id.is_none() {
                // Validate the small compatibility discriminator before consuming the JSON value.
                // This preserves the unknown-mode guard without cloning the whole rollout line.
                reject_unknown_thread_history_mode(&v)?;
            }

            // Secondary session metadata is provenance for a fork source, not
            // the canonical thread's schema/operational history. Preserve the
            // existing recoverable-gap treatment for that optional provenance.
            let secondary_metadata = thread_id.is_some()
                && v.get("type").and_then(Value::as_str) == Some("session_meta");
            // Parse the rollout line structure
            match serde_json::from_value::<RolloutLine>(v) {
                Ok(rollout_line) => {
                    let item = rollout_line.item;
                    // Use the FIRST SessionMeta encountered in the file as the canonical
                    // thread id and main session information. Keep all items intact.
                    if thread_id.is_none()
                        && let RolloutItem::SessionMeta(session_meta_line) = &item
                    {
                        thread_id = Some(session_meta_line.meta.id);
                    }
                    visit(item, record_number);
                }
                Err(e) => {
                    if !secondary_metadata && e.to_string().contains("unknown variant") {
                        return Err(IoError::new(
                            ErrorKind::Unsupported,
                            format!(
                                "unsupported rollout representation at record {record_number}: {e}"
                            ),
                        ));
                    }
                    trace!("failed to parse rollout line: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                }
            }
        }
        if !saw_non_empty_line {
            return Err(IoError::new(ErrorKind::InvalidData, "empty session file"));
        }

        // A damaged metadata payload must not hide an otherwise usable rollout.
        let thread_id = thread_id.or_else(|| {
            let (_, id) =
                crate::list::parse_timestamp_uuid_from_filename(path.file_name()?.to_str()?)?;
            ThreadId::from_string(&id.to_string()).ok()
        });
        Ok((
            thread_id,
            parse_errors,
            usize::from(last_record_partial),
            reader.unresolved_payloads(),
            reader.take_payload_versions(),
        ))
    }

    #[cfg(test)]
    async fn existing_tool_manifests(
        path: &Path,
    ) -> std::io::Result<crate::ToolManifestDictionary> {
        let (_, manifests) = Self::existing_rollout_state(path).await?;
        Ok(manifests)
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
            if line.trim().is_empty() {
                continue;
            }
            let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(line.trim()) else {
                parse_errors = parse_errors.saturating_add(1);
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
        let (items, thread_id, parse_errors) = Self::load_rollout_items(path).await?;
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
        let permit = self.tx.reserve().await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed to reserve rollout discard command: {e}"))
            })
        })?;
        self.writer_task.begin_shutdown()?;
        let (ack, done) = oneshot::channel();
        permit.send(RolloutCmd::Discard { ack });
        drop(enqueue_guard);
        done.await.map_err(|e| {
            self.writer_task.terminal_failure().unwrap_or_else(|| {
                IoError::other(format!("failed waiting for rollout discard: {e}"))
            })
        })
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

fn truncate_fs_page(
    mut page: ThreadsPage,
    page_size: usize,
    sort_key: ThreadSortKey,
) -> ThreadsPage {
    if page.items.len() <= page_size {
        return page;
    }
    page.items.truncate(page_size);
    page.next_cursor = page
        .items
        .last()
        .and_then(|item| cursor_from_thread_item(item, sort_key));
    page
}

fn page_from_filesystem_scan(
    page: ThreadsPage,
    sort_direction: SortDirection,
    page_size: usize,
    sort_key: ThreadSortKey,
) -> ThreadsPage {
    match sort_direction {
        SortDirection::Asc => page,
        SortDirection::Desc => truncate_fs_page(page, page_size, sort_key),
    }
}

async fn overlay_thread_item_metadata_from_state_db(
    state_db_ctx: Option<&StateRuntime>,
    mut page: ThreadsPage,
    reconciled_ids: &HashSet<ThreadId>,
) -> ThreadsPage {
    let Some(state_db_ctx) = state_db_ctx else {
        return page;
    };

    for item in &mut page.items {
        let Some(thread_id) = item.thread_id else {
            continue;
        };
        let metadata = match state_db_ctx.get_thread(thread_id).await {
            Ok(Some(metadata)) => metadata,
            Ok(None) => continue,
            Err(err) => {
                warn!(
                    "state db get_thread failed while overlaying filesystem scan thread metadata: {err}"
                );
                continue;
            }
        };
        if !reconciled_ids.contains(&thread_id) {
            if !metadata.title.trim().is_empty()
                && metadata.first_user_message.as_deref().map(str::trim)
                    != Some(metadata.title.trim())
            {
                item.title = Some(metadata.title);
            }
            continue;
        }
        overlay_thread_item_metadata(
            item,
            thread_item_from_state_metadata(metadata, /*parent_thread_id*/ None),
        );
    }

    page
}

fn overlay_thread_item_metadata(item: &mut ThreadItem, state_item: ThreadItem) {
    let ThreadItem {
        path,
        thread_id: _state_thread_id,
        first_user_message,
        title,
        preview,
        cwd,
        git_branch,
        git_sha,
        git_origin_url,
        source,
        history_mode,
        parent_thread_id,
        agent_nickname,
        agent_role,
        model_provider,
        cli_version,
        created_at,
        updated_at,
        recency_at,
    } = state_item;

    item.path = path;
    item.first_user_message = first_user_message;
    item.title = title;
    item.preview = preview;
    item.cwd = cwd;
    item.git_branch = git_branch;
    item.git_sha = git_sha;
    item.git_origin_url = git_origin_url;
    item.source = source;
    item.history_mode = history_mode;
    if item.parent_thread_id.is_none() {
        item.parent_thread_id = parent_thread_id;
    }
    item.agent_nickname = agent_nickname;
    item.agent_role = agent_role;
    item.model_provider = model_provider;
    item.cli_version = cli_version;
    item.created_at = created_at;
    item.updated_at = updated_at;
    item.recency_at = recency_at;
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_desc(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
    search_term: Option<&str>,
) -> std::io::Result<ThreadsPage> {
    if let Some(search_term) = search_term {
        let mut matching_items = Vec::new();
        let mut scanned_files = 0usize;
        let mut reached_scan_cap = false;
        let mut page_cursor = cursor.cloned();
        let scan_page_size = page_size.saturating_mul(8).clamp(256, 2048);

        loop {
            let mut page = list_threads_from_files_desc_unfiltered(
                codex_home,
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
                archived,
            )
            .await?;
            scanned_files = scanned_files.saturating_add(page.num_scanned_files);
            reached_scan_cap |= page.reached_scan_cap;
            filter_thread_items_by_search_term(
                state_db_ctx,
                codex_home,
                &mut page.items,
                Some(search_term),
            )
            .await?;
            matching_items.extend(page.items);
            page_cursor = page.next_cursor;
            if matching_items.len() > page_size || page_cursor.is_none() {
                break;
            }
        }

        let more_matches_available =
            matching_items.len() > page_size || page_cursor.is_some() || reached_scan_cap;
        matching_items.truncate(page_size);
        let next_cursor = if more_matches_available {
            matching_items
                .last()
                .and_then(|item| cursor_from_thread_item(item, sort_key))
        } else {
            None
        };

        return Ok(ThreadsPage {
            items: matching_items,
            next_cursor,
            num_scanned_files: scanned_files,
            reached_scan_cap,
        });
    }

    list_threads_from_files_desc_unfiltered(
        codex_home,
        page_size,
        cursor,
        sort_key,
        allowed_sources,
        model_providers,
        cwd_filters,
        default_provider,
        archived,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_desc_unfiltered(
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
) -> std::io::Result<ThreadsPage> {
    if archived {
        let root = codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
        get_threads_in_root(
            root,
            page_size,
            cursor,
            sort_key,
            ThreadListConfig {
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
                layout: ThreadListLayout::Flat,
            },
        )
        .await
    } else {
        get_threads(
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_asc(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
    search_term: Option<&str>,
) -> std::io::Result<ThreadsPage> {
    let scan_page_size =
        search_term.map_or(page_size, |_| page_size.saturating_mul(8).clamp(256, 2048));
    let mut page = ThreadsPage::default();
    let mut page_cursor = cursor.cloned();
    loop {
        let mut scanned = if archived {
            get_threads_in_root_ascending(
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                ThreadListConfig {
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    layout: ThreadListLayout::Flat,
                },
            )
            .await?
        } else {
            get_threads_ascending(
                codex_home,
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
            )
            .await?
        };
        page.num_scanned_files = page
            .num_scanned_files
            .saturating_add(scanned.num_scanned_files);
        page.reached_scan_cap |= scanned.reached_scan_cap;
        filter_thread_items_by_search_term(
            state_db_ctx,
            codex_home,
            &mut scanned.items,
            search_term,
        )
        .await?;
        page.items.extend(scanned.items);
        page_cursor = scanned.next_cursor;
        if search_term.is_none() || page.items.len() > page_size || page_cursor.is_none() {
            break;
        }
    }

    let more_matches_available =
        page_cursor.is_some() || page.items.len() > page_size || page.reached_scan_cap;
    page.items.truncate(page_size);
    page.next_cursor = if more_matches_available {
        page.items
            .last()
            .and_then(|item| cursor_from_thread_item(item, sort_key))
    } else {
        None
    };
    Ok(page)
}

async fn filter_thread_items_by_search_term(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    items: &mut Vec<ThreadItem>,
    search_term: Option<&str>,
) -> std::io::Result<()> {
    let Some(search_term) = search_term else {
        return Ok(());
    };

    // Match the title the listing displays: SQLite owns explicit titles, and the sidecar index
    // is the compatibility fallback (it can lack a committed rename). Previews come from the
    // rollout summary. Match SQLite's title-or-preview substring predicate in both sort orders.
    let thread_ids = items
        .iter()
        .filter_map(|item| item.thread_id)
        .collect::<HashSet<_>>();
    let mut thread_names = HashMap::with_capacity(thread_ids.len());
    if let Some(state_db_ctx) = state_db_ctx
        && !thread_ids.is_empty()
    {
        let ids = thread_ids.iter().copied().collect::<Vec<_>>();
        match state_db_ctx.get_thread_titles(&ids).await {
            Ok(titles) => {
                for (thread_id, title, first_user_message) in titles {
                    let title = title.trim();
                    if !title.is_empty()
                        && first_user_message.as_deref().map(str::trim) != Some(title)
                    {
                        thread_names.insert(thread_id, title.to_string());
                    }
                }
            }
            Err(err) => {
                warn!("state db get_thread_titles failed while filtering thread search: {err}");
            }
        }
    }
    let unresolved_thread_ids = thread_ids
        .into_iter()
        .filter(|thread_id| !thread_names.contains_key(thread_id))
        .collect::<HashSet<_>>();
    for (thread_id, name) in find_thread_names_by_ids(codex_home, &unresolved_thread_ids).await? {
        thread_names.entry(thread_id).or_insert(name);
    }
    items.retain(|item| {
        item.thread_id
            .and_then(|thread_id| thread_names.get(&thread_id))
            .is_some_and(|title| title.contains(search_term))
            || item
                .preview
                .as_deref()
                .is_some_and(|preview| preview.contains(search_term))
    });
    Ok(())
}

fn cursor_from_thread_item(item: &ThreadItem, sort_key: ThreadSortKey) -> Option<Cursor> {
    let (timestamp, id) = thread_item_sort_key(item, sort_key)?;
    Some(Cursor::with_thread_id(
        timestamp,
        ThreadId::from_string(&id.to_string()).ok()?,
    ))
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
            #[cfg(test)]
            write_fault: None,
            #[cfg(test)]
            append_transaction_count: 0,
        }
    }
}

fn open_log_file(path: &Path) -> std::io::Result<LockedRolloutFile> {
    open_log_file_with_options(path, /*create*/ true).map(|(_, file)| file)
}

fn open_log_file_with_options(
    path: &Path,
    create: bool,
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

    ensure_rollout_is_newline_terminated(&mut file)?;
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
                if let RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::AgentMessage(
                    actual,
                )) = &captured.item
                    && expected.message == actual.message
                    && expected.phase == actual.phase
                {
                    continue;
                }
                self.pending_agent_message_mirrors.clear();
            }
            match &captured.item {
                // The recorder owns the single canonical metadata slot. Inherited history must
                // never append another session_meta record.
                RolloutItem::SessionMeta(_) => {
                    trace!(
                        "ignoring inherited session metadata; recorder owns the canonical record"
                    );
                }
                RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::ItemCompleted(event)) => {
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
                RolloutItem::ToolManifest(manifest) => {
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
                #[cfg(test)]
                write_fault: None,
                #[cfg(test)]
                append_transaction_count: 0,
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
                let RolloutItem::ToolManifest(manifest) = &mut captured.item else {
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
                tokio::task::spawn_blocking(codex_utils_build_info::executable_sha256)
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
            if let RolloutItem::EventMsg(
                codex_protocol::protocol::EventMsg::ThreadSettingsApplied(settings),
            ) = &captured.item
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
            .chain(compacted.iter().map(|captured| captured.as_ref()))
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
    mut rx: mpsc::Receiver<RolloutCmd>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    known_repository_context: Option<Option<RepositoryContext>>,
    rollout_path: PathBuf,
    tool_manifests: crate::ToolManifestDictionary,
    writer_task: Arc<RolloutWriterTask>,
) -> std::io::Result<()> {
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
            RolloutCmd::AddItems {
                items,
                flush_if_materialized,
                accepted,
            } => {
                state.add_items(items);
                if let Some(accepted) = accepted {
                    let _ = accepted.send(());
                }
                let mut flush_if_materialized = flush_if_materialized;
                for _ in 1..32 {
                    match rx.try_recv() {
                        Ok(RolloutCmd::AddItems { items, flush_if_materialized: flush, accepted }) => {
                            state.add_items(items);
                            flush_if_materialized |= flush;
                            if let Some(accepted) = accepted {
                                let _ = accepted.send(());
                            }
                        }
                        Ok(barrier) => {
                            next_command = Some(barrier);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if flush_if_materialized {
                    state.flush_if_materialized().await;
                }
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
            RolloutCmd::Maintenance { ack, decision } => {
                let materialized = !state.is_deferred();
                if materialized && let Err(error) = state.flush().await {
                    if state.retry_blocked_error.is_some() {
                        let _ = ack.send(Err(clone_io_error(&error)));
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
                    MaintenanceDecision::RestoreFailed => {
                        return Err(IoError::other("rollout maintenance restoration failed"));
                    }
                    MaintenanceDecision::Resume if materialized => {
                        // Restore must have succeeded: never create a new file.
                        let (_, writer) = open_rollout_for_append(&state.rollout_path).await?;
                        state.residency = Some((
                            Arc::clone(&writer._append_lock),
                            Arc::clone(&writer._payload_retention),
                        ));
                        state.writer = Some(writer);
                    }
                    MaintenanceDecision::Resume => {}
                }
            }
            #[cfg(test)]
            RolloutCmd::Pause { entered, resume } => {
                let _ = entered.send(());
                let _ = resume.await;
            }
        }
        if let Some(message) = state.retry_blocked_error.as_ref() {
            let pending_records = state.pending_items.len();
            let err = IoError::other(format!(
                "{message}; {pending_records} buffered rollout records could not be confirmed persisted"
            ));
            error!(pending_records, path = %state.rollout_path.display(), "rollout writer stopped with unconfirmed buffered records");
            writer_task.mark_failed(&err);
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

async fn open_rollout_for_append(path: &Path) -> std::io::Result<(PathBuf, JsonlWriter)> {
    let (path, writer, _) = open_rollout_for_append_with_replay(path, |_, _| {}).await?;
    Ok((path, writer))
}

type AppendReplayIntegrity = (Option<ThreadId>, usize, usize, usize);

async fn open_rollout_for_append_with_replay<F>(
    path: &Path,
    visit: F,
) -> std::io::Result<(PathBuf, JsonlWriter, Option<AppendReplayIntegrity>)>
where
    F: FnMut(RolloutItem, usize),
{
    let path = path.to_path_buf();
    let lock_path = path.clone();
    let (payload_retention, append_lock, snapshot) = tokio::task::spawn_blocking(move || {
        // Same order as ordinary publication: payload lease, representation,
        // then byte writer. Keep all three across preflight and materialization.
        let retention = crate::payload_artifact::lock_publication(&lock_path)?;
        let append = compression::lock_rollout_before_append_blocking(&lock_path)?;
        let snapshot = compression::lock_rollout_for_write_blocking(&lock_path)?;
        Ok::<_, IoError>((retention, append, snapshot))
    })
    .await
    .map_err(IoError::other)??;
    let payload_sync_owner = crate::payload_artifact::PayloadSyncOwner::new(&path);
    // Collection and append preflight share this one verified observation,
    // while all publication/representation/byte-write owners remain held.
    let integrity = match RolloutRecorder::for_each_rollout_item_with_integrity(&path, true, visit).await {
        // A live owner can reopen an empty file after an interrupted initial
        // write. There is no unknown semantic record to fence in that case.
        Err(error)
            if error.kind() == ErrorKind::InvalidData
                && error.to_string() == "empty session file" => None,
        result => Some(result?),
    };
    let (path, writer) = tokio::task::spawn_blocking(move || {
        let path = compression::materialize_rollout_for_append_locked(&path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)?;
        ensure_rollout_is_newline_terminated(&mut file)?;
        let file = LockedRolloutFile {
            path: path.clone(),
            file,
            payload_sync_owner,
            append_lock,
            payload_retention,
        };
        drop(snapshot);
        Ok::<_, IoError>((path, file.into_jsonl_writer()))
    })
    .await
    .map_err(IoError::other)??;
    Ok((path, writer, integrity))
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
    #[cfg(test)]
    write_fault: Option<JsonlWriteFault>,
    #[cfg(test)]
    append_transaction_count: usize,
}

#[derive(Debug, Eq, PartialEq)]
enum FailedAppendRecovery {
    Committed,
    RolledBack,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum JsonlWriteFault {
    Partial(usize),
    Complete,
    Unexpected,
}

#[derive(serde::Serialize)]
struct RolloutLineRef<'a> {
    timestamp: String,
    format_version: u32,
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
            item,
        })
    }

    fn write_jsonl(&self, mut writer: impl Write) -> std::io::Result<()> {
        serde_json::to_writer(&mut writer, self)?;
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
        let mut bytes = Vec::new();
        for captured in rollout_items {
            if !crate::payload_artifact::is_artifact_candidate(&captured.item) {
                Self::serialize_rollout_item(&mut bytes, &captured.item, captured.captured_at)?;
                continue;
            }
            let line = RolloutLineRef::new(&captured.item, captured.captured_at)?;
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
        self.write_transaction(&bytes, append_start).await
    }

    fn serialize_rollout_item(
        bytes: &mut Vec<u8>,
        rollout_item: &RolloutItem,
        captured_at: OffsetDateTime,
    ) -> std::io::Result<()> {
        RolloutLineRef::new(rollout_item, captured_at)?.write_jsonl(bytes)
    }

    async fn write_transaction(&mut self, bytes: &[u8], append_start: u64) -> std::io::Result<u64> {
        #[cfg(test)]
        {
            self.append_transaction_count += 1;
        }
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
        #[cfg(test)]
        if let Some(fault) = self.write_fault.take() {
            match fault {
                JsonlWriteFault::Partial(max_bytes) => {
                    let partial_len = max_bytes.min(line.len().saturating_sub(1));
                    self.file.write_all(&line[..partial_len]).await?;
                }
                JsonlWriteFault::Complete => {
                    self.file.write_all(line).await?;
                }
                JsonlWriteFault::Unexpected => {
                    self.file.write_all(b"unexpected bytes").await?;
                }
            }
            self.file.flush().await?;
            return Err(IoError::other("injected rollout append failure"));
        }

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

impl From<codex_state::ThreadsPage> for ThreadsPage {
    fn from(db_page: codex_state::ThreadsPage) -> Self {
        let codex_state::ThreadsPage {
            items,
            parent_thread_ids,
            next_anchor,
            num_scanned_rows,
        } = db_page;
        let items = items
            .into_iter()
            .map(|item| {
                let parent_thread_id = parent_thread_ids.get(&item.id).copied();
                thread_item_from_state_metadata(item, parent_thread_id)
            })
            .collect();
        Self {
            items,
            next_cursor: next_anchor.map(Into::into),
            num_scanned_files: num_scanned_rows,
            reached_scan_cap: false,
        }
    }
}

fn thread_item_from_state_metadata(
    item: codex_state::ThreadMetadata,
    parent_thread_id: Option<ThreadId>,
) -> ThreadItem {
    ThreadItem {
        path: item.rollout_path,
        thread_id: Some(item.id),
        first_user_message: item.first_user_message,
        title: Some(item.title),
        preview: item.preview,
        cwd: Some(item.cwd),
        git_branch: item.git_branch,
        git_sha: item.git_sha,
        git_origin_url: item.git_origin_url,
        source: Some(
            serde_json::from_str(item.source.as_str())
                .or_else(|_| serde_json::from_value(Value::String(item.source)))
                .unwrap_or(SessionSource::Unknown),
        ),
        history_mode: item.history_mode,
        parent_thread_id,
        agent_nickname: item.agent_nickname,
        agent_role: item.agent_role,
        model_provider: Some(item.model_provider),
        cli_version: Some(item.cli_version),
        created_at: Some(item.created_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        updated_at: Some(item.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
        recency_at: Some(item.recency_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
    }
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recorder_compat_tests.rs"]
mod compatibility_tests;
