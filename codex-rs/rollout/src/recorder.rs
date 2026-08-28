//! Persist Codex session rollouts (.jsonl) so sessions can be replayed or inspected later.

use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::Error as IoError;
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
use super::metadata;
use super::session_index::find_thread_names_by_ids;
use crate::config::RolloutConfigView;
use crate::state_integration;
use crate::state_integration::StateDbHandle;
use codex_git_utils::RepositoryContext;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::protocol::CURRENT_ROLLOUT_FORMAT_VERSION;
use codex_protocol::protocol::EventMsg;
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
use codex_utils_absolute_path as path_utils;

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

enum RolloutCmd {
    AddItems {
        items: Vec<RolloutItem>,
        flush_if_materialized: bool,
        accepted: Option<oneshot::Sender<()>>,
    },
    Persist {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    /// Ensure all prior writes are processed; respond when flushed.
    Flush {
        ack: oneshot::Sender<std::io::Result<()>>,
    },
    Shutdown {
        ack: oneshot::Sender<std::io::Result<()>>,
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
    lifecycle: AtomicU8,
    /// Serializes the lifecycle check with command enqueue across recorder
    /// clones so no command can be accepted behind Shutdown.
    enqueue_gate: tokio::sync::Semaphore,
}

const WRITER_ACTIVE: u8 = 0;
const WRITER_SHUTTING_DOWN: u8 = 1;
const WRITER_SHUT_DOWN: u8 = 2;
const WRITER_FAILED: u8 = 3;

impl RolloutWriterTask {
    /// Create task observability state before spawning the writer.
    fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            terminal_failure: Mutex::new(None),
            lifecycle: AtomicU8::new(WRITER_ACTIVE),
            enqueue_gate: tokio::sync::Semaphore::new(1),
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

fn warn_thread_list_db_fallback() {
    tracing::warn!(
        operation = "list_threads",
        reason = "db_error",
        "state DB listing failed; using filesystem rollout scan"
    );
}

impl RolloutRecorder {
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
                    codex_home,
                    page_size.saturating_mul(2),
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
        for item in &fs_page.items {
            state_integration::reconcile_rollout(
                state_db_ctx.as_deref(),
                item.path.as_path(),
                default_provider,
                /*builder*/ None,
                &[],
                Some(archived),
                /*new_thread_memory_mode*/ None,
            )
            .await;
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
                        /*new_thread_memory_mode*/ None,
                    )
                    .await;
                }
                let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
                return Ok(overlay_thread_item_metadata_from_state_db(
                    state_db_ctx.as_deref(),
                    page,
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
                        /*new_thread_memory_mode*/ None,
                    )
                    .await;
                }
                if sort_key == ThreadSortKey::RecencyAt {
                    let page =
                        page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
                    return Ok(overlay_thread_item_metadata_from_state_db(
                        state_db_ctx.as_deref(),
                        page,
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
                )
                .await);
            }
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            return Ok(
                overlay_thread_item_metadata_from_state_db(state_db_ctx.as_deref(), page).await,
            );
        }
        if listing_has_metadata_filters {
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            codex_state::record_fallback(
                "list_threads",
                "db_error",
                /*telemetry_override*/ None,
            );
            return Ok(
                overlay_thread_item_metadata_from_state_db(state_db_ctx.as_deref(), page).await,
            );
        }
        // If SQLite listing still fails, return the filesystem page rather than failing the list.
        warn_thread_list_db_fallback();
        codex_state::record_fallback("list_threads", "db_error", /*telemetry_override*/ None);
        Ok(page_from_filesystem_scan(
            fs_page,
            sort_direction,
            page_size,
            sort_key,
        ))
    }

    fn db_hit_needs_reconciliation(
        filesystem_thread_ids: &HashSet<ThreadId>,
        db_thread_id: ThreadId,
    ) -> bool {
        !filesystem_thread_ids.contains(&db_thread_id)
    }

    /// Find the newest recorded thread path, optionally filtering to a matching cwd.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_latest_thread_path(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        default_provider: &str,
        filter_cwd: Option<&Path>,
    ) -> std::io::Result<Option<PathBuf>> {
        let codex_home = config.codex_home();
        let mut fallback_reason = state_db_ctx.is_none().then_some("db_unavailable");
        if state_db_ctx.is_some() {
            let mut db_cursor = cursor.cloned();
            loop {
                let Some(db_page) = state_integration::list_threads_db(
                    state_db_ctx.as_deref(),
                    codex_home,
                    page_size,
                    db_cursor.as_ref(),
                    sort_key,
                    SortDirection::Desc,
                    allowed_sources,
                    model_providers,
                    /*cwd_filters*/ None,
                    /*relation_filter*/ None,
                    /*archived*/ false,
                    /*search_term*/ None,
                )
                .await
                else {
                    fallback_reason = Some("db_error");
                    break;
                };
                if let Some(path) =
                    select_resume_path_from_db_page(&db_page, filter_cwd, default_provider).await
                {
                    return Ok(Some(path));
                }
                db_cursor = db_page.next_anchor.map(Into::into);
                if db_cursor.is_none() {
                    fallback_reason = Some("missing_row");
                    break;
                }
            }
        }
        if let Some(reason) = fallback_reason {
            codex_state::record_fallback(
                "find_latest_thread_path",
                reason,
                /*telemetry_override*/ None,
            );
        }

        let mut cursor = cursor.cloned();
        loop {
            let page = get_threads(
                codex_home,
                page_size,
                cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                /*cwd_filters*/ None,
                default_provider,
            )
            .await?;
            if let Some(path) = select_resume_path(&page, filter_cwd, default_provider).await {
                return Ok(Some(path));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(None);
            }
        }
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
                    memory_mode: (!config.generate_memories()).then_some("disabled".to_string()),
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
                let tool_manifests = Self::existing_tool_manifests(path.as_path()).await?;
                let (path, writer) = open_rollout_for_append(path.as_path()).await?;
                (Some(writer), None, path, None, tool_manifests)
            }
        };

        // Clone the cwd for the spawned task to collect git info asynchronously
        let cwd = config.cwd().to_path_buf();

        // A reasonably-sized bounded channel. If the buffer fills up the send
        // future will yield, which is fine – we only need to ensure we do not
        // perform *blocking* I/O on the caller's thread.
        let (tx, rx) = mpsc::channel::<RolloutCmd>(256);
        // Spawn a Tokio task that owns the file handle and performs async
        // writes. Using `tokio::fs::File` keeps everything on the async I/O
        // driver instead of blocking the runtime.
        let writer_task = Arc::new(RolloutWriterTask::new());
        let writer_task_for_spawn = Arc::clone(&writer_task);
        let rollout_path_for_spawn = rollout_path.clone();
        let handle = tokio::task::spawn(async move {
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

    /// Queue canonical items in writer order without forcing the writer to flush. A later
    /// persist/flush/shutdown command is the durability barrier for the queued prefix.
    pub async fn record_canonical_items_ordered(
        &self,
        items: &[RolloutItem],
    ) -> std::io::Result<()> {
        self.record_canonical_items_with_flush(items, false, true)
            .await
    }

    async fn record_canonical_items_with_flush(
        &self,
        items: &[RolloutItem],
        flush_if_materialized: bool,
        wait_for_acceptance: bool,
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
        let (accepted, acceptance) = if wait_for_acceptance {
            let (accepted, acceptance) = oneshot::channel();
            (Some(accepted), Some(acceptance))
        } else {
            (None, None)
        };
        let result = self
            .tx
            .send(RolloutCmd::AddItems {
                items: items.to_vec(),
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
        let enqueue_guard = self
            .writer_task
            .enqueue_gate
            .acquire()
            .await
            .map_err(|_| IoError::other("rollout enqueue gate is closed"))?;
        self.writer_task.ensure_active("flush the rollout")?;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Flush { ack: tx })
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

    pub async fn load_rollout_items(
        path: &Path,
    ) -> std::io::Result<(Vec<RolloutItem>, Option<ThreadId>, usize)> {
        let mut items = Vec::new();
        let (thread_id, parse_errors) =
            Self::for_each_rollout_item(path, |item| items.push(item)).await?;
        tracing::debug!(
            "Resumed rollout with {} items, thread ID: {:?}, parse errors: {}",
            items.len(),
            thread_id,
            parse_errors,
        );
        Ok((items, thread_id, parse_errors))
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
        mut visit: F,
    ) -> std::io::Result<(Option<ThreadId>, usize)>
    where
        F: FnMut(RolloutItem, usize),
    {
        trace!("Resuming rollout from {path:?}");
        let mut thread_id: Option<ThreadId> = None;
        let mut parse_errors = 0usize;
        let mut reader = compression::open_rollout_line_reader(path).await?;
        let mut saw_non_empty_line = false;
        let mut record_number = 0usize;
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            saw_non_empty_line = true;
            record_number = record_number.saturating_add(1);
            let mut v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse line as JSON: {line:?}, error: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                    continue;
                }
            };
            let is_v0 = match v.get("format_version") {
                None => true,
                Some(Value::Number(version)) => version.as_u64() == Some(0),
                Some(_) => false,
            };
            if is_v0 && migrate_v0_ghost_snapshot_rollout_line(&mut v) {
                trace!("skipping legacy ghost_snapshot rollout line");
                continue;
            }

            if thread_id.is_none() {
                // Validate the small compatibility discriminator before consuming the JSON value.
                // This preserves the unknown-mode guard without cloning the whole rollout line.
                reject_unknown_thread_history_mode(&v)?;
            }

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
                    trace!("failed to parse rollout line: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                }
            }
        }
        if !saw_non_empty_line {
            return Err(IoError::other("empty session file"));
        }

        Ok((thread_id, parse_errors))
    }

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
        let mut reader = compression::open_rollout_line_reader(path).await?;
        while let Some(line) = reader.next_line().await? {
            let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(line.trim()) else {
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
        Ok((has_session_meta, manifests))
    }

    pub async fn get_rollout_history(path: &Path) -> std::io::Result<InitialHistory> {
        let (items, thread_id, _parse_errors) = Self::load_rollout_items(path).await?;
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
        .map_err(|err| IoError::other(format!("invalid session metadata history_mode: {err}")))
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
            filter_thread_items_by_search_term(codex_home, &mut page.items, Some(search_term))
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
    let scan_page_size = search_term.map_or(page_size, |_| usize::MAX);
    let mut page = if archived {
        get_threads_in_root_ascending(
            codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
            scan_page_size,
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
        .await?
    } else {
        get_threads_ascending(
            codex_home,
            scan_page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
        )
        .await?
    };

    filter_thread_items_by_search_term(codex_home, &mut page.items, search_term).await?;
    let more_matches_available =
        page.next_cursor.is_some() || page.items.len() > page_size || page.reached_scan_cap;
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
    codex_home: &Path,
    items: &mut Vec<ThreadItem>,
    search_term: Option<&str>,
) -> std::io::Result<()> {
    let Some(search_term) = search_term else {
        return Ok(());
    };

    // The file-backed fallback only has the thread title in the sidecar session index.
    // Match the SQLite path's title substring filter so search pagination behaves the same
    // whether the state DB is available or not.
    let thread_ids = items
        .iter()
        .filter_map(|item| item.thread_id)
        .collect::<HashSet<_>>();
    let thread_names = find_thread_names_by_ids(codex_home, &thread_ids).await?;
    items.retain(|item| {
        item.thread_id
            .and_then(|thread_id| thread_names.get(&thread_id))
            .is_some_and(|title| title.contains(search_term))
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
    append_lock: compression::RolloutAppendLock,
}

impl LockedRolloutFile {
    fn into_jsonl_writer(self) -> JsonlWriter {
        JsonlWriter {
            path: self.path,
            file: tokio::fs::File::from_std(self.file),
            _append_lock: self.append_lock,
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
        append_lock,
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
    deferred_log_file_info: Option<LogFileInfo>,
    pending_items: Vec<RolloutItem>,
    meta: Option<SessionMeta>,
    cwd: PathBuf,
    known_repository_context: Option<Option<RepositoryContext>>,
    rollout_path: PathBuf,
    last_logged_error: Option<String>,
    retry_blocked_error: Option<String>,
    tool_manifests: crate::ToolManifestDictionary,
    pending_token_count: Option<RolloutItem>,
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
        Self {
            writer,
            deferred_log_file_info,
            pending_items: Vec::new(),
            meta,
            cwd,
            known_repository_context,
            rollout_path,
            last_logged_error: None,
            retry_blocked_error: None,
            tool_manifests,
            pending_token_count: None,
        }
    }

    fn add_items(&mut self, items: Vec<RolloutItem>) {
        for item in items {
            match item {
                // The recorder owns the single canonical metadata slot. Inherited history must
                // never append another session_meta record.
                RolloutItem::SessionMeta(_) => {}
                RolloutItem::ToolManifest(manifest) => {
                    match self.tool_manifests.encode_item(&manifest) {
                        Ok(manifest) => {
                            self.pending_items.push(RolloutItem::ToolManifest(manifest))
                        }
                        Err(err) => {
                            tracing::warn!(%err, "failed to encode tool manifest; preserving input record");
                            self.pending_items.push(RolloutItem::ToolManifest(manifest));
                        }
                    }
                }
                RolloutItem::EventMsg(EventMsg::TokenCount(_)) => {
                    self.pending_token_count = Some(item);
                }
                RolloutItem::EventMsg(
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnStarted(_),
                ) => {
                    if let Some(token_count) = self.pending_token_count.take() {
                        self.pending_items.push(token_count);
                    }
                    self.pending_items.push(item);
                }
                item => self.pending_items.push(item),
            }
        }
    }

    async fn flush_if_materialized(&mut self) {
        if self.is_deferred() {
            return;
        }
        if let Err(err) = self.flush().await {
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

    async fn shutdown(&mut self) -> std::io::Result<()> {
        if let Some(token_count) = self.pending_token_count.take() {
            self.pending_items.push(token_count);
        }
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
        let file = tokio::task::spawn_blocking(move || open_log_file(path.as_path()))
            .await
            .map_err(IoError::other)??;
        // Multiple recorders for the same newly-created thread can be initialized before any of
        // them materializes the rollout. Re-check under the append lock so a later writer does not
        // append another canonical session_meta or an already-persisted manifest.
        let existing_rollout_state = if self.meta.is_some() && file.file.metadata()?.len() > 0 {
            Some(RolloutRecorder::existing_rollout_state(&file.path).await?)
        } else {
            None
        };
        self.writer = Some(file.into_jsonl_writer());
        self.deferred_log_file_info = None;
        if let Some((has_session_meta, mut persisted_tool_manifests)) = existing_rollout_state {
            if has_session_meta {
                self.meta = None;
            }
            let known_manifests = self.tool_manifests.clone();
            for item in &mut self.pending_items {
                let RolloutItem::ToolManifest(manifest) = item else {
                    continue;
                };
                let Some(full) = known_manifests.manifest(&manifest.hash).cloned() else {
                    continue;
                };
                *manifest = persisted_tool_manifests.encode(manifest.hash.clone(), full);
            }
            self.tool_manifests = persisted_tool_manifests;
        }
        Ok(())
    }

    async fn session_meta_item_if_needed(&self) -> std::io::Result<Option<RolloutItem>> {
        let Some(session_meta) = self.meta.as_ref().cloned() else {
            return Ok(None);
        };
        let git_info = match self.known_repository_context.clone() {
            Some(repository_context) => repository_context.map(|context| context.git_info),
            None if get_git_repo_root(&self.cwd).is_some() => collect_git_info(&self.cwd).await,
            None => None,
        };
        Ok(Some(RolloutItem::SessionMeta(SessionMetaLine {
            meta: session_meta,
            git: git_info,
        })))
    }

    async fn write_pending_once(&mut self) -> std::io::Result<()> {
        self.ensure_writer_open().await?;
        let session_meta_item = self.session_meta_item_if_needed().await?;
        let Some(writer) = self.writer.as_mut() else {
            return Err(IoError::other("rollout writer is not open"));
        };

        let items = session_meta_item
            .iter()
            .chain(self.pending_items.iter())
            .collect::<Vec<_>>();
        if items.is_empty() {
            return Ok(());
        }
        writer.write_rollout_items(&items).await?;
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

    // Process rollout commands
    while let Some(cmd) = rx.recv().await {
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
                if flush_if_materialized {
                    state.flush_if_materialized().await;
                }
            }
            RolloutCmd::Persist { ack } => {
                let _ = ack.send(state.persist().await);
            }
            RolloutCmd::Flush { ack } => {
                let _ = ack.send(state.flush().await);
            }
            RolloutCmd::Shutdown { ack } => match state.shutdown().await {
                Ok(()) => {
                    writer_task.finish_shutdown(true);
                    let _ = ack.send(Ok(()));
                    break;
                }
                Err(err) => {
                    writer_task.finish_shutdown(false);
                    let _ = ack.send(Err(err));
                }
            },
            #[cfg(test)]
            RolloutCmd::Pause { entered, resume } => {
                let _ = entered.send(());
                let _ = resume.await;
            }
        }
    }

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
    let (_rollout_path, mut writer) = open_rollout_for_append(rollout_path).await?;
    writer.write_rollout_item(item).await
}

async fn open_rollout_for_append(path: &Path) -> std::io::Result<(PathBuf, JsonlWriter)> {
    let path = path.to_path_buf();
    let (path, file) = tokio::task::spawn_blocking(move || {
        open_log_file_with_options(path.as_path(), /*create*/ false)
    })
    .await
    .map_err(IoError::other)??;
    Ok((path, file.into_jsonl_writer()))
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

struct JsonlWriter {
    path: PathBuf,
    file: tokio::fs::File,
    _append_lock: compression::RolloutAppendLock,
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
    AfterPartialWrite(usize),
    AfterCompleteWrite,
}

#[derive(serde::Serialize)]
struct RolloutLineRef<'a> {
    timestamp: String,
    format_version: u32,
    #[serde(flatten)]
    item: &'a RolloutItem,
}

impl JsonlWriter {
    async fn write_rollout_item(&mut self, rollout_item: &RolloutItem) -> std::io::Result<()> {
        self.write_rollout_items(&[rollout_item]).await
    }

    async fn write_rollout_items(&mut self, rollout_items: &[&RolloutItem]) -> std::io::Result<()> {
        if rollout_items.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for rollout_item in rollout_items {
            bytes.extend_from_slice(&Self::serialize_rollout_item(rollout_item)?);
        }
        self.write_transaction(&bytes).await
    }

    fn serialize_rollout_item(rollout_item: &RolloutItem) -> std::io::Result<Vec<u8>> {
        let timestamp_format: &[FormatItem] = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        let timestamp = OffsetDateTime::now_utc()
            .format(timestamp_format)
            .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

        let line = RolloutLineRef {
            timestamp,
            format_version: CURRENT_ROLLOUT_FORMAT_VERSION,
            item: rollout_item,
        };
        let mut json = serde_json::to_string(&line)?;
        json.push('\n');
        Ok(json.into_bytes())
    }

    async fn write_transaction(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        {
            self.append_transaction_count += 1;
        }
        let path = self.path.clone();
        let _write_lock = tokio::task::spawn_blocking(move || {
            compression::lock_rollout_for_write_blocking(path.as_path())
        })
        .await
        .map_err(IoError::other)??;
        let append_start = self.file.metadata().await?.len();
        let write_result = self.write_line_bytes(bytes).await;
        let Err(append_error) = write_result else {
            return Ok(());
        };

        match self.recover_failed_append(append_start, bytes).await {
            Ok(FailedAppendRecovery::Committed) => Ok(()),
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
                JsonlWriteFault::AfterPartialWrite(max_bytes) => {
                    let partial_len = max_bytes.min(line.len().saturating_sub(1));
                    self.file.write_all(&line[..partial_len]).await?;
                }
                JsonlWriteFault::AfterCompleteWrite => {
                    self.file.write_all(line).await?;
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

async fn select_resume_path(
    page: &ThreadsPage,
    filter_cwd: Option<&Path>,
    default_provider: &str,
) -> Option<PathBuf> {
    match filter_cwd {
        Some(cwd) => {
            for item in &page.items {
                if resume_candidate_matches_cwd(
                    item.path.as_path(),
                    item.cwd.as_deref(),
                    cwd,
                    default_provider,
                )
                .await
                {
                    return Some(item.path.clone());
                }
            }
            None
        }
        None => page.items.first().map(|item| item.path.clone()),
    }
}

async fn resume_candidate_matches_cwd(
    rollout_path: &Path,
    cached_cwd: Option<&Path>,
    cwd: &Path,
    default_provider: &str,
) -> bool {
    let Ok((items, _, _)) = RolloutRecorder::load_rollout_items(rollout_path).await else {
        return false;
    };
    if let Some(latest_turn_context_cwd) = items.iter().rev().find_map(|item| match item {
        RolloutItem::TurnContext(turn_context) => Some(&turn_context.cwd),
        RolloutItem::SessionMeta(_)
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::EventMsg(_) => None,
    }) {
        return cwd_matches(latest_turn_context_cwd.as_path(), cwd);
    }

    if cached_cwd.is_some_and(|session_cwd| cwd_matches(session_cwd, cwd)) {
        return true;
    }

    metadata::extract_metadata_from_rollout(rollout_path, default_provider)
        .await
        .is_ok_and(|outcome| cwd_matches(outcome.metadata.cwd.as_path(), cwd))
}

async fn select_resume_path_from_db_page(
    page: &codex_state::ThreadsPage,
    filter_cwd: Option<&Path>,
    default_provider: &str,
) -> Option<PathBuf> {
    match filter_cwd {
        Some(cwd) => {
            for item in &page.items {
                if resume_candidate_matches_cwd(
                    item.rollout_path.as_path(),
                    Some(item.cwd.as_path()),
                    cwd,
                    default_provider,
                )
                .await
                {
                    return Some(item.rollout_path.clone());
                }
            }
            None
        }
        None => page.items.first().map(|item| item.rollout_path.clone()),
    }
}

fn cwd_matches(session_cwd: &Path, cwd: &Path) -> bool {
    path_utils::paths_match_after_normalization(session_cwd, cwd)
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
