use std::collections::HashMap;
use std::sync::Arc;

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
use crate::ReadThreadParams;
use crate::StoredThreadItem;
use crate::StoredTurn;
use crate::StoredTurnError;
use crate::StoredTurnItemsView;
use crate::StoredTurnStatus;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::TurnPage;

pub(super) type SharedLocalThreadProjection = Arc<LocalThreadProjectionEntry>;

pub(super) struct LocalThreadProjectionEntry {
    state: Mutex<LocalThreadProjection>,
    operation_gate: Arc<Semaphore>,
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
        let persisted_items = persisted_rollout_items(items, ThreadHistoryMode::Legacy);
        self.apply_persisted_items(persisted_items.as_slice())
    }

    fn apply_persisted_items(&mut self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
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

    fn append_durable(&mut self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        let mut durable_items = std::mem::take(&mut self.pending_items);
        durable_items.extend_from_slice(items);
        self.apply_persisted_items(durable_items.as_slice())
    }

    pub(super) fn append_pending(&mut self, items: Vec<RolloutItem>) {
        self.pending_items.extend(items);
    }

    fn commit_pending(&mut self) -> ThreadStoreResult<()> {
        let pending_items = std::mem::take(&mut self.pending_items);
        self.apply_persisted_items(pending_items.as_slice())
    }

    fn list_turns(&self, params: &ListTurnsParams) -> ThreadStoreResult<TurnPage> {
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
        let mut turns = ordered_page_indexes(start, end, params.sort_direction)
            .take(params.page_size.saturating_add(1))
            .map(|index| self.materialize_turn(&self.turn_order[index], params.items_view))
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        let has_more = turns.len() > params.page_size;
        turns.truncate(params.page_size);
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
        let mut page_items = Vec::with_capacity(params.page_size.saturating_add(1));
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
            for index in ordered_page_indexes(start, end, params.sort_direction)
                .take(params.page_size.saturating_add(1))
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
            for index in ordered_page_indexes(start, end, params.sort_direction)
                .take(params.page_size.saturating_add(1))
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
        let has_more = page_items.len() > params.page_size;
        page_items.truncate(params.page_size);
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
        let mut items: Vec<ThreadItem> = projected
            .items
            .iter()
            .map(|item| item.item.clone())
            .collect();
        let api_items_view = match items_view {
            StoredTurnItemsView::NotLoaded => {
                items.clear();
                TurnItemsView::NotLoaded
            }
            StoredTurnItemsView::Summary => {
                items = summary_items(items.as_slice());
                TurnItemsView::Summary
            }
            StoredTurnItemsView::Full => TurnItemsView::Full,
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
            reasoning_policy_history: metadata
                .and_then(|metadata| metadata.reasoning_policy_history.clone()),
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
        }
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

    pub(super) async fn append_durable(&self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        self.state.lock().await.append_durable(items)
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

pub(super) async fn initialize_empty(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    initialize_from_items(store, thread_id, &[]).await
}

pub(super) async fn initialize_from_items(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    let projection = projection_entry(store, thread_id).await;
    let _operation = projection.acquire_operation().await?;
    projection.initialize(items).await
}

pub(super) async fn initialize_from_store(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<()> {
    let projection = projection_entry(store, thread_id).await;
    let _operation = projection.acquire_operation().await?;
    initialize_entry_from_store(store, thread_id, include_archived, &projection).await
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
    let projection = projection_entry(store, thread_id).await;
    let operation = projection.acquire_operation().await?;
    initialize_entry_from_store(
        store,
        thread_id,
        /*include_archived*/ true,
        &projection,
    )
    .await?;
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
    validate_thread_visibility(store, params.thread_id, params.include_archived).await?;
    initialize_from_store(store, params.thread_id, params.include_archived).await?;
    let projection = projection_entry(store, params.thread_id).await;
    projection.list_turns(&params).await
}

pub(super) async fn list_items(
    store: &LocalThreadStore,
    params: ListItemsParams,
) -> ThreadStoreResult<ItemPage> {
    validate_thread_visibility(store, params.thread_id, params.include_archived).await?;
    initialize_from_store(store, params.thread_id, params.include_archived).await?;
    let projection = projection_entry(store, params.thread_id).await;
    projection.list_items(&params).await
}

async fn validate_thread_visibility(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<()> {
    super::read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        },
    )
    .await
    .map(|_| ())
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

fn summary_items(items: &[ThreadItem]) -> Vec<ThreadItem> {
    let first_user_message = items
        .iter()
        .find(|item| matches!(item, ThreadItem::UserMessage { .. }))
        .cloned();
    let final_agent_message = items
        .iter()
        .rev()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_index_window_starts_at_the_cursor_without_allocating_the_prefix() {
        let (start, end) = page_bounds(10_000, Some(9_990), Some(false), SortDirection::Asc);
        assert_eq!((start, end), (9_991, 10_000));
        assert_eq!(
            ordered_page_indexes(start, end, SortDirection::Asc)
                .take(3)
                .collect::<Vec<_>>(),
            vec![9_991, 9_992, 9_993]
        );

        let (start, end) = page_bounds(10_000, Some(9), Some(false), SortDirection::Desc);
        assert_eq!((start, end), (0, 9));
        assert_eq!(
            ordered_page_indexes(start, end, SortDirection::Desc)
                .take(3)
                .collect::<Vec<_>>(),
            vec![8, 7, 6]
        );
    }

    #[test]
    fn late_item_keys_remain_grouped_by_turn_order() {
        let mut projection = LocalThreadProjection::default();
        projection.ensure_turn("turn-1");
        projection.ensure_turn("turn-2");
        let turn_2_item = ProjectedItemKey {
            turn_id: "turn-2".to_string(),
            item_id: "item-2".to_string(),
        };
        let turn_1_item = ProjectedItemKey {
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
        };

        projection.insert_item_key(turn_2_item.clone());
        projection.insert_item_key(turn_1_item.clone());

        assert_eq!(
            projection.item_order,
            vec![turn_1_item.clone(), turn_2_item.clone()]
        );
        assert_eq!(projection.item_positions[&turn_1_item], 0);
        assert_eq!(projection.item_positions[&turn_2_item], 1);
    }
}
