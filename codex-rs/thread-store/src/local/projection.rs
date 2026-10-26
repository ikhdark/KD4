use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Instant;
use std::time::SystemTime;

use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::ThreadHistoryTurnChange;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SortDirection;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::persisted_rollout_items;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

use super::LocalThreadStore;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::StoredThreadItem;
use crate::StoredTurn;
use crate::StoredTurnError;
use crate::StoredTurnItemsView;
use crate::StoredTurnStatus;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;

pub(super) type SharedLocalThreadProjection = Arc<LocalThreadProjectionEntry>;

/// Projections of threads without a live writer are cached only so consecutive pages avoid
/// reloading history; each holds the thread's materialized history twice.
const MAX_UNLOADED_PROJECTIONS: usize = 4;

pub(super) struct LocalThreadProjectionEntry {
    state: Mutex<LocalThreadProjection>,
    operation_gate: Arc<Semaphore>,
    source_version: Mutex<Option<(PathBuf, u64, Option<SystemTime>)>>,
    payload_versions: Mutex<Vec<codex_rollout::PayloadDependencyVersion>>,
    /// Source version whose last unloaded read had reconstruction gaps.
    gap_source_version: Mutex<Option<(PathBuf, u64, Option<SystemTime>)>>,
    gap_retry_after: Mutex<Option<Instant>>,
    last_unloaded_read: StdMutex<Option<Instant>>,
}

struct ProjectedItem {
    item: ThreadItem,
    ordinal: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProjectedItemKey {
    turn_id: String,
    item_id: String,
}

#[derive(Default)]
struct ProjectedTurn {
    metadata: Option<ThreadHistoryTurnChange>,
    items: Vec<ProjectedItem>,
    item_indexes: HashMap<String, usize>,
}

pub(super) struct LocalThreadProjection {
    builder: ThreadHistoryBuilder,
    initialized: bool,
    deferred_seed: Option<Arc<Vec<RolloutItem>>>,
    deferred_committed: Vec<RolloutItem>,
    source_partial: bool,
    turn_order: Vec<String>,
    turn_positions: HashMap<String, usize>,
    turns: HashMap<String, ProjectedTurn>,
    item_order: Vec<ProjectedItemKey>,
    item_positions: HashMap<ProjectedItemKey, usize>,
    next_item_ordinal: u64,
    pending_items: Vec<RolloutItem>,
}

impl Default for LocalThreadProjection {
    fn default() -> Self {
        Self {
            builder: ThreadHistoryBuilder::new(),
            initialized: false,
            deferred_seed: None,
            deferred_committed: Vec::new(),
            source_partial: false,
            turn_order: Vec::new(),
            turn_positions: HashMap::new(),
            turns: HashMap::new(),
            item_order: Vec::new(),
            item_positions: HashMap::new(),
            next_item_ordinal: 0,
            pending_items: Vec::new(),
        }
    }
}

impl LocalThreadProjection {
    fn initialize(&mut self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        *self = Self::default();
        self.initialized = true;
        self.source_partial = codex_rollout::ensure_rollout_source_integrity(items).is_err();
        let persisted_items = persisted_rollout_items(items, ThreadHistoryMode::Legacy);
        self.apply_persisted_items(persisted_items.as_slice())
    }

    fn apply_persisted_items(&mut self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.source_partial |= codex_rollout::ensure_rollout_source_integrity(items).is_err();
        if items.is_empty() {
            return Ok(());
        }
        let changes = self.builder.handle_rollout_items_with_changes(items);
        self.apply_changes(changes)
    }

    fn apply_changes(&mut self, changes: ThreadHistoryChangeSet) -> ThreadStoreResult<()> {
        let mut removed_turn = false;
        for turn_id in changes.removed_turn_ids {
            self.turns.remove(&turn_id);
            self.turn_order.retain(|candidate| candidate != &turn_id);
            removed_turn = true;
        }
        if removed_turn {
            self.rebuild_positions();
        }

        for change in changes.changed_items {
            let turn_id = change.turn_id;
            self.ensure_turn(turn_id.as_str());
            let item_key = change.item.id().to_string();
            let existing_index = self
                .turns
                .get(&turn_id)
                .and_then(|turn| turn.item_indexes.get(&item_key).copied());
            if let Some(index) = existing_index {
                let turn = self
                    .turns
                    .get_mut(&turn_id)
                    .ok_or_else(|| projection_invariant_error("projected turn was not inserted"))?;
                let item = turn.items.get_mut(index).ok_or_else(|| {
                    projection_invariant_error("projected item index is out of bounds")
                })?;
                item.item = change.item;
            } else {
                let ordinal = self.next_item_ordinal;
                self.next_item_ordinal = self.next_item_ordinal.saturating_add(1);
                {
                    let turn = self.turns.get_mut(&turn_id).ok_or_else(|| {
                        projection_invariant_error("projected turn was not inserted")
                    })?;
                    let index = turn.items.len();
                    turn.item_indexes.insert(item_key.clone(), index);
                    turn.items.push(ProjectedItem {
                        item: change.item,
                        ordinal,
                    });
                }
                let key = ProjectedItemKey {
                    turn_id,
                    item_id: item_key,
                };
                self.insert_item_key(key);
            }
        }

        for change in changes.changed_turns {
            let turn_id = change.turn_id.clone();
            self.ensure_turn(turn_id.as_str());
            self.turns
                .get_mut(&turn_id)
                .ok_or_else(|| projection_invariant_error("projected turn was not inserted"))?
                .metadata = Some(change);
        }
        Ok(())
    }

    fn ensure_turn(&mut self, turn_id: &str) {
        if self.turns.contains_key(turn_id) {
            return;
        }
        self.turn_positions
            .insert(turn_id.to_string(), self.turn_order.len());
        self.turn_order.push(turn_id.to_string());
        self.turns
            .insert(turn_id.to_string(), ProjectedTurn::default());
    }

    fn insert_item_key(&mut self, key: ProjectedItemKey) {
        let turn_position = self.turn_positions[&key.turn_id];
        let append = self
            .item_order
            .last()
            .is_none_or(|last| self.turn_positions[&last.turn_id] <= turn_position);
        if append {
            self.item_positions
                .insert(key.clone(), self.item_order.len());
            self.item_order.push(key);
            return;
        }

        let insertion_index = self
            .item_order
            .iter()
            .position(|existing| self.turn_positions[&existing.turn_id] > turn_position)
            .unwrap_or(self.item_order.len());
        self.item_order.insert(insertion_index, key);
        for (position, key) in self.item_order.iter().enumerate().skip(insertion_index) {
            self.item_positions.insert(key.clone(), position);
        }
    }

    fn rebuild_positions(&mut self) {
        self.turn_positions.clear();
        self.item_order.clear();
        self.item_positions.clear();
        for (turn_position, turn_id) in self.turn_order.iter().enumerate() {
            self.turn_positions.insert(turn_id.clone(), turn_position);
            let Some(turn) = self.turns.get(turn_id) else {
                continue;
            };
            for item in &turn.items {
                let key = ProjectedItemKey {
                    turn_id: turn_id.clone(),
                    item_id: item.item.id().to_string(),
                };
                self.item_positions
                    .insert(key.clone(), self.item_order.len());
                self.item_order.push(key);
            }
        }
    }

    pub(super) fn append_pending(&mut self, items: Vec<RolloutItem>) {
        self.pending_items.extend(items);
    }

    fn commit_pending(&mut self) -> ThreadStoreResult<()> {
        let pending_items = std::mem::take(&mut self.pending_items);
        if self.deferred_seed.is_some() {
            self.deferred_committed.extend(pending_items);
            return Ok(());
        }
        self.apply_persisted_items(pending_items.as_slice())
    }

    fn list_turns(&self, params: &ListTurnsParams) -> ThreadStoreResult<TurnPage> {
        if self.source_partial {
            return Err(ThreadStoreError::Conflict {
                message: "rollout history has known reconstruction gaps; turn view is incomplete"
                    .into(),
            });
        }
        require_positive_page_size(params.page_size)?;
        let anchor = params
            .cursor
            .as_deref()
            .map(parse_turn_cursor)
            .transpose()?;
        let anchor_index = anchor
            .as_ref()
            .and_then(|anchor| self.turn_positions.get(&anchor.turn_id).copied());
        if anchor.is_some() && anchor_index.is_none() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "invalid cursor: anchor turn is no longer present".to_string(),
            });
        }

        let (start, end) = page_bounds(
            self.turn_order.len(),
            anchor_index,
            anchor.as_ref().map(|anchor| anchor.include_anchor),
            params.sort_direction,
        );
        let has_more = end - start > params.page_size;
        let turns = ordered_page_indexes(start, end, params.sort_direction)
            .take(params.page_size)
            .map(|index| self.materialize_turn(&self.turn_order[index], params.items_view))
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        let backwards_cursor = turns
            .first()
            .map(|turn| serialize_turn_cursor(turn.turn_id.as_str(), true))
            .transpose()?;
        let next_cursor = if has_more {
            turns
                .last()
                .map(|turn| serialize_turn_cursor(turn.turn_id.as_str(), false))
                .transpose()?
        } else {
            None
        };

        Ok(TurnPage {
            turns,
            next_cursor,
            backwards_cursor,
        })
    }

    fn list_items(&self, params: &ListItemsParams) -> ThreadStoreResult<ItemPage> {
        if self.source_partial {
            return Err(ThreadStoreError::Conflict {
                message: "rollout history has known reconstruction gaps; item view is incomplete"
                    .into(),
            });
        }
        require_positive_page_size(params.page_size)?;
        if let Some(turn_id) = params.turn_id.as_ref()
            && !self.turns.contains_key(turn_id)
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "turn {turn_id} is not present in thread {}",
                    params.thread_id
                ),
            });
        }

        let anchor = params
            .cursor
            .as_deref()
            .map(parse_item_cursor)
            .transpose()?;
        let mut page_items = Vec::new();
        let has_more;
        if let Some(turn_id) = params.turn_id.as_deref() {
            let turn = self
                .turns
                .get(turn_id)
                .ok_or_else(|| projection_invariant_error("validated projected turn is missing"))?;
            let anchor_index = anchor.as_ref().and_then(|anchor| {
                (anchor.turn_id == turn_id)
                    .then(|| turn.item_indexes.get(&anchor.item_id).copied())
                    .flatten()
            });
            if anchor.is_some() && anchor_index.is_none() {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "invalid cursor: anchor item is no longer present".to_string(),
                });
            }

            let (start, end) = page_bounds(
                turn.items.len(),
                anchor_index,
                anchor.as_ref().map(|anchor| anchor.include_anchor),
                params.sort_direction,
            );
            has_more = end - start > params.page_size;
            page_items.reserve(params.page_size.min(end - start));
            for index in
                ordered_page_indexes(start, end, params.sort_direction).take(params.page_size)
            {
                page_items.push(self.materialize_item(turn_id, &turn.items[index])?);
            }
        } else {
            let anchor_index = anchor.as_ref().and_then(|anchor| {
                self.item_positions
                    .get(&ProjectedItemKey {
                        turn_id: anchor.turn_id.clone(),
                        item_id: anchor.item_id.clone(),
                    })
                    .copied()
            });
            if anchor.is_some() && anchor_index.is_none() {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "invalid cursor: anchor item is no longer present".to_string(),
                });
            }

            let (start, end) = page_bounds(
                self.item_order.len(),
                anchor_index,
                anchor.as_ref().map(|anchor| anchor.include_anchor),
                params.sort_direction,
            );
            has_more = end - start > params.page_size;
            page_items.reserve(params.page_size.min(end - start));
            for index in
                ordered_page_indexes(start, end, params.sort_direction).take(params.page_size)
            {
                let key = &self.item_order[index];
                let item = self
                    .turns
                    .get(&key.turn_id)
                    .and_then(|turn| {
                        turn.item_indexes
                            .get(&key.item_id)
                            .and_then(|index| turn.items.get(*index))
                    })
                    .ok_or_else(|| {
                        projection_invariant_error("item order contains an unknown projected item")
                    })?;
                page_items.push(self.materialize_item(key.turn_id.as_str(), item)?);
            }
        }
        let backwards_cursor = page_items
            .first()
            .map(|item| serialize_item_cursor(item, true))
            .transpose()?;
        let next_cursor = if has_more {
            page_items
                .last()
                .map(|item| serialize_item_cursor(item, false))
                .transpose()?
        } else {
            None
        };

        Ok(ItemPage {
            items: page_items,
            next_cursor,
            backwards_cursor,
        })
    }

    fn materialize_item(
        &self,
        turn_id: &str,
        item: &ProjectedItem,
    ) -> ThreadStoreResult<StoredThreadItem> {
        Ok(StoredThreadItem {
            turn_id: Some(turn_id.to_string()),
            item_key: item.item.id().to_string(),
            item_ordinal: item.ordinal,
            item_created_at_ms: i64::try_from(item.ordinal).unwrap_or(i64::MAX),
            materialized_thread_item_json: serde_json::to_vec(&item.item).map_err(|err| {
                ThreadStoreError::Internal {
                    message: format!(
                        "failed to serialize projected thread item {}: {err}",
                        item.item.id()
                    ),
                }
            })?,
        })
    }

    fn materialize_turn(
        &self,
        turn_id: &str,
        items_view: StoredTurnItemsView,
    ) -> ThreadStoreResult<StoredTurn> {
        let projected = self.turns.get(turn_id).ok_or_else(|| {
            projection_invariant_error("turn order contains an unknown projected turn")
        })?;
        let metadata = projected.metadata.as_ref();
        let (items, api_items_view) = match items_view {
            StoredTurnItemsView::NotLoaded => (Vec::new(), TurnItemsView::NotLoaded),
            StoredTurnItemsView::Summary => {
                (summary_items(&projected.items), TurnItemsView::Summary)
            }
            StoredTurnItemsView::Full => (
                projected
                    .items
                    .iter()
                    .map(|item| item.item.clone())
                    .collect(),
                TurnItemsView::Full,
            ),
        };
        let status = metadata
            .map(|metadata| metadata.status.clone())
            .unwrap_or(TurnStatus::InProgress);
        let turn = Turn {
            id: turn_id.to_string(),
            items,
            items_view: api_items_view,
            status: status.clone(),
            error: metadata.and_then(|metadata| metadata.error.clone()),
            started_at: metadata.and_then(|metadata| metadata.started_at),
            completed_at: metadata.and_then(|metadata| metadata.completed_at),
            duration_ms: metadata.and_then(|metadata| metadata.duration_ms),
            timing: metadata.and_then(|metadata| metadata.timing.clone()),
            surfaced_result: metadata.and_then(|metadata| metadata.surfaced_result.clone()),
        };

        Ok(StoredTurn {
            turn_id: turn_id.to_string(),
            items: Vec::new(),
            metadata_json: Some(serde_json::to_vec(&turn).map_err(|err| {
                ThreadStoreError::Internal {
                    message: format!("failed to serialize projected turn {turn_id}: {err}"),
                }
            })?),
            turn_created_at_ms: None,
            items_view,
            status: stored_turn_status(&status),
            error: turn.error.as_ref().map(|error| StoredTurnError {
                message: error.message.clone(),
                additional_details: error.additional_details.clone(),
            }),
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
        })
    }
}

impl LocalThreadProjectionEntry {
    fn new() -> Self {
        Self {
            state: Mutex::new(LocalThreadProjection::default()),
            operation_gate: Arc::new(Semaphore::new(1)),
            source_version: Mutex::new(None),
            payload_versions: Mutex::new(Vec::new()),
            gap_source_version: Mutex::new(None),
            gap_retry_after: Mutex::new(None),
            last_unloaded_read: StdMutex::new(None),
        }
    }

    fn last_unloaded_read(&self) -> Option<Instant> {
        *self
            .last_unloaded_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mark_unloaded_read(&self) {
        *self
            .last_unloaded_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }

    pub(super) async fn acquire_operation(&self) -> ThreadStoreResult<OwnedSemaphorePermit> {
        Arc::clone(&self.operation_gate)
            .acquire_owned()
            .await
            .map_err(|_| ThreadStoreError::Internal {
                message: "local thread projection operation gate was closed".to_string(),
            })
    }

    async fn is_initialized(&self) -> bool {
        self.state.lock().await.initialized
    }

    async fn initialize(&self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.state.lock().await.initialize(items)
    }

    pub(super) async fn append_pending(&self, items: Vec<RolloutItem>) {
        self.state.lock().await.append_pending(items);
    }

    pub(super) async fn commit_pending(&self) -> ThreadStoreResult<()> {
        self.state.lock().await.commit_pending()
    }

    async fn list_turns(&self, params: &ListTurnsParams) -> ThreadStoreResult<TurnPage> {
        self.state.lock().await.list_turns(params)
    }

    async fn list_items(&self, params: &ListItemsParams) -> ThreadStoreResult<ItemPage> {
        self.state.lock().await.list_items(params)
    }
}

pub(super) async fn initialize_deferred(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    history: Option<Arc<Vec<RolloutItem>>>,
) -> ThreadStoreResult<()> {
    let projection = projection_entry(store, thread_id).await;
    let _operation = projection.acquire_operation().await?;
    *projection.state.lock().await = LocalThreadProjection {
        deferred_seed: Some(history.unwrap_or_else(|| Arc::new(Vec::new()))),
        ..Default::default()
    };
    Ok(())
}

pub(super) async fn initialize_from_store(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
    rollout_path: Option<PathBuf>,
) -> ThreadStoreResult<SharedLocalThreadProjection> {
    let projection = projection_entry(store, thread_id).await;
    let _operation = projection.acquire_operation().await?;
    if !store.live_recorders.lock().await.contains_key(&thread_id) {
        // Unloaded rollouts may be appended, rolled back, or compressed by another process.
        // Compare the durable source (the physical plain or compressed file) before reusing its
        // pagination projection.
        let source_path = match rollout_path {
            Some(path) => codex_rollout::existing_rollout_path(&path).await,
            None => None,
        };
        let source_version = if let Some(path) = source_path {
            tokio::fs::metadata(&path)
                .await
                .ok()
                .map(|metadata| (path, metadata.len(), metadata.modified().ok()))
        } else {
            None
        };
        // The operation permit serializes initialization; the version lock need
        // not stay held while loading the durable history.
        let dependencies = projection.payload_versions.lock().await.clone();
        let source_changed = source_version.is_none()
            || *projection.source_version.lock().await != source_version
            || projection.gap_retry_after.lock().await.is_some_and(|deadline| Instant::now() >= deadline)
            || !codex_rollout::payload_versions_match(&dependencies).await;
        if source_changed {
            // The canonical reader stamps each named dependency before hydration.
            // A repair/replacement during replay still invalidates the next page.
            let mut dependencies = None;
            let history = store
                .load_history_with_payload_versions(
                    LoadThreadHistoryParams {
                        thread_id,
                        include_archived,
                    },
                    Some(&mut dependencies),
                )
                .await?;
            projection.initialize(history.items.as_slice()).await?;
            let degraded = history.items.iter().any(|item| match item {
                RolloutItem::ResponseItem(item) => {
                    codex_rollout::is_rollout_reconstruction_notice(item)
                }
                RolloutItem::Compacted(item) => {
                    item.replacement_history.as_ref().is_some_and(|items| {
                        items
                            .iter()
                            .any(codex_rollout::is_rollout_reconstruction_notice)
                    })
                }
                _ => false,
            });
            // Metadata stamps cannot prove content readability. Even confirmed
            // gaps must retry periodically after transient sharing/I/O failures.
            *projection.gap_retry_after.lock().await = degraded.then(||
                Instant::now() + std::time::Duration::from_secs(30));
            let confirmed_gap = {
                let mut gap_source_version = projection.gap_source_version.lock().await;
                let confirmed = degraded
                    && source_version.is_some()
                    && *gap_source_version == source_version;
                *gap_source_version = if degraded {
                    source_version.clone()
                } else {
                    None
                };
                confirmed
            };
            let cacheable = match &dependencies {
                Some(versions) if confirmed_gap => {
                    codex_rollout::payload_versions_match(versions).await
                }
                Some(versions) if !degraded => {
                    codex_rollout::payload_dependencies_unchanged(versions).await
                }
                _ => false,
            };
            *projection.payload_versions.lock().await = dependencies.unwrap_or_default();
            *projection.source_version.lock().await = if cacheable { source_version } else { None };
        }
        projection.mark_unloaded_read();
        evict_least_recent_unloaded_projections(store).await;
    }
    initialize_entry_from_store(store, thread_id, include_archived, &projection).await?;
    Ok(projection)
}

async fn evict_least_recent_unloaded_projections(store: &LocalThreadStore) {
    let mut projections = store.projections.lock().await;
    // Liveness must not change while entries are dropped: a writer appending to a dropped entry
    // would leave its replacement without those items. Skip this pass rather than wait here.
    let Ok(live_recorders) = store.live_recorders.try_lock() else {
        return;
    };
    let mut unloaded = projections
        .iter()
        .filter(|(thread_id, _)| !live_recorders.contains_key(thread_id))
        .map(|(thread_id, projection)| (projection.last_unloaded_read(), *thread_id))
        .collect::<Vec<_>>();
    if unloaded.len() <= MAX_UNLOADED_PROJECTIONS {
        return;
    }
    unloaded.sort_unstable_by_key(|(last_read, _)| std::cmp::Reverse(*last_read));
    for (_, thread_id) in unloaded.into_iter().skip(MAX_UNLOADED_PROJECTIONS) {
        projections.remove(&thread_id);
    }
}

async fn initialize_entry_from_store(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
    projection: &SharedLocalThreadProjection,
) -> ThreadStoreResult<()> {
    if projection.is_initialized().await {
        return Ok(());
    }
    {
        let mut state = projection.state.lock().await;
        if let Some(seed) = state.deferred_seed.take() {
            // Share the resume snapshot until pagination actually needs the
            // reducer/index. Keep pending versus committed visibility identical
            // to the eager path, without forcing a recorder persistence barrier.
            let committed = std::mem::take(&mut state.deferred_committed);
            let pending = std::mem::take(&mut state.pending_items);
            state.initialize(&seed)?;
            state.apply_persisted_items(&committed)?;
            state.pending_items = pending;
            return Ok(());
        }
    }
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id,
            include_archived,
        })
        .await?;
    projection.initialize(history.items.as_slice()).await
}

pub(super) async fn initialized_entry_for_append(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<(SharedLocalThreadProjection, OwnedSemaphorePermit)> {
    // Appends require a live writer. Reject a closed thread before creating an entry that would
    // load its whole history only for the writer lookup to fail.
    if !store.live_recorders.lock().await.contains_key(&thread_id) {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }
    let projection = projection_entry(store, thread_id).await;
    let operation = projection.acquire_operation().await?;
    Ok((projection, operation))
}

pub(super) async fn existing_entry(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> Option<SharedLocalThreadProjection> {
    store.projections.lock().await.get(&thread_id).cloned()
}

pub(super) async fn remove(store: &LocalThreadStore, thread_id: ThreadId) {
    store.projections.lock().await.remove(&thread_id);
}

pub(super) async fn remove_if_current(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    projection: &SharedLocalThreadProjection,
) {
    let mut projections = store.projections.lock().await;
    if projections
        .get(&thread_id)
        .is_some_and(|current| Arc::ptr_eq(current, projection))
    {
        projections.remove(&thread_id);
    }
}

pub(super) async fn list_turns(
    store: &LocalThreadStore,
    params: ListTurnsParams,
) -> ThreadStoreResult<TurnPage> {
    require_positive_page_size(params.page_size)?;
    let rollout_path =
        validate_thread_visibility(store, params.thread_id, params.include_archived).await?;
    let projection = initialize_from_store(
        store,
        params.thread_id,
        params.include_archived,
        rollout_path,
    )
    .await?;
    projection.list_turns(&params).await
}

pub(super) async fn list_items(
    store: &LocalThreadStore,
    params: ListItemsParams,
) -> ThreadStoreResult<ItemPage> {
    require_positive_page_size(params.page_size)?;
    let rollout_path =
        validate_thread_visibility(store, params.thread_id, params.include_archived).await?;
    let projection = initialize_from_store(
        store,
        params.thread_id,
        params.include_archived,
        rollout_path,
    )
    .await?;
    projection.list_items(&params).await
}

async fn validate_thread_visibility(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<Option<PathBuf>> {
    // Every page runs this check, so it resolves only the rollout path rather than a summary.
    super::read_thread::read_thread_rollout_path(store, thread_id, include_archived).await
}

async fn projection_entry(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> SharedLocalThreadProjection {
    store
        .projections
        .lock()
        .await
        .entry(thread_id)
        .or_insert_with(|| Arc::new(LocalThreadProjectionEntry::new()))
        .clone()
}

fn projection_invariant_error(message: &str) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("local thread projection invariant failed: {message}"),
    }
}

fn require_positive_page_size(page_size: usize) -> ThreadStoreResult<()> {
    if page_size == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "page size must be greater than zero".to_string(),
        });
    }
    Ok(())
}

fn page_bounds(
    len: usize,
    anchor_index: Option<usize>,
    include_anchor: Option<bool>,
    sort_direction: SortDirection,
) -> (usize, usize) {
    match (sort_direction, anchor_index, include_anchor) {
        (SortDirection::Asc, Some(anchor), Some(true)) => (anchor, len),
        (SortDirection::Asc, Some(anchor), _) => (anchor.saturating_add(1), len),
        (SortDirection::Asc, None, _) => (0, len),
        (SortDirection::Desc, Some(anchor), Some(true)) => (0, anchor.saturating_add(1)),
        (SortDirection::Desc, Some(anchor), _) => (0, anchor),
        (SortDirection::Desc, None, _) => (0, len),
    }
}

fn ordered_page_indexes(
    start: usize,
    end: usize,
    sort_direction: SortDirection,
) -> Box<dyn Iterator<Item = usize>> {
    match sort_direction {
        SortDirection::Asc => Box::new(start..end),
        SortDirection::Desc => Box::new((start..end).rev()),
    }
}

fn summary_items(items: &[ProjectedItem]) -> Vec<ThreadItem> {
    let first_user_message = items
        .iter()
        .map(|item| &item.item)
        .find(|item| matches!(item, ThreadItem::UserMessage { .. }))
        .cloned();
    let final_agent_message = items
        .iter()
        .rev()
        .map(|item| &item.item)
        .find(|item| matches!(item, ThreadItem::AgentMessage { .. }))
        .cloned();
    match (first_user_message, final_agent_message) {
        (Some(user_message), Some(agent_message)) if user_message.id() != agent_message.id() => {
            vec![user_message, agent_message]
        }
        (Some(user_message), _) => vec![user_message],
        (None, Some(agent_message)) => vec![agent_message],
        (None, None) => Vec::new(),
    }
}

fn stored_turn_status(status: &TurnStatus) -> StoredTurnStatus {
    match status {
        TurnStatus::Completed => StoredTurnStatus::Completed,
        TurnStatus::Interrupted => StoredTurnStatus::Interrupted,
        TurnStatus::Failed => StoredTurnStatus::Failed,
        TurnStatus::InProgress => StoredTurnStatus::InProgress,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCursor {
    turn_id: String,
    include_anchor: bool,
}

fn parse_turn_cursor(cursor: &str) -> ThreadStoreResult<TurnCursor> {
    serde_json::from_str(cursor).map_err(|_| ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    })
}

fn serialize_turn_cursor(turn_id: &str, include_anchor: bool) -> ThreadStoreResult<String> {
    serde_json::to_string(&TurnCursor {
        turn_id: turn_id.to_string(),
        include_anchor,
    })
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to serialize turn cursor: {err}"),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemCursor {
    turn_id: String,
    item_id: String,
    include_anchor: bool,
}

fn parse_item_cursor(cursor: &str) -> ThreadStoreResult<ItemCursor> {
    serde_json::from_str(cursor).map_err(|_| ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {cursor}"),
    })
}

fn serialize_item_cursor(
    item: &StoredThreadItem,
    include_anchor: bool,
) -> ThreadStoreResult<String> {
    serde_json::to_string(&ItemCursor {
        turn_id: item.turn_id.clone().unwrap_or_default(),
        item_id: item.item_key.clone(),
        include_anchor,
    })
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to serialize item cursor: {err}"),
    })
}

