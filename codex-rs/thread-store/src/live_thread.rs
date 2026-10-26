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
        if metadata_sync.append_ordering_uncertain {
            // Settle any previously admitted write before submitting a successor,
            // even when rebuilding its metadata projection is unavailable.
            self.thread_store.persist_thread(self.thread_id).await?;
            metadata_sync.append_ordering_uncertain = false;
        }
        let terminal_append =
            TerminalAppendGuard::new(Arc::clone(&self.terminal_events), items.as_slice());
        // A failed projection recovery must not prevent a distinct canonical
        // append. The store still owns stream ordering; do not replay the
        // uncertain batch or acknowledge metadata derived from a partial prefix.
        metadata_sync.history_observation_uncertain = true;
        metadata_sync.append_ordering_uncertain = true;
        self.thread_store
            .append_persisted_items_ordered(self.thread_id, items.as_slice())
            .await?;
        metadata_sync.append_ordering_uncertain = false;
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
            sync.append_ordering_uncertain = false;
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

