use crate::CreateProjectParams;
use crate::CreatedProject;
use crate::DeletedProject;
use crate::ListProjectsParams;
use crate::MoveProjectParams;
use crate::ProjectMoveOutcome;
mod archive_retention;
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
pub use delete_thread::thread_mailbox_path;



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

use codex_rollout::DiagnosticOriginSource;

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
/// The SQLite state DB, when available, owns explicit names/clears and supplies
/// the metadata index for list/read paths. Canonical read errors must not be
/// treated as missing rows when consulting compatibility names.
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
    archive_retention: Option<Arc<tokio::sync::watch::Sender<()>>>,
    compression_worker: Option<Arc<codex_rollout::RolloutCompressionWorker>>,
}

struct LiveRecorderEntry {
    recorder: RolloutRecorder,
    // Local rollout files are materialized lazily, but metadata updates can arrive before the
    // canonical SessionMeta is durable. Retain the mode captured when live persistence was opened
    // so missing SQLite rows can still be seeded.
    history_mode: ThreadHistoryMode,
    git_compatibility_needs_check: bool,
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
    /// Common admission boundary for compatibility publication and live
    /// recorder operations that may enqueue/flush rollout bytes.
    async fn lock_rollout_publication(&self, thread_id: ThreadId) -> tokio::sync::OwnedMutexGuard<()> {
        let order = {
            let mut orders = self
                .metadata_update_order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !orders.contains_key(&thread_id) {
                orders.retain(|_, order| order.strong_count() > 0);
            }
            match orders.get(&thread_id).and_then(std::sync::Weak::upgrade) {
                Some(order) => order,
                None => {
                    let order = Arc::new(Mutex::new(()));
                    orders.insert(thread_id, Arc::downgrade(&order));
                    order
                }
            }
        };
        order.lock_owned().await
    }

    async fn mark_git_compatibility_check(&self, thread_id: ThreadId, needs_check: bool) {
        if let Some(entry) = self.live_recorders.lock().await.get_mut(&thread_id) {
            entry.git_compatibility_needs_check = needs_check;
        }
    }

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
            archive_retention: None,
            compression_worker: None,
        }
    }

    /// Bound to a private opt-in marker, not enabled for other Codex homes.
    pub async fn run_archive_retention(&self) -> ThreadStoreResult<()> {
        let home = self.config.codex_home.clone();
        let guard = tokio::task::spawn_blocking(move || {
            if !codex_rollout::home_retention_enabled(&home) {
                return Ok(None);
            }
            let tmp = home.join(".tmp");
            std::fs::create_dir_all(&tmp)?;
            let guard = std::fs::File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(tmp.join("archive-retention.worker.lock"))?;
            match guard.try_lock() {
                Ok(()) => Ok(Some(guard)),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(err) => Err(std::io::Error::from(err)),
            }
        })
        .await
        .map_err(delete_thread::delete_recovery_error)?
        .map_err(delete_thread::delete_recovery_error)?;
        let Some(_guard) = guard else {
            return Ok(());
        };
        let maintenance = Self::new(self.config.clone(), self.state_db.clone());
        delete_thread::expire_archives(&maintenance, chrono::Utc::now().timestamp() - 7 * 86400)
            .await?;
        codex_rollout::collect_home_payloads(&self.config.codex_home)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: err.to_string(),
            })?;
        Ok(())
    }

    /// Retain shared maintenance for exactly this store's lifetime.
    pub fn with_compression_worker(mut self) -> Self {
        self.compression_worker =
            codex_rollout::spawn_rollout_compression_worker(self.config.codex_home.to_path_buf());
        self
    }

    /// Make one bounded recovery pass for idle threads at local-store startup.
    /// A live writer's admission, resume and explicit metadata updates use the
    /// same per-thread recovery path; this builder does not start a retry loop.
    pub fn with_git_compatibility_recovery(self) -> Self {
        if let Some(state_db) = self.state_db.clone()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let store = self.clone();
            runtime.spawn(async move {
                update_thread_metadata::recover_idle_git_compatibility(&store, &state_db).await;
            });
        }
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

    /// Session import reservations require both a durable rollout and a SQLite receipt.
    pub fn supports_durable_session_import(&self) -> bool {
        self.state_db.is_some()
    }

    /// Commit the import patch together with proof for this exact reservation.
    pub async fn commit_imported_session_metadata(
        &self,
        params: UpdateThreadMetadataParams,
        content_sha256: &str,
    ) -> ThreadStoreResult<()> {
        self.session_import_db()?;
        update_thread_metadata::commit_imported_session_metadata(self, params, content_sha256).await
    }

    /// Check the historical commit, not the thread's current editable metadata.
    pub async fn has_imported_session_metadata_receipt(
        &self,
        thread_id: ThreadId,
        content_sha256: &str,
    ) -> ThreadStoreResult<bool> {
        self.session_import_db()?
            .has_imported_thread_metadata_receipt(thread_id, content_sha256)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to read session import metadata receipt for {thread_id}: {error}"),
            })
    }

    /// A history miss alone does not exclude a live writer, SQLite-only row,
    /// or a rollout that is moving between the active and archived namespaces.
    pub async fn reserved_import_thread_is_absent(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<bool> {
        let db = self.session_import_db()?;
        let maintenance_busy = || ThreadStoreError::Conflict {
            message: "rollout maintenance is busy; session import recovery deferred".into(),
        };
        if !delete_thread::recover_staged_deletes(self).await? {
            return Err(maintenance_busy());
        }
        if self.live_recorder(thread_id).await.is_ok() {
            return Ok(false);
        }
        let _lease = self.acquire_writer_lease(thread_id, false).await?;
        let _moves = codex_rollout::try_lock_rollout_moves(&self.config.codex_home)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to inspect reserved import thread {thread_id}: {error}"),
            })?
            .ok_or_else(maintenance_busy)?;
        if db
            .get_thread(thread_id)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to read reserved import thread {thread_id}: {error}"),
            })?
            .is_some()
        {
            return Ok(false);
        }
        // Only the import ledger allocates this reserved ID. Creation obtains
        // the normal exclusive writer lease again; an absence check never
        // confers ownership of an existing writer.
        codex_rollout::find_thread_rollout_paths(&self.config.codex_home, thread_id)
            .await
            .map(|paths| paths.is_empty())
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to inspect reserved import rollout {thread_id}: {error}"),
            })
    }

    fn session_import_db(&self) -> ThreadStoreResult<&StateDbHandle> {
        self.state_db.as_ref().ok_or(ThreadStoreError::Unsupported {
            operation: "durable_session_import",
        })
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

    /// Check index-backed visibility against the committed rollout location.
    /// A filesystem fallback can also repair the stale SQLite archive state.
    pub async fn indexed_thread_is_archived(
        &self,
        metadata: &mut codex_state::ThreadMetadata,
    ) -> ThreadStoreResult<bool> {
        read_thread::resolve_sqlite_rollout_location(self, metadata).await?;
        Ok(metadata.archived_at.is_some()
            || helpers::rollout_path_is_archived(
                self.config.codex_home.as_path(),
                metadata.rollout_path.as_path(),
            ))
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
    ) -> ThreadStoreResult<RolloutRecorder> {
        match self.live_recorders.lock().await.entry(thread_id) {
            Entry::Occupied(entry) => Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {} already has a live local writer", entry.key()),
            }),
            Entry::Vacant(entry) => {
                entry.insert(LiveRecorderEntry {
                    recorder: recorder.clone(),
                    history_mode,
                    git_compatibility_needs_check: false,
                });
                Ok(recorder)
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
            let store = self.clone();
            // Once writer work is admitted, retain its projection settlement
            // and operation order even if the requesting future is cancelled.
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::flush_thread(&store, thread_id, durable).await?;
                projection.commit_pending().await
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("flush projection handoff failed: {error}"),
            })??;
        } else {
            let store = self.clone();
            tokio::spawn(async move { live_writer::flush_thread(&store, thread_id, durable).await })
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: format!("rollout flush handoff failed: {error}"),
                })??;
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
                live_writer::create_thread(&store, params).await
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
                .await
            })
            .await
        })
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let thread_id = params.thread_id;
            live_writer::initialize(self.clone(), thread_id, |store| async move {
                live_writer::resume_thread(&store, params).await
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
            let store = self.clone();
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::append_persisted_items_ordered(&store, thread_id, &persisted_items).await?;
                projection.append_pending(persisted_items).await;
                live_writer::flush_thread(&store, thread_id, false).await?;
                projection.commit_pending().await
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("append projection handoff failed: {error}"),
            })?
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
            let store = self.clone();
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::append_persisted_items_ordered(&store, thread_id, &persisted_items).await?;
                if !persisted_items.is_empty() {
                    projection.append_pending(persisted_items).await;
                }
                Ok(())
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("ordered append projection handoff failed: {error}"),
            })?
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
            let store = self.clone();
            let items = items.to_vec();
            tokio::spawn(async move {
                let _operation = _operation;
                live_writer::append_persisted_items_ordered(&store, thread_id, &items).await?;
                projection.append_pending(items).await;
                live_writer::flush_thread(&store, thread_id, false).await?;
                projection.commit_pending().await
            })
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("persisted append projection handoff failed: {error}"),
            })?
        })
    }

    fn append_persisted_items_ordered<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
    ) -> ThreadStoreFuture<'a, ()> {
        self.append_persisted_items_ordered_with_origins(thread_id, items, DiagnosticOriginSource::Unknown)
    }

    fn append_persisted_items_ordered_with_origins<'a>(
        &'a self,
        thread_id: ThreadId,
        items: &'a [RolloutItem],
        origins: DiagnosticOriginSource,
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
                live_writer::append_persisted_items_ordered_with_origins(
                    &store, thread_id, &items, &origins,
                ).await?;
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
                let store = self.clone();
                tokio::spawn(async move {
                    let _operation = _operation;
                    live_writer::persist_thread(&store, thread_id).await?;
                    projection.commit_pending().await
                })
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: format!("persist projection handoff failed: {error}"),
                })??;
            } else {
                let store = self.clone();
                tokio::spawn(async move { live_writer::persist_thread(&store, thread_id).await })
                    .await
                    .map_err(|error| ThreadStoreError::Internal {
                        message: format!("rollout persist handoff failed: {error}"),
                    })??;
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
                let store = self.clone();
                tokio::spawn(async move {
                    let _operation = _operation;
                    live_writer::shutdown_thread(&store, thread_id).await?;
                    projection.commit_pending().await?;
                    projection::remove_if_current(&store, thread_id, &projection).await;
                    Ok::<_, ThreadStoreError>(())
                })
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: format!("shutdown projection handoff failed: {error}"),
                })??;
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

