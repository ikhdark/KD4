use crate::CreateProjectParams;
use crate::CreatedProject;
use crate::DeletedProject;
use crate::ListProjectsParams;
use crate::MoveProjectParams;
use crate::ProjectMoveOutcome;
mod archive_thread;
mod create_thread;
mod delete_thread;
mod helpers;
mod list_threads;
mod live_writer;
mod projection;
mod projects;
mod read_thread;
mod search_threads;
mod unarchive_thread;
mod update_thread_metadata;

pub use delete_thread::StagedThreadDelete;

#[cfg(test)]
mod audit34_tests;
#[cfg(test)]
mod test_support;

#[cfg(test)]
tokio::task_local! {
    static TEST_APPEND_HANDOFF_PAUSE: Arc<(tokio::sync::Notify, tokio::sync::Notify)>;
}

use codex_git_utils::RepositoryContext;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutRecorder;
use codex_rollout::StateDbHandle;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LoadThreadHistoryParams;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::SearchThreadsParams;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::UpdateProjectParams;
use crate::UpdateThreadMetadataParams;
use crate::UpdatedProject;

/// Local filesystem/SQLite-backed implementation of [`ThreadStore`].
///
/// Local storage has two compatibility surfaces. Rollout JSONL files are the
/// durable replay format and remain readable without SQLite, including older
/// files that encode metadata in `SessionMeta` items and name-index entries.
/// The SQLite state DB, when available, is the queryable metadata index used by
/// list/read paths for fast lookup.
///
/// Live appends still write canonical JSONL history, but append-derived
/// metadata is observed above the store and applied through
/// [`ThreadStore::update_thread_metadata`]. This implementation applies that
/// patch literally to SQLite while keeping the JSONL/name-index compatibility
/// behavior needed for SQLite-less reads, repair, and old local rollout files.
#[derive(Clone)]
pub struct LocalThreadStore {
    pub(super) config: LocalThreadStoreConfig,
    live_recorders: Arc<Mutex<HashMap<ThreadId, LiveRecorderEntry>>>,
    projections: Arc<Mutex<HashMap<ThreadId, projection::SharedLocalThreadProjection>>>,
    state_db: Option<StateDbHandle>,
    deletion_recovery: Arc<tokio::sync::OnceCell<()>>,
    metadata_update_slots: Arc<tokio::sync::Semaphore>,
    metadata_update_order: Arc<std::sync::Mutex<HashMap<ThreadId, std::sync::Weak<Mutex<()>>>>>,
    compression_worker: Option<Arc<codex_rollout::RolloutCompressionWorker>>,
}

struct LiveRecorderEntry {
    recorder: RolloutRecorder,
    // Local rollout files are materialized lazily, but metadata updates can arrive before the
    // canonical SessionMeta is durable. Retain the mode captured when live persistence was opened
    // so missing SQLite rows can still be seeded.
    history_mode: ThreadHistoryMode,
}

/// Process-scoped configuration for local thread storage.
///
/// This describes where local storage lives. New-thread rollout metadata such
/// as cwd and provider is supplied when live persistence is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalThreadStoreConfig {
    pub codex_home: PathBuf,
    pub sqlite_home: PathBuf,
    /// Provider used only when older local metadata does not contain one.
    pub default_model_provider_id: String,
}

impl LocalThreadStoreConfig {
    pub fn from_config(config: &impl codex_rollout::RolloutConfigView) -> Self {
        Self {
            codex_home: config.codex_home().to_path_buf(),
            sqlite_home: config.sqlite_home().to_path_buf(),
            default_model_provider_id: config.model_provider_id().to_string(),
        }
    }
}

impl std::fmt::Debug for LocalThreadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalThreadStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LocalThreadStore {
    pub(super) async fn acquire_writer_lease(
        &self,
        id: ThreadId,
        allow_live: bool,
    ) -> ThreadStoreResult<Arc<std::fs::File>> {
        if allow_live
            && let Ok(recorder) = self.live_recorder(id).await
            && let Some(lease) = recorder.writer_lease()
        {
            return Ok(lease);
        }
        codex_rollout::acquire_thread_writer(&self.config.codex_home, id)
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    ThreadStoreError::Conflict {
                        message: format!(
                            "thread {id} has another live writer or maintenance owner"
                        ),
                    }
                } else {
                    ThreadStoreError::Internal {
                        message: format!("thread {id} ownership: {error}"),
                    }
                }
            })
    }

    /// Create a local store using an already initialized state DB handle.
    pub fn new(config: LocalThreadStoreConfig, state_db: Option<StateDbHandle>) -> Self {
        Self {
            config,
            live_recorders: Arc::new(Mutex::new(HashMap::new())),
            projections: Arc::new(Mutex::new(HashMap::new())),
            state_db,
            deletion_recovery: Arc::new(tokio::sync::OnceCell::new()),
            metadata_update_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            metadata_update_order: Default::default(),
            compression_worker: None,
        }
    }

    /// Retain shared maintenance for exactly this store's lifetime.
    pub fn with_compression_worker(mut self) -> Self {
        self.compression_worker =
            codex_rollout::spawn_rollout_compression_worker(self.config.codex_home.to_path_buf());
        self
    }

    /// Return the state DB handle used by local rollout writers.
    pub async fn state_db(&self) -> Option<StateDbHandle> {
        if let Err(err) = self
            .deletion_recovery
            .get_or_try_init(|| async {
                if delete_thread::recover_staged_deletes(self).await? {
                    Ok(())
                } else {
                    Err(ThreadStoreError::Conflict {
                        message: "rollout maintenance is busy; deletion recovery deferred"
                            .to_string(),
                    })
                }
            })
            .await
        {
            tracing::warn!(%err, "staged deletion recovery incomplete; unresolved files retained for retry");
        }
        self.state_db.clone()
    }

    /// Remove all local persistence for a newly created thread that failed to start.
    ///
    /// This owns the staged rollout deletion and SQLite commit together, including
    /// after its caller stops waiting. Missing or lazily created rollouts are valid.
    pub async fn rollback_created_thread(&self, thread_id: ThreadId) -> ThreadStoreResult<()> {
        delete_thread::rollback_created_thread(self, thread_id).await
    }

    /// Stage local rollout files for deletion while a caller commits related state.
    pub async fn stage_thread_deletes(
        &self,
        thread_ids: &[ThreadId],
    ) -> ThreadStoreResult<StagedThreadDelete<'_>> {
        delete_thread::stage_thread_deletes(self, thread_ids).await
    }

    /// Read a local rollout-backed thread by path.
    pub async fn read_thread_by_rollout_path(
        &self,
        rollout_path: PathBuf,
        include_archived: bool,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        read_thread::read_thread_by_rollout_path(
            self,
            rollout_path,
            include_archived,
            include_history,
        )
        .await
    }

    /// Return the live local rollout path for legacy local-only code paths.
    pub async fn live_rollout_path(&self, thread_id: ThreadId) -> ThreadStoreResult<PathBuf> {
        live_writer::rollout_path(self, thread_id).await
    }

    /// Resolve an existing rollout using the same live/active/archive policy as reads.
    pub async fn resolve_existing_rollout_path(
        &self,
        thread_id: ThreadId,
        include_archived: bool,
    ) -> ThreadStoreResult<Option<PathBuf>> {
        helpers::resolve_rollout_path(
            self,
            thread_id,
            include_archived,
            /*require_materialized*/ true,
        )
        .await
        .map(|resolved| resolved.map(|resolved| resolved.path))
    }

    pub(super) async fn live_recorder(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<RolloutRecorder> {
        self.live_recorders
            .lock()
            .await
            .get(&thread_id)
            .map(|entry| entry.recorder.clone())
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })
    }

    pub(super) async fn ensure_live_recorder_absent(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<()> {
        if self.live_recorders.lock().await.contains_key(&thread_id) {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {thread_id} already has a live local writer"),
            });
        }
        Ok(())
    }

    pub(super) async fn insert_live_recorder(
        &self,
        thread_id: ThreadId,
        recorder: RolloutRecorder,
        history_mode: ThreadHistoryMode,
    ) -> ThreadStoreResult<()> {
        match self.live_recorders.lock().await.entry(thread_id) {
            Entry::Occupied(entry) => Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {} already has a live local writer", entry.key()),
            }),
            Entry::Vacant(entry) => {
                entry.insert(LiveRecorderEntry {
                    recorder,
                    history_mode,
                });
                Ok(())
            }
        }
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        if let Ok(rollout_path) = live_writer::rollout_path(self, params.thread_id).await {
            if !params.include_archived
                && helpers::rollout_path_is_archived(
                    self.config.codex_home.as_path(),
                    rollout_path.as_path(),
                )
            {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {} is archived", params.thread_id),
                });
            }
            return read_thread::read_thread_by_rollout_path(
                self,
                rollout_path,
                /*include_archived*/ true,
                /*include_history*/ true,
            )
            .await?
            .history
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("failed to load history for thread {}", params.thread_id),
            });
        }

        read_thread::read_thread(
            self,
            ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: true,
            },
        )
        .await?
        .history
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!("failed to load history for thread {}", params.thread_id),
        })
    }

    async fn read_thread_by_rollout_path_params(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        read_thread::read_thread_by_rollout_path(
            self,
            params.rollout_path,
            params.include_archived,
            params.include_history,
        )
        .await
    }

    async fn flush_thread_with_durability(
        &self,
        thread_id: ThreadId,
        durable: bool,
    ) -> ThreadStoreResult<()> {
        if let Some(projection) = projection::existing_entry(self, thread_id).await {
            let _operation = projection.acquire_operation().await?;
            live_writer::flush_thread(self, thread_id, durable).await?;
            projection.commit_pending().await?;
        } else {
            live_writer::flush_thread(self, thread_id, durable).await?;
        }
        Ok(())
    }
}

impl ThreadStore for LocalThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            live_writer::initialize(self.clone(), thread_id, |store| async move {
                live_writer::create_thread(&store, params).await?;
                Ok(None)
            })
            .await
        })
    }

    fn create_thread_with_repository_context(
        &self,
        params: CreateThreadParams,
        repository_context: Option<RepositoryContext>,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            live_writer::initialize(self.clone(), thread_id, |store| async move {
                live_writer::create_thread_with_repository_context(
                    &store,
                    params,
                    repository_context,
                )
                .await?;
                Ok(None)
            })
            .await
        })
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            live_writer::initialize(self.clone(), thread_id, |store| async move {
                live_writer::resume_thread(&store, params).await.map(Some)
            })
            .await
        })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            let persisted_items = codex_rollout::persisted_rollout_items(
                params.items.as_slice(),
                ThreadHistoryMode::Legacy,
            );
            let (projection, _operation) =
                projection::initialized_entry_for_append(self, thread_id).await?;
            if persisted_items.is_empty() {
                return Ok(());
            }
            live_writer::append_persisted_items_ordered(self, thread_id, &persisted_items).await?;
            projection.append_pending(persisted_items).await;
            live_writer::flush_thread(self, thread_id, false).await?;
            projection.commit_pending().await
        })
    }

    fn append_items_ordered(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            let persisted_items = codex_rollout::persisted_rollout_items(
                params.items.as_slice(),
                ThreadHistoryMode::Legacy,
            );
            let (projection, _operation) =
                projection::initialized_entry_for_append(self, thread_id).await?;
            live_writer::append_persisted_items_ordered(self, thread_id, &persisted_items).await?;
            if !persisted_items.is_empty() {
                projection.append_pending(persisted_items).await;
            }
            Ok(())
        })
    }

    fn append_persisted_items<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
    ) -> ThreadStoreFuture<'a, ()> {
        Box::pin(async move {
            if items.is_empty() {
                return Ok(());
            }
            let (projection, _operation) =
                projection::initialized_entry_for_append(self, thread_id).await?;
            live_writer::append_persisted_items_ordered(self, thread_id, items).await?;
            projection.append_pending(items.to_vec()).await;
            live_writer::flush_thread(self, thread_id, false).await?;
            projection.commit_pending().await
        })
    }

    fn append_persisted_items_ordered<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
    ) -> ThreadStoreFuture<'a, ()> {
        Box::pin(async move {
            if items.is_empty() {
                return Ok(());
            }
            let (projection, _operation) =
                projection::initialized_entry_for_append(self, thread_id).await?;
            let store = self.clone();
            let items = items.to_vec();
            #[cfg(test)]
            let pause = TEST_APPEND_HANDOFF_PAUSE.try_with(Arc::clone).ok();
            // The operation permit bounds this to one admitted worker per thread.
            // Cancellation may drop the observer, never the queue/projection handoff.
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::append_persisted_items_ordered(&store, thread_id, &items).await?;
                #[cfg(test)]
                if let Some(pause) = pause {
                    pause.0.notify_one();
                    pause.1.notified().await;
                }
                projection.append_pending(items).await;
                Ok(())
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("append handoff failed: {error}"),
            })?
        })
    }

    fn persist_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if let Some(projection) = projection::existing_entry(self, thread_id).await {
                let _operation = projection.acquire_operation().await?;
                live_writer::persist_thread(self, thread_id).await?;
                projection.commit_pending().await?;
            } else {
                live_writer::persist_thread(self, thread_id).await?;
            }
            Ok(())
        })
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(self.flush_thread_with_durability(thread_id, /*durable*/ false))
    }

    fn flush_thread_durable(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(self.flush_thread_with_durability(thread_id, /*durable*/ true))
    }

    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if let Some(projection) = projection::existing_entry(self, thread_id).await {
                let _operation = projection.acquire_operation().await?;
                live_writer::shutdown_thread(self, thread_id).await?;
                projection.commit_pending().await?;
                projection::remove_if_current(self, thread_id, &projection).await;
            } else {
                live_writer::shutdown_thread(self, thread_id).await?;
            }
            Ok(())
        })
    }

    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(live_writer::discard_thread(self, thread_id))
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(LocalThreadStore::load_history(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { read_thread::read_thread(self, params).await })
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(LocalThreadStore::read_thread_by_rollout_path_params(
            self, params,
        ))
    }

    fn supports_projects(&self) -> bool {
        self.state_db.is_some()
    }

    fn list_projects(
        &self,
        params: ListProjectsParams,
    ) -> ThreadStoreFuture<'_, StoredProjectsPage> {
        Box::pin(async move { projects::list_projects(self, params).await })
    }

    fn read_project(&self, project_id: String) -> ThreadStoreFuture<'_, Option<StoredProject>> {
        Box::pin(async move { projects::read_project(self, project_id).await })
    }

    fn create_project(&self, params: CreateProjectParams) -> ThreadStoreFuture<'_, CreatedProject> {
        Box::pin(async move { projects::create_project(self, params).await })
    }

    fn update_project(
        &self,
        params: UpdateProjectParams,
    ) -> ThreadStoreFuture<'_, Option<UpdatedProject>> {
        Box::pin(async move { projects::update_project(self, params).await })
    }

    fn move_project(
        &self,
        params: MoveProjectParams,
    ) -> ThreadStoreFuture<'_, Option<ProjectMoveOutcome>> {
        Box::pin(async move { projects::move_project(self, params).await })
    }

    fn delete_project(&self, project_id: String) -> ThreadStoreFuture<'_, Option<DeletedProject>> {
        Box::pin(async move { projects::delete_project(self, project_id).await })
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async move { list_threads::list_threads(self, params).await })
    }

    fn search_threads(
        &self,
        params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(async move { search_threads::search_threads(self, params).await })
    }

    fn list_turns(&self, params: crate::ListTurnsParams) -> ThreadStoreFuture<'_, crate::TurnPage> {
        Box::pin(async move { projection::list_turns(self, params).await })
    }

    fn list_items(&self, params: crate::ListItemsParams) -> ThreadStoreFuture<'_, crate::ItemPage> {
        Box::pin(async move { projection::list_items(self, params).await })
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { update_thread_metadata::update_thread_metadata(self, params).await })
    }

    fn update_thread_metadata_ack(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(
            async move { update_thread_metadata::update_thread_metadata_ack(self, params).await },
        )
    }

    fn update_thread_metadata_applied(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(update_thread_metadata::update_thread_metadata_applied(
            self, params,
        ))
    }

    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { archive_thread::archive_thread(self, params).await })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { unarchive_thread::unarchive_thread(self, params).await })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { delete_thread::delete_thread(self, params).await })
    }

    fn preflight_delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { delete_thread::preflight_delete_thread(self, params).await })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use codex_protocol::ThreadId;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::MessagePhase;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::AgentMessageEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadHistoryMode;
    use codex_protocol::protocol::TurnCompleteEvent;
    use codex_protocol::protocol::TurnStartedEvent;
    use codex_protocol::protocol::UserMessageEvent;
    use tempfile::TempDir;

    use super::*;
    use crate::InMemoryThreadStore;
    use crate::LiveThread;
    use crate::ThreadPersistenceMetadata;
    use crate::local::test_support::test_config;
    use crate::local::test_support::write_archived_session_file;
    use crate::local::test_support::write_session_file;
    use crate::local::test_support::write_session_file_with_history_mode;

    #[tokio::test]
    async fn live_writer_lifecycle_writes_and_closes() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("load rollout path");

        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("first live write")],
            })
            .await
            .expect("append live item");
        store
            .persist_thread(thread_id)
            .await
            .expect("persist live thread");
        store
            .flush_thread(thread_id)
            .await
            .expect("flush live thread");

        assert_rollout_contains_message(rollout_path.as_path(), "first live write").await;

        store
            .shutdown_thread(thread_id)
            .await
            .expect("shutdown live thread");
        let err = store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("write after shutdown")],
            })
            .await
            .expect_err("shutdown should remove the live thread writer");
        assert!(
            matches!(err, ThreadStoreError::ThreadNotFound { thread_id: missing } if missing == thread_id)
        );
        assert!(
            !store.projections.lock().await.contains_key(&thread_id),
            "a rejected append must not load and retain the closed thread's history"
        );
    }

    #[tokio::test]
    async fn shutdown_evicts_projection_and_next_read_rebuilds_it() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![
                    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: "turn-1".to_string(),
                        trace_id: None,
                        started_at: None,
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                    })),
                    user_message_item("persisted message"),
                    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                        surfaced_result: None,
                        turn_id: "turn-1".to_string(),
                        last_agent_message: None,
                        error: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                        timing: None,
                    })),
                ],
            })
            .await
            .expect("append turn");
        assert!(store.projections.lock().await.contains_key(&thread_id));

        store
            .shutdown_thread(thread_id)
            .await
            .expect("shutdown live thread");
        assert!(!store.projections.lock().await.contains_key(&thread_id));

        let rebuilt = store
            .list_turns(crate::ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor: None,
                page_size: 10,
                sort_direction: crate::SortDirection::Asc,
                items_view: crate::StoredTurnItemsView::Full,
            })
            .await
            .expect("rebuild projection from durable rollout");
        assert_eq!(
            rebuilt
                .turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-1"]
        );
        assert!(store.projections.lock().await.contains_key(&thread_id));
    }

    #[tokio::test]
    async fn unloaded_turn_pages_keep_a_bounded_projection_cache() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let live_thread_id = ThreadId::default();
        store
            .create_thread(create_thread_params(live_thread_id))
            .await
            .expect("create live thread");
        let unloaded_thread_ids = (0..6)
            .map(|index| {
                let uuid = uuid::Uuid::from_u128(500 + index);
                write_session_file(home.path(), &format!("2025-01-03T12-00-0{index}"), uuid)
                    .expect("session file");
                ThreadId::from_string(&uuid.to_string()).expect("valid thread id")
            })
            .collect::<Vec<_>>();

        for thread_id in &unloaded_thread_ids {
            store
                .list_turns(crate::ListTurnsParams {
                    thread_id: *thread_id,
                    include_archived: false,
                    cursor: None,
                    page_size: 10,
                    sort_direction: crate::SortDirection::Asc,
                    items_view: crate::StoredTurnItemsView::Summary,
                })
                .await
                .expect("list unloaded thread turns");
        }

        let projections = store.projections.lock().await;
        assert!(
            projections.contains_key(&live_thread_id),
            "live projections must never be evicted"
        );
        let retained = unloaded_thread_ids
            .iter()
            .filter(|thread_id| projections.contains_key(thread_id))
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(retained, unloaded_thread_ids[2..].to_vec());
    }

    #[tokio::test]
    async fn raw_append_items_does_not_update_sqlite_metadata() {
        // This pins the ThreadStore contract: raw appends are history-only. Callers that need
        // metadata updates must use LiveThread or call update_thread_metadata explicitly.
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = LocalThreadStore::new(config, Some(runtime.clone()));
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("raw append")],
            })
            .await
            .expect("append raw item");
        store.flush_thread(thread_id).await.expect("flush thread");

        assert_eq!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("sqlite metadata read"),
            None
        );
    }

    #[tokio::test]
    async fn local_projection_pages_durable_turns_and_items_incrementally() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();
        let turn_items = |turn_id: &str, message: &str| {
            vec![
                RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: turn_id.to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                })),
                user_message_item(message),
                RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                    surfaced_result: None,
                    turn_id: turn_id.to_string(),
                    last_agent_message: None,
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                })),
            ]
        };

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: turn_items("turn-1", "first"),
            })
            .await
            .expect("append durable turn");
        store
            .append_items_ordered(AppendThreadItemsParams {
                thread_id,
                items: turn_items("turn-2", "second"),
            })
            .await
            .expect("append ordered turn");

        let before_flush = store
            .list_turns(crate::ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor: None,
                page_size: 10,
                sort_direction: crate::SortDirection::Asc,
                items_view: crate::StoredTurnItemsView::Full,
            })
            .await
            .expect("list durable turns before flush");
        assert_eq!(
            before_flush
                .turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-1"]
        );

        store.flush_thread(thread_id).await.expect("flush thread");
        let first_page = store
            .list_turns(crate::ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor: None,
                page_size: 1,
                sort_direction: crate::SortDirection::Asc,
                items_view: crate::StoredTurnItemsView::Full,
            })
            .await
            .expect("list first turn page");
        assert_eq!(first_page.turns[0].turn_id, "turn-1");
        let second_page = store
            .list_turns(crate::ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor: first_page.next_cursor,
                page_size: 1,
                sort_direction: crate::SortDirection::Asc,
                items_view: crate::StoredTurnItemsView::Full,
            })
            .await
            .expect("list second turn page");
        assert_eq!(second_page.turns[0].turn_id, "turn-2");
        let materialized_turn: codex_app_server_protocol::Turn = serde_json::from_slice(
            second_page.turns[0]
                .metadata_json
                .as_deref()
                .expect("materialized turn"),
        )
        .expect("deserialize materialized turn");
        assert_eq!(materialized_turn.id, "turn-2");

        let item_page = store
            .list_items(crate::ListItemsParams {
                thread_id,
                turn_id: Some("turn-2".to_string()),
                include_archived: false,
                cursor: None,
                page_size: 10,
                sort_direction: crate::SortDirection::Asc,
            })
            .await
            .expect("list projected turn items");
        assert_eq!(item_page.items.len(), 1);
        let item: codex_app_server_protocol::ThreadItem =
            serde_json::from_slice(item_page.items[0].materialized_thread_item_json.as_slice())
                .expect("deserialize projected item");
        assert!(matches!(
            item,
            codex_app_server_protocol::ThreadItem::UserMessage { .. }
        ));
    }

    #[tokio::test]
    async fn live_thread_observes_appended_items_into_sqlite_metadata() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create live thread");

        live_thread
            .append_items(&[user_message_item("observed append")])
            .await
            .expect("append observed item");
        live_thread.flush().await.expect("flush thread");

        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("observed append")
        );
        assert_eq!(metadata.preview.as_deref(), Some("observed append"));
        assert_eq!(metadata.title, "observed append");
    }

    #[tokio::test]
    async fn ordered_live_append_defers_rollout_and_metadata_until_flush() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create live thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("load rollout path");

        live_thread
            .append_items_ordered(&[user_message_item("ordered append")])
            .await
            .expect("queue ordered append");

        assert!(
            !tokio::fs::try_exists(&rollout_path)
                .await
                .expect("stat rollout"),
            "ordered append must not materialize the rollout before the terminal barrier"
        );
        assert_eq!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("sqlite metadata read"),
            None,
            "ordered append must leave metadata pending"
        );

        live_thread.flush().await.expect("flush terminal barrier");
        assert_rollout_contains_message(rollout_path.as_path(), "ordered append").await;
        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("metadata after terminal barrier");
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("ordered append")
        );
    }

    #[tokio::test]
    async fn live_thread_output_advances_updated_at_but_not_recency_at() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store, create_thread_params(thread_id))
            .await
            .expect("create live thread");

        live_thread
            .append_items(&[user_message_item("start thread")])
            .await
            .expect("append initial user message");
        live_thread.flush().await.expect("flush thread");
        let mut before_turn_start = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");

        // SQLite stores milliseconds; consecutive operations need not cross a clock tick.
        let baseline = chrono::DateTime::<chrono::Utc>::from_timestamp(1_704_067_200, 0)
            .expect("baseline timestamp");
        before_turn_start.updated_at = baseline;
        runtime
            .upsert_thread_preserving_timestamps(&before_turn_start)
            .await
            .expect("seed timestamp baseline");
        let started_at = before_turn_start.recency_at.timestamp() + 2;

        live_thread
            .append_items(&[RolloutItem::EventMsg(EventMsg::TurnStarted(
                TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    trace_id: None,
                    started_at: Some(started_at),
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                },
            ))])
            .await
            .expect("append turn start");
        live_thread.flush().await.expect("flush thread");
        let mut after_turn_start = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");
        assert!(after_turn_start.recency_at > before_turn_start.recency_at);
        assert_eq!(
            after_turn_start.recency_at,
            chrono::DateTime::<chrono::Utc>::from_timestamp(started_at, 0).expect("turn timestamp")
        );
        after_turn_start.updated_at = baseline;
        runtime
            .upsert_thread_preserving_timestamps(&after_turn_start)
            .await
            .expect("seed output timestamp baseline");

        live_thread
            .append_items(&[
                RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                    message: "commentary".to_string(),
                    phase: Some(MessagePhase::Commentary),
                })),
                RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "call-1".to_string(),
                    output: FunctionCallOutputPayload::from_text("tool output".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                }),
                RolloutItem::EventMsg(EventMsg::TokenCount(
                    codex_protocol::protocol::TokenCountEvent {
                        info: None,
                        rate_limits: None,
                    },
                )),
                RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                    surfaced_result: None,
                    turn_id: "turn-1".to_string(),
                    last_agent_message: None,
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                })),
            ])
            .await
            .expect("append post-start items");
        live_thread.flush().await.expect("flush thread");
        let completed = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");

        assert!(completed.updated_at > after_turn_start.updated_at);
        assert_eq!(completed.recency_at, after_turn_start.recency_at);
    }

    #[tokio::test]
    async fn live_thread_shutdown_does_not_materialize_empty_thread_metadata() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create live thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("live rollout path");

        live_thread.shutdown().await.expect("shutdown thread");

        assert!(
            !tokio::fs::try_exists(rollout_path.as_path())
                .await
                .expect("rollout path should be checkable")
        );
        assert_eq!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("sqlite metadata read"),
            None
        );
    }

    #[tokio::test]
    async fn live_thread_shutdown_failure_does_not_publish_pending_metadata() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create live thread");
        live_thread
            .append_items_ordered(&[user_message_item("pending shutdown")])
            .await
            .expect("queue ordered append");
        store.fail_next_shutdown().await;

        live_thread
            .shutdown()
            .await
            .expect_err("injected rollout shutdown must fail");

        let calls = store.calls().await;
        assert_eq!(calls.shutdown_thread, 1);
        assert_eq!(
            calls.update_thread_metadata, 0,
            "metadata must not advance if the rollout shutdown barrier fails"
        );
    }

    #[tokio::test]
    async fn live_thread_shutdown_with_buffered_items_materializes_before_metadata_read() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live_thread = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create live thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("live rollout path");

        // Only an ordered append stays buffered; a plain append flushes and applies its
        // metadata before shutdown is ever called.
        live_thread
            .append_items_ordered(&[RolloutItem::EventMsg(EventMsg::TokenCount(
                codex_protocol::protocol::TokenCountEvent {
                    info: None,
                    rate_limits: None,
                },
            ))])
            .await
            .expect("append metadata-only item");
        assert!(
            !tokio::fs::try_exists(rollout_path.as_path())
                .await
                .expect("rollout path should be checkable"),
            "the appended item must still be buffered when shutdown starts"
        );
        assert_eq!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("sqlite metadata read"),
            None,
            "buffered metadata must wait for the shutdown barrier"
        );
        live_thread.shutdown().await.expect("shutdown thread");

        assert!(
            tokio::fs::try_exists(rollout_path.as_path())
                .await
                .expect("rollout path should be checkable")
        );
        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");
        assert_eq!(metadata.rollout_path, rollout_path);
    }

    #[tokio::test]
    async fn live_thread_resume_loads_history_before_observing_metadata() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let uuid = uuid::Uuid::from_u128(401);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path =
            write_session_file(home.path(), "2025-01-03T17-00-00", uuid).expect("session file");
        let live_thread = LiveThread::resume(
            store,
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path),
                history: None,
                include_archived: false,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(home.path().to_path_buf()),
                    model_provider: "different-provider".to_string(),
                },
            },
        )
        .await
        .expect("resume live thread");

        live_thread
            .append_items(&[user_message_item("new live append")])
            .await
            .expect("append after resume");

        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");
        assert_eq!(
            metadata.created_at.to_rfc3339(),
            "2025-01-03T17:00:00+00:00"
        );
        assert_eq!(metadata.model_provider, "test-provider");
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("Hello from user")
        );
    }

    #[tokio::test]
    async fn live_thread_resume_loads_history_from_explicit_external_rollout_path() {
        let home = TempDir::new().expect("temp dir");
        let external_home = TempDir::new().expect("external temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let uuid = uuid::Uuid::from_u128(402);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = write_session_file(external_home.path(), "2025-01-03T17-30-00", uuid)
            .expect("external session file");
        let live_thread = LiveThread::resume(
            store,
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path),
                history: None,
                include_archived: false,
                metadata: ThreadPersistenceMetadata {
                    cwd: Some(home.path().to_path_buf()),
                    model_provider: "different-provider".to_string(),
                },
            },
        )
        .await
        .expect("resume external live thread");

        live_thread
            .append_items(&[user_message_item("new external append")])
            .await
            .expect("append after external resume");

        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("sqlite metadata read")
            .expect("sqlite metadata");
        assert_eq!(
            metadata.created_at.to_rfc3339(),
            "2025-01-03T17:30:00+00:00"
        );
        assert_eq!(metadata.model_provider, "test-provider");
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("Hello from user")
        );
    }

    #[tokio::test]
    async fn create_thread_rejects_missing_cwd() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();
        let mut params = create_thread_params(thread_id);
        params.metadata.cwd = None;

        let err = store
            .create_thread(params)
            .await
            .expect_err("local thread store should require cwd");

        assert!(matches!(
            err,
            ThreadStoreError::InvalidRequest { message }
                if message == "local thread store requires a cwd"
        ));
    }

    #[tokio::test]
    async fn discard_thread_drops_unmaterialized_live_writer() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("load rollout path");
        let recorder = store
            .live_recorder(thread_id)
            .await
            .expect("recorder clone");
        store
            .append_items_ordered(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("accepted but discarded")],
            })
            .await
            .expect("accept buffered item");
        store
            .discard_thread(thread_id)
            .await
            .expect("discard live thread");
        assert!(
            recorder.flush().await.is_err(),
            "discard must stop the writer, not just drop one handle"
        );
        drop(recorder);

        assert!(
            !tokio::fs::try_exists(rollout_path.as_path())
                .await
                .expect("check rollout path")
        );
        let err = store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("write after discard")],
            })
            .await
            .expect_err("discard should remove the live thread writer");
        assert!(
            matches!(err, ThreadStoreError::ThreadNotFound { thread_id: missing } if missing == thread_id)
        );
    }

    #[tokio::test]
    async fn resume_thread_reopens_live_writer_and_appends() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let thread_id = ThreadId::default();

        let first_store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
        first_store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create initial thread");
        first_store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("before resume")],
            })
            .await
            .expect("append initial item");
        first_store
            .persist_thread(thread_id)
            .await
            .expect("persist initial thread");
        first_store
            .flush_thread(thread_id)
            .await
            .expect("flush initial thread");
        let rollout_path = first_store
            .live_rollout_path(thread_id)
            .await
            .expect("load rollout path");
        first_store
            .shutdown_thread(thread_id)
            .await
            .expect("shutdown initial writer");

        let resumed_store = LocalThreadStore::new(config, /*state_db*/ None);
        resumed_store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: None,
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume live thread");
        resumed_store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("after resume")],
            })
            .await
            .expect("append resumed item");
        resumed_store
            .flush_thread(thread_id)
            .await
            .expect("flush resumed thread");

        assert_rollout_contains_message(rollout_path.as_path(), "before resume").await;
        assert_rollout_contains_message(rollout_path.as_path(), "after resume").await;
    }

    #[tokio::test]
    async fn lifecycle_rejects_duplicate_live_writer() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create live thread");

        let err = store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect_err("duplicate live writer should fail");

        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
        assert!(err.to_string().contains("already has a live local writer"));
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("live rollout path");
        let err = store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .expect_err("duplicate live resume should fail");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
        assert!(err.to_string().contains("already has a live local writer"));
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("original writer survives")],
            })
            .await
            .expect("original writer remains usable");
        store
            .persist_thread(thread_id)
            .await
            .expect("persist original writer");
        assert_rollout_contains_message(&rollout_path, "original writer survives").await;
        store
            .shutdown_thread(thread_id)
            .await
            .expect("shutdown original writer");
    }

    #[tokio::test]
    async fn resume_thread_rejects_mismatched_path_and_history_without_opening_writer() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), None);
        let uuid = uuid::Uuid::from_u128(9992);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("id");
        let path = write_session_file(home.path(), "2025-01-04T11-30-00", uuid).expect("rollout");
        let other_path = write_session_file(
            home.path(),
            "2025-01-04T12-30-00",
            uuid::Uuid::from_u128(9993),
        )
        .expect("other rollout");
        let original = std::fs::read(&path).expect("bytes");
        let other_original = std::fs::read(&other_path).expect("other bytes");
        let (other_history, _, _) = RolloutRecorder::load_rollout_items(&other_path)
            .await
            .expect("history");
        for (rollout_path, history) in [
            (other_path.clone(), None),
            (path.clone(), Some(Arc::new(other_history))),
        ] {
            let error = store
                .resume_thread(ResumeThreadParams {
                    thread_id,
                    rollout_path: Some(rollout_path),
                    history,
                    include_archived: true,
                    metadata: thread_metadata(),
                })
                .await
                .expect_err("identity mismatch");
            assert!(matches!(error, ThreadStoreError::InvalidRequest { .. }));
            assert!(store.live_recorders.lock().await.is_empty());
            assert_eq!(std::fs::read(&path).expect("bytes"), original);
            assert_eq!(
                std::fs::read(&other_path).expect("other bytes"),
                other_original
            );
        }
    }

    #[tokio::test]
    async fn resume_thread_rejects_missing_cwd() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = uuid::Uuid::from_u128(407);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = home.path().join("missing-rollout.jsonl");
        let err = store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path),
                history: None,
                include_archived: true,
                metadata: ThreadPersistenceMetadata {
                    cwd: None,
                    model_provider: "test-provider".to_string(),
                },
            })
            .await
            .expect_err("missing cwd should fail");

        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
        assert!(err.to_string().contains("requires a cwd"));
    }

    #[tokio::test]
    async fn load_history_uses_live_writer_rollout_path() {
        let home = TempDir::new().expect("temp dir");
        let external_home = TempDir::new().expect("external temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = uuid::Uuid::from_u128(404);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = write_session_file(external_home.path(), "2025-01-04T10-00-00", uuid)
            .expect("external session file");

        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path),
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume live thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("external history item")],
            })
            .await
            .expect("append live item");
        store
            .flush_thread(thread_id)
            .await
            .expect("flush live thread");

        let history = store
            .load_history(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .expect("load external live history");

        assert!(history.items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "external history item"
            )
        }));
    }

    #[tokio::test]
    async fn read_thread_uses_live_writer_rollout_path_for_external_resume() {
        let home = TempDir::new().expect("temp dir");
        let external_home = TempDir::new().expect("external temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = uuid::Uuid::from_u128(406);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = write_session_file(external_home.path(), "2025-01-04T11-00-00", uuid)
            .expect("external session file");

        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume live thread");

        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: true,
            })
            .await
            .expect("read external live thread");

        assert_eq!(
            thread.rollout_path,
            Some(rollout_path.canonicalize().expect("canonical rollout path"))
        );
        assert!(thread.history.expect("history").items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "Hello from user"
            )
        }));
    }

    #[tokio::test]
    async fn load_history_uses_live_writer_rollout_path_for_archived_source() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = uuid::Uuid::from_u128(405);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = write_archived_session_file(home.path(), "2025-01-04T10-30-00", uuid)
            .expect("archived session file");

        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path),
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume live archived thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("archived live history item")],
            })
            .await
            .expect("append live item");
        store
            .flush_thread(thread_id)
            .await
            .expect("flush live thread");

        let err = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect_err("active-only read should reject archived live thread");
        assert!(matches!(
            err,
            ThreadStoreError::ThreadNotFound {
                thread_id: missing_thread_id
            } if missing_thread_id == thread_id
        ));

        let err = store
            .load_history(LoadThreadHistoryParams {
                thread_id,
                include_archived: false,
            })
            .await
            .expect_err("active-only history should reject archived live thread");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
        assert!(err.to_string().contains("archived"));

        let history = store
            .load_history(LoadThreadHistoryParams {
                thread_id,
                include_archived: true,
            })
            .await
            .expect("load archived live history");

        assert!(history.items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "archived live history item"
            )
        }));
    }

    #[tokio::test]
    async fn read_thread_by_rollout_path_includes_history() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::default();

        store
            .create_thread(create_thread_params(thread_id))
            .await
            .expect("create thread");
        store
            .append_items(AppendThreadItemsParams {
                thread_id,
                items: vec![user_message_item("path read")],
            })
            .await
            .expect("append item");
        store.flush_thread(thread_id).await.expect("flush thread");
        let rollout_path = store
            .live_rollout_path(thread_id)
            .await
            .expect("load rollout path");

        let thread = store
            .read_thread_by_rollout_path(
                rollout_path,
                /*include_archived*/ true,
                /*include_history*/ true,
            )
            .await
            .expect("read thread by rollout path");

        assert_eq!(thread.thread_id, thread_id);
        assert_eq!(thread.history_mode, ThreadHistoryMode::Legacy);
        assert_eq!(
            thread
                .history
                .as_ref()
                .expect("history")
                .items
                .iter()
                .filter_map(|item| match item {
                    RolloutItem::EventMsg(EventMsg::UserMessage(event)) =>
                        Some(event.message.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["path read"]
        );
    }

    #[tokio::test]
    async fn paginated_threads_allow_metadata_reads_and_reject_legacy_history_paths() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = uuid::Uuid::from_u128(408);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = write_session_file_with_history_mode(
            home.path(),
            "2025-01-04T12-00-00",
            uuid,
            ThreadHistoryMode::Paginated,
        )
        .expect("session file");

        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        let thread = store
            .read_thread_by_rollout_path(
                rollout_path.clone(),
                /*include_archived*/ true,
                /*include_history*/ false,
            )
            .await
            .expect("metadata path read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        assert_paginated_threads_unsupported(
            store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .read_thread_by_rollout_path(
                    rollout_path.clone(),
                    /*include_archived*/ true,
                    /*include_history*/ true,
                )
                .await
                .expect_err("full history path read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await
                .expect_err("history load should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .resume_thread(ResumeThreadParams {
                    thread_id,
                    rollout_path: Some(rollout_path),
                    history: None,
                    include_archived: false,
                    metadata: thread_metadata(),
                })
                .await
                .expect_err("resume should fail"),
        );

        let mut create_params = create_thread_params(ThreadId::default());
        create_params.history_mode = ThreadHistoryMode::Paginated;
        assert_paginated_threads_unsupported(
            store
                .create_thread(create_params)
                .await
                .expect_err("paginated create should fail"),
        );
    }

    /// Real live append -> rollout barrier -> SQLite metadata path. Setup and verification
    /// are outside each sample; case elapsed includes shutdown and authenticated reload.
    #[tokio::test]
    #[ignore = "wall-clock benchmark; run explicitly without competing benchmarks"]
    async fn benchmark_live_metadata_projection() {
        const SAMPLES: usize = 20;
        let executable = std::env::current_exe().expect("benchmark executable");
        for index_entries in [0usize, 10_000] {
            for named in [false, true] {
                let home = TempDir::new().expect("temp dir");
                let config = test_config(home.path());
                let runtime = codex_state::StateRuntime::init(
                    config.sqlite_home.clone(),
                    config.default_model_provider_id.clone(),
                )
                .await
                .expect("state db");
                let store = Arc::new(LocalThreadStore::new(config.clone(), Some(runtime.clone())));
                let thread_id = ThreadId::default();
                let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
                    .await
                    .expect("create live thread");
                let first = "projection benchmark first user message";
                live.append_items_ordered(&[user_message_item(first)])
                    .await
                    .expect("initial append");
                live.flush().await.expect("initial flush");
                let expected_title = if named {
                    "Explicit benchmark title"
                } else {
                    first
                };
                if named {
                    let projected = live
                        .update_metadata(
                            crate::ThreadMetadataPatch {
                                name: Some(Some(expected_title.to_string())),
                                ..Default::default()
                            },
                            true,
                        )
                        .await
                        .expect("explicit name update");
                    assert_eq!(projected.name.as_deref(), Some(expected_title));
                }
                // Deliberately omit this thread: unnamed live commits currently scan the
                // whole legacy index. SQLite remains authoritative for the named control.
                let mut index = String::new();
                for entry in 0..index_entries {
                    let id = uuid::Uuid::from_u128(entry as u128 + 1);
                    assert_ne!(id.to_string(), thread_id.to_string());
                    index.push_str(&serde_json::json!({
                        "id": id.to_string(), "thread_name": format!("Unrelated thread {entry}"),
                        "updated_at": "2025-01-01T00:00:00Z"
                    }).to_string());
                    index.push('\n');
                }
                let index_bytes = index.len();
                std::fs::write(home.path().join("session_index.jsonl"), index)
                    .expect("fixture index");
                let path = store
                    .live_rollout_path(thread_id)
                    .await
                    .expect("rollout path");
                let mut expected_messages = vec![first.to_string()];
                let mut samples = Vec::new();
                let case_started = std::time::Instant::now();
                for sample in 0..=SAMPLES {
                    let message = format!("projection benchmark sample {sample}");
                    let items = [user_message_item(&message)];
                    let started = std::time::Instant::now();
                    live.append_items_ordered(&items)
                        .await
                        .expect("ordered append");
                    let append_ns = started.elapsed().as_nanos();
                    let barrier_started = std::time::Instant::now();
                    live.flush().await.expect("metadata barrier");
                    let flush_ns = barrier_started.elapsed().as_nanos();
                    let elapsed_ns = started.elapsed().as_nanos();
                    expected_messages.push(message);
                    if sample != 0 {
                        samples.push((sample, append_ns, flush_ns, elapsed_ns));
                    }
                }
                live.shutdown().await.expect("shutdown");
                drop(live);
                drop(store);
                runtime.close().await;
                let reopened = codex_state::StateRuntime::init(
                    config.sqlite_home,
                    config.default_model_provider_id,
                )
                .await
                .expect("reopen state db");
                let metadata = reopened
                    .get_thread(thread_id)
                    .await
                    .expect("metadata read")
                    .expect("metadata");
                assert_eq!(metadata.first_user_message.as_deref(), Some(first));
                assert_eq!(metadata.preview.as_deref(), Some(first));
                assert_eq!(metadata.title, expected_title);
                assert_eq!(metadata.rollout_path, path);
                let (items, loaded_id, errors) = RolloutRecorder::load_rollout_items(&path)
                    .await
                    .expect("authenticated reload");
                assert_eq!(loaded_id, Some(thread_id));
                assert_eq!(errors, 0);
                let messages = items
                    .iter()
                    .filter_map(|item| match item {
                        RolloutItem::EventMsg(EventMsg::UserMessage(event)) => {
                            Some(event.message.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    messages, expected_messages,
                    "no lost, reordered, or duplicate accepted records"
                );
                let case_elapsed_ns = case_started.elapsed().as_nanos();
                reopened.close().await;
                for (sample, append_ns, flush_ns, elapsed_ns) in samples {
                    println!(
                        "{}",
                        serde_json::json!({
                            "benchmark": "live_metadata_projection", "named": named,
                            "executable": executable,
                            "index_entries": index_entries, "index_bytes": index_bytes,
                            "sample": sample, "append_ns": append_ns, "flush_ns": flush_ns,
                            "elapsed_ns": elapsed_ns, "case_elapsed_ns": case_elapsed_ns,
                            "exact_recovery": true, "model_calls": 0
                        })
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn live_metadata_flush_retries_after_rollout_materialization_failure() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::default();
        let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create");
        let path = store
            .live_rollout_path(thread_id)
            .await
            .expect("rollout path");
        std::fs::create_dir_all(&path).expect("block rollout file with a directory");
        live.append_items_ordered(&[user_message_item("retained after failure")])
            .await
            .expect("queue");
        live.flush()
            .await
            .expect_err("file materialization must fail");
        assert!(
            runtime
                .get_thread(thread_id)
                .await
                .expect("metadata read")
                .is_none(),
            "failed rollout barrier must not acknowledge pending metadata"
        );
        std::fs::remove_dir(&path).expect("remove only the empty fixture blocker");
        live.flush()
            .await
            .expect("retry without resubmitting history");
        live.shutdown().await.expect("shutdown");
        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("metadata read")
            .expect("metadata");
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("retained after failure")
        );
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&path)
            .await
            .expect("reload");
        assert_eq!(errors, 0);
        assert_eq!(items.iter().filter(|item| matches!(item,
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "retained after failure"
        )).count(), 1, "retry must neither lose nor duplicate the accepted message");
        runtime.close().await;
    }

    #[tokio::test]
    async fn live_metadata_flush_without_sqlite_preserves_rollout_fallback() {
        let home = TempDir::new().expect("temp dir");
        let store = Arc::new(LocalThreadStore::new(test_config(home.path()), None));
        let thread_id = ThreadId::default();
        let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create");
        live.append_items_ordered(&[user_message_item("filesystem fallback")])
            .await
            .expect("queue");
        live.flush().await.expect("flush with no optional SQLite");
        let thread = live
            .update_metadata(crate::ThreadMetadataPatch::default(), true)
            .await
            .expect("explicit projection");
        assert_eq!(
            thread.first_user_message.as_deref(),
            Some("filesystem fallback")
        );
        let path = store
            .live_rollout_path(thread_id)
            .await
            .expect("rollout path");
        live.shutdown().await.expect("shutdown");
        assert_rollout_contains_message(&path, "filesystem fallback").await;
    }

    #[tokio::test]
    async fn synchronous_append_failure_recovers_live_projection_and_metadata() {
        // Cover both public store entrypoints and the metadata-owning LiveThread path.
        for entrypoint in ["raw", "canonical", "live"] {
            let home = TempDir::new().expect("temp dir");
            let config = test_config(home.path());
            let runtime = codex_state::StateRuntime::init(
                config.sqlite_home.clone(),
                config.default_model_provider_id.clone(),
            )
            .await
            .expect("state db");
            let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
            let thread_id = ThreadId::new();
            let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
                .await
                .expect("create");
            let path = store.live_rollout_path(thread_id).await.expect("path");
            std::fs::create_dir_all(&path).expect("block file with empty directory");
            let mut items = Vec::new();
            for turn_id in ["recovered-1", "recovered-2"] {
                items.extend([
                    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: turn_id.into(),
                        trace_id: None,
                        started_at: None,
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                    })),
                    user_message_item(turn_id),
                    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                        surfaced_result: None,
                        turn_id: turn_id.into(),
                        last_agent_message: None,
                        error: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                        timing: None,
                    })),
                ]);
            }
            let result = match entrypoint {
                "raw" => {
                    store
                        .append_items(AppendThreadItemsParams {
                            thread_id,
                            items: items.clone(),
                        })
                        .await
                }
                "canonical" => store.append_persisted_items(thread_id, &items).await,
                _ => live.append_items(&items).await,
            };
            result.expect_err("the accepted batch must fail its file barrier");
            assert!(
                runtime
                    .get_thread(thread_id)
                    .await
                    .expect("metadata read")
                    .is_none()
            );
            std::fs::remove_dir(&path).expect("remove only empty fixture blocker");
            live.flush()
                .await
                .expect("retry barrier without resubmitting");
            let mut cursor = None;
            for turn_id in ["recovered-1", "recovered-2"] {
                let page = store
                    .list_turns(crate::ListTurnsParams {
                        thread_id,
                        include_archived: false,
                        cursor,
                        page_size: 1,
                        sort_direction: crate::SortDirection::Asc,
                        items_view: crate::StoredTurnItemsView::Full,
                    })
                    .await
                    .expect("page live projection before shutdown");
                assert_eq!(page.turns.len(), 1, "{entrypoint}");
                assert_eq!(page.turns[0].turn_id, turn_id, "{entrypoint}");
                cursor = page.next_cursor;
                let page = store
                    .list_items(crate::ListItemsParams {
                        thread_id,
                        turn_id: Some(turn_id.into()),
                        include_archived: false,
                        cursor: None,
                        page_size: 1,
                        sort_direction: crate::SortDirection::Asc,
                    })
                    .await
                    .expect("projected user item");
                assert_eq!(page.items.len(), 1, "{entrypoint}");
                assert!(page.next_cursor.is_none(), "retry must not duplicate items");
            }
            assert!(cursor.is_none(), "retry must not duplicate turns");
            if entrypoint == "live" {
                let metadata = runtime
                    .get_thread(thread_id)
                    .await
                    .expect("read")
                    .expect("metadata");
                assert_eq!(metadata.first_user_message.as_deref(), Some("recovered-1"));
                assert_eq!(metadata.preview.as_deref(), Some("recovered-1"));
                assert_eq!(metadata.title, "recovered-1");
            }
            let (loaded, _, errors) = RolloutRecorder::load_rollout_items(&path)
                .await
                .expect("reload");
            assert_eq!(errors, 0);
            for turn_id in ["recovered-1", "recovered-2"] {
                assert_eq!(loaded.iter().filter(|item| matches!(item,
                    RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == turn_id
                )).count(), 1, "accepted records must persist exactly once");
            }
            live.shutdown().await.expect("shutdown");
            runtime.close().await;
        }
    }

    #[tokio::test]
    async fn resume_append_preserves_custom_sqlite_title_and_explicit_title_patch() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db");
        runtime
            .mark_backfill_complete(None)
            .await
            .expect("backfill");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create");
        live.append_items(&[user_message_item("original first message")])
            .await
            .expect("append");
        live.update_metadata(
            crate::ThreadMetadataPatch {
                name: Some(Some("custom searchable title".into())),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("rename");
        let path = store.live_rollout_path(thread_id).await.expect("path");
        live.shutdown().await.expect("shutdown");
        let resumed = LiveThread::resume(
            store.clone(),
            ThreadHistoryMode::Legacy,
            ResumeThreadParams {
                thread_id,
                rollout_path: Some(path),
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            },
        )
        .await
        .expect("resume");
        resumed
            .append_items(&[user_message_item("new message")])
            .await
            .expect("append after resume");
        let metadata = runtime
            .get_thread(thread_id)
            .await
            .expect("read")
            .expect("metadata");
        assert_eq!(
            metadata.title, "custom searchable title",
            "legacy name index must not mask SQLite corruption"
        );
        let mut params = crate::ListThreadsParams {
            project_id: None,
            page_size: 10,
            cursor: None,
            sort_key: crate::ThreadSortKey::CreatedAt,
            sort_direction: crate::SortDirection::Desc,
            allowed_sources: Vec::new(),
            model_providers: None,
            cwd_filters: None,
            archived: false,
            search_term: Some("custom searchable".into()),
            relation_filter: None,
            storage_mode: crate::ThreadListStorageMode::StateDbOnly,
        };
        let page = store
            .list_threads(params.clone())
            .await
            .expect("SQLite title search");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].thread_id, thread_id);
        resumed
            .update_metadata(
                crate::ThreadMetadataPatch {
                    title: Some("explicit replacement title".into()),
                    ..Default::default()
                },
                true,
            )
            .await
            .expect("explicit title remains authoritative");
        params.search_term = Some("explicit replacement".into());
        let page = store
            .list_threads(params)
            .await
            .expect("replacement title search");
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].name.as_deref(),
            Some("explicit replacement title")
        );
        resumed.shutdown().await.expect("shutdown");
        runtime.close().await;
    }

    #[tokio::test]
    async fn live_metadata_ack_default_preserves_durable_updates_and_errors() {
        // InMemoryThreadStore deliberately uses the trait's acknowledgement default.
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create");
        live.append_items(&[user_message_item("default durable metadata")])
            .await
            .expect("durable append");
        assert_eq!(store.calls().await.update_thread_metadata, 1);
        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            .expect("read committed metadata");
        assert_eq!(
            thread.first_user_message.as_deref(),
            Some("default durable metadata")
        );
        live.flush().await.expect("flush acknowledged update");
        assert_eq!(
            store.calls().await.update_thread_metadata,
            1,
            "must not replay acknowledged metadata"
        );

        let missing = ThreadId::new();
        let error = store
            .update_thread_metadata_ack(UpdateThreadMetadataParams {
                thread_id: missing,
                patch: crate::ThreadMetadataPatch::default(),
                include_archived: true,
            })
            .await
            .expect_err("default must retain returning API errors");
        assert!(
            matches!(error, ThreadStoreError::ThreadNotFound { thread_id } if thread_id == missing)
        );
        live.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn live_metadata_ack_preserves_missing_thread_fallback_errors() {
        for failed_sqlite in [false, true] {
            let home = TempDir::new().expect("temp dir");
            let config = test_config(home.path());
            let runtime = if failed_sqlite {
                let runtime = codex_state::StateRuntime::init(
                    config.sqlite_home.clone(),
                    config.default_model_provider_id.clone(),
                )
                .await
                .expect("state db");
                runtime.close().await;
                Some(runtime)
            } else {
                None
            };
            let store = LocalThreadStore::new(config, runtime);
            let thread_id = ThreadId::new();
            for patch in [
                crate::ThreadMetadataPatch::default(),
                crate::ThreadMetadataPatch {
                    preview: Some("phantom".into()),
                    ..Default::default()
                },
            ] {
                let error = store
                    .update_thread_metadata_ack(UpdateThreadMetadataParams {
                        thread_id,
                        patch,
                        include_archived: true,
                    })
                    .await
                    .expect_err(
                        "empty or uncommitted updates must still validate canonical existence",
                    );
                assert!(
                    matches!(error, ThreadStoreError::ThreadNotFound { thread_id: missing } if missing == thread_id)
                );
            }
        }
    }

    #[tokio::test]
    async fn live_metadata_flush_after_sqlite_failure_uses_rollout_fallback() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db");
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let thread_id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
            .await
            .expect("create");
        runtime.close().await;
        live.append_items_ordered(&[user_message_item("survives optional SQLite failure")])
            .await
            .expect("queue");
        live.flush()
            .await
            .expect("optional SQLite failure must use durable rollout fallback");
        let thread = live
            .update_metadata(crate::ThreadMetadataPatch::default(), true)
            .await
            .expect("returning API still projects canonical fallback");
        assert_eq!(
            thread.first_user_message.as_deref(),
            Some("survives optional SQLite failure")
        );
        let path = store
            .live_rollout_path(thread_id)
            .await
            .expect("rollout path");
        live.shutdown().await.expect("shutdown");
        assert_rollout_contains_message(&path, "survives optional SQLite failure").await;
    }

    #[tokio::test]
    async fn live_metadata_ack_retries_after_post_rollout_metadata_rejection() {
        for durable in [false, true] {
            let home = TempDir::new().expect("temp dir");
            let config = test_config(home.path());
            let runtime = codex_state::StateRuntime::init(
                config.sqlite_home.clone(),
                config.default_model_provider_id.clone(),
            )
            .await
            .expect("state db");
            let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
            let thread_id = ThreadId::new();
            let live = LiveThread::create(store.clone(), create_thread_params(thread_id))
                .await
                .expect("create");
            let path = store
                .live_rollout_path(thread_id)
                .await
                .expect("rollout path");
            // Use the metadata tests' SQLite-trigger injection pattern. A validation rejection
            // must propagate (unlike optional Internal errors) after the history barrier succeeds.
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(runtime.codex_home().join(codex_state::STATE_DB_FILENAME))
                        .create_if_missing(false),
                )
                .await
                .expect("open state db");
            sqlx::query("CREATE TRIGGER reject_live_metadata BEFORE INSERT ON threads BEGIN SELECT RAISE(FAIL, 'is archived: injected metadata rejection'); END")
                .execute(&pool).await.expect("install metadata rejection");
            let items = [user_message_item("committed once before metadata retry")];
            let error = if durable {
                live.append_items(&items)
                    .await
                    .expect_err("metadata rejection after durable append")
            } else {
                live.append_items_ordered(&items).await.expect("queue");
                live.flush()
                    .await
                    .expect_err("metadata rejection after rollout flush")
            };
            assert!(
                matches!(error, ThreadStoreError::InvalidRequest { message } if message.contains("injected metadata rejection"))
            );
            assert!(
                runtime
                    .get_thread(thread_id)
                    .await
                    .expect("read rejected metadata")
                    .is_none()
            );
            assert_rollout_contains_message(&path, "committed once before metadata retry").await;
            sqlx::query("DROP TRIGGER reject_live_metadata")
                .execute(&pool)
                .await
                .expect("remove rejection");
            pool.close().await;

            // Retry only the barrier: replaying the append would duplicate the accepted record.
            live.flush()
                .await
                .expect("retry pending metadata without resubmission");
            let metadata = runtime
                .get_thread(thread_id)
                .await
                .expect("metadata read")
                .expect("committed metadata");
            assert_eq!(
                metadata.first_user_message.as_deref(),
                Some("committed once before metadata retry")
            );
            assert_eq!(
                metadata.preview.as_deref(),
                Some("committed once before metadata retry")
            );
            live.shutdown().await.expect("shutdown");
            let (items, loaded_id, errors) = RolloutRecorder::load_rollout_items(&path)
                .await
                .expect("authenticated reload");
            assert_eq!(loaded_id, Some(thread_id));
            assert_eq!(errors, 0);
            assert_eq!(items.iter().filter(|item| matches!(item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "committed once before metadata retry"
            )).count(), 1, "metadata retry must not lose or duplicate accepted history");
            runtime.close().await;
        }
    }

    #[tokio::test]
    async fn durable_audit_cancelled_initialization_discards_only_unclaimed_writer() {
        let home = TempDir::new().unwrap();
        let store = LocalThreadStore::new(test_config(home.path()), None);
        let id = ThreadId::new();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let owner = store.clone();
        let observer = tokio::spawn(async move {
            live_writer::initialize(owner, id, move |store| async move {
                live_writer::create_thread(&store, create_thread_params(id)).await?;
                let _ = started.send(store.live_recorder(id).await?);
                let _ = wait.await;
                Ok(None)
            })
            .await
        });
        let obsolete = ready.await.unwrap();
        let path = obsolete.rollout_path().to_path_buf();
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        wait_for_recorder_removal(&store, id).await;
        assert!(
            !path.exists(),
            "abandoned lazy startup must not materialize"
        );
        assert!(
            obsolete
                .record_canonical_items_ordered(&[user_message_item("late")])
                .await
                .is_err()
        );
        let replacement = LocalThreadStore::new(test_config(home.path()), None);
        replacement
            .create_thread(create_thread_params(id))
            .await
            .unwrap();
        replacement.discard_thread(id).await.unwrap();
    }

    #[tokio::test]
    async fn durable_audit_cancelled_shutdown_settles_registry_and_obsolete_lease() {
        let home = TempDir::new().unwrap();
        let store = LocalThreadStore::new(test_config(home.path()), None);
        let id = ThreadId::new();
        store.create_thread(create_thread_params(id)).await.unwrap();
        store
            .append_items_ordered(AppendThreadItemsParams {
                thread_id: id,
                items: vec![user_message_item("accepted exactly once")],
            })
            .await
            .unwrap();
        let obsolete = store.live_recorder(id).await.unwrap();
        let mut shutdown = store.shutdown_thread(id);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(shutdown.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(shutdown);
        wait_for_recorder_removal(&store, id).await;
        assert!(
            obsolete
                .record_canonical_items_ordered(&[user_message_item("late")])
                .await
                .is_err()
        );
        let replacement = LocalThreadStore::new(test_config(home.path()), None);
        replacement
            .resume_thread(ResumeThreadParams {
                thread_id: id,
                rollout_path: None,
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .unwrap();
        let history = replacement
            .load_history(LoadThreadHistoryParams {
                thread_id: id,
                include_archived: true,
            })
            .await
            .unwrap();
        assert_eq!(history.items.iter().filter(|item| matches!(item,
            RolloutItem::EventMsg(EventMsg::UserMessage(message)) if message.message == "accepted exactly once"
        )).count(), 1);
        replacement.shutdown_thread(id).await.unwrap();
    }

    #[tokio::test]
    async fn durable_audit_cancelled_discard_settles_without_materializing() {
        let home = TempDir::new().unwrap();
        let store = LocalThreadStore::new(test_config(home.path()), None);
        let id = ThreadId::new();
        store.create_thread(create_thread_params(id)).await.unwrap();
        let obsolete = store.live_recorder(id).await.unwrap();
        let path = obsolete.rollout_path().to_path_buf();
        let mut discard = store.discard_thread(id);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(discard.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(discard);
        wait_for_recorder_removal(&store, id).await;
        assert!(!path.exists());
        assert!(
            obsolete
                .record_canonical_items_ordered(&[user_message_item("late")])
                .await
                .is_err()
        );
        let replacement = LocalThreadStore::new(test_config(home.path()), None);
        replacement
            .create_thread(create_thread_params(id))
            .await
            .unwrap();
        replacement.discard_thread(id).await.unwrap();
    }

    async fn wait_for_recorder_removal(store: &LocalThreadStore, id: ThreadId) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.live_recorders.lock().await.contains_key(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned cleanup completes");
    }

    fn create_thread_params(thread_id: ThreadId) -> CreateThreadParams {
        CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "test_originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Legacy,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: thread_metadata(),
        }
    }

    #[tokio::test]
    async fn independent_stores_exclude_live_writer_before_create_resume_or_delete() {
        let home = TempDir::new().unwrap();
        let config = test_config(home.path());
        let first = LocalThreadStore::new(config.clone(), None);
        let second = LocalThreadStore::new(config, None);
        let id = ThreadId::default();
        first.create_thread(create_thread_params(id)).await.unwrap();
        assert!(matches!(
            second.create_thread(create_thread_params(id)).await,
            Err(ThreadStoreError::Conflict { .. })
        ));
        first.persist_thread(id).await.unwrap();
        let path = first.live_rollout_path(id).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();
        assert!(matches!(
            second
                .resume_thread(ResumeThreadParams {
                    thread_id: id,
                    rollout_path: None,
                    history: None,
                    include_archived: true,
                    metadata: thread_metadata(),
                })
                .await,
            Err(ThreadStoreError::Conflict { .. })
        ));
        assert!(
            second
                .delete_thread(crate::DeleteThreadParams { thread_id: id })
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        first.shutdown_thread(id).await.unwrap();
        second
            .resume_thread(ResumeThreadParams {
                thread_id: id,
                rollout_path: None,
                history: None,
                include_archived: true,
                metadata: thread_metadata(),
            })
            .await
            .unwrap();
        second.shutdown_thread(id).await.unwrap();
    }

    fn assert_paginated_threads_unsupported(err: ThreadStoreError) {
        assert!(matches!(
            err,
            ThreadStoreError::Unsupported {
                operation: "paginated_threads"
            }
        ));
    }

    fn thread_metadata() -> ThreadPersistenceMetadata {
        ThreadPersistenceMetadata {
            cwd: Some(std::env::current_dir().expect("cwd")),
            model_provider: "test-provider".to_string(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_admitted_append_settles_projection_and_metadata() {
        let home = TempDir::new().unwrap();
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_thread_params(id))
            .await
            .unwrap();
        let pause = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let writer = live.clone();
        let append = tokio::spawn(super::TEST_APPEND_HANDOFF_PAUSE.scope(
            pause.clone(),
            async move {
                writer
                    .append_items_ordered(&[user_message_item("accepted once")])
                    .await
            },
        ));
        pause.0.notified().await;
        append.abort();
        assert!(append.await.unwrap_err().is_cancelled());
        pause.1.notify_one();
        live.flush().await.unwrap();
        let metadata = runtime.get_thread(id).await.unwrap().unwrap();
        assert_eq!(
            metadata.first_user_message.as_deref(),
            Some("accepted once")
        );
        let page = store
            .list_items(crate::ListItemsParams {
                thread_id: id,
                turn_id: None,
                include_archived: false,
                cursor: None,
                page_size: 10,
                sort_direction: crate::SortDirection::Asc,
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let path = store.live_rollout_path(id).await.unwrap();
        let (items, _, _) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(items.iter().filter(|item| matches!(item,
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == "accepted once")).count(), 1);
        live.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deferred_metadata_retries_after_optional_insert_failure() {
        let home = TempDir::new().unwrap();
        let config = test_config(home.path());
        let runtime = codex_state::StateRuntime::init(
            config.sqlite_home.clone(),
            config.default_model_provider_id.clone(),
        )
        .await
        .unwrap();
        let store = Arc::new(LocalThreadStore::new(config, Some(runtime.clone())));
        let id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_thread_params(id))
            .await
            .unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(runtime.codex_home().join(codex_state::STATE_DB_FILENAME)),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER transient_metadata BEFORE INSERT ON threads BEGIN SELECT RAISE(FAIL, 'temporary storage failure'); END")
            .execute(&pool).await.unwrap();
        live.append_items_ordered(&[user_message_item("repair without replay")])
            .await
            .unwrap();
        live.flush().await.unwrap();
        assert!(runtime.get_thread(id).await.unwrap().is_none());
        sqlx::query("DROP TRIGGER transient_metadata")
            .execute(&pool)
            .await
            .unwrap();
        live.flush().await.unwrap();
        let metadata = runtime.get_thread(id).await.unwrap().unwrap();
        assert_eq!(metadata.preview.as_deref(), Some("repair without replay"));
        let path = store.live_rollout_path(id).await.unwrap();
        let (items, _, _) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, RolloutItem::EventMsg(EventMsg::UserMessage(_))))
                .count(),
            1
        );
        live.shutdown().await.unwrap();
        pool.close().await;
    }

    fn user_message_item(message: &str) -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            client_id: None,
            message: message.to_string(),
            images: None,
            local_images: Vec::new(),
            text_elements: Vec::new(),
            ..Default::default()
        }))
    }

    async fn assert_rollout_contains_message(path: &std::path::Path, expected: &str) {
        let (items, _, _) = RolloutRecorder::load_rollout_items(path)
            .await
            .expect("load rollout items");
        assert!(items.iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::UserMessage(event)) if event.message == expected
            )
        }));
    }
}
