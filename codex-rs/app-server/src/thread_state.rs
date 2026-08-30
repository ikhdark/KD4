use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadHistoryChangeSet;
use codex_app_server_protocol::ThreadHistoryTurnChange;
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

type PendingInterruptQueue = Vec<ConnectionRequestId>;
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
    pub(crate) listener_generation: u64,
    pub(crate) config_snapshot: ThreadConfigSnapshot,
    pub(crate) instruction_sources: Vec<LegacyAppPathString>,
    pub(crate) thread_summary: codex_app_server_protocol::Thread,
    pub(crate) emit_thread_goal_update: bool,
    pub(crate) thread_goal_state_db: Option<StateDbHandle>,
    pub(crate) include_turns: bool,
    pub(crate) initial_turns_page:
        Option<codex_app_server_protocol::ThreadResumeInitialTurnsPageParams>,
    pub(crate) prepared_initial_turns_page: Option<codex_app_server_protocol::TurnsPage>,
    pub(crate) redact_resume_payloads: bool,
}

// ThreadListenerCommand is used to perform operations in the context of the thread listener, for serialization purposes.
pub(crate) enum ThreadListenerCommand {
    // SendThreadResumeResponse is used to resume an already running thread by sending the thread's history to the client and atomically subscribing for new updates.
    SendThreadResumeResponse(Box<PendingThreadResumeRequest>),
    // EmitThreadGoalUpdated is used to order goal updates with running-thread resume responses and goal clears.
    EmitThreadGoalUpdated {
        turn_id: Option<String>,
        goal: ThreadGoal,
    },
    // EmitThreadGoalCleared is used to order app-server goal clears with running-thread resume responses.
    EmitThreadGoalCleared,
    // EmitThreadGoalSnapshot is used to read and emit the latest goal state in the listener order.
    EmitThreadGoalSnapshot {
        state_db: StateDbHandle,
    },
    // ResolveServerRequest is used to notify the client that the request has been resolved.
    // It is executed in the thread listener's context to ensure that the resolved notification is ordered with regard to the request itself.
    ResolveServerRequest {
        request_id: RequestId,
        completion_tx: oneshot::Sender<()>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolveServerRequestFailure {
    ListenerNotRunning,
    ListenerClosed,
    CompletionDropped,
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

#[derive(Default)]
struct ThreadTurnIndex {
    initialized: bool,
    order: Vec<String>,
    turn_positions: HashMap<String, usize>,
    turns: HashMap<String, Turn>,
    item_order: Vec<IndexedItemKey>,
    item_positions: HashMap<IndexedItemKey, usize>,
    item_offsets: HashMap<IndexedItemKey, usize>,
}

impl ThreadTurnIndex {
    fn apply_changes(&mut self, changes: ThreadHistoryChangeSet) {
        for turn_id in changes.removed_turn_ids {
            self.turns.remove(&turn_id);
            self.order.retain(|candidate| candidate != &turn_id);
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
            let existing_offset = self.item_offsets.get(&key).copied();
            if let Some(index) = existing_offset {
                self.turn_mut(&change.turn_id).items[index] = change.item;
            } else {
                let offset = {
                    let turn = self.turn_mut(&change.turn_id);
                    let offset = turn.items.len();
                    turn.items.push(change.item);
                    offset
                };
                self.item_offsets.insert(key.clone(), offset);
                self.insert_item_key(key);
            }
        }
    }

    fn insert_item_key(&mut self, key: IndexedItemKey) {
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

    fn apply_turn_change(&mut self, change: ThreadHistoryTurnChange) {
        let turn = self.turn_mut(&change.turn_id);
        turn.status = change.status;
        turn.error = change.error;
        turn.started_at = change.started_at;
        turn.completed_at = change.completed_at;
        turn.duration_ms = change.duration_ms;
        turn.timing = change.timing;
        turn.surfaced_result = change.surfaced_result;
        turn.reasoning_policy_history = change.reasoning_policy_history;
    }

    fn overlay_turn(&mut self, turn: Turn) {
        if !self.turns.contains_key(&turn.id) {
            self.turn_positions
                .insert(turn.id.clone(), self.order.len());
            self.order.push(turn.id.clone());
        }
        self.turns.insert(turn.id.clone(), turn);
        self.rebuild_positions();
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
                reasoning_policy_history: None,
            })
    }

    fn page(
        &self,
        anchor: Option<(&str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
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
        let mut turns = indexes
            .take(page_size.saturating_add(1))
            .filter_map(|index| self.turns.get(&self.order[index]).cloned())
            .collect::<Vec<_>>();
        let more_turns_available = turns.len() > page_size;
        turns.truncate(page_size);
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
                        self.item_offsets
                            .get(&IndexedItemKey {
                                turn_id: turn_id.to_string(),
                                item_id: item_id.to_string(),
                            })
                            .copied()
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
            let mut items = turn
                .into_iter()
                .flat_map(|turn| {
                    ordered_indexes(start, end, sort_direction).filter_map(move |index| {
                        turn.items
                            .get(index)
                            .cloned()
                            .map(|item| IndexedThreadItem {
                                turn_id: turn_id.to_string(),
                                item,
                            })
                    })
                })
                .take(page_size.saturating_add(1))
                .collect::<Vec<_>>();
            let more_items_available = items.len() > page_size;
            items.truncate(page_size);
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
            self.item_positions.get(&key).copied()
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
        let mut items = Vec::with_capacity(page_size.saturating_add(1));
        for index in ordered_indexes(start, end, sort_direction) {
            let key = &self.item_order[index];
            let Some(offset) = self.item_offsets.get(key).copied() else {
                continue;
            };
            let Some(item) = self
                .turns
                .get(&key.turn_id)
                .and_then(|turn| turn.items.get(offset))
                .cloned()
            else {
                continue;
            };
            items.push(IndexedThreadItem {
                turn_id: key.turn_id.clone(),
                item,
            });
            if items.len() > page_size {
                break;
            }
        }
        let more_items_available = items.len() > page_size;
        items.truncate(page_size);
        Ok(Some(IndexedItemPage {
            items,
            more_items_available,
        }))
    }

    fn rebuild_positions(&mut self) {
        self.turn_positions.clear();
        self.item_order.clear();
        self.item_positions.clear();
        self.item_offsets.clear();
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
                self.item_offsets.insert(key.clone(), offset);
                self.item_positions
                    .insert(key.clone(), self.item_order.len());
                self.item_order.push(key);
            }
        }
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

#[derive(Default)]
pub(crate) struct ThreadState {
    pub(crate) pending_interrupts: PendingInterruptQueue,
    pub(crate) pending_rollbacks: Option<ConnectionRequestId>,
    pub(crate) turn_summary: TurnSummary,
    pub(crate) cancel_tx: Option<oneshot::Sender<()>>,
    pub(crate) experimental_raw_events: bool,
    pub(crate) listener_generation: u64,
    resume_history_seeded_generation: Option<u64>,
    last_thread_settings: Option<ThreadSettings>,
    listener_command_tx: Option<mpsc::Sender<ThreadListenerCommand>>,
    current_turn_history: ThreadHistoryBuilder,
    turn_index: ThreadTurnIndex,
    turn_origin_tracker: TurnOriginTracker,
    listener_thread: Option<Weak<CodexThread>>,
    watch_registration: WatchRegistration,
}

impl ThreadState {
    #[cfg(test)]
    fn retained_server_request_resolution_count(&self) -> usize {
        0
    }

    pub(crate) fn listener_matches(&self, conversation: &Arc<CodexThread>) -> bool {
        self.listener_thread
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|existing| Arc::ptr_eq(&existing, conversation))
    }

    pub(crate) fn set_listener(
        &mut self,
        cancel_tx: oneshot::Sender<()>,
        conversation: &Arc<CodexThread>,
        watch_registration: WatchRegistration,
        thread_settings_baseline: ThreadSettings,
    ) -> (mpsc::Receiver<ThreadListenerCommand>, u64) {
        if let Some(previous) = self.cancel_tx.replace(cancel_tx) {
            let _ = previous.send(());
        }
        self.listener_generation = self.listener_generation.wrapping_add(1);
        self.last_thread_settings = Some(thread_settings_baseline);
        let (listener_command_tx, listener_command_rx) = thread_listener_command_channel();
        self.listener_command_tx = Some(listener_command_tx);
        self.listener_thread = Some(Arc::downgrade(conversation));
        self.watch_registration = watch_registration;
        (listener_command_rx, self.listener_generation)
    }

    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancel_tx) = self.cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
        self.listener_command_tx = None;
        self.current_turn_history.reset();
        self.listener_thread = None;
        self.watch_registration = WatchRegistration::default();
    }

    pub(crate) fn listener_command_tx(&self) -> Option<mpsc::Sender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    pub(crate) fn resume_history_is_seeded_for_current_listener(&self) -> bool {
        self.listener_command_tx.is_some()
            && self.resume_history_seeded_generation == Some(self.listener_generation)
    }

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
    }

    pub(crate) fn indexed_turns_page(
        &self,
        anchor: Option<(&str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
    ) -> Result<Option<IndexedTurnPage>, ()> {
        self.turn_index.page(anchor, page_size, sort_direction)
    }

    pub(crate) fn indexed_items_page(
        &self,
        turn_id_filter: Option<&str>,
        anchor: Option<(&str, &str, bool)>,
        page_size: usize,
        sort_direction: SortDirection,
    ) -> Result<Option<IndexedItemPage>, ()> {
        self.turn_index
            .items_page(turn_id_filter, anchor, page_size, sort_direction)
    }

    /// Seeds the pagination index once from persisted history while preserving
    /// live events that arrived before the initial history load completed.
    pub(crate) fn seed_turn_index_from_history(&mut self, items: &[RolloutItem]) {
        let live_index = std::mem::take(&mut self.turn_index);
        let mut builder = ThreadHistoryBuilder::new();
        for item in items {
            let changes = builder.handle_rollout_item_with_changes(item);
            self.turn_index.apply_changes(changes);
        }
        for turn_id in live_index.order {
            if let Some(turn) = live_index.turns.get(&turn_id).cloned() {
                self.turn_index.overlay_turn(turn);
            }
        }
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

    pub(crate) fn track_current_turn_event(&mut self, event_turn_id: &str, event: &EventMsg) {
        if let EventMsg::TurnStarted(payload) = event {
            self.turn_summary.started_at = payload.started_at;
            self.turn_summary.origin_connection_id = self.turn_origin_tracker.take(event_turn_id);
        }
        let changes = self.current_turn_history.handle_event_with_changes(event);
        self.turn_index.apply_changes(changes);
        if matches!(event, EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_))
            && !self.current_turn_history.has_active_turn()
        {
            self.current_turn_history.reset();
        }
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
    ) -> bool {
        if self.listener_generation != listener_generation || self.listener_command_tx.is_none() {
            return false;
        }
        self.seed_current_turn_history(items);
        self.resume_history_seeded_generation = Some(listener_generation);
        true
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
        state.listener_command_tx()
    };
    let Some(listener_command_tx) = listener_command_tx else {
        return Err(unresolved(
            request_id,
            ResolveServerRequestFailure::ListenerNotRunning,
        ));
    };

    let unresolved_request_id = request_id.clone();
    if listener_command_tx
        .send(ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        })
        .await
        .is_err()
    {
        return Err(unresolved(
            unresolved_request_id,
            ResolveServerRequestFailure::ListenerClosed,
        ));
    }

    if completion_rx.await.is_err() {
        return Err(unresolved(
            unresolved_request_id,
            ResolveServerRequestFailure::CompletionDropped,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ApprovalsReviewer;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::SandboxPolicy;
    use codex_protocol::config_types::CollaborationMode;
    use codex_protocol::config_types::ModeKind;
    use codex_protocol::config_types::Settings;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    #[test]
    fn internal_api_visibility_is_minimal() {
        let source = include_str!("thread_state.rs");
        let obsolete_declaration = ["fn", " set_experimental_raw_events"].concat();
        let obsolete_in_flight_order = ["in_flight_task", "_order"].concat();

        assert!(
            !source.contains(&obsolete_declaration),
            "single-use thread-state mutator must remain removed"
        );
        assert!(
            !source.contains(&obsolete_in_flight_order),
            "unused in-flight task ordering state must remain removed"
        );
    }

    fn terminal_event(turn_id: &str, message: &str) -> EventMsg {
        EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
            turn_id: turn_id.to_string(),
            last_agent_message: Some(message.to_string()),
            surfaced_result: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        })
    }

    #[test]
    fn note_thread_settings_reports_only_effective_changes() {
        let mut state = ThreadState::default();
        let initial = thread_settings("mock-model");
        let updated = thread_settings("mock-model-2");

        let results = vec![
            state.note_thread_settings(initial.clone()),
            state.note_thread_settings(initial),
            state.note_thread_settings(updated.clone()),
            state.note_thread_settings(updated),
        ];

        assert_eq!(results, vec![true, false, true, false]);
    }

    #[test]
    fn turn_started_claims_the_origin_reserved_for_its_canonical_id() {
        let mut state = ThreadState::default();
        let turn_id = "turn-1".to_string();
        state
            .turn_origin_tracker()
            .reserve(turn_id.clone(), ConnectionId(7))
            .commit();

        state.track_current_turn_event(
            &turn_id,
            &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                turn_id: turn_id.clone(),
                trace_id: None,
                started_at: Some(42),
                model_context_window: None,
                collaboration_mode_kind: ModeKind::Default,
            }),
        );

        assert_eq!(
            state.turn_summary.origin_connection_id,
            Some(ConnectionId(7))
        );
        assert_eq!(state.turn_summary.started_at, Some(42));
    }

    #[test]
    fn cancelled_turn_origin_reservation_is_removed() {
        let tracker = TurnOriginTracker::default();
        let reservation = tracker.reserve("turn-1".to_string(), ConnectionId(7));
        drop(reservation);

        assert_eq!(tracker.take("turn-1"), None);
    }

    #[test]
    fn live_turn_origins_are_not_evicted_by_unrelated_reservations() {
        let tracker = TurnOriginTracker::default();
        tracker
            .reserve("turn-0".to_string(), ConnectionId(7))
            .commit();
        for index in 1..300 {
            tracker
                .reserve(format!("turn-{index}"), ConnectionId(index))
                .commit();
        }

        assert_eq!(tracker.take("turn-0"), Some(ConnectionId(7)));
    }

    #[tokio::test]
    async fn resolving_without_a_listener_returns_a_typed_error() {
        let state = Arc::new(Mutex::new(ThreadState::default()));
        let request_id = RequestId::Integer(1);

        assert_eq!(
            resolve_server_request_on_thread_listener(&state, request_id.clone()).await,
            Err(ResolveServerRequestError {
                request_id: request_id.clone(),
                failure: ResolveServerRequestFailure::ListenerNotRunning,
            })
        );
        assert_eq!(
            state
                .lock()
                .await
                .retained_server_request_resolution_count(),
            0
        );
    }

    #[tokio::test]
    async fn resolving_on_a_closed_listener_returns_a_typed_error() {
        let state = Arc::new(Mutex::new(ThreadState::default()));
        let (listener_command_tx, listener_command_rx) = thread_listener_command_channel();
        drop(listener_command_rx);
        state.lock().await.listener_command_tx = Some(listener_command_tx);
        let request_id = RequestId::Integer(2);

        assert_eq!(
            resolve_server_request_on_thread_listener(&state, request_id.clone()).await,
            Err(ResolveServerRequestError {
                request_id: request_id.clone(),
                failure: ResolveServerRequestFailure::ListenerClosed,
            })
        );
        assert_eq!(
            state
                .lock()
                .await
                .retained_server_request_resolution_count(),
            0
        );
    }

    #[tokio::test]
    async fn resolving_with_dropped_completion_returns_a_typed_error() {
        let state = Arc::new(Mutex::new(ThreadState::default()));
        let (listener_command_tx, mut listener_command_rx) = thread_listener_command_channel();
        state.lock().await.listener_command_tx = Some(listener_command_tx);
        let listener = tokio::spawn(async move {
            let Some(ThreadListenerCommand::ResolveServerRequest { completion_tx, .. }) =
                listener_command_rx.recv().await
            else {
                panic!("expected a server-request resolution command");
            };
            drop(completion_tx);
        });
        let request_id = RequestId::Integer(3);

        assert_eq!(
            resolve_server_request_on_thread_listener(&state, request_id.clone()).await,
            Err(ResolveServerRequestError {
                request_id: request_id.clone(),
                failure: ResolveServerRequestFailure::CompletionDropped,
            })
        );
        listener.await.expect("listener task should complete");
        assert_eq!(
            state
                .lock()
                .await
                .retained_server_request_resolution_count(),
            0
        );
    }

    #[tokio::test]
    async fn turn_start_claims_coalesce_tasks_and_serialize_each_thread() {
        let manager = ThreadStateManager::new();
        let first_thread = ThreadId::new();
        let second_thread = ThreadId::new();

        assert_eq!(
            manager
                .claim_turn_start(Some("fingerprint"), first_thread, "turn-1")
                .await,
            TurnStartClaim::Claimed
        );
        assert_eq!(
            manager
                .claim_turn_start(Some("fingerprint"), second_thread, "turn-2")
                .await,
            TurnStartClaim::IdenticalTask(InFlightTaskReference {
                thread_id: first_thread,
                turn_id: "turn-1".to_string(),
            })
        );
        assert_eq!(
            manager
                .claim_turn_start(Some("different-task"), first_thread, "turn-2")
                .await,
            TurnStartClaim::ActiveTurn(InFlightTaskReference {
                thread_id: first_thread,
                turn_id: "turn-1".to_string(),
            })
        );

        manager.release_turn_start(first_thread, "turn-1").await;
        assert!(manager.state.lock().await.in_flight_tasks.is_empty());
        assert_eq!(
            manager
                .claim_turn_start(Some("fingerprint"), second_thread, "turn-2")
                .await,
            TurnStartClaim::Claimed
        );
    }

    #[tokio::test]
    async fn in_flight_task_capacity_never_evicts_live_fingerprints() {
        let manager = ThreadStateManager::new();
        let original_thread = ThreadId::new();

        for index in 0..MAX_TRACKED_IN_FLIGHT_TASKS {
            let thread_id = if index == 0 {
                original_thread
            } else {
                ThreadId::new()
            };
            assert_eq!(
                manager
                    .claim_turn_start(
                        Some(&format!("fingerprint-{index}")),
                        thread_id,
                        &format!("turn-{index}"),
                    )
                    .await,
                TurnStartClaim::Claimed
            );
        }

        assert_eq!(
            manager
                .claim_turn_start(Some("overflow"), ThreadId::new(), "overflow-turn")
                .await,
            TurnStartClaim::CapacityExceeded
        );
        assert_eq!(
            manager
                .claim_turn_start(Some("fingerprint-0"), ThreadId::new(), "duplicate-turn")
                .await,
            TurnStartClaim::IdenticalTask(InFlightTaskReference {
                thread_id: original_thread,
                turn_id: "turn-0".to_string(),
            })
        );
    }

    #[test]
    fn interrupted_history_snapshot_is_not_live_turn_state() {
        let mut state = ThreadState::default();
        state.track_current_turn_event(
            "turn-1",
            &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                turn_id: "turn-1".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: ModeKind::Default,
            }),
        );
        assert_eq!(state.in_progress_turn_id(), Some("turn-1"));

        state.track_current_turn_event(
            "turn-1",
            &EventMsg::TurnAborted(codex_protocol::protocol::TurnAbortedEvent {
                turn_id: Some("turn-1".to_string()),
                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        );

        assert_eq!(state.in_progress_turn_id(), None);
        assert_eq!(
            state.active_turn_snapshot().map(|turn| turn.status),
            Some(codex_app_server_protocol::TurnStatus::Interrupted)
        );
    }

    #[test]
    fn seeded_turn_index_pages_without_replaying_full_history() {
        let mut items = Vec::new();
        for (turn_id, message) in [("turn-1", "first"), ("turn-2", "second")] {
            items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                codex_protocol::protocol::TurnStartedEvent {
                    turn_id: turn_id.to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: ModeKind::Default,
                },
            )));
            items.push(RolloutItem::EventMsg(EventMsg::UserMessage(
                codex_protocol::protocol::UserMessageEvent {
                    client_id: None,
                    message: message.to_string(),
                    images: None,
                    local_images: Vec::new(),
                    text_elements: Vec::new(),
                    ..Default::default()
                },
            )));
            items.push(RolloutItem::EventMsg(terminal_event(turn_id, message)));
        }

        let mut state = ThreadState::default();
        state.seed_turn_index_from_history(&items);

        let newest = state
            .indexed_turns_page(None, 1, SortDirection::Desc)
            .expect("valid page")
            .expect("initialized index");
        assert_eq!(newest.turns[0].id, "turn-2");
        assert!(newest.more_turns_available);

        let older = state
            .indexed_turns_page(
                Some(("turn-2", /*include_anchor*/ false)),
                1,
                SortDirection::Desc,
            )
            .expect("valid page")
            .expect("initialized index");
        assert_eq!(older.turns[0].id, "turn-1");
        assert!(!older.more_turns_available);
    }

    #[test]
    fn seeded_item_index_pages_without_replaying_full_history() {
        let mut items = Vec::new();
        for (turn_id, message) in [("turn-1", "first"), ("turn-2", "second")] {
            items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(
                codex_protocol::protocol::TurnStartedEvent {
                    turn_id: turn_id.to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: ModeKind::Default,
                },
            )));
            items.push(RolloutItem::EventMsg(EventMsg::UserMessage(
                codex_protocol::protocol::UserMessageEvent {
                    client_id: None,
                    message: message.to_string(),
                    images: None,
                    local_images: Vec::new(),
                    text_elements: Vec::new(),
                    ..Default::default()
                },
            )));
            items.push(RolloutItem::EventMsg(terminal_event(turn_id, message)));
        }

        let mut state = ThreadState::default();
        state.seed_turn_index_from_history(&items);

        let first = state
            .indexed_items_page(None, None, 2, SortDirection::Asc)
            .expect("valid page")
            .expect("initialized index");
        assert_eq!(first.items.len(), 2);
        assert!(first.more_items_available);
        let anchor = first.items.last().expect("page anchor");

        let second = state
            .indexed_items_page(
                None,
                Some((anchor.turn_id.as_str(), anchor.item.id(), false)),
                2,
                SortDirection::Asc,
            )
            .expect("valid page")
            .expect("initialized index");
        assert_eq!(second.items.len(), 2);
        assert!(!second.more_items_available);
        assert!(second.items.iter().all(|entry| entry.turn_id == "turn-2"));
    }

    #[test]
    fn late_indexed_items_remain_grouped_by_turn_order() {
        let mut index = ThreadTurnIndex::default();
        index.turn_mut("turn-1");
        index.turn_mut("turn-2");
        let turn_2_item = IndexedItemKey {
            turn_id: "turn-2".to_string(),
            item_id: "item-2".to_string(),
        };
        let turn_1_item = IndexedItemKey {
            turn_id: "turn-1".to_string(),
            item_id: "item-1".to_string(),
        };

        index.insert_item_key(turn_2_item.clone());
        index.insert_item_key(turn_1_item.clone());

        assert_eq!(
            index.item_order,
            vec![turn_1_item.clone(), turn_2_item.clone()]
        );
        assert_eq!(index.item_positions[&turn_1_item], 0);
        assert_eq!(index.item_positions[&turn_2_item], 1);
    }

    #[test]
    fn resume_history_seed_is_scoped_to_listener_generation() {
        let event = EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            trace_id: None,
            started_at: None,
            model_context_window: None,
            collaboration_mode_kind: ModeKind::Default,
        });
        let mut state = ThreadState::default();
        let (listener_command_tx, _listener_command_rx) = thread_listener_command_channel();
        state.listener_command_tx = Some(listener_command_tx);
        state.listener_generation = 7;
        let listener_generation = state.listener_generation;

        assert!(!state.resume_history_is_seeded_for_current_listener());
        assert!(state.seed_resume_history_for_listener(
            &[RolloutItem::EventMsg(event)],
            listener_generation,
        ));
        assert!(state.resume_history_is_seeded_for_current_listener());
        assert_eq!(state.in_progress_turn_id(), Some("turn-1"));

        state.listener_generation += 1;
        assert!(!state.resume_history_is_seeded_for_current_listener());
        assert!(!state.seed_resume_history_for_listener(&[], 7));
    }

    fn thread_settings(model: &str) -> ThreadSettings {
        ThreadSettings {
            cwd: AbsolutePathBuf::from_absolute_path("/tmp").expect("absolute path"),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox_policy: SandboxPolicy::ReadOnly {
                network_access: false,
            },
            permission_profile: None,
            active_permission_profile: None,
            model: model.to_string(),
            model_provider: "mock_provider".to_string(),
            service_tier: None,
            effort: None,
            summary: None,
            collaboration_mode: CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: model.to_string(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            },
            personality: None,
        }
    }
}

struct ThreadEntry {
    state: Arc<Mutex<ThreadState>>,
    connection_ids: HashSet<ConnectionId>,
    has_connections_watcher: watch::Sender<bool>,
}

impl Default for ThreadEntry {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadState::default())),
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
    listener_commands: Arc<StdMutex<HashMap<ThreadId, mpsc::Sender<ThreadListenerCommand>>>>,
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

    pub(crate) async fn claim_turn_start(
        &self,
        fingerprint: Option<&str>,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> TurnStartClaim {
        let mut state = self.state.lock().await;
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

    #[cfg(test)]
    pub(crate) async fn out_of_band_elicitation_lease_count(&self, thread_id: ThreadId) -> usize {
        self.state
            .lock()
            .await
            .out_of_band_elicitation_leases
            .get(&thread_id)
            .map_or(0, HashMap::len)
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

    #[cfg(test)]
    pub(crate) async fn first_attestation_capable_connection_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Option<ConnectionId> {
        self.attestation_capable_connections_for_thread(thread_id)
            .await
            .into_iter()
            .next()
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

    pub(crate) async fn experimental_api_connection_ids(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)
            .into_iter()
            .flat_map(|thread_entry| thread_entry.connection_ids.iter())
            .filter_map(|connection_id| {
                state
                    .live_connections
                    .get(connection_id)
                    .is_some_and(|capabilities| capabilities.experimental_api)
                    .then_some(*connection_id)
            })
            .collect()
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

    #[cfg(test)]
    pub(crate) async fn is_connection_initialized(&self, connection_id: ConnectionId) -> bool {
        self.state
            .lock()
            .await
            .live_connections
            .contains_key(&connection_id)
    }

    pub(crate) async fn thread_state(&self, thread_id: ThreadId) -> Arc<Mutex<ThreadState>> {
        let mut state = self.state.lock().await;
        state.threads.entry(thread_id).or_default().state.clone()
    }

    pub(crate) fn current_listener_command_tx(
        &self,
        thread_id: ThreadId,
    ) -> Option<mpsc::Sender<ThreadListenerCommand>> {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn register_listener_command_tx(
        &self,
        thread_id: ThreadId,
        tx: mpsc::Sender<ThreadListenerCommand>,
    ) {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id, tx);
    }

    pub(crate) fn unregister_listener_command_tx(&self, thread_id: ThreadId) {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&thread_id);
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
            state
                .in_flight_tasks
                .retain(|_, entry| entry.thread_id != thread_id);
            thread_state
        };
        self.unregister_listener_command_tx(thread_id);

        if let Some(thread_state) = thread_state {
            let mut thread_state = thread_state.lock().await;
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.cancel_tx.is_some(),
                had_active_turn = thread_state.active_turn_snapshot().is_some(),
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
            self.unregister_listener_command_tx(thread_id);
            let mut thread_state = thread_state.lock().await;
            tracing::debug!(
                thread_id = %thread_id,
                listener_generation = thread_state.listener_generation,
                had_listener = thread_state.cancel_tx.is_some(),
                had_active_turn = thread_state.active_turn_snapshot().is_some(),
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

    #[cfg(test)]
    pub(crate) async fn has_subscribers(&self, thread_id: ThreadId) -> bool {
        self.state
            .lock()
            .await
            .threads
            .get(&thread_id)
            .is_some_and(|thread_entry| !thread_entry.connection_ids.is_empty())
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
