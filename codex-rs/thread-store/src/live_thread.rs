use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutPersistenceTelemetry;
use codex_rollout::is_persisted_rollout_item;
use codex_rollout::measure_and_filter_rollout_items;
use codex_rollout::persisted_rollout_items;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tracing::warn;

use crate::AppendThreadItemsReceipt;
use crate::CreateThreadParams;
use crate::LoadThreadHistoryParams;
use crate::LocalThreadStore;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::ThreadStoreError;
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
    metadata_projection_order: Arc<MetadataProjectionOrder>,
    persistence_telemetry: RolloutPersistenceTelemetry,
}

struct MetadataProjectionOrder {
    next_sequence: Mutex<u64>,
    advanced: Notify,
}

impl MetadataProjectionOrder {
    fn new() -> Self {
        Self {
            next_sequence: Mutex::new(1),
            advanced: Notify::new(),
        }
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
        let metadata_sync = ThreadMetadataSync::for_create(&params).await;
        thread_store.create_thread(params).await?;
        Ok(Self {
            thread_id,
            history_mode,
            thread_store,
            metadata_sync: Arc::new(Mutex::new(metadata_sync)),
            metadata_projection_order: Arc::new(MetadataProjectionOrder::new()),
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
                Ok(history) => metadata_sync.record_resume_history(&history.items),
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
            metadata_projection_order: Arc::new(MetadataProjectionOrder::new()),
            persistence_telemetry: RolloutPersistenceTelemetry::new(thread_id),
        })
    }

    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(item_count = raw_items.len())
    )]
    pub async fn append_items(&self, raw_items: &[RolloutItem]) -> ThreadStoreResult<()> {
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
        if items.is_empty() {
            if let Some(measurement) = measurement.as_ref() {
                self.persistence_telemetry
                    .record_batch(raw_items, measurement);
            }
            return Ok(());
        }
        let receipt = self
            .thread_store
            .append_persisted_items(self.thread_id, items.as_slice())
            .await?;
        let projection = self.schedule_metadata_projection(receipt, items);
        if let Some(measurement) = measurement.as_ref() {
            self.persistence_telemetry
                .record_batch(raw_items, measurement);
        }
        Self::await_metadata_projection(projection).await
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn append_item(&self, item: &RolloutItem) -> ThreadStoreResult<()> {
        let persisted = is_persisted_rollout_item(item, self.history_mode);
        let measurement = if self.persistence_telemetry.is_enabled() {
            let (_, measurement) =
                measure_and_filter_rollout_items(std::slice::from_ref(item), self.history_mode);
            Some(measurement)
        } else {
            None
        };
        if persisted {
            let receipt = self
                .thread_store
                .append_persisted_items(self.thread_id, std::slice::from_ref(item))
                .await?;
            let projection =
                self.schedule_metadata_projection(receipt, vec![item.clone()]);
            if let Some(measurement) = measurement.as_ref() {
                self.persistence_telemetry
                    .record_batch(std::slice::from_ref(item), measurement);
            }
            return Self::await_metadata_projection(projection).await;
        }
        if let Some(measurement) = measurement.as_ref() {
            self.persistence_telemetry
                .record_batch(std::slice::from_ref(item), measurement);
        }
        Ok(())
    }

    fn schedule_metadata_projection(
        &self,
        receipt: AppendThreadItemsReceipt,
        items: Vec<RolloutItem>,
    ) -> oneshot::Receiver<ThreadStoreResult<()>> {
        let live_thread = self.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let result = live_thread
                .observe_appended_items_in_receipt_order(receipt, items.as_slice())
                .await;
            let _ = tx.send(result);
        });
        rx
    }

    async fn await_metadata_projection(
        projection: oneshot::Receiver<ThreadStoreResult<()>>,
    ) -> ThreadStoreResult<()> {
        projection.await.map_err(|err| ThreadStoreError::Internal {
            message: format!("ordered metadata projection task stopped before acknowledgement: {err}"),
        })?
    }

    async fn observe_appended_items_in_receipt_order(
        &self,
        receipt: AppendThreadItemsReceipt,
        items: &[RolloutItem],
    ) -> ThreadStoreResult<()> {
        let sequence = receipt.sequence();
        loop {
            let advanced = self.metadata_projection_order.advanced.notified();
            let mut next_sequence = self.metadata_projection_order.next_sequence.lock().await;
            if sequence < *next_sequence {
                return Err(ThreadStoreError::Conflict {
                    message: format!(
                        "append receipt sequence {sequence} was already projected; next sequence is {}",
                        *next_sequence
                    ),
                });
            }
            if sequence > *next_sequence {
                drop(next_sequence);
                advanced.await;
                continue;
            }

            let result = self.observe_appended_items(items).await;
            *next_sequence = (*next_sequence).saturating_add(1);
            drop(next_sequence);
            self.metadata_projection_order.advanced.notify_waiters();
            return result;
        }
    }

    async fn observe_appended_items(&self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        let update = self
            .metadata_sync
            .lock()
            .await
            .observe_appended_items(items);
        if let Some(update) = update {
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
        self.flush_pending_metadata_update_for_existing_history()
            .await?;
        self.thread_store.shutdown_thread(self.thread_id).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::InMemoryThreadStore;
    use crate::ThreadPersistenceMetadata;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::protocol::AgentMessageContentDeltaEvent;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::PlanDeltaEvent;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::UserMessageEvent;

    #[tokio::test]
    async fn empty_and_transient_only_batches_make_zero_store_calls() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let live_thread = LiveThread::create(
            store.clone(),
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
                metadata: ThreadPersistenceMetadata {
                    cwd: None,
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            },
        )
        .await
        .expect("create in-memory live thread");
        let transient = vec![
            RolloutItem::EventMsg(EventMsg::AgentMessageContentDelta(
                AgentMessageContentDeltaEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                    delta: "first".to_string(),
                },
            )),
            RolloutItem::EventMsg(EventMsg::PlanDelta(PlanDeltaEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-2".to_string(),
                delta: "second".to_string(),
            })),
        ];

        live_thread
            .append_items(&[])
            .await
            .expect("empty append succeeds");
        live_thread
            .append_items(&transient)
            .await
            .expect("transient batch succeeds");
        live_thread
            .append_item(&transient[0])
            .await
            .expect("transient single append succeeds");

        let calls = store.calls().await;
        assert_eq!(calls.append_persisted_items, 0);
        assert_eq!(calls.append_items, 0);
    }

    #[tokio::test]
    async fn one_append_batch_coalesces_metadata_into_one_store_write() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let live_thread = LiveThread::create(
            store.clone(),
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
                metadata: ThreadPersistenceMetadata {
                    cwd: None,
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            },
        )
        .await
        .expect("create in-memory live thread");
        let user_message = |message: &str| {
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: message.to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            }))
        };

        live_thread
            .append_items(&[user_message("first"), user_message("second")])
            .await
            .expect("batched append");

        let calls = store.calls().await;
        assert_eq!(calls.append_persisted_items, 1);
        assert_eq!(calls.update_thread_metadata, 1);
    }

    #[tokio::test]
    async fn reverse_post_receipt_scheduling_preserves_sqlite_projection_order() {
        let store = Arc::new(InMemoryThreadStore::default());
        let thread_id = ThreadId::new();
        let live_thread = LiveThread::create(
            store.clone(),
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
                metadata: ThreadPersistenceMetadata {
                    cwd: None,
                    model_provider: "test-provider".to_string(),
                    memory_mode: ThreadMemoryMode::Enabled,
                },
            },
        )
        .await
        .expect("create in-memory live thread");
        let user_message = |message: &str| {
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: message.to_string(),
                images: None,
                local_images: Vec::new(),
                text_elements: Vec::new(),
                ..Default::default()
            }))
        };

        let mut second_projection = live_thread.schedule_metadata_projection(
            AppendThreadItemsReceipt::new(2),
            vec![user_message("second")],
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second_projection)
                .await
                .is_err(),
            "sequence two must wait even when it resumes first"
        );

        let first_projection = live_thread.schedule_metadata_projection(
            AppendThreadItemsReceipt::new(1),
            vec![user_message("first")],
        );
        LiveThread::await_metadata_projection(first_projection)
            .await
            .expect("first projection");
        LiveThread::await_metadata_projection(second_projection)
            .await
            .expect("second projection");

        let stored = ThreadStore::read_thread(
            store.as_ref(),
            ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            },
        )
        .await
        .expect("read projected metadata");
        assert_eq!(stored.preview, "first");
        assert_eq!(stored.first_user_message.as_deref(), Some("first"));
        assert_eq!(store.calls().await.update_thread_metadata, 2);
    }
}
