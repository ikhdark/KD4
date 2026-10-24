use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutPersistenceTelemetry;
use codex_rollout::is_rollout_activity_event;
use codex_rollout::measure_and_filter_rollout_items_with_activity;
use codex_rollout::persisted_rollout_items_with_activity;
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
    writer_retired: Arc<std::sync::atomic::AtomicBool>,
    terminal_events: Arc<StdMutex<TerminalEventIndex>>,
    fork_history: Arc<StdMutex<Weak<StoredThreadHistory>>>,
    persistence_telemetry: RolloutPersistenceTelemetry,
}

struct TerminalEventIndex {
    by_turn_id: HashMap<String, EventMsg>,
    input_turn_ids: HashSet<String>,
    current_turn_id: Option<String>,
    inputs_trusted: bool,
    trusted: bool,
    pending_terminal_appends: usize,
    revision: u64,
}

impl TerminalEventIndex {
    fn trusted_empty() -> Self {
        Self {
            by_turn_id: HashMap::new(),
            input_turn_ids: HashSet::new(),
            current_turn_id: None,
            inputs_trusted: true,
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
            match event {
                EventMsg::TurnStarted(event) => {
                    self.current_turn_id = Some(event.turn_id.clone());
                }
                EventMsg::UserMessage(_) => {
                    if let Some(turn_id) = &self.current_turn_id {
                        self.input_turn_ids.insert(turn_id.clone());
                    }
                }
                EventMsg::ItemCompleted(event)
                    if matches!(&event.item, codex_protocol::items::TurnItem::UserMessage(_)) =>
                {
                    self.input_turn_ids.insert(event.turn_id.clone());
                }
                EventMsg::ThreadRolledBack(_) => {
                    // A receipt from a removed prefix is not custody evidence.
                    // Leave historical rollback reconstruction to the fallback.
                    self.input_turn_ids.clear();
                    self.current_turn_id = None;
                    self.inputs_trusted = false;
                }
                _ => {}
            }
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
            matches!(item, RolloutItem::EventMsg(event) if terminal_event_turn_id(event).is_some()
                || matches!(event, EventMsg::TurnStarted(_) | EventMsg::UserMessage(_) | EventMsg::ThreadRolledBack(_))
                || matches!(event, EventMsg::ItemCompleted(event) if matches!(&event.item, codex_protocol::items::TurnItem::UserMessage(_))))
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
/// If initialization returns early after persistence has been opened, dropping this guard schedules
/// best-effort cleanup on the current Tokio runtime to discard
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
        let Some(live_thread) = self.live_thread.as_ref() else {
            return;
        };
        if let Err(err) = live_thread.discard().await {
            warn!("failed to discard thread persistence for failed session init: {err}");
        }
        self.commit();
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
            writer_retired: Default::default(),
            terminal_events: Arc::new(StdMutex::new(TerminalEventIndex::trusted_empty())),
            fork_history: Arc::default(),
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
        let terminal_events = params
            .history
            .as_deref()
            .map(|history| TerminalEventIndex::from_items(history));
        let metadata_sync = ThreadMetadataSync::for_resume(&params);
        thread_store.resume_thread(params).await?;
        let live_thread = Self {
            thread_id,
            history_mode,
            thread_store,
            metadata_sync: Arc::new(Mutex::new(metadata_sync)),
            writer_retired: Default::default(),
            terminal_events: Arc::new(StdMutex::new(
                terminal_events.unwrap_or_else(TerminalEventIndex::trusted_empty),
            )),
            fork_history: Arc::default(),
            persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
        };
        let mut init_guard = LiveThreadInitGuard::new(Some(live_thread.clone()));
        if should_load_history {
            match live_thread
                .thread_store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived,
                })
                .await
            {
                Ok(history) => {
                    live_thread
                        .metadata_sync
                        .lock()
                        .await
                        .record_resume_history(&history.items);
                    *live_thread
                        .terminal_events
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        TerminalEventIndex::from_items(&history.items);
                }
                Err(err) => {
                    init_guard.discard().await;
                    return Err(err);
                }
            }
        }
        init_guard.commit();
        Ok(live_thread)
    }

    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(item_count = raw_items.len())
    )]
    /// Append history and apply its metadata. A metadata failure can occur after history
    /// was accepted; retry pending metadata with a barrier instead of replaying the batch.
    pub async fn append_items(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.append_items_with_durability(raw_items, true, false)
            .await
    }

    /// Queue items in canonical order and defer rollout plus metadata durability to the next
    /// persist/flush/shutdown barrier.
    pub async fn append_items_ordered(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.append_items_with_durability(raw_items, false, false)
            .await
    }

    /// Capture one explicitly enabled diagnostic observation through the existing ordered
    /// writer. No per-delta flush: the normal terminal barrier owns durability.
    pub async fn append_activity_event_ordered(&self, event: &EventMsg) -> ThreadStoreResult<()> {
        if !is_rollout_activity_event(event) {
            return Ok(());
        }
        self.append_items_with_durability(&[RolloutItem::EventMsg(event.clone())], false, true)
            .await
    }

    pub fn should_persist_event(&self, event: &EventMsg) -> bool {
        should_persist_event_msg(event, self.history_mode)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    async fn append_items_with_durability(
        &self,
        raw_items: &[RolloutItem],
        durable: bool,
        capture_activity: bool,
    ) -> ThreadStoreResult<()> {
        // Empty appends are intentionally ignored rather than represented as zero-sized batches.
        if raw_items.is_empty() {
            return Ok(());
        }
        let (items, measurement) = if self.persistence_telemetry.is_enabled() {
            let (items, measurement) = measure_and_filter_rollout_items_with_activity(
                raw_items,
                self.history_mode,
                capture_activity,
            );
            (items, Some(measurement))
        } else {
            (
                persisted_rollout_items_with_activity(
                    raw_items,
                    self.history_mode,
                    capture_activity,
                ),
                None,
            )
        };
        if items.is_empty() {
            return Ok(());
        }
        // Keep canonical submission, observation and metadata acknowledgement in one
        // per-thread critical section, shared with explicit updates and barriers.
        let mut metadata_sync = self.metadata_sync.lock().await;
        let observation_was_uncertain = metadata_sync.history_observation_uncertain;
        // Invalidate before submitting: cancellation or an uncertain append
        // must never leave a reusable snapshot of an older accepted prefix.
        *self
            .fork_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Weak::new();
        if observation_was_uncertain {
            // Settle any previously admitted write before submitting a successor,
            // even when rebuilding its metadata projection is unavailable.
            self.thread_store.persist_thread(self.thread_id).await?;
        }
        let terminal_append =
            TerminalAppendGuard::new(Arc::clone(&self.terminal_events), items.as_slice());
        // A failed projection recovery must not prevent a distinct canonical
        // append. The store still owns stream ordering; do not replay the
        // uncertain batch or acknowledge metadata derived from a partial prefix.
        metadata_sync.history_observation_uncertain = true;
        self.thread_store
            .append_persisted_items_ordered(self.thread_id, items.as_slice())
            .await?;
        // Commit measurements in canonical acceptance order, not arrival order.
        // A later durability failure does not undo an accepted append.
        if let Some(measurement) = measurement.as_ref() {
            self.persistence_telemetry
                .record_batch(raw_items, measurement);
        }
        terminal_append.commit(items.as_slice());
        let update = if observation_was_uncertain {
            if !durable {
                // Ordered appends defer projection repair to the next barrier.
                return Ok(());
            }
            self.recover_uncertain_metadata(&mut metadata_sync).await?;
            metadata_sync.take_pending_update_for_existing_history()
        } else {
            let update = metadata_sync.observe_appended_items(items.as_slice());
            metadata_sync.history_observation_uncertain = false;
            update
        };
        // Acceptance and durability are separate: a failed barrier must retain the
        // observed metadata for flush recovery, without resubmitting the history.
        if durable {
            self.thread_store.flush_thread(self.thread_id).await?;
        }
        if durable && let Some(update) = update {
            let applied = self
                .thread_store
                .update_thread_metadata_applied(UpdateThreadMetadataParams {
                    thread_id: self.thread_id,
                    patch: update.patch.clone(),
                    include_archived: true,
                })
                .await?;
            if applied {
                metadata_sync.mark_pending_update_applied(&update);
            }
        }
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    pub async fn persist(&self) -> ThreadStoreResult<()> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.thread_store.persist_thread(self.thread_id).await?;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        let update = metadata_sync.take_pending_update();
        self.apply_pending_metadata_update(&mut metadata_sync, update)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    pub async fn flush(&self) -> ThreadStoreResult<()> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.thread_store.flush_thread(self.thread_id).await?;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        let update = metadata_sync.take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(&mut metadata_sync, update)
            .await
    }

    /// [`Self::flush`] with the store's stable-storage barrier, for terminal checkpoints.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    pub async fn flush_durable(&self) -> ThreadStoreResult<()> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.thread_store
            .flush_thread_durable(self.thread_id)
            .await?;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        let update = metadata_sync.take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(&mut metadata_sync, update)
            .await
    }

    pub async fn shutdown(&self) -> ThreadStoreResult<()> {
        let thread = self.clone();
        // Keep phase publication with the accepted retirement, even when its
        // observer is cancelled between writer exit and metadata settlement.
        tokio::spawn(async move { thread.shutdown_owned().await }).await
            .map_err(|error| crate::ThreadStoreError::Internal { message: error.to_string() })?
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    async fn shutdown_owned(&self) -> ThreadStoreResult<()> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        if !self.writer_retired.load(std::sync::atomic::Ordering::Acquire) {
            self.thread_store.shutdown_thread(self.thread_id).await?;
            self.writer_retired.store(true, std::sync::atomic::Ordering::Release);
        }
        let update = metadata_sync.take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(&mut metadata_sync, update)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    pub async fn discard(&self) -> ThreadStoreResult<()> {
        let _metadata_sync = self.metadata_sync.lock().await;
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

    /// Proves input custody without materializing history on the trusted live
    /// path. Serialize the readable persistence barrier with canonical appends;
    /// an accepted in-memory append alone is never a durable receipt.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Binds the input receipt to the successfully flushed canonical prefix"
    )]
    pub async fn durable_input_recorded(&self, turn_id: &str) -> ThreadStoreResult<Option<bool>> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.thread_store.flush_thread(self.thread_id).await?;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        let update = metadata_sync.take_pending_update_for_existing_history();
        self.apply_pending_metadata_update(&mut metadata_sync, update).await?;
        let index = self
            .terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorded = index.input_turn_ids.contains(turn_id);
        Ok((index.trusted
            && index.pending_terminal_appends == 0
            && (index.inputs_trusted || recorded))
            .then_some(recorded))
    }

    /// Returns the first terminal event accepted into the canonical stream for `turn_id`.
    /// An indexed result does not itself establish durability.
    ///
    /// Successful live appends and resume history keep this lookup process-local. A full history
    /// read is reserved for an ambiguous append whose future was cancelled or returned an error.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes canonical history, metadata commits and recovery across asynchronous store operations"
    )]
    pub async fn terminal_event(
        &self,
        turn_id: &str,
        include_archived: bool,
    ) -> ThreadStoreResult<Option<EventMsg>> {
        {
            let terminal_events = self
                .terminal_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if terminal_events.trusted && terminal_events.pending_terminal_appends == 0 {
                return Ok(terminal_events.by_turn_id.get(turn_id).cloned());
            }
        }

        let _metadata_sync = self.metadata_sync.lock().await;
        let revision = {
            let terminal_events = self
                .terminal_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // An append or another recovery may have repaired the index while we waited.
            if terminal_events.trusted && terminal_events.pending_terminal_appends == 0 {
                return Ok(terminal_events.by_turn_id.get(turn_id).cloned());
            }
            terminal_events.revision
        };
        // Recovery reads must include accepted ordered appends, even in a lazy writer.
        // The normal trusted lookup above never forces persistence.
        self.thread_store.persist_thread(self.thread_id).await?;
        let history = self.load_history(include_archived).await?;
        let mut replacement = TerminalEventIndex::from_items(&history.items);
        let loaded_event = replacement.by_turn_id.get(turn_id).cloned();
        let mut terminal_events = self
            .terminal_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if terminal_events.revision == revision && terminal_events.pending_terminal_appends == 0 {
            replacement.revision = terminal_events.revision;
            std::mem::swap(&mut *terminal_events, &mut replacement);
        } else if terminal_events.trusted && terminal_events.pending_terminal_appends == 0 {
            return Ok(terminal_events.by_turn_id.get(turn_id).cloned());
        }
        drop(terminal_events);
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

    /// Share one flushed immutable history among overlapping sibling forks.
    /// The weak entry retains no history once consumers retire. The canonical
    /// append lock binds reuse to the accepted prefix, including failed or
    /// cancelled append attempts; filesystem timestamps are not involved.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Binds a fork snapshot to the canonical accepted history prefix"
    )]
    pub async fn fork_history_snapshot(&self) -> ThreadStoreResult<Arc<StoredThreadHistory>> {
        let mut metadata_sync = self.metadata_sync.lock().await;
        self.thread_store.flush_thread(self.thread_id).await?;
        self.recover_uncertain_metadata(&mut metadata_sync).await?;
        let update = metadata_sync.take_pending_update();
        self.apply_pending_metadata_update(&mut metadata_sync, update)
            .await?;
        if let Some(history) = self
            .fork_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade()
        {
            return Ok(history);
        }
        let history = self.read_thread(true, true).await?.history.ok_or_else(|| {
            crate::ThreadStoreError::Internal {
                message: format!("thread history unavailable for fork: {}", self.thread_id),
            }
        })?;
        let history = Arc::new(history);
        *self
            .fork_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&history);
        Ok(history)
    }

    pub async fn update_metadata(
        &self,
        patch: ThreadMetadataPatch,
        include_archived: bool,
    ) -> ThreadStoreResult<StoredThread> {
        // Waiting for admission stays cancellable. Once admitted, the store mutation and
        // live observation must share an owner: local compatibility finalizers can still
        // be pending after SQLite commits, even if the caller stops waiting.
        let mut metadata_sync = Arc::clone(&self.metadata_sync).lock_owned().await;
        let live_thread = self.clone();
        let thread_id = self.thread_id;
        tokio::spawn(async move {
            let result: ThreadStoreResult<_> = async {
                live_thread
                    .recover_uncertain_metadata(&mut metadata_sync)
                    .await?;
                let update = metadata_sync.take_pending_update();
                live_thread
                    .apply_pending_metadata_update(&mut metadata_sync, update)
                    .await?;
                let explicit_patch = patch.clone();
                *live_thread
                    .fork_history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Weak::new();
                let thread = live_thread
                    .thread_store
                    .update_thread_metadata(UpdateThreadMetadataParams {
                        thread_id,
                        patch,
                        include_archived,
                    })
                    .await?;
                metadata_sync.record_explicit_update(explicit_patch);
                Ok(thread)
            }
            .await;
            if let Err(error) = &result {
                warn!(%thread_id, %error, "live metadata update did not settle successfully");
            }
            result
        })
        .await
        .map_err(|error| crate::ThreadStoreError::Internal {
            message: format!("live metadata update owner failed for {thread_id}: {error}"),
        })?
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

    async fn recover_uncertain_metadata(
        &self,
        sync: &mut ThreadMetadataSync,
    ) -> ThreadStoreResult<()> {
        if sync.history_observation_uncertain {
            self.thread_store.flush_thread(self.thread_id).await?;
            // Never replay an uncertain write. The store's authoritative history
            // barrier waits for its admitted projection worker as well.
            let history = self
                .thread_store
                .load_history(LoadThreadHistoryParams {
                    thread_id: self.thread_id,
                    include_archived: true,
                })
                .await;
            match history {
                Ok(history) => {
                    sync.observe_appended_items(&history.items);
                }
                // A barrier with no accepted items leaves a genuinely lazy local
                // thread pathless; do not invent observed facts for that case.
                Err(crate::ThreadStoreError::RolloutNotMaterialized { path, .. })
                    if matches!(tokio::fs::try_exists(&path).await, Ok(false)) => {}
                Err(error) => return Err(error),
            }
            sync.history_observation_uncertain = false;
        }
        Ok(())
    }

    async fn apply_pending_metadata_update(
        &self,
        metadata_sync: &mut ThreadMetadataSync,
        update: Option<crate::thread_metadata_sync::PendingThreadMetadataPatch>,
    ) -> ThreadStoreResult<()> {
        let Some(update) = update else {
            return Ok(());
        };
        let applied = self
            .thread_store
            .update_thread_metadata_applied(UpdateThreadMetadataParams {
                thread_id: self.thread_id,
                patch: update.patch.clone(),
                include_archived: true,
            })
            .await?;
        if applied {
            metadata_sync.mark_pending_update_applied(&update);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryThreadStore;
    use crate::LocalThreadStoreConfig;
    use crate::ThreadPersistenceMetadata;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::TurnCompleteEvent;

    fn create_params(thread_id: ThreadId, cwd: PathBuf) -> CreateThreadParams {
        CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Legacy,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(cwd),
                model_provider: "test-provider".to_string(),
            },
        }
    }

    fn terminal(turn: &str) -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
            surfaced_result: None,
            turn_id: turn.to_string(),
            last_agent_message: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        }))
    }

    fn input_start(turn_id: &str) -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
            turn_id: turn_id.into(), trace_id: None, started_at: None,
            model_context_window: None, collaboration_mode_kind: Default::default(),
        }))
    }

    fn user_input() -> RolloutItem {
        RolloutItem::EventMsg(EventMsg::UserMessage(codex_protocol::protocol::UserMessageEvent {
            message: "input".into(), ..Default::default()
        }))
    }

    #[tokio::test]
    async fn durable_input_receipts_avoid_history_reads_and_reject_uncertain_prefixes() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(store.clone(), create_params(ThreadId::new(), PathBuf::new())).await.unwrap();
        live.append_items_ordered(&[input_start("first")]).await.unwrap();
        assert_eq!(live.durable_input_recorded("first").await.unwrap(), Some(false));
        live.append_items_ordered(&[user_input()]).await.unwrap();
        assert_eq!(live.durable_input_recorded("first").await.unwrap(), Some(true));
        live.append_items_ordered(&[input_start("second")]).await.unwrap();
        assert_eq!(live.durable_input_recorded("second").await.unwrap(), Some(false));
        assert_eq!(live.durable_input_recorded("first").await.unwrap(), Some(true));
        let calls = store.calls().await;
        assert_eq!(calls.load_history, 0, "trusted handoffs never materialize growing history");
        assert_eq!(calls.read_thread, 0);
        assert_eq!(calls.flush_thread, 4, "every receipt is bound to a successful barrier");
        live.append_items_ordered(&[RolloutItem::EventMsg(EventMsg::ThreadRolledBack(codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 }))]).await.unwrap();
        assert_eq!(live.durable_input_recorded("first").await.unwrap(), None, "rollback needs authoritative replay");
        live.append_items_ordered(&[input_start("third"), user_input()]).await.unwrap();
        assert_eq!(live.durable_input_recorded("third").await.unwrap(), Some(true));
        drop(TerminalAppendGuard::new(live.terminal_events.clone(), &[user_input()]));
        assert_eq!(live.durable_input_recorded("third").await.unwrap(), None, "ambiguous append cannot issue receipts");
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Queues append and receipt behind the canonical owner to exercise their admission order"
    )]
    async fn durable_input_receipt_serializes_with_an_admitted_append() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(store.clone(), create_params(ThreadId::new(), PathBuf::new())).await.unwrap();
        let items = [input_start("racing"), user_input()];
        let owner = live.metadata_sync.lock().await;
        let mut append = Box::pin(live.append_items_ordered(&items));
        assert!(matches!(std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(append.as_mut(), cx))).await, std::task::Poll::Pending));
        let mut receipt = Box::pin(live.durable_input_recorded("racing"));
        assert!(matches!(std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(receipt.as_mut(), cx))).await, std::task::Poll::Pending));
        assert_eq!(store.calls().await.flush_thread, 0);
        drop(owner);
        let (append, receipt) = tokio::join!(append, receipt);
        append.unwrap();
        assert_eq!(receipt.unwrap(), Some(true));
        assert_eq!(store.calls().await.load_history, 0);
    }

    #[tokio::test]
    async fn durable_input_receipts_cover_modern_completed_user_items() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let mut params = create_params(thread_id, PathBuf::new());
        params.history_mode = ThreadHistoryMode::Paginated;
        let live = LiveThread::create(store, params).await.unwrap();
        let item = codex_protocol::items::TurnItem::UserMessage(codex_protocol::items::UserMessageItem::new(&[]));
        live.append_items_ordered(&[RolloutItem::EventMsg(EventMsg::ItemStarted(codex_protocol::protocol::ItemStartedEvent { thread_id, turn_id: "modern".into(), item: item.clone(), started_at_ms: 0 }))]).await.unwrap();
        assert_eq!(live.durable_input_recorded("modern").await.unwrap(), Some(false));
        live.append_items_ordered(&[RolloutItem::EventMsg(EventMsg::ItemCompleted(codex_protocol::protocol::ItemCompletedEvent { thread_id, turn_id: "modern".into(), item, completed_at_ms: 0 }))]).await.unwrap();
        assert_eq!(live.durable_input_recorded("modern").await.unwrap(), Some(true));
        assert_eq!(live.durable_input_recorded("other").await.unwrap(), Some(false));
    }

    #[tokio::test]
    async fn durable_input_receipt_failure_retains_custody_until_flush_recovers() {
        let home = tempfile::TempDir::new().unwrap();
        let store = Arc::new(LocalThreadStore::new(LocalThreadStoreConfig {
            codex_home: home.path().to_path_buf(), sqlite_home: home.path().to_path_buf(), default_model_provider_id: "test-provider".into(),
        }, None));
        let thread_id = ThreadId::new();
        let live = LiveThread::create(store.clone(), create_params(thread_id, home.path().to_path_buf())).await.unwrap();
        let path = store.live_rollout_path(thread_id).await.unwrap();
        live.append_items_ordered(&[input_start("turn"), user_input()]).await.unwrap();
        assert!(!path.exists());
        std::fs::create_dir_all(&path).unwrap();
        assert!(live.durable_input_recorded("turn").await.is_err());
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(live.durable_input_recorded("turn").await.unwrap(), Some(true));
        live.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rollout_activity_failed_flush_retries_without_losing_or_duplicating_events() {
        let home = tempfile::TempDir::new().unwrap();
        let store = Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: home.path().to_path_buf(),
                sqlite_home: home.path().to_path_buf(),
                default_model_provider_id: "test-provider".into(),
            },
            None,
        ));
        let thread_id = ThreadId::new();
        let live = LiveThread::create(
            store.clone(),
            create_params(thread_id, home.path().to_path_buf()),
        )
        .await
        .unwrap();
        let path = store.live_rollout_path(thread_id).await.unwrap();
        let event =
            EventMsg::McpToolCallProgress(codex_protocol::protocol::McpToolCallProgressEvent {
                call_id: "call".into(),
                message: "halfway".into(),
                progress: Some(1.0),
                total: Some(2.0),
            });
        live.append_activity_event_ordered(&event).await.unwrap();
        assert!(!path.exists(), "activity must not force a per-event flush");
        // A directory occupying the file name is a deterministic storage failure, even
        // when the test process has privileges that bypass read-only permission bits.
        std::fs::create_dir_all(&path).unwrap();
        assert!(live.persist().await.is_err());
        std::fs::remove_dir(&path).unwrap();
        live.persist().await.unwrap();
        live.shutdown().await.unwrap();
        let (items, _, errors) = codex_rollout::RolloutRecorder::load_rollout_items(&path)
            .await
            .unwrap();
        assert_eq!(errors, 0);
        let events: Vec<_> = items
            .into_iter()
            .filter_map(|item| match item {
                RolloutItem::EventMsg(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            serde_json::to_value(vec![event]).unwrap()
        );
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Exercises cancellation while the metadata owner blocks an activity append"
    )]
    async fn rollout_activity_cancelled_before_admission_does_not_duplicate_on_retry() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(
            store.clone(),
            create_params(ThreadId::new(), PathBuf::new()),
        )
        .await
        .unwrap();
        let event =
            EventMsg::McpToolCallProgress(codex_protocol::protocol::McpToolCallProgressEvent {
                call_id: "call".into(),
                message: "halfway".into(),
                progress: Some(1.0),
                total: Some(2.0),
            });
        let owner = live.metadata_sync.lock().await;
        let mut append = Box::pin(live.append_activity_event_ordered(&event));
        assert!(matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                append.as_mut(),
                cx
            )))
            .await,
            std::task::Poll::Pending
        ));
        assert_eq!(store.calls().await.append_items, 0);
        drop(append);
        drop(owner);
        live.append_activity_event_ordered(&event).await.unwrap();
        assert_eq!(store.calls().await.append_items, 1);
        assert_eq!(store.calls().await.flush_thread, 0);
        let history = live.load_history(true).await.unwrap();
        let events: Vec<_> = history
            .items
            .into_iter()
            .filter_map(|item| match item {
                RolloutItem::EventMsg(event) => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            serde_json::to_value(events).unwrap(),
            serde_json::to_value(vec![event]).unwrap()
        );
    }

    #[tokio::test]
    async fn terminal_recovery_includes_queued_local_appends() {
        let home = tempfile::TempDir::new().expect("temp dir");
        let store = Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig {
                codex_home: home.path().to_path_buf(),
                sqlite_home: home.path().to_path_buf(),
                default_model_provider_id: "test-provider".to_string(),
            },
            None,
        ));
        let thread_id = ThreadId::new();
        let live = LiveThread::create(
            store.clone(),
            create_params(thread_id, home.path().to_path_buf()),
        )
        .await
        .expect("create live thread");
        let path = store
            .live_rollout_path(thread_id)
            .await
            .expect("rollout path");
        live.append_items_ordered(&[terminal("first")])
            .await
            .expect("queue terminal");
        assert!(!path.exists(), "ordered append remains lazy");
        // Dropping an uncommitted append guard models the ambiguous cancellation path.
        drop(TerminalAppendGuard::new(
            live.terminal_events.clone(),
            &[terminal("cancelled")],
        ));
        assert!(
            matches!(live.terminal_event("first", true).await.expect("recovery"),
            Some(EventMsg::TurnComplete(event)) if event.turn_id == "first")
        );
        assert!(
            live.terminal_event("cancelled", true)
                .await
                .expect("absent event")
                .is_none()
        );
        assert!(live.terminal_events.lock().expect("index").trusted);
        let history = live
            .load_history(true)
            .await
            .expect("persisted recovery history");
        assert_eq!(
            history
                .items
                .iter()
                .filter(|item| matches!(item,
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) if event.turn_id == "first"))
                .count(),
            1
        );
        live.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Holds the metadata owner to assert that history submission waits for its release"
    )]
    async fn append_waits_for_metadata_owner_before_submitting_history() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(
            store.clone(),
            create_params(ThreadId::new(), PathBuf::new()),
        )
        .await
        .expect("create live thread");
        let owner = live.metadata_sync.lock().await;
        let items = [terminal("ordered")];
        let append = live.append_items_ordered(&items);
        tokio::pin!(append);
        assert!(matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                append.as_mut(),
                cx
            )))
            .await,
            std::task::Poll::Pending
        ));
        assert_eq!(
            store.calls().await.append_items,
            0,
            "history must wait for the same owner as metadata"
        );
        drop(owner);
        append.await.expect("append after owner releases");
        assert_eq!(store.calls().await.append_items, 1);
        assert!(
            live.terminal_event("ordered", true)
                .await
                .expect("lookup")
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_rename_after_commit_settles_before_live_append_and_barrier() {
        for fail_name_index in [false, true] {
            let home = tempfile::TempDir::new().expect("temp dir");
            let runtime = codex_state::StateRuntime::init(
                home.path().to_path_buf(),
                "test-provider".to_string(),
            )
            .await
            .expect("state db");
            let store = Arc::new(LocalThreadStore::new(
                LocalThreadStoreConfig {
                    codex_home: home.path().to_path_buf(),
                    sqlite_home: home.path().to_path_buf(),
                    default_model_provider_id: "test-provider".to_string(),
                },
                Some(runtime.clone()),
            ));
            let thread_id = ThreadId::new();
            let live = LiveThread::create(
                store,
                create_params(thread_id, home.path().to_path_buf()),
            )
            .await
            .expect("create");
            // Use the compatibility writer's actual lock to hold its finalizer after
            // the SQLite commit, without relying on sleeps or scheduler ordering.
            let index_lock = std::fs::File::options()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(home.path().join("session_index.jsonl.lock"))
                .expect("index lock file");
            index_lock.lock().expect("hold index finalizer");
            let renaming = live.clone();
            let rename = tokio::spawn(async move {
                renaming
                    .update_metadata(
                        ThreadMetadataPatch {
                            name: Some(Some("custom title".to_string())),
                            ..Default::default()
                        },
                        false,
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if runtime
                        .get_thread(thread_id)
                        .await
                        .expect("read committed title")
                        .is_some_and(|metadata| metadata.title == "custom title")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("rename commits before compatibility finalizer");
            assert!(!rename.is_finished(), "name-index finalizer is still blocked");
            rename.abort();
            assert!(rename.await.expect_err("cancel observer").is_cancelled());
            assert!(
                live.metadata_sync.try_lock().is_err(),
                "observer cancellation must not release the live metadata owner"
            );

            let mut flush = Box::pin(live.flush());
            assert!(matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                    flush.as_mut(),
                    cx
                )))
                .await,
                std::task::Poll::Pending
            ));
            let items = [RolloutItem::EventMsg(EventMsg::UserMessage(
                codex_protocol::protocol::UserMessageEvent {
                    message: "first user message".to_string(),
                    ..Default::default()
                },
            ))];
            let mut append = Box::pin(live.append_items(&items));
            assert!(matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                    append.as_mut(),
                    cx
                )))
                .await,
                std::task::Poll::Pending
            ));
            if fail_name_index {
                // A committed rename still succeeds when the compatibility write fails.
                std::fs::create_dir(home.path().join("session_index.jsonl"))
                    .expect("inject name-index write failure");
            }
            drop(index_lock);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                flush.await.expect("barrier after rename settles");
                append.await.expect("append after rename settles");
            })
            .await
            .expect("live successors complete");
            let metadata = runtime
                .get_thread(thread_id)
                .await
                .expect("read sqlite metadata")
                .expect("sqlite metadata");
            assert_eq!(metadata.title, "custom title");
            assert_eq!(
                metadata.first_user_message.as_deref(),
                Some("first user message")
            );
            if !fail_name_index {
                assert_eq!(
                    codex_rollout::find_thread_name_by_id(home.path(), &thread_id)
                        .await
                        .expect("compatibility name"),
                    Some("custom title".to_string())
                );
            }
            live.shutdown().await.expect("shutdown");
            runtime.close().await;
        }
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Holds the metadata owner to cancel an explicit update before admission"
    )]
    async fn cancelled_rename_before_admission_does_not_start_store_mutation() {
        let store = Arc::new(InMemoryThreadStore::default());
        let live = LiveThread::create(
            store.clone(),
            create_params(ThreadId::new(), PathBuf::new()),
        )
        .await
        .expect("create");
        let owner = live.metadata_sync.lock().await;
        let mut rename = Box::pin(live.update_metadata(
            ThreadMetadataPatch {
                name: Some(Some("not admitted".to_string())),
                ..Default::default()
            },
            false,
        ));
        assert!(matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                rename.as_mut(),
                cx
            )))
            .await,
            std::task::Poll::Pending
        ));
        drop(rename);
        drop(owner);
        live.flush().await.expect("barrier after cancelled waiter");
        assert_eq!(store.calls().await.update_thread_metadata, 0);
    }
}
