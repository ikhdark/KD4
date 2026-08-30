use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutPersistenceTelemetry;
use codex_rollout::measure_and_filter_rollout_items;
use codex_rollout::persisted_rollout_items;
use codex_rollout::should_persist_event_msg;
use tokio::sync::Mutex;
use tracing::warn;

use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::thread_metadata_sync::ThreadMetadataSync;

/// Handle for an active thread's persistence lifecycle.
///
/// `LiveThread` keeps lifecycle decisions with the caller while delegating storage details to
/// [`ThreadStore`]. Local stores may use a rollout file internally and remote stores may use a
/// service, but session code should only need this handle for the active thread.
#[derive(Clone)]
pub struct LiveThread {
    thread_id: ThreadId,
    history_mode: ThreadHistoryMode,
    thread_store: Arc<dyn ThreadStore>,
    metadata_sync: Arc<Mutex<ThreadMetadataSync>>,
    terminal_events: Arc<StdMutex<TerminalEventIndex>>,
    persistence_telemetry: RolloutPersistenceTelemetry,
}

struct TerminalEventIndex {
    by_turn_id: HashMap<String, EventMsg>,
    trusted: bool,
    pending_terminal_appends: usize,
    revision: u64,
}

impl TerminalEventIndex {
    fn trusted_empty() -> Self {
        Self {
            by_turn_id: HashMap::new(),
            trusted: true,
            pending_terminal_appends: 0,
            revision: 0,
        }
    }

    fn from_items(items: &[RolloutItem]) -> Self {
        let mut index = Self::trusted_empty();
        index.observe_items(items);
        index
    }

    fn observe_items(&mut self, items: &[RolloutItem]) {
        for item in items {
            let RolloutItem::EventMsg(event) = item else {
                continue;
            };
            let Some(turn_id) = terminal_event_turn_id(event) else {
                continue;
            };
            self.by_turn_id
                .entry(turn_id.to_string())
                .or_insert_with(|| event.clone());
        }
    }
}

struct TerminalAppendGuard {
    index: Arc<StdMutex<TerminalEventIndex>>,
    active: bool,
}

impl TerminalAppendGuard {
    fn new(index: Arc<StdMutex<TerminalEventIndex>>, items: &[RolloutItem]) -> Self {
        let active = items.iter().any(|item| {
            matches!(item, RolloutItem::EventMsg(event) if terminal_event_turn_id(event).is_some())
        });
        if active {
            let mut terminal_events = index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            terminal_events.pending_terminal_appends += 1;
            terminal_events.revision = terminal_events.revision.wrapping_add(1);
        }
        Self { index, active }
    }

    fn commit(mut self, items: &[RolloutItem]) {
        if self.active {
            let mut terminal_events = self
                .index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            terminal_events.observe_items(items);
            terminal_events.pending_terminal_appends -= 1;
            terminal_events.revision = terminal_events.revision.wrapping_add(1);
            self.active = false;
        }
    }
}

impl Drop for TerminalAppendGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut terminal_events = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        terminal_events.pending_terminal_appends -= 1;
        terminal_events.trusted = false;
        terminal_events.revision = terminal_events.revision.wrapping_add(1);
    }
}

fn terminal_event_turn_id(event: &EventMsg) -> Option<&str> {
    match event {
        EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
        EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
        _ => None,
    }
}

/// Owns a live thread while session initialization is still fallible.
///
/// If initialization returns early after persistence has been opened, dropping this guard discards
/// the live writer without forcing lazy in-memory state to become durable. Call [`commit`] once the
/// session owns the live thread for normal operation.
pub struct LiveThreadInitGuard {
    live_thread: Option<LiveThread>,
}

impl LiveThreadInitGuard {
    pub fn new(live_thread: Option<LiveThread>) -> Self {
        Self { live_thread }
    }

    pub fn as_ref(&self) -> Option<&LiveThread> {
        self.live_thread.as_ref()
    }

    pub fn commit(&mut self) {
        self.live_thread = None;
    }

    pub async fn discard(&mut self) {
        let Some(live_thread) = self.live_thread.take() else {
            return;
        };
        if let Err(err) = live_thread.discard().await {
            warn!("failed to discard thread persistence for failed session init: {err}");
        }
    }
}

impl Drop for LiveThreadInitGuard {
    fn drop(&mut self) {
        let Some(live_thread) = self.live_thread.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("failed to discard thread persistence for failed session init: no Tokio runtime");
            return;
        };
        handle.spawn(async move {
            if let Err(err) = live_thread.discard().await {
                warn!("failed to discard thread persistence for failed session init: {err}");
            }
        });
    }
}

impl LiveThread {
    pub async fn create(
        thread_store: Arc<dyn ThreadStore>,
        params: CreateThreadParams,
    ) -> ThreadStoreResult<Self> {
        let thread_id = params.thread_id;
        let history_mode = params.history_mode;
        let repository_context =
            ThreadMetadataSync::collect_repository_context_for_create(&params).await;
        let git_info = repository_context
            .as_ref()
            .map(ThreadMetadataSync::git_info_from_repository_context);
        let metadata_sync = ThreadMetadataSync::for_create_with_git_info(&params, git_info.clone());
        thread_store
            .create_thread_with_repository_context(params, repository_context)
            .await?;
        Ok(Self {
            thread_id,
            history_mode,
            thread_store,
            metadata_sync: Arc::new(Mutex::new(metadata_sync)),
            terminal_events: Arc::new(StdMutex::new(TerminalEventIndex::trusted_empty())),
            persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
        })
    }

    pub async fn resume(
        thread_store: Arc<dyn ThreadStore>,
        history_mode: ThreadHistoryMode,
        params: ResumeThreadParams,
    ) -> ThreadStoreResult<Self> {
        let thread_id = params.thread_id;
        let should_load_history = params.history.is_none();
        let include_archived = params.include_archived;
        let mut terminal_events = params
            .history
            .as_deref()
            .map(|history| TerminalEventIndex::from_items(history));
        let mut metadata_sync = ThreadMetadataSync::for_resume(&params);
        thread_store.resume_thread(params).await?;
        if should_load_history {
            match thread_store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived,
                })
                .await
            {
                Ok(history) => {
                    metadata_sync.record_resume_history(&history.items);
                    terminal_events = Some(TerminalEventIndex::from_items(&history.items));
                }
                Err(err) => {
                    if let Err(discard_err) = thread_store.discard_thread(thread_id).await {
                        warn!(
                            "failed to discard thread persistence after resume history load failed: {discard_err}"
                        );
                    }
                    return Err(err);
                }
            }
        }
        Ok(Self {
            thread_id,
            history_mode,
            thread_store,
            metadata_sync: Arc::new(Mutex::new(metadata_sync)),
            terminal_events: Arc::new(StdMutex::new(
                terminal_events.unwrap_or_else(TerminalEventIndex::trusted_empty),
            )),
            persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
        })
    }

    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(item_count = raw_items.len())
    )]
    pub async fn append_items(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.append_items_with_durability(raw_items, true).await
    }

    /// Queue items in canonical order and defer rollout plus metadata durability to the next
    /// persist/flush/shutdown barrier.
    pub async fn append_items_ordered(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.append_items_with_durability(raw_items, false).await
    }

    pub fn should_persist_event(&self, event: &EventMsg) -> bool {
        should_persist_event_msg(event, self.history_mode)
    }

    async fn append_items_with_durability(
        &self,
        raw_items: &[RolloutItem],
        durable: bool,
    ) -> ThreadStoreResult<()> {
        // Empty appends are intentionally ignored rather than represented as zero-sized batches.
        if raw_items.is_empty() {
            return Ok(());
        }
        let (items, measurement) = if self.persistence_telemetry.is_enabled() {
            let (items, measurement) =
                measure_and_filter_rollout_items(raw_items, self.history_mode);
            (items, Some(measurement))
        } else {
            (persisted_rollout_items(raw_items, self.history_mode), None)
        };
        if let Some(measurement) = measurement.as_ref() {
            self.persistence_telemetry
                .record_batch(raw_items, measurement);
        }
        if items.is_empty() {
            return Ok(());
        }
        let terminal_append =
            TerminalAppendGuard::new(Arc::clone(&self.terminal_events), items.as_slice());
        if durable {
            self.thread_store
                .append_persisted_items(self.thread_id, items.as_slice())
                .await?;
        } else {
            self.thread_store
                .append_persisted_items_ordered(self.thread_id, items.as_slice())
                .await?;
        }
        terminal_append.commit(items.as_slice());
        let update = self
            .metadata_sync
            .lock()
            .await
            .observe_appended_items(items.as_slice());
        if durable && let Some(update) = update {
            self.thread_store
                .update_thread_metadata(UpdateThreadMetadataParams {
                    thread_id: self.thread_id,
                    patch: update.patch.clone(),
                    include_archived: true,
                })
                .await?;
            self.metadata_sync
                .lock()
                .await
                .mark_pending_update_applied(&update);
        }
        Ok(())
    }

    pub async fn persist(&self) -> ThreadStoreResult<()> {
        self.thread_store.persist_thread(self.thread_id).await?;
        self.flush_pending_metadata_update().await
    }

    pub async fn flush(&self) -> ThreadStoreResult<()> {
        self.thread_store.flush_thread(self.thread_id).await?;
        self.flush_pending_metadata_update_for_existing_history()
            .await
    }

    pub async fn shutdown(&self) -> ThreadStoreResult<()> {
        self.thread_store.shutdown_thread(self.thread_id).await?;
        self.flush_pending_metadata_update_for_existing_history()
            .await
    }

    pub async fn discard(&self) -> ThreadStoreResult<()> {
        self.thread_store.discard_thread(self.thread_id).await
    }

    pub async fn load_history(
        &self,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        self.thread_store
            .load_history(LoadThreadHistoryParams {
                thread_id: self.thread_id,
                include_archived,
            })
            .await
    }

    /// Returns the first terminal event persisted for `turn_id`.
    ///
    /// Successful live appends and resume history keep this lookup process-local. A full history
    /// read is reserved for an ambiguous append whose future was cancelled or returned an error.
    pub async fn terminal_event(
        &self,
        turn_id: &str,
        include_archived: bool,
    ) -> ThreadStoreResult<Option<EventMsg>> {
        let revision = {
            let terminal_events = self
                .terminal_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if terminal_events.trusted && terminal_events.pending_terminal_appends == 0 {
                return Ok(terminal_events.by_turn_id.get(turn_id).cloned());
            }
            terminal_events.revision
        };

        let history = self.load_history(include_archived).await?;
        let loaded_event = history.items.iter().find_map(|item| {
            let RolloutItem::EventMsg(event) = item else {
                return None;
            };
            (terminal_event_turn_id(event) == Some(turn_id)).then(|| event.clone())
        });
        let mut terminal_events = self
            .terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if terminal_events.revision == revision && terminal_events.pending_terminal_appends == 0 {
            let next_revision = terminal_events.revision;
            *terminal_events = TerminalEventIndex::from_items(&history.items);
            terminal_events.revision = next_revision;
        } else if terminal_events.trusted && terminal_events.pending_terminal_appends == 0 {
            return Ok(terminal_events.by_turn_id.get(turn_id).cloned());
        }
        Ok(loaded_event)
    }

    pub async fn read_thread(
        &self,
        include_archived: bool,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        self.thread_store
            .read_thread(ReadThreadParams {
                thread_id: self.thread_id,
                include_archived,
                include_history,
            })
            .await
    }

    pub async fn update_memory_mode(
        &self,
        mode: ThreadMemoryMode,
        include_archived: bool,
    ) -> ThreadStoreResult<()> {
        self.flush_pending_metadata_update().await?;
        self.thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch: ThreadMetadataPatch {
                    memory_mode: Some(mode),
                    ..Default::default()
                },
                include_archived,
            })
            .await?;
        Ok(())
    }

    pub async fn update_metadata(
        &self,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThread> {
        self.flush_pending_metadata_update().await?;
        self.thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch,
                include_archived,
            })
            .await
    }

    /// Returns the live local rollout path for legacy local-only callers.
    ///
    /// Remote stores do not expose rollout files, so they return `Ok(None)`.
    pub async fn local_rollout_path(&self) -> ThreadStoreResult<Option<PathBuf>> {
        let Some(local_store) = self
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        else {
            return Ok(None);
        };
        local_store
            .live_rollout_path(self.thread_id)
            .await
            .map(Some)
    }

    async fn flush_pending_metadata_update(&self) -> ThreadStoreResult<()> {
        let update = self.metadata_sync.lock().await.take_pending_update();
        self.apply_pending_metadata_update(update).await
    }

    async fn flush_pending_metadata_update_for_existing_history(&self) -> ThreadStoreResult<()> {
        let update = self
            .metadata_sync
            .lock()
            .await
            .take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(update).await
    }

    async fn apply_pending_metadata_update(
        &self,
        update: Option<crate::thread_metadata_sync::PendingThreadMetadataPatch>,
    ) -> ThreadStoreResult<()> {
        let Some(update) = update else {
            return Ok(());
        };
        self.thread_store
            .update_thread_metadata(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch: update.patch.clone(),
                include_archived: true,
            })
            .await?;
        self.metadata_sync
            .lock()
            .await
            .mark_pending_update_applied(&update);
        Ok(())
    }
}
