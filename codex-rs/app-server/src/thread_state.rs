use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
#[path = "active_stream_replay.rs"]
mod active_stream_replay;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::ThreadHistoryItemChange;
use codex_app_server_protocol::ThreadHistoryTurnChange;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_core::CodexThread;
use codex_core::OutOfBandElicitationLeaseId;
use codex_core::ThreadConfigSnapshot;
use codex_file_watcher::WatchRegistration;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_rollout::state_integration::StateDbHandle;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const MAX_TRACKED_IN_FLIGHT_TASKS: usize = 1_024;
pub(crate) const THREAD_LISTENER_COMMAND_CAPACITY: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InFlightTaskReference {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TurnStartClaim {
    Claimed,
    Replay(String),
    SubmissionConflict,
    IdenticalTask(InFlightTaskReference),
    ActiveTurn(InFlightTaskReference),
    CapacityExceeded,
}

#[derive(Default)]
struct TurnOriginState {
    by_turn_id: HashMap<String, ConnectionId>,
}

#[derive(Clone, Default)]
pub(crate) struct TurnOriginTracker {
    state: Arc<StdMutex<TurnOriginState>>,
}

pub(crate) struct TurnOriginReservation {
    tracker: TurnOriginTracker,
    turn_id: String,
    connection_id: ConnectionId,
    committed: bool,
}

impl TurnOriginTracker {
    pub(crate) fn reserve(
        &self,
        turn_id: String,
        connection_id: ConnectionId,
    ) -> TurnOriginReservation {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.by_turn_id.insert(turn_id.clone(), connection_id);
        drop(state);
        TurnOriginReservation {
            tracker: self.clone(),
            turn_id,
            connection_id,
            committed: false,
        }
    }

    fn take(&self, turn_id: &str) -> Option<ConnectionId> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.by_turn_id.remove(turn_id)
    }

    fn remove_if_matches(&self, turn_id: &str, connection_id: ConnectionId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.by_turn_id.get(turn_id) == Some(&connection_id) {
            state.by_turn_id.remove(turn_id);
        }
    }
}

impl TurnOriginReservation {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for TurnOriginReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.tracker
                .remove_if_matches(&self.turn_id, self.connection_id);
        }
    }
}

pub(crate) struct PendingThreadResumeRequest {
    pub(crate) request_id: ConnectionRequestId,
    pub(crate) history_items: Option<Vec<RolloutItem>>,
    pub(crate) history_generation: u64,
    pub(crate) listener_generation: u64,
    pub(crate) dynamic_tool_executor: bool,
    pub(crate) config_snapshot: ThreadConfigSnapshot,
    pub(crate) instruction_sources: Vec<LegacyAppPathString>,
    pub(crate) thread_summary: codex_app_server_protocol::Thread,
    pub(crate) include_turns: bool,
    pub(crate) initial_turns_page:
        Option<codex_app_server_protocol::ThreadResumeInitialTurnsPageParams>,
}

// ThreadListenerCommand is used to perform operations in the context of the thread listener, for serialization purposes.
pub(crate) enum ThreadListenerCommand {
    // SendThreadResumeResponse is used to resume an already running thread by sending the thread's history to the client and atomically subscribing for new updates.
    SendThreadResumeResponse(Box<PendingThreadResumeRequest>),
    // ResolveServerRequest is used to notify the client that the request has been resolved.
    // It is executed in the thread listener's context to ensure that the resolved notification is ordered with regard to the request itself.
    ResolveServerRequest {
        request_id: RequestId,
        completion_tx: oneshot::Sender<()>,
    },
}

pub(crate) type ThreadListenerCommandRoute =
    (mpsc::Sender<ThreadListenerCommand>, CancellationToken);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolveServerRequestFailure {
    ListenerNotRunning,
    ListenerClosed,
    CompletionDropped,
    DeliveryTimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolveServerRequestError {
    pub(crate) request_id: RequestId,
    pub(crate) failure: ResolveServerRequestFailure,
}

pub(crate) fn thread_listener_command_channel() -> (
    mpsc::Sender<ThreadListenerCommand>,
    mpsc::Receiver<ThreadListenerCommand>,
) {
    mpsc::channel(THREAD_LISTENER_COMMAND_CAPACITY)
}

/// Per-conversation accumulation of the latest states e.g. error message while a turn runs.
#[derive(Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) started_at: Option<i64>,
    pub(crate) command_execution_started: HashSet<String>,
    pub(crate) last_error: Option<TurnError>,
    pub(crate) origin_connection_id: Option<ConnectionId>,
}

#[derive(Debug)]
pub(crate) struct IndexedTurnPage {
    pub(crate) turns: Vec<Turn>,
    pub(crate) more_turns_available: bool,
}

#[derive(Debug)]
pub(crate) struct IndexedItemPage {
    pub(crate) items: Vec<IndexedThreadItem>,
    pub(crate) more_items_available: bool,
}

#[derive(Debug)]
pub(crate) struct IndexedThreadItem {
    pub(crate) turn_id: String,
    pub(crate) item: codex_app_server_protocol::ThreadItem,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct IndexedItemKey {
    turn_id: String,
    item_id: String,
}

#[derive(Clone, Copy)]
struct ItemLocation {
    position: usize,
    offset: usize,
}

#[derive(Default)]
struct ThreadTurnIndex {
    initialized: bool,
    rollback_eligible_turn_ids: HashSet<String>,
    order: Vec<String>,
    turn_positions: HashMap<String, usize>,
    turns: HashMap<String, Turn>,
    item_order: Vec<IndexedItemKey>,
    item_locations: HashMap<IndexedItemKey, ItemLocation>,
}

impl ThreadTurnIndex {
    fn index_appended_retained_turn_items(&mut self, turn_id: &str, previous_len: usize) {
        let new_items = self.turns[turn_id]
            .items
            .iter()
            .enumerate()
            .skip(previous_len)
            .filter_map(|(offset, item)| {
                let key = IndexedItemKey {
                    turn_id: turn_id.to_string(),
                    item_id: item.id().to_string(),
                };
                (!self.item_locations.contains_key(&key)).then_some((key, offset))
            })
            .collect::<Vec<_>>();
        for (key, offset) in new_items {
            self.insert_item_key(key, offset);
        }
    }

    fn apply_changes(&mut self, changes: ThreadHistoryChangeSet) {
        if !changes.removed_turn_ids.is_empty() {
            for turn_id in changes.removed_turn_ids {
                self.turns.remove(&turn_id);
                self.rollback_eligible_turn_ids.remove(&turn_id);
            }
            self.order
                .retain(|candidate| self.turns.contains_key(candidate));
            self.rebuild_positions();
        }

        for change in changes.changed_turns {
            self.apply_turn_change(change);
        }
        for change in changes.changed_items {
            let key = IndexedItemKey {
                turn_id: change.turn_id.clone(),
                item_id: change.item.id().to_string(),
            };
            let existing_offset = self
                .item_locations
                .get(&key)
                .map(|location| location.offset);
            if let Some(index) = existing_offset {
                self.turn_mut(&change.turn_id).items[index] = change.item;
            } else {
                let offset = {
                    let turn = self.turn_mut(&change.turn_id);
                    let offset = turn.items.len();
                    turn.items.push(change.item);
                    offset
                };
                self.insert_item_key(key, offset);
            }
        }
    }

    fn insert_item_key(&mut self, key: IndexedItemKey, offset: usize) {
        let turn_position = self.turn_positions[&key.turn_id];
        let append = self
            .item_order
            .last()
            .is_none_or(|last| self.turn_positions[&last.turn_id] <= turn_position);
        if append {
            self.item_locations.insert(
                key.clone(),
                ItemLocation {
                    position: self.item_order.len(),
                    offset,
                },
            );
            self.item_order.push(key);
            return;
        }

        let insertion_index = self
            .item_order
            .iter()
            .position(|existing| self.turn_positions[&existing.turn_id] > turn_position)
            .unwrap_or(self.item_order.len());
        self.item_locations.insert(
            key.clone(),
            ItemLocation {
                position: insertion_index,
                offset,
            },
        );
        self.item_order.insert(insertion_index, key);
        for (position, key) in self.item_order.iter().enumerate().skip(insertion_index) {
            if let Some(location) = self.item_locations.get_mut(key) {
                location.position = position;
            }
        }
    }

    fn apply_turn_change(&mut self, change: ThreadHistoryTurnChange) {
        let turn = self.turn_mut(&change.turn_id);
        turn.status = change.status;
        turn.error = change.error;
        turn.started_at = change.started_at;
        turn.completed_at = change.completed_at;
        turn.duration_ms = change.duration_ms;
        turn.timing = change.timing;
        turn.surfaced_result = change.surfaced_result;
    }

    fn overlay_turn(&mut self, mut turn: Turn) {
        if let Some(persisted) = self.turns.get_mut(&turn.id) {
            let mut items = std::mem::take(&mut persisted.items);
            let mut offsets = items
                .iter()
                .enumerate()
                .map(|(offset, item)| (item.id().to_string(), offset))
                .collect::<HashMap<_, _>>();
            for item in turn.items {
                if let Some(&offset) = offsets.get(item.id()) {
                    items[offset] = item;
                } else {
                    offsets.insert(item.id().to_string(), items.len());
                    items.push(item);
                }
            }
            turn.items = items;
            turn.started_at = turn.started_at.or(persisted.started_at);
        } else {
            self.order.push(turn.id.clone());
        }
        self.turns.insert(turn.id.clone(), turn);
    }

    fn turn_mut(&mut self, turn_id: &str) -> &mut Turn {
        if !self.turns.contains_key(turn_id) {
            self.turn_positions
                .insert(turn_id.to_string(), self.order.len());
            self.order.push(turn_id.to_string());
        }
        self.turns
            .entry(turn_id.to_string())
            .or_insert_with(|| Turn {
                id: turn_id.to_string(),
                items: Vec::new(),
                items_view: TurnItemsView::Full,
                status: TurnStatus::InProgress,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                timing: None,
                surfaced_result: None,
            })
    }

    fn page(
        &self,
        anchor: Option<(&str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> Result<Option<IndexedTurnPage>, ()> {
        if !self.initialized {
            return Ok(None);
        }
        let anchor_index =
            anchor.and_then(|(turn_id, _)| self.turn_positions.get(turn_id).copied());
        if anchor.is_some() && anchor_index.is_none() {
            return Err(());
        }

        let (start, end) = page_bounds(
            self.order.len(),
            anchor_index,
            anchor.map(|(_, include)| include),
            sort_direction,
        );
        let indexes = ordered_indexes(start, end, sort_direction);
        let mut turns_iter = indexes.filter_map(|index| self.turns.get(&self.order[index]));
        let turns = turns_iter
            .by_ref()
            .take(page_size)
            .map(|turn| clone_turn_with_items_view(turn, items_view))
            .collect();
        let more_turns_available = turns_iter.next().is_some();
        Ok(Some(IndexedTurnPage {
            turns,
            more_turns_available,
        }))
    }

    fn items_page(
        &self,
        turn_id_filter: Option<&str>,
        anchor: Option<(&str, &str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
    ) -> Result<Option<IndexedItemPage>, ()> {
        if !self.initialized {
            return Ok(None);
        }
        if let Some(turn_id) = turn_id_filter {
            let turn = self.turns.get(turn_id);
            let anchor_index = anchor.and_then(|(anchor_turn_id, item_id, _)| {
                (anchor_turn_id == turn_id)
                    .then(|| {
                        self.item_locations
                            .get(&IndexedItemKey {
                                turn_id: turn_id.to_string(),
                                item_id: item_id.to_string(),
                            })
                            .map(|location| location.offset)
                    })
                    .flatten()
            });
            if anchor.is_some() && anchor_index.is_none() {
                return Err(());
            }

            let (start, end) = page_bounds(
                turn.map_or(0, |turn| turn.items.len()),
                anchor_index,
                anchor.map(|(_, _, include)| include),
                sort_direction,
            );
            let mut item_refs = turn.into_iter().flat_map(|turn| {
                ordered_indexes(start, end, sort_direction)
                    .filter_map(move |index| turn.items.get(index))
            });
            let items = item_refs
                .by_ref()
                .take(page_size)
                .map(|item| IndexedThreadItem {
                    turn_id: turn_id.to_string(),
                    item: item.clone(),
                })
                .collect();
            let more_items_available = item_refs.next().is_some();
            return Ok(Some(IndexedItemPage {
                items,
                more_items_available,
            }));
        }

        let anchor_index = anchor.and_then(|(turn_id, item_id, _)| {
            let key = IndexedItemKey {
                turn_id: turn_id.to_string(),
                item_id: item_id.to_string(),
            };
            self.item_locations
                .get(&key)
                .map(|location| location.position)
        });
        if anchor.is_some() && anchor_index.is_none() {
            return Err(());
        }

        let (start, end) = page_bounds(
            self.item_order.len(),
            anchor_index,
            anchor.map(|(_, _, include)| include),
            sort_direction,
        );
        let mut items = Vec::new();
        let mut more_items_available = false;
        for index in ordered_indexes(start, end, sort_direction) {
            let key = &self.item_order[index];
            let Some(offset) = self.item_locations.get(key).map(|location| location.offset) else {
                continue;
            };
            let Some(item) = self
                .turns
                .get(&key.turn_id)
                .and_then(|turn| turn.items.get(offset))
            else {
                continue;
            };
            if items.len() == page_size {
                more_items_available = true;
                break;
            }
            items.push(IndexedThreadItem {
                turn_id: key.turn_id.clone(),
                item: item.clone(),
            });
        }

        Ok(Some(IndexedItemPage {
            items,
            more_items_available,
        }))
    }

    fn rebuild_positions(&mut self) {
        self.turn_positions.clear();
        self.item_order.clear();
        self.item_locations.clear();
        for (turn_position, turn_id) in self.order.iter().enumerate() {
            self.turn_positions.insert(turn_id.clone(), turn_position);
            let Some(turn) = self.turns.get(turn_id) else {
                continue;
            };
            for (offset, item) in turn.items.iter().enumerate() {
                let key = IndexedItemKey {
                    turn_id: turn_id.clone(),
                    item_id: item.id().to_string(),
                };
                self.item_locations.insert(
                    key.clone(),
                    ItemLocation {
                        position: self.item_order.len(),
                        offset,
                    },
                );
                self.item_order.push(key);
            }
        }
    }
}

pub(crate) fn turn_summary_item_indices(items: &[ThreadItem]) -> (Option<usize>, Option<usize>) {
    let user = items
        .iter()
        .position(|item| matches!(item, ThreadItem::UserMessage { .. }));
    let agent = items
        .iter()
        .rposition(|item| matches!(item, ThreadItem::AgentMessage { interruption: None, .. }))
        .filter(|&index| user.is_none_or(|user| items[user].id() != items[index].id()));
    (user, agent)
}

pub(crate) fn clone_turn_with_items_view(turn: &Turn, items_view: TurnItemsView) -> Turn {
    let items = match items_view {
        TurnItemsView::NotLoaded => Vec::new(),
        TurnItemsView::Summary => {
            let (user, agent) = turn_summary_item_indices(&turn.items);
            user.into_iter()
                .chain(agent)
                .map(|index| turn.items[index].clone())
                .collect()
        }
        TurnItemsView::Full => turn.items.clone(),
    };
    Turn {
        id: turn.id.clone(),
        items,
        items_view,
        status: turn.status.clone(),
        error: turn.error.clone(),
        started_at: turn.started_at,
        completed_at: turn.completed_at,
        duration_ms: turn.duration_ms,
        timing: turn.timing.clone(),
        surfaced_result: turn.surfaced_result.clone(),
    }
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

fn ordered_indexes(
    start: usize,
    end: usize,
    sort_direction: SortDirection,
) -> Box<dyn Iterator<Item = usize>> {
    match sort_direction {
        SortDirection::Asc => Box::new(start..end),
        SortDirection::Desc => Box::new((start..end).rev()),
    }
}

struct PendingInterrupt {
    request_id: ConnectionRequestId,
    turn_id: String,
    cancelled: tokio_util::sync::CancellationToken,
}

struct PendingRollback {
    request_id: ConnectionRequestId,
    cancelled: tokio_util::sync::CancellationToken,
}

#[derive(Default)]
pub(crate) struct ThreadState {
    pending_interrupts: Vec<PendingInterrupt>,
    pending_rollbacks: Option<PendingRollback>,
    pub(crate) turn_summary: TurnSummary,
    turn_summary_id: Option<String>,
    // Core can publish an old terminal after its successor has started. Retain
    // only that unfinished turn's metadata, never a second copy of its items.
    pending_turn_summaries: HashMap<String, TurnSummary>,
    pub(crate) listener_cancellation: Option<CancellationToken>,
    pub(crate) experimental_raw_events: bool,
    pub(crate) listener_generation: u64,
    resume_history_seeded_generation: Option<u64>,
    last_thread_settings: Option<ThreadSettings>,
    listener_command_tx: Option<mpsc::Sender<ThreadListenerCommand>>,
    current_turn_history: ThreadHistoryBuilder,
    active_stream_replay: active_stream_replay::ActiveStreamReplay,
    turn_index: ThreadTurnIndex,
    turn_index_source_partial: bool,
    turn_index_rollback_generation: u64,
    rolled_back_wait_ids: HashSet<String>,
    turn_origin_tracker: TurnOriginTracker,
    listener_thread: Option<Weak<CodexThread>>,
    watch_registration: WatchRegistration,
}

impl ThreadState {
    pub(crate) fn reserve_interrupt(
        &mut self,
        request_id: ConnectionRequestId,
        turn_id: String,
    ) -> tokio_util::sync::DropGuard {
        // A dropped request invalidates its entry synchronously, even while the
        // async ThreadState mutex is held elsewhere. Prune those tombstones on
        // each admission; terminal draining also removes and filters them.
        self.pending_interrupts
            .retain(|pending| !pending.cancelled.is_cancelled());
        let cancelled = tokio_util::sync::CancellationToken::new();
        let guard = cancelled.clone().drop_guard();
        self.pending_interrupts.push(PendingInterrupt {
            request_id,
            turn_id,
            cancelled,
        });
        guard
    }

    pub(crate) fn take_pending_interrupts(&mut self, turn_id: &str) -> Vec<ConnectionRequestId> {
        let mut requests = Vec::new();
        self.pending_interrupts.retain(|pending| {
            if pending.cancelled.is_cancelled() {
                return false;
            }
            if pending.turn_id == turn_id {
                requests.push(pending.request_id.clone());
                return false;
            }
            true
        });
        requests
    }

    /// Claim a response when core settled without an abort event (or before its delivery).
    pub(crate) fn finish_interrupt(&mut self, request_id: &ConnectionRequestId) -> bool {
        let mut pending_response = false;
        self.pending_interrupts.retain(|pending| {
            if &pending.request_id == request_id {
                pending_response = !pending.cancelled.is_cancelled();
                return false;
            }
            true
        });
        pending_response
    }


    pub(crate) fn reserve_rollback(
        &mut self,
        request_id: ConnectionRequestId,
    ) -> Option<tokio_util::sync::DropGuard> {
        if self.has_pending_rollback() {
            return None;
        }
        let cancelled = tokio_util::sync::CancellationToken::new();
        let guard = cancelled.clone().drop_guard();
        self.pending_rollbacks = Some(PendingRollback {
            request_id,
            cancelled,
        });
        Some(guard)
    }

    pub(crate) fn has_pending_rollback(&self) -> bool {
        self.pending_rollbacks
            .as_ref()
            .is_some_and(|pending| !pending.cancelled.is_cancelled())
    }

    pub(crate) fn take_pending_rollback(&mut self) -> Option<ConnectionRequestId> {
        self.pending_rollbacks
            .take()
            .filter(|pending| !pending.cancelled.is_cancelled())
            .map(|pending| pending.request_id)
    }

    pub(crate) fn listener_matches(&self, conversation: &Arc<CodexThread>) -> bool {
        self.listener_thread
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|existing| Arc::ptr_eq(&existing, conversation))
    }

    pub(crate) fn set_listener(
        &mut self,
        cancellation: CancellationToken,
        conversation: &Arc<CodexThread>,
        watch_registration: WatchRegistration,
        thread_settings_baseline: ThreadSettings,
    ) -> (mpsc::Receiver<ThreadListenerCommand>, u64) {
        if let Some(previous) = self.listener_cancellation.replace(cancellation) {
            previous.cancel();
        }
        self.active_stream_replay = active_stream_replay::ActiveStreamReplay::default();
        self.listener_generation = self.listener_generation.wrapping_add(1);
        self.last_thread_settings = Some(thread_settings_baseline);
        let (listener_command_tx, listener_command_rx) = thread_listener_command_channel();
        self.listener_command_tx = Some(listener_command_tx);
        self.listener_thread = Some(Arc::downgrade(conversation));
        self.watch_registration = watch_registration;
        (listener_command_rx, self.listener_generation)
    }

    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancellation) = self.listener_cancellation.take() {
            cancellation.cancel();
        }
        self.listener_command_tx = None;
        self.active_stream_replay = active_stream_replay::ActiveStreamReplay::default();
        self.current_turn_history.reset();
        self.turn_summary = TurnSummary::default();
        self.turn_summary_id = None;
        self.pending_turn_summaries.clear();
        self.listener_thread = None;
        self.watch_registration = WatchRegistration::default();
    }

    pub(crate) fn listener_command_tx(&self) -> Option<mpsc::Sender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    pub(crate) fn listener_command_route(&self) -> Option<ThreadListenerCommandRoute> {
        self.listener_command_tx
            .clone()
            .zip(self.listener_cancellation.clone())
    }

    pub(crate) fn resume_history_is_seeded_for_current_listener(&self) -> bool {
        self.listener_command_tx.is_some()
            && self.resume_history_seeded_generation == Some(self.listener_generation)
    }

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
    }

    pub(crate) fn indexed_turn_snapshot(&self, turn_id: &str) -> Option<Turn> {
        self.turn_index.turns.get(turn_id).map(|turn| clone_turn_with_items_view(turn, TurnItemsView::NotLoaded))
    }

    pub(crate) fn active_stream_replay(
        &self,
    ) -> Vec<codex_app_server_protocol::ServerNotification> {
        self.active_stream_replay.snapshot(self.in_progress_turn_id())
    }

    pub(crate) fn active_turn_id(&self) -> Option<&str> {
        self.current_turn_history.active_turn_id()
    }

    pub(crate) fn indexed_turns_page(
        &self,
        anchor: Option<(&str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
        items_view: TurnItemsView,
    ) -> Result<Option<IndexedTurnPage>, ()> {
        if self.turn_index_source_partial {
            return Err(());
        }
        self.turn_index
            .page(anchor, page_size, sort_direction, items_view)
    }

    pub(crate) fn indexed_items_page(
        &self,
        turn_id_filter: Option<&str>,
        anchor: Option<(&str, &str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
    ) -> Result<Option<IndexedItemPage>, ()> {
        if self.turn_index_source_partial {
            return Err(());
        }
        self.turn_index
            .items_page(turn_id_filter, anchor, page_size, sort_direction)
    }

    pub(crate) fn turn_index_history_generation(&self) -> u64 {
        self.turn_index_rollback_generation
    }

    /// Commits an asynchronous history read only if no rollback invalidated it.
    /// Another reader may already have initialized the live index; never replace it.
    pub(crate) fn try_seed_turn_index_from_history(
        &mut self,
        items: &[RolloutItem],
        expected_generation: u64,
    ) -> Result<(), ()> {
        if codex_rollout::ensure_rollout_source_integrity(items).is_err() {
            if self.turn_index_rollback_generation == expected_generation {
                self.turn_index_source_partial = true;
            }
            return Err(());
        }
        if self.turn_index_source_partial {
            if self.turn_index_rollback_generation != expected_generation {
                return Err(());
            }
            self.seed_turn_index_from_checked_history(items, false);
            return Ok(());
        }
        if self.turn_index.initialized {
            return Ok(());
        }
        if self.turn_index_rollback_generation != expected_generation {
            return Err(());
        }
        self.seed_turn_index_from_checked_history(items, false);
        Ok(())
    }

    /// Seeds a synchronous history snapshot while preserving live additions.
    /// Asynchronous readers must use the generation-checked entrypoint above.
    pub(crate) fn seed_turn_index_from_history(&mut self, items: &[RolloutItem]) {
        let source_partial =
            codex_rollout::ensure_rollout_source_integrity(items).is_err();
        self.seed_turn_index_from_checked_history(items, source_partial);
    }

    fn seed_turn_index_from_checked_history(&mut self, items: &[RolloutItem], source_partial: bool) {
        self.turn_index_source_partial = source_partial;
        let mut live_index = std::mem::take(&mut self.turn_index);
        let mut builder = ThreadHistoryBuilder::new();
        for item in items {
            let changes = builder.handle_rollout_item_with_changes(item);
            for turn_id in changes
                .changed_turns
                .iter()
                .map(|change| &change.turn_id)
                .chain(changes.changed_items.iter().map(|change| &change.turn_id))
            {
                if builder.is_rollback_eligible_turn(turn_id) {
                    self.turn_index
                        .rollback_eligible_turn_ids
                        .insert(turn_id.clone());
                }
            }
            self.turn_index.apply_changes(changes);
        }
        self.turn_index
            .rollback_eligible_turn_ids
            .extend(live_index.rollback_eligible_turn_ids);
        for turn_id in live_index.order {
            if let Some(turn) = live_index.turns.remove(&turn_id) {
                self.turn_index.overlay_turn(turn);
            }
        }
        self.turn_index.rebuild_positions();
        self.turn_index.initialized = true;
    }

    pub(crate) fn in_progress_turn_id(&self) -> Option<&str> {
        self.current_turn_history.in_progress_turn_id()
    }

    pub(crate) fn open_turn_id(&self) -> Option<&str> {
        self.current_turn_history
            .has_active_turn()
            .then(|| self.current_turn_history.active_turn_id())
            .flatten()
    }

    pub(crate) fn is_current_summary_turn(&self, turn_id: &str) -> bool {
        self.turn_summary_id
            .as_deref()
            .or_else(|| self.in_progress_turn_id())
            .map_or(
                !self.pending_turn_summaries.contains_key(turn_id),
                |current| current == turn_id,
            )
    }

    pub(crate) fn turn_summary_for(&self, turn_id: &str) -> Option<&TurnSummary> {
        if self.is_current_summary_turn(turn_id) {
            Some(&self.turn_summary)
        } else {
            self.pending_turn_summaries.get(turn_id)
        }
    }

    pub(crate) fn turn_summary_for_mut(&mut self, turn_id: &str) -> Option<&mut TurnSummary> {
        if self.is_current_summary_turn(turn_id) {
            Some(&mut self.turn_summary)
        } else {
            self.pending_turn_summaries.get_mut(turn_id)
        }
    }

    pub(crate) fn take_turn_summary(&mut self, turn_id: &str) -> TurnSummary {
        if let Some(summary) = self.pending_turn_summaries.remove(turn_id) {
            summary
        } else if self.is_current_summary_turn(turn_id) {
            self.turn_summary_id = None;
            std::mem::take(&mut self.turn_summary)
        } else {
            TurnSummary::default()
        }
    }


    pub(crate) fn track_current_turn_event_with_reconciled_wait_items(
        &mut self,
        event_turn_id: &str,
        event: &EventMsg,
    ) -> Vec<ThreadHistoryItemChange> {
        self.active_stream_replay.track(event_turn_id, event);
        if let EventMsg::TurnStarted(payload) = event {
            if self.turn_summary_id.as_deref() != Some(event_turn_id) {
                let previous = std::mem::take(&mut self.turn_summary);
                if let Some(previous_id) = self.turn_summary_id.replace(event_turn_id.to_string()) {
                    self.pending_turn_summaries.insert(previous_id, previous);
                }
            }
            self.turn_summary.started_at = payload.started_at;
            self.turn_summary.origin_connection_id = self.turn_origin_tracker.take(event_turn_id);
        }
        if let EventMsg::ThreadRolledBack(rollback) = event {
            self.turn_index_rollback_generation =
                self.turn_index_rollback_generation.wrapping_add(1);
            // The current-turn reducer is reset at terminal boundaries. Only
            // the pagination index retains the complete ordered live history.
            let mut remaining = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
            let mut retained = self.turn_index.order.len();
            while retained > 0 && remaining > 0 {
                retained -= 1;
                if self
                    .turn_index
                    .rollback_eligible_turn_ids
                    .contains(&self.turn_index.order[retained])
                {
                    remaining -= 1;
                }
            }
            let removed_turn_ids = self.turn_index.order[retained..].to_vec();
            for turn_id in &removed_turn_ids {
                self.pending_turn_summaries.remove(turn_id);
                if self.turn_summary_id.as_ref() == Some(turn_id) {
                    self.turn_summary_id = None;
                    self.turn_summary = TurnSummary::default();
                }
                self.rolled_back_wait_ids.extend(
                    self.turn_index.turns[turn_id]
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            codex_app_server_protocol::ThreadItem::CollabAgentToolCall {
                                id,
                                ..
                            } => Some(id.clone()),
                            _ => None,
                        }),
                );
            }
            self.current_turn_history.handle_event(event);
            self.turn_index.apply_changes(ThreadHistoryChangeSet {
                removed_turn_ids,
                ..Default::default()
            });
            return Vec::new();
        }
        if let EventMsg::RawResponseItem(payload) = event {
            // Every listener requests these events, including trusted mailbox
            // inputs that have no visible UserMessage. Do not expose raw bytes
            // or resurrect a removed turn just to retain rollback provenance.
            if self.turn_index.turns.contains_key(event_turn_id)
                && self
                    .current_turn_history
                    .record_raw_user_turn_boundary(event_turn_id, &payload.item)
            {
                self.turn_index
                    .rollback_eligible_turn_ids
                    .insert(event_turn_id.to_string());
            }
            return Vec::new();
        }
        // Legacy wait completions lack a turn id. Resolve their owner from the
        // durable index, preferring the current owner just as replay does.
        let wait_owner = if let Some((call_id, operation)) =
            ThreadHistoryBuilder::legacy_collab_end_identity(event)
        {
            let owns_wait = |turn: &&Turn| {
                turn.items.iter().any(|item| {
                    matches!(item,
                codex_app_server_protocol::ThreadItem::CollabAgentToolCall {
                    id, tool, ..
                } if id == call_id && *tool == operation)
                })
            };
            let current = self
                .current_turn_history
                .active_turn_id()
                .and_then(|id| self.turn_index.turns.get(id))
                .filter(owns_wait);
            if let Some(turn) = current {
                Some(turn.id.clone())
            } else {
                let mut owners = self.turn_index.turns.values().filter(owns_wait);
                let first = owners.next().map(|turn| turn.id.clone());
                if owners.next().is_some()
                    || (first.is_none() && self.rolled_back_wait_ids.contains(call_id))
                {
                    return Vec::new();
                }
                first
            }
        } else {
            None
        };
        let retained_turn = ThreadHistoryBuilder::scoped_event_turn_id(event)
            .or(wait_owner.as_deref())
            .filter(|id| !self.current_turn_history.contains_turn(id))
            .and_then(|id| self.turn_index.turns.get_mut(id).map(|turn| (id, turn)));
        let has_retained_turn = retained_turn.is_some();
        let changes = if let Some((turn_id, turn)) = retained_turn {
            // Retained-turn reduction replaces existing IDs in place or appends
            // new items; replacement and metadata events cannot move old offsets.
            let previous_len = turn.items.len();
            let changes = self
                .current_turn_history
                .handle_event_with_retained_turn(event, turn);
            self.turn_index
                .index_appended_retained_turn_items(turn_id, previous_len);
            changes
        } else {
            self.current_turn_history.handle_event_with_changes(event)
        };
        // Ordinary item events already emit their own notifications. Only abort
        // reconciliation creates terminal items that have no core ItemCompleted.
        let reconciled_wait_items = if matches!(event, EventMsg::TurnAborted(_)) {
            changes
                .changed_items
                .iter()
                .filter(|change| {
                    matches!(
                        change.item,
                        codex_app_server_protocol::ThreadItem::CollabAgentToolCall {
                            tool: codex_app_server_protocol::CollabAgentTool::Wait,
                            status: codex_app_server_protocol::CollabAgentToolCallStatus::Failed,
                            ..
                        }
                    )
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        for turn_id in changes
            .changed_turns
            .iter()
            .map(|change| &change.turn_id)
            .chain(changes.changed_items.iter().map(|change| &change.turn_id))
        {
            if self.current_turn_history.is_rollback_eligible_turn(turn_id) {
                self.turn_index
                    .rollback_eligible_turn_ids
                    .insert(turn_id.clone());
            }
        }
        if !has_retained_turn {
            self.turn_index.apply_changes(changes);
        }
        if matches!(event, EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_))
            && !self.current_turn_history.has_active_turn()
        {
            self.current_turn_history.reset();
        }
        reconciled_wait_items
    }

    fn seed_current_turn_history(&mut self, items: &[RolloutItem]) {
        self.current_turn_history.reset();
        for item in items {
            self.current_turn_history.handle_rollout_item(item);
        }
    }

    pub(crate) fn seed_resume_history_for_listener(
        &mut self,
        items: &[RolloutItem],
        listener_generation: u64,
        history_generation: u64,
    ) -> bool {
        if self.listener_generation != listener_generation || self.listener_command_tx.is_none() {
            return false;
        }
        if !self.turn_index.initialized || self.turn_index_source_partial {
            if self.turn_index_rollback_generation != history_generation {
                return false;
            }
            self.seed_turn_index_from_history(items);
        }
        // Persisted history can lag events already consumed by this listener.
        // Keep the live turn, including its pending approvals, when rejoining it.
        if self.current_turn_history.in_progress_turn_id().is_none() {
            self.seed_current_turn_history(items);
            // A lagging resume snapshot must not resurrect a turn whose terminal
            // event this listener has already consumed and indexed.
            if self
                .current_turn_history
                .in_progress_turn_id()
                .and_then(|turn_id| self.turn_index.turns.get(turn_id))
                .is_some_and(|turn| turn.status != TurnStatus::InProgress)
            {
                self.current_turn_history.reset();
            }
        }
        self.resume_history_seeded_generation = Some(listener_generation);
        true
    }

    /// The listener's index overlays every completion consumed since the durable
    /// snapshot. Capture this before admitting the observer, not in the RPC task.
    pub(crate) fn resume_turns_snapshot(
        &self,
        limit: usize,
        direction: SortDirection,
        items_view: TurnItemsView,
    ) -> Option<IndexedTurnPage> {
        self.turn_index.page(None, limit, direction, items_view).ok().flatten()
    }

    pub(crate) fn turn_origin_tracker(&self) -> TurnOriginTracker {
        self.turn_origin_tracker.clone()
    }

    pub(crate) fn note_thread_settings(&mut self, thread_settings: ThreadSettings) -> bool {
        let changed = self.last_thread_settings.as_ref() != Some(&thread_settings);
        self.last_thread_settings = Some(thread_settings);
        changed
    }
}

pub(crate) async fn resolve_server_request_on_thread_listener(
    thread_state: &Arc<Mutex<ThreadState>>,
    request_id: RequestId,
) -> Result<(), ResolveServerRequestError> {
    fn unresolved(
        request_id: RequestId,
        failure: ResolveServerRequestFailure,
    ) -> ResolveServerRequestError {
        ResolveServerRequestError {
            request_id,
            failure,
        }
    }

    let (completion_tx, completion_rx) = oneshot::channel();
    let listener_command_tx = {
        let state = thread_state.lock().await;
        state.listener_command_route()
    };
    let Some((listener_command_tx, cancellation)) = listener_command_tx else {
        return Err(unresolved(
            request_id,
            ResolveServerRequestFailure::ListenerNotRunning,
        ));
    };

    let unresolved_request_id = request_id.clone();
    let deadline = tokio::time::Instant::now() + crate::outgoing_message::RESOURCE_DELIVERY_TIMEOUT;
    let sent = tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep_until(deadline) => {
            cancellation.cancel();
            return Err(unresolved(unresolved_request_id, ResolveServerRequestFailure::DeliveryTimedOut));
        }
        result = listener_command_tx.send(ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        }) => result.is_ok(),
    };
    if !sent {
        return Err(unresolved(
            unresolved_request_id,
            ResolveServerRequestFailure::ListenerClosed,
        ));
    }

    let completed = tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        _ = tokio::time::sleep_until(deadline) => {
            cancellation.cancel();
            return Err(unresolved(unresolved_request_id, ResolveServerRequestFailure::DeliveryTimedOut));
        }
        result = completion_rx => result.is_ok(),
    };
    if !completed {
        return Err(unresolved(
            unresolved_request_id,
            ResolveServerRequestFailure::CompletionDropped,
        ));
    }
    Ok(())
}



struct ThreadEntry {
    state: Arc<Mutex<ThreadState>>,
    volatile_project: Option<(Weak<CodexThread>, String)>,
    connection_ids: HashSet<ConnectionId>,
    has_connections_watcher: watch::Sender<bool>,
}

impl Default for ThreadEntry {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadState::default())),
            volatile_project: None,
            connection_ids: HashSet::new(),
            has_connections_watcher: watch::channel(false).0,
        }
    }
}

impl ThreadEntry {
    fn update_has_connections(&self) {
        let _ = self.has_connections_watcher.send_if_modified(|current| {
            let prev = *current;
            *current = !self.connection_ids.is_empty();
            prev != *current
        });
    }
}

#[derive(Default)]
struct ThreadStateManagerInner {
    live_connections: HashMap<ConnectionId, ConnectionCapabilities>,
    threads: HashMap<ThreadId, ThreadEntry>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<ThreadId>>,
    out_of_band_elicitation_leases:
        HashMap<ThreadId, HashMap<OutOfBandElicitationLeaseKey, Weak<CodexThread>>>,
    in_flight_tasks: HashMap<String, InFlightTaskReference>,
    in_flight_turn_starts: HashMap<ThreadId, String>,
    in_flight_client_submissions: HashMap<ThreadId, (String, String, String)>,
    turn_start_cancellation: HashMap<ThreadId, tokio_util::sync::CancellationToken>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OutOfBandElicitationLeaseKey {
    connection_id: ConnectionId,
    lease_id: String,
}

impl OutOfBandElicitationLeaseKey {
    pub(crate) fn new(connection_id: ConnectionId, lease_id: String) -> Self {
        Self {
            connection_id,
            lease_id,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ConnectionCapabilities {
    pub(crate) request_attestation: bool,
    pub(crate) experimental_api: bool,
}

#[derive(Clone, Default)]
pub(crate) struct ThreadStateManager {
    state: Arc<Mutex<ThreadStateManagerInner>>,
    // Extension event sinks are synchronous, so they need an await-free way to
    // enqueue work on the active per-thread listener.
    listener_commands: Arc<StdMutex<HashMap<ThreadId, ListenerCommandRoute>>>,
}

struct ListenerCommandRoute {
    tx: mpsc::Sender<ThreadListenerCommand>,
    overflow: CancellationToken,
}

fn core_lease_id(lease: &OutOfBandElicitationLeaseKey) -> OutOfBandElicitationLeaseId {
    OutOfBandElicitationLeaseId::new(lease.connection_id.0, lease.lease_id.clone())
}

fn release_out_of_band_elicitation_leases(
    leases: impl IntoIterator<Item = (OutOfBandElicitationLeaseKey, Weak<CodexThread>)>,
) {
    for (lease, thread) in leases {
        if let Some(thread) = thread.upgrade() {
            thread.release_out_of_band_elicitation_lease(&core_lease_id(&lease));
        }
    }
}

fn release_out_of_band_elicitation_leases_for_connection(
    state: &mut ThreadStateManagerInner,
    thread_id: ThreadId,
    connection_id: ConnectionId,
) {
    let mut removed = Vec::new();
    let mut remove_thread_entry = false;
    if let Some(leases) = state.out_of_band_elicitation_leases.get_mut(&thread_id) {
        let lease_keys = leases
            .keys()
            .filter(|lease| lease.connection_id == connection_id)
            .cloned()
            .collect::<Vec<_>>();
        for lease in lease_keys {
            if let Some(thread) = leases.remove(&lease) {
                removed.push((lease, thread));
            }
        }
        remove_thread_entry = leases.is_empty();
    }
    if remove_thread_entry {
        state.out_of_band_elicitation_leases.remove(&thread_id);
    }
    release_out_of_band_elicitation_leases(removed);
}

fn release_all_out_of_band_elicitation_leases_for_connection(
    state: &mut ThreadStateManagerInner,
    connection_id: ConnectionId,
) {
    let thread_ids = state
        .out_of_band_elicitation_leases
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for thread_id in thread_ids {
        release_out_of_band_elicitation_leases_for_connection(state, thread_id, connection_id);
    }
}

impl ThreadStateManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn turn_start_cancellation(
        &self,
        thread_id: ThreadId,
    ) -> tokio_util::sync::CancellationToken {
        self.state.lock().await.turn_start_cancellation.entry(thread_id).or_default().clone()
    }

    pub(crate) async fn cancel_pending_turn_starts(&self, thread_id: ThreadId) -> Option<String> {
        let mut state = self.state.lock().await;
        let token = state.turn_start_cancellation.entry(thread_id).or_default();
        token.cancel();
        *token = tokio_util::sync::CancellationToken::new();
        state.in_flight_turn_starts.get(&thread_id).cloned()
    }

    pub(crate) async fn pending_turn_start_id(&self, thread_id: ThreadId) -> Option<String> {
        self.state.lock().await.in_flight_turn_starts.get(&thread_id).cloned()
    }

    pub(crate) async fn pending_client_submission(
        &self, thread_id: ThreadId, client_id: &str, fingerprint: &str,
    ) -> Result<Option<String>, ()> {
        let state = self.state.lock().await;
        match state.in_flight_client_submissions.get(&thread_id) {
            Some((id, hash, turn_id)) if id == client_id => {
                if hash == fingerprint { Ok(Some(turn_id.clone())) } else { Err(()) }
            }
            _ => Ok(None),
        }
    }

    pub(crate) async fn claim_turn_start(
        &self,
        fingerprint: Option<&str>,
        thread_id: ThreadId,
        turn_id: &str,
        client_submission: Option<(&str, &str)>,
    ) -> TurnStartClaim {
        let mut state = self.state.lock().await;
        if let Some((client_id, fingerprint)) = client_submission
            && let Some((id, hash, original)) = state.in_flight_client_submissions.get(&thread_id)
            && id == client_id
        {
            return if hash == fingerprint { TurnStartClaim::Replay(original.clone()) }
                else { TurnStartClaim::SubmissionConflict };
        }
        if let Some(existing) = fingerprint.and_then(|key| state.in_flight_tasks.get(key)) {
            return TurnStartClaim::IdenticalTask(existing.clone());
        }
        if let Some(existing_turn_id) = state.in_flight_turn_starts.get(&thread_id) {
            return TurnStartClaim::ActiveTurn(InFlightTaskReference {
                thread_id,
                turn_id: existing_turn_id.clone(),
            });
        }

        // This map is live correctness state. Evicting an active fingerprint
        // would admit the same task again while its original execution runs.
        if fingerprint.is_some() && state.in_flight_tasks.len() >= MAX_TRACKED_IN_FLIGHT_TASKS {
            return TurnStartClaim::CapacityExceeded;
        }

        state
            .in_flight_turn_starts
            .insert(thread_id, turn_id.to_string());
        if let Some((client_id, fingerprint)) = client_submission {
            state.in_flight_client_submissions.insert(thread_id,
                (client_id.to_owned(), fingerprint.to_owned(), turn_id.to_owned()));
        }
        if let Some(fingerprint) = fingerprint {
            state.in_flight_tasks.insert(
                fingerprint.to_string(),
                InFlightTaskReference {
                    thread_id,
                    turn_id: turn_id.to_string(),
                },
            );
        }
        TurnStartClaim::Claimed
    }


    pub(crate) async fn release_turn_start(&self, thread_id: ThreadId, turn_id: &str) {
        let mut state = self.state.lock().await;
        state
            .in_flight_tasks
            .retain(|_, entry| entry.thread_id != thread_id || entry.turn_id.as_str() != turn_id);
        if state
            .in_flight_turn_starts
            .get(&thread_id)
            .is_some_and(|active_turn_id| active_turn_id == turn_id)
        {
            state.in_flight_turn_starts.remove(&thread_id);
            state.in_flight_client_submissions.remove(&thread_id);
        }
    }

    pub(crate) async fn connection_initialized(
        &self,
        connection_id: ConnectionId,
        capabilities: ConnectionCapabilities,
    ) {
        self.state
            .lock()
            .await
            .live_connections
            .insert(connection_id, capabilities);
    }

    /// Publish synchronous connection-owned bookkeeping under the same admission
    /// lock that removes a connection. The callback must not perform I/O or await.
    pub(crate) async fn with_live_connection<T>(
        &self,
        connection_id: ConnectionId,
        publish: impl FnOnce() -> T,
    ) -> Option<T> {
        let state = self.state.lock().await;
        state.live_connections.contains_key(&connection_id).then(publish)
    }

    pub(crate) async fn acquire_out_of_band_elicitation_lease(
        &self,
        thread_id: ThreadId,
        lease: OutOfBandElicitationLeaseKey,
        thread: &Arc<CodexThread>,
    ) -> CodexResult<i64> {
        let mut state = self.state.lock().await;
        if !state.live_connections.contains_key(&lease.connection_id) {
            return Err(CodexErr::InvalidRequest(
                "connection is no longer active".to_string(),
            ));
        }

        let leases = state
            .out_of_band_elicitation_leases
            .entry(thread_id)
            .or_default();
        if leases.contains_key(&lease) {
            return Err(CodexErr::InvalidRequest(
                "out-of-band elicitation lease already exists".to_string(),
            ));
        }

        let count = thread.acquire_out_of_band_elicitation_lease(core_lease_id(&lease))?;
        leases.insert(lease, Arc::downgrade(thread));
        Ok(count)
    }

    pub(crate) async fn release_out_of_band_elicitation_lease(
        &self,
        thread_id: ThreadId,
        lease: &OutOfBandElicitationLeaseKey,
    ) -> Option<i64> {
        let mut state = self.state.lock().await;
        let entry = state
            .out_of_band_elicitation_leases
            .get_mut(&thread_id)
            .and_then(|leases| leases.remove(lease));
        if state
            .out_of_band_elicitation_leases
            .get(&thread_id)
            .is_some_and(HashMap::is_empty)
        {
            state.out_of_band_elicitation_leases.remove(&thread_id);
        }

        entry.map(|thread| {
            thread.upgrade().map_or(0, |thread| {
                thread.release_out_of_band_elicitation_lease(&core_lease_id(lease))
            })
        })
    }

    pub(crate) async fn clear_all_out_of_band_elicitation_leases(&self) {
        let mut state = self.state.lock().await;
        let leases = std::mem::take(&mut state.out_of_band_elicitation_leases);
        release_out_of_band_elicitation_leases(leases.into_values().flatten());
    }


    pub(crate) async fn attestation_capable_connections_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ConnectionId> {
        let state = self.state.lock().await;
        let Some(thread) = state.threads.get(&thread_id) else {
            return Vec::new();
        };
        let mut connection_ids = thread
            .connection_ids
            .iter()
            .filter_map(|connection_id| {
                state
                    .live_connections
                    .get(connection_id)?
                    .request_attestation
                    .then_some(*connection_id)
            })
            .collect::<Vec<_>>();
        connection_ids.sort_by_key(|connection_id| connection_id.0);
        connection_ids
    }


    pub(crate) async fn wait_for_thread_subscriber(&self, thread_id: ThreadId) {
        let mut has_connections = {
            let mut state = self.state.lock().await;
            state
                .threads
                .entry(thread_id)
                .or_default()
                .has_connections_watcher
                .subscribe()
        };
        while !*has_connections.borrow_and_update() {
            if has_connections.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn subscribed_connection_ids(&self, thread_id: ThreadId) -> Vec<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .map(|thread_entry| thread_entry.connection_ids.iter().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) async fn connection_ids_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> (Vec<ConnectionId>, Vec<ConnectionId>) {
        let state = self.state.lock().await;
        let Some(thread_entry) = state.threads.get(&thread_id) else {
            return (Vec::new(), Vec::new());
        };
        let subscribed = thread_entry
            .connection_ids
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let experimental = thread_entry
            .connection_ids
            .iter()
            .filter_map(|connection_id| {
                state
                    .live_connections
                    .get(connection_id)
                    .is_some_and(|capabilities| capabilities.experimental_api)
                    .then_some(*connection_id)
            })
            .collect();
        (subscribed, experimental)
    }

    pub(crate) async fn connection_supports_experimental_api(
        &self,
        connection_id: ConnectionId,
    ) -> bool {
        self.state
            .lock()
            .await
            .live_connections
            .get(&connection_id)
            .is_some_and(|capabilities| capabilities.experimental_api)
    }


    pub(crate) async fn thread_state(&self, thread_id: ThreadId) -> Arc<Mutex<ThreadState>> {
        let mut state = self.state.lock().await;
        state.threads.entry(thread_id).or_default().state.clone()
    }

    /// Read-only history access must not create a lifecycle entry without a
    /// listener/subscription owner to dispose of it.
    pub(crate) async fn existing_thread_state(
        &self,
        thread_id: ThreadId,
    ) -> Option<Arc<Mutex<ThreadState>>> {
        self.state
            .lock()
            .await
            .threads
            .get(&thread_id)
            .map(|entry| Arc::clone(&entry.state))
    }

    pub(crate) async fn set_volatile_project(
        &self,
        id: ThreadId,
        thread: &Arc<CodexThread>,
        project: String,
    ) {
        self.state
            .lock()
            .await
            .threads
            .entry(id)
            .or_default()
            .volatile_project = Some((Arc::downgrade(thread), project));
    }

    pub(crate) async fn volatile_project(
        &self,
        id: ThreadId,
        thread: &CodexThread,
    ) -> Option<String> {
        let state = self.state.lock().await;
        let (owner, project) = state.threads.get(&id)?.volatile_project.as_ref()?;
        let owner = owner.upgrade()?;
        std::ptr::eq(owner.as_ref(), thread).then(|| project.clone())
    }

    pub(crate) async fn clear_volatile_project(&self, project: &str) -> Vec<String> {
        let mut state = self.state.lock().await;
        let mut affected = Vec::new();
        for (id, entry) in &mut state.threads {
            if entry
                .volatile_project
                .as_ref()
                .is_some_and(|(owner, assigned)| assigned == project && owner.strong_count() > 0)
            {
                entry.volatile_project = None;
                affected.push(id.to_string());
            }
        }
        affected
    }

    pub(crate) fn current_listener_command_tx(
        &self,
        thread_id: ThreadId,
    ) -> Option<mpsc::Sender<ThreadListenerCommand>> {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .map(|route| route.tx.clone())
    }

    pub(crate) fn register_listener_command_tx(
        &self,
        thread_id: ThreadId,
        tx: mpsc::Sender<ThreadListenerCommand>,
    ) -> CancellationToken {
        let overflow = CancellationToken::new();
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                thread_id,
                ListenerCommandRoute {
                    tx,
                    overflow: overflow.clone(),
                },
            );
        overflow
    }

    pub(crate) fn fail_listener_command_delivery(
        &self,
        thread_id: ThreadId,
        tx: &mpsc::Sender<ThreadListenerCommand>,
    ) {
        if let Some(route) = self
            .listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            && route.tx.same_channel(tx)
        {
            route.overflow.cancel();
        }
    }

    pub(crate) fn unregister_listener_command_tx(
        &self,
        thread_id: ThreadId,
        retiring_tx: &mpsc::Sender<ThreadListenerCommand>,
    ) {
        let mut routes = self
            .listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if routes
            .get(&thread_id)
            .is_some_and(|route| route.tx.same_channel(retiring_tx))
        {
            routes.remove(&thread_id);
        }
    }

    pub(crate) async fn remove_thread_state(&self, thread_id: ThreadId) {
        let thread_state = {
            let mut state = self.state.lock().await;
            let thread_state = state
                .threads
                .remove(&thread_id)
                .map(|thread_entry| thread_entry.state);
            state.thread_ids_by_connection.retain(|_, thread_ids| {
                thread_ids.remove(&thread_id);
                !thread_ids.is_empty()
            });
            if let Some(leases) = state.out_of_band_elicitation_leases.remove(&thread_id) {
                release_out_of_band_elicitation_leases(leases);
            }
            state.in_flight_turn_starts.remove(&thread_id);
            state.in_flight_client_submissions.remove(&thread_id);
            if let Some(cancellation) = state.turn_start_cancellation.remove(&thread_id) {
                cancellation.cancel();
            }
            state
                .in_flight_tasks
                .retain(|_, entry| entry.thread_id != thread_id);
            thread_state
        };
        if let Some(thread_state) = thread_state {
            let mut thread_state = thread_state.lock().await;
            if let Some(tx) = thread_state.listener_command_tx.as_ref() {
                self.unregister_listener_command_tx(thread_id, tx);
            }
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.listener_cancellation.is_some(),
                had_active_turn = thread_state.active_turn_id().is_some(),
                "clearing thread listener during thread-state teardown"
            );
            thread_state.clear_listener();
        }
    }

    pub(crate) async fn clear_all_listeners(&self) {
        let thread_states = {
            let state = self.state.lock().await;
            state
                .threads
                .iter()
                .map(|(thread_id, thread_entry)| (*thread_id, thread_entry.state.clone()))
                .collect::<Vec<_>>()
        };

        for (thread_id, thread_state) in thread_states {
            let mut thread_state = thread_state.lock().await;
            if let Some(tx) = thread_state.listener_command_tx.as_ref() {
                self.unregister_listener_command_tx(thread_id, tx);
            }
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.listener_cancellation.is_some(),
                had_active_turn = thread_state.active_turn_id().is_some(),
                "clearing thread listener during app-server shutdown"
            );
            thread_state.clear_listener();
        }
    }

    pub(crate) async fn unsubscribe_connection_from_thread(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            if !state.threads.contains_key(&thread_id) {
                return false;
            }

            if !state
                .thread_ids_by_connection
                .get(&connection_id)
                .is_some_and(|thread_ids| thread_ids.contains(&thread_id))
            {
                return false;
            }

            if let Some(thread_ids) = state.thread_ids_by_connection.get_mut(&connection_id) {
                thread_ids.remove(&thread_id);
                if thread_ids.is_empty() {
                    state.thread_ids_by_connection.remove(&connection_id);
                }
            }
            if let Some(thread_entry) = state.threads.get_mut(&thread_id) {
                thread_entry.connection_ids.remove(&connection_id);
                thread_entry.update_has_connections();
            }
            release_out_of_band_elicitation_leases_for_connection(
                &mut state,
                thread_id,
                connection_id,
            );
        };

        true
    }


    pub(crate) async fn try_ensure_connection_subscribed(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        experimental_raw_events: bool,
    ) -> Option<Arc<Mutex<ThreadState>>> {
        let thread_state = {
            let mut state = self.state.lock().await;
            if !state.live_connections.contains_key(&connection_id) {
                return None;
            }
            state
                .thread_ids_by_connection
                .entry(connection_id)
                .or_default()
                .insert(thread_id);
            let thread_entry = state.threads.entry(thread_id).or_default();
            thread_entry.connection_ids.insert(connection_id);
            thread_entry.update_has_connections();
            thread_entry.state.clone()
        };
        {
            let mut thread_state_guard = thread_state.lock().await;
            if experimental_raw_events {
                thread_state_guard.experimental_raw_events = true;
            }
        }
        Some(thread_state)
    }

    pub(crate) async fn try_add_connection_to_thread(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        let mut state = self.state.lock().await;
        if !state.live_connections.contains_key(&connection_id) {
            return false;
        }
        state
            .thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_entry = state.threads.entry(thread_id).or_default();
        thread_entry.connection_ids.insert(connection_id);
        thread_entry.update_has_connections();
        true
    }

    pub(crate) async fn remove_connection(&self, connection_id: ConnectionId) -> Vec<ThreadId> {
        {
            let mut state = self.state.lock().await;
            state.live_connections.remove(&connection_id);
            release_all_out_of_band_elicitation_leases_for_connection(&mut state, connection_id);
            let thread_ids = state
                .thread_ids_by_connection
                .remove(&connection_id)
                .unwrap_or_default();
            for thread_id in &thread_ids {
                if let Some(thread_entry) = state.threads.get_mut(thread_id) {
                    thread_entry.connection_ids.remove(&connection_id);
                    thread_entry.update_has_connections();
                }
            }
            thread_ids
                .into_iter()
                .filter(|thread_id| {
                    state
                        .threads
                        .get(thread_id)
                        .is_some_and(|thread_entry| thread_entry.connection_ids.is_empty())
                })
                .collect::<Vec<_>>()
        }
    }

    pub(crate) async fn subscribe_to_has_connections(
        &self,
        thread_id: ThreadId,
    ) -> Option<watch::Receiver<bool>> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .map(|thread_entry| thread_entry.has_connections_watcher.subscribe())
    }
}
