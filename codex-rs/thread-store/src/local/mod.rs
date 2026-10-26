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
        self.load_history_with_payload_versions(params, None).await
    }

    async fn load_history_with_payload_versions(
        &self,
        params: LoadThreadHistoryParams,
        payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
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
            return read_thread::read_thread_by_rollout_path_with_payload_versions(
                self,
                rollout_path,
                /*include_archived*/ true,
                /*include_history*/ true,
                payload_versions,
            )
            .await?
            .history
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("failed to load history for thread {}", params.thread_id),
            });
        }

        read_thread::read_thread_with_payload_versions(
            self,
            ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: true,
            },
            payload_versions,
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
            // The operation permit bounds this to one admitted worker per thread.
            // Cancellation may drop the observer, never the queue/projection handoff.
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::append_persisted_items_ordered(&store, thread_id, &items).await?;
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

