use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_core::CodexThread;
use codex_core::OutOfBandElicitationLeaseId;
use codex_core::ThreadConfigSnapshot;
use codex_core::terminal_event_fingerprint;
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
pub(crate) enum InFlightTaskClaim {
    Claimed,
    Existing(InFlightTaskReference),
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
    pub(crate) history_items: Vec<RolloutItem>,
    pub(crate) config_snapshot: ThreadConfigSnapshot,
    pub(crate) instruction_sources: Vec<LegacyAppPathString>,
    pub(crate) thread_summary: codex_app_server_protocol::Thread,
    pub(crate) emit_thread_goal_update: bool,
    pub(crate) thread_goal_state_db: Option<StateDbHandle>,
    pub(crate) include_turns: bool,
    pub(crate) initial_turns_page:
        Option<codex_app_server_protocol::ThreadResumeInitialTurnsPageParams>,
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

#[derive(Clone)]
pub(crate) struct TerminalNotificationReplay {
    pub(crate) fingerprint: String,
    pub(crate) notification: ServerNotification,
    pub(crate) origin_connection_id: Option<ConnectionId>,
    pub(crate) target_connection_ids: Vec<ConnectionId>,
}

pub(crate) enum TerminalEventDisposition {
    NotTerminal,
    Apply { fingerprint: String },
    ProjectNotification { fingerprint: String },
    RetryNotification(Box<TerminalNotificationReplay>),
    Acknowledge { fingerprint: String },
    RejectConflict,
    RejectStale,
}

#[derive(Clone)]
struct TerminalLedgerEntry {
    fingerprint: String,
    state_reduced: bool,
    notification: Option<ServerNotification>,
    origin_connection_id: Option<ConnectionId>,
    accepted_connection_ids: HashSet<ConnectionId>,
    notification_accepted: bool,
    acknowledged_queued: bool,
}

#[derive(Default)]
pub(crate) struct ThreadState {
    pub(crate) pending_interrupts: PendingInterruptQueue,
    pub(crate) pending_rollbacks: Option<ConnectionRequestId>,
    pub(crate) turn_summary: TurnSummary,
    terminal_ledger: HashMap<String, TerminalLedgerEntry>,
    pub(crate) cancel_tx: Option<oneshot::Sender<()>>,
    pub(crate) experimental_raw_events: bool,
    pub(crate) listener_generation: u64,
    last_thread_settings: Option<ThreadSettings>,
    listener_command_tx: Option<mpsc::Sender<ThreadListenerCommand>>,
    unresolved_server_request_resolutions: Vec<ResolveServerRequestError>,
    current_turn_history: ThreadHistoryBuilder,
    turn_origin_tracker: TurnOriginTracker,
    listener_thread: Option<Weak<CodexThread>>,
    watch_registration: WatchRegistration,
}

impl ThreadState {
    fn mark_server_request_resolution_unresolved(
        &mut self,
        request_id: RequestId,
        failure: ResolveServerRequestFailure,
    ) -> ResolveServerRequestError {
        self.unresolved_server_request_resolutions
            .retain(|unresolved| unresolved.request_id != request_id);
        let error = ResolveServerRequestError {
            request_id,
            failure,
        };
        self.unresolved_server_request_resolutions
            .push(error.clone());
        error
    }

    fn clear_unresolved_server_request_resolution(&mut self, request_id: &RequestId) {
        self.unresolved_server_request_resolutions
            .retain(|unresolved| &unresolved.request_id != request_id);
    }

    #[cfg(test)]
    fn unresolved_server_request_resolution(
        &self,
        request_id: &RequestId,
    ) -> Option<ResolveServerRequestFailure> {
        self.unresolved_server_request_resolutions
            .iter()
            .find(|unresolved| &unresolved.request_id == request_id)
            .map(|unresolved| unresolved.failure)
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

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
    }

    pub(crate) fn in_progress_turn_id(&self) -> Option<&str> {
        self.current_turn_history.in_progress_turn_id()
    }

    pub(crate) fn track_current_turn_event(&mut self, event_turn_id: &str, event: &EventMsg) {
        if let EventMsg::TurnStarted(payload) = event {
            self.turn_summary.started_at = payload.started_at;
            self.turn_summary.origin_connection_id = self.turn_origin_tracker.take(event_turn_id);
        }
        self.current_turn_history.handle_event(event);
        if matches!(event, EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_))
            && !self.current_turn_history.has_active_turn()
        {
            self.current_turn_history.reset();
        }
    }

    pub(crate) fn classify_terminal_event(
        &mut self,
        event_turn_id: &str,
        event: &EventMsg,
        current_connection_ids: &[ConnectionId],
    ) -> TerminalEventDisposition {
        let Some(fingerprint) = terminal_event_fingerprint(event) else {
            return TerminalEventDisposition::NotTerminal;
        };
        if terminal_turn_id(event) != Some(event_turn_id) {
            return TerminalEventDisposition::RejectConflict;
        }
        if let Some(entry) = self.terminal_ledger.get_mut(event_turn_id) {
            if entry.fingerprint != fingerprint {
                return TerminalEventDisposition::RejectConflict;
            }
            if !entry.state_reduced {
                return TerminalEventDisposition::Apply { fingerprint };
            }
            if entry.notification_accepted {
                return TerminalEventDisposition::Acknowledge { fingerprint };
            }
            let Some(notification) = entry.notification.clone() else {
                return TerminalEventDisposition::ProjectNotification { fingerprint };
            };
            let mut targets = current_connection_ids
                .iter()
                .copied()
                .filter(|connection_id| !entry.accepted_connection_ids.contains(connection_id))
                .collect::<Vec<_>>();
            targets.sort_unstable_by_key(|connection_id| connection_id.0);
            targets.dedup();
            if targets.is_empty() {
                entry.notification_accepted = true;
                return TerminalEventDisposition::Acknowledge { fingerprint };
            }
            return TerminalEventDisposition::RetryNotification(Box::new(
                TerminalNotificationReplay {
                    fingerprint,
                    notification,
                    origin_connection_id: entry.origin_connection_id,
                    target_connection_ids: targets,
                },
            ));
        }
        if self
            .in_progress_turn_id()
            .is_some_and(|active_turn_id| active_turn_id != event_turn_id)
        {
            return TerminalEventDisposition::RejectStale;
        }
        self.terminal_ledger.insert(
            event_turn_id.to_string(),
            TerminalLedgerEntry {
                fingerprint: fingerprint.clone(),
                state_reduced: false,
                notification: None,
                origin_connection_id: None,
                accepted_connection_ids: HashSet::new(),
                notification_accepted: false,
                acknowledged_queued: false,
            },
        );
        TerminalEventDisposition::Apply { fingerprint }
    }

    /// Reconstructs exactly-once terminal state application from retained rollout history.
    /// Notification acceptance is intentionally not inferred: core must replay the exact event
    /// so the notification can be handed to the current outbound owner.
    pub(crate) fn seed_terminal_ledger_from_history(&mut self, items: &[RolloutItem]) {
        self.current_turn_history.reset();
        for item in items {
            self.current_turn_history.handle_rollout_item(item);
            let RolloutItem::EventMsg(event) = item else {
                continue;
            };
            let (Some(turn_id), Some(fingerprint)) =
                (terminal_turn_id(event), terminal_event_fingerprint(event))
            else {
                continue;
            };
            self.terminal_ledger
                .entry(turn_id.to_string())
                .or_insert(TerminalLedgerEntry {
                    fingerprint,
                    state_reduced: true,
                    notification: None,
                    origin_connection_id: None,
                    accepted_connection_ids: HashSet::new(),
                    notification_accepted: false,
                    acknowledged_queued: false,
                });
        }
    }

    pub(crate) fn mark_terminal_state_reduced(&mut self, turn_id: &str, fingerprint: &str) {
        if let Some(entry) = self.terminal_ledger.get_mut(turn_id)
            && entry.fingerprint == fingerprint
        {
            entry.state_reduced = true;
            self.queue_acknowledged_terminal_tombstone(turn_id);
        }
    }

    pub(crate) fn record_terminal_notification_attempt(
        &mut self,
        turn_id: &str,
        fingerprint: &str,
        notification: ServerNotification,
        origin_connection_id: Option<ConnectionId>,
        targeted_connection_ids: &[ConnectionId],
        accepted_connection_ids: &[ConnectionId],
    ) -> bool {
        let Some(entry) = self.terminal_ledger.get_mut(turn_id) else {
            return false;
        };
        if entry.fingerprint != fingerprint || !entry.state_reduced {
            return false;
        }
        entry.notification = Some(notification);
        entry.origin_connection_id = origin_connection_id;
        entry
            .accepted_connection_ids
            .extend(accepted_connection_ids.iter().copied());
        entry.notification_accepted = targeted_connection_ids
            .iter()
            .all(|connection_id| entry.accepted_connection_ids.contains(connection_id));
        entry.notification_accepted
    }

    pub(crate) fn cache_terminal_notification(
        &mut self,
        turn_id: &str,
        fingerprint: &str,
        notification: ServerNotification,
        origin_connection_id: Option<ConnectionId>,
    ) -> bool {
        let Some(entry) = self.terminal_ledger.get_mut(turn_id) else {
            return false;
        };
        if entry.fingerprint != fingerprint || !entry.state_reduced {
            return false;
        }
        if entry.notification.is_some() {
            return true;
        }
        entry.notification = Some(notification);
        entry.origin_connection_id = origin_connection_id;
        true
    }

    pub(crate) fn mark_terminal_acknowledged(&mut self, turn_id: &str, fingerprint: &str) {
        if let Some(entry) = self.terminal_ledger.get_mut(turn_id)
            && entry.fingerprint == fingerprint
        {
            entry.notification_accepted = true;
            entry.notification = None;
            entry.origin_connection_id = None;
            entry.accepted_connection_ids.clear();
            self.queue_acknowledged_terminal_tombstone(turn_id);
        }
    }

    fn queue_acknowledged_terminal_tombstone(&mut self, turn_id: &str) {
        let Some(entry) = self.terminal_ledger.get_mut(turn_id) else {
            return;
        };
        if !entry.state_reduced
            || !entry.notification_accepted
            || entry.notification.is_some()
            || entry.acknowledged_queued
        {
            return;
        }
        entry.acknowledged_queued = true;
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

fn terminal_turn_id(event: &EventMsg) -> Option<&str> {
    match event {
        EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
        EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
        _ => None,
    }
}

pub(crate) async fn resolve_server_request_on_thread_listener(
    thread_state: &Arc<Mutex<ThreadState>>,
    request_id: RequestId,
) -> Result<(), ResolveServerRequestError> {
    async fn unresolved(
        thread_state: &Arc<Mutex<ThreadState>>,
        request_id: RequestId,
        failure: ResolveServerRequestFailure,
    ) -> ResolveServerRequestError {
        thread_state
            .lock()
            .await
            .mark_server_request_resolution_unresolved(request_id, failure)
    }

    let (completion_tx, completion_rx) = oneshot::channel();
    let listener_command_tx = {
        let state = thread_state.lock().await;
        state.listener_command_tx()
    };
    let Some(listener_command_tx) = listener_command_tx else {
        return Err(unresolved(
            thread_state,
            request_id,
            ResolveServerRequestFailure::ListenerNotRunning,
        )
        .await);
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
            thread_state,
            unresolved_request_id,
            ResolveServerRequestFailure::ListenerClosed,
        )
        .await);
    }

    if completion_rx.await.is_err() {
        return Err(unresolved(
            thread_state,
            unresolved_request_id,
            ResolveServerRequestFailure::CompletionDropped,
        )
        .await);
    }

    thread_state
        .lock()
        .await
        .clear_unresolved_server_request_resolution(&unresolved_request_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ApprovalsReviewer;
    use codex_app_server_protocol::AskForApproval;
    use codex_app_server_protocol::SandboxPolicy;
    use codex_app_server_protocol::ThreadGoalClearedNotification;
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
            completion: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            timing: None,
        })
    }

    fn cached_notification() -> ServerNotification {
        ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
            thread_id: "thread-1".to_string(),
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
                .unresolved_server_request_resolution(&request_id),
            Some(ResolveServerRequestFailure::ListenerNotRunning)
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
                .unresolved_server_request_resolution(&request_id),
            Some(ResolveServerRequestFailure::ListenerClosed)
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
                .unresolved_server_request_resolution(&request_id),
            Some(ResolveServerRequestFailure::CompletionDropped)
        );
    }

    #[tokio::test]
    async fn in_flight_task_coalescing_claims_and_releases() {
        let manager = ThreadStateManager::new();
        let first_thread = ThreadId::new();
        let second_thread = ThreadId::new();

        assert_eq!(
            manager
                .claim_in_flight_task(
                    "fingerprint".to_string(),
                    first_thread,
                    "turn-1".to_string(),
                )
                .await,
            InFlightTaskClaim::Claimed
        );
        assert_eq!(
            manager
                .claim_in_flight_task(
                    "fingerprint".to_string(),
                    second_thread,
                    "turn-2".to_string(),
                )
                .await,
            InFlightTaskClaim::Existing(InFlightTaskReference {
                thread_id: first_thread,
                turn_id: "turn-1".to_string(),
            })
        );

        manager.release_in_flight_task(first_thread, "turn-1").await;
        assert!(manager.state.lock().await.in_flight_tasks.is_empty());
        assert_eq!(
            manager
                .claim_in_flight_task(
                    "fingerprint".to_string(),
                    second_thread,
                    "turn-2".to_string(),
                )
                .await,
            InFlightTaskClaim::Claimed
        );
    }

    #[tokio::test]
    async fn in_flight_task_capacity_never_evicts_live_fingerprints() {
        let manager = ThreadStateManager::new();
        let original_thread = ThreadId::new();

        for index in 0..MAX_TRACKED_IN_FLIGHT_TASKS {
            assert_eq!(
                manager
                    .claim_in_flight_task(
                        format!("fingerprint-{index}"),
                        original_thread,
                        format!("turn-{index}"),
                    )
                    .await,
                InFlightTaskClaim::Claimed
            );
        }

        assert_eq!(
            manager
                .claim_in_flight_task(
                    "overflow".to_string(),
                    ThreadId::new(),
                    "overflow-turn".to_string(),
                )
                .await,
            InFlightTaskClaim::CapacityExceeded
        );
        assert_eq!(
            manager
                .claim_in_flight_task(
                    "fingerprint-0".to_string(),
                    ThreadId::new(),
                    "duplicate-turn".to_string(),
                )
                .await,
            InFlightTaskClaim::Existing(InFlightTaskReference {
                thread_id: original_thread,
                turn_id: "turn-0".to_string(),
            })
        );
    }

    #[test]
    fn identical_terminal_replay_retries_only_pending_notification_targets() {
        let mut state = ThreadState::default();
        let event = terminal_event("turn-1", "done");
        let fingerprint = terminal_event_fingerprint(&event).expect("terminal fingerprint");
        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7), ConnectionId(8)]),
            TerminalEventDisposition::Apply { .. }
        ));
        state.mark_terminal_state_reduced("turn-1", &fingerprint);
        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7), ConnectionId(8)]),
            TerminalEventDisposition::ProjectNotification { .. }
        ));

        assert!(!state.record_terminal_notification_attempt(
            "turn-1",
            &fingerprint,
            cached_notification(),
            None,
            &[ConnectionId(7), ConnectionId(8)],
            &[ConnectionId(7)],
        ));
        let TerminalEventDisposition::RetryNotification(replay) =
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7), ConnectionId(8)])
        else {
            panic!("pending terminal notification should be retried");
        };
        assert_eq!(replay.target_connection_ids, vec![ConnectionId(8)]);
        assert!(state.record_terminal_notification_attempt(
            "turn-1",
            &fingerprint,
            replay.notification,
            None,
            &[ConnectionId(8)],
            &[ConnectionId(8)],
        ));
        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7), ConnectionId(8)]),
            TerminalEventDisposition::Acknowledge { .. }
        ));
    }

    #[test]
    fn terminal_acknowledgement_discards_retry_payload_but_retains_deduplication() {
        let mut state = ThreadState::default();
        let event = terminal_event("turn-1", "done");
        let fingerprint = terminal_event_fingerprint(&event).expect("terminal fingerprint");
        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7)]),
            TerminalEventDisposition::Apply { .. }
        ));
        state.mark_terminal_state_reduced("turn-1", &fingerprint);
        assert!(state.record_terminal_notification_attempt(
            "turn-1",
            &fingerprint,
            cached_notification(),
            Some(ConnectionId(7)),
            &[ConnectionId(7)],
            &[ConnectionId(7)],
        ));

        state.mark_terminal_acknowledged("turn-1", &fingerprint);

        let entry = state.terminal_ledger.get("turn-1").expect("ledger entry");
        assert!(entry.notification.is_none());
        assert!(entry.origin_connection_id.is_none());
        assert!(entry.accepted_connection_ids.is_empty());
        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7)]),
            TerminalEventDisposition::Acknowledge { .. }
        ));
    }

    #[test]
    fn terminal_ledger_retains_acknowledged_tombstones_for_exactly_once_replay() {
        let mut state = ThreadState::default();

        let unreduced_event = terminal_event("unreduced", "done");
        let unreduced_fingerprint =
            terminal_event_fingerprint(&unreduced_event).expect("terminal fingerprint");
        assert!(matches!(
            state.classify_terminal_event("unreduced", &unreduced_event, &[]),
            TerminalEventDisposition::Apply { .. }
        ));
        state.mark_terminal_acknowledged("unreduced", &unreduced_fingerprint);

        let pending_event = terminal_event("pending", "done");
        let pending_fingerprint =
            terminal_event_fingerprint(&pending_event).expect("terminal fingerprint");
        assert!(matches!(
            state.classify_terminal_event("pending", &pending_event, &[ConnectionId(7)]),
            TerminalEventDisposition::Apply { .. }
        ));
        state.mark_terminal_state_reduced("pending", &pending_fingerprint);
        assert!(!state.record_terminal_notification_attempt(
            "pending",
            &pending_fingerprint,
            cached_notification(),
            Some(ConnectionId(7)),
            &[ConnectionId(7)],
            &[],
        ));

        const TOMBSTONES_BEYOND_FORMER_CAP: usize = 1_025;
        for index in 0..TOMBSTONES_BEYOND_FORMER_CAP {
            let turn_id = format!("acknowledged-{index}");
            let event = terminal_event(&turn_id, "done");
            let fingerprint = terminal_event_fingerprint(&event).expect("terminal fingerprint");
            assert!(matches!(
                state.classify_terminal_event(&turn_id, &event, &[]),
                TerminalEventDisposition::Apply { .. }
            ));
            state.mark_terminal_state_reduced(&turn_id, &fingerprint);
            state.mark_terminal_acknowledged(&turn_id, &fingerprint);
        }

        let newest_turn_id = format!("acknowledged-{}", TOMBSTONES_BEYOND_FORMER_CAP - 1);
        let newest_fingerprint = state
            .terminal_ledger
            .get(&newest_turn_id)
            .expect("newest acknowledged tombstone")
            .fingerprint
            .clone();
        state.mark_terminal_acknowledged(&newest_turn_id, &newest_fingerprint);

        assert!(state.terminal_ledger.contains_key("acknowledged-0"));
        assert!(state.terminal_ledger.contains_key(&newest_turn_id));
        assert_eq!(
            state.terminal_ledger.len(),
            TOMBSTONES_BEYOND_FORMER_CAP + 2
        );

        let unreduced = state
            .terminal_ledger
            .get("unreduced")
            .expect("unreduced entry must remain");
        assert!(!unreduced.state_reduced);
        assert!(!unreduced.acknowledged_queued);

        state.mark_terminal_state_reduced("unreduced", &unreduced_fingerprint);
        let reduced_tombstone = state
            .terminal_ledger
            .get("unreduced")
            .expect("newest acknowledged tombstone must remain");
        assert!(reduced_tombstone.state_reduced);
        assert!(reduced_tombstone.acknowledged_queued);
        assert!(state.terminal_ledger.contains_key("acknowledged-1"));

        let pending = state
            .terminal_ledger
            .get("pending")
            .expect("notification-pending entry must remain");
        assert!(pending.state_reduced);
        assert!(!pending.notification_accepted);
        assert!(pending.notification.is_some());
        assert!(!pending.acknowledged_queued);
    }

    #[test]
    fn terminal_ledger_rejects_conflicts_and_stale_turns() {
        let mut state = ThreadState::default();
        let first = terminal_event("turn-1", "done");
        assert!(matches!(
            state.classify_terminal_event("turn-1", &first, &[]),
            TerminalEventDisposition::Apply { .. }
        ));
        assert!(matches!(
            state.classify_terminal_event("turn-1", &terminal_event("turn-1", "different"), &[]),
            TerminalEventDisposition::RejectConflict
        ));

        state.track_current_turn_event(
            "turn-2",
            &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                turn_id: "turn-2".to_string(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: ModeKind::Default,
            }),
        );
        assert!(matches!(
            state.classify_terminal_event("old-turn", &terminal_event("old-turn", "done"), &[]),
            TerminalEventDisposition::RejectStale
        ));
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
    fn retained_terminal_history_requires_notification_projection_after_restart() {
        let event = terminal_event("turn-1", "done");
        let mut state = ThreadState::default();
        state.seed_terminal_ledger_from_history(&[RolloutItem::EventMsg(event.clone())]);

        assert!(matches!(
            state.classify_terminal_event("turn-1", &event, &[ConnectionId(7)]),
            TerminalEventDisposition::ProjectNotification { .. }
        ));
    }

    fn thread_settings(model: &str) -> ThreadSettings {
        ThreadSettings {
            cwd: AbsolutePathBuf::from_absolute_path("/tmp").expect("absolute path"),
            approval_policy: AskForApproval::OnRequest,
            approvals_reviewer: ApprovalsReviewer::User,
            sandbox_policy: SandboxPolicy::ReadOnly {
                network_access: false,
            },
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

    pub(crate) async fn claim_in_flight_task(
        &self,
        fingerprint: String,
        thread_id: ThreadId,
        turn_id: String,
    ) -> InFlightTaskClaim {
        let mut state = self.state.lock().await;
        if let Some(existing) = state.in_flight_tasks.get(&fingerprint) {
            return InFlightTaskClaim::Existing(existing.clone());
        }

        // This map is live correctness state. Evicting an active fingerprint
        // would admit the same task again while its original execution runs.
        if state.in_flight_tasks.len() >= MAX_TRACKED_IN_FLIGHT_TASKS {
            return InFlightTaskClaim::CapacityExceeded;
        }

        state.in_flight_tasks.insert(
            fingerprint.clone(),
            InFlightTaskReference {
                thread_id,
                turn_id: turn_id.clone(),
            },
        );
        InFlightTaskClaim::Claimed
    }

    pub(crate) async fn release_in_flight_task(&self, thread_id: ThreadId, turn_id: &str) {
        let mut state = self.state.lock().await;
        state
            .in_flight_tasks
            .retain(|_, entry| entry.thread_id != thread_id || entry.turn_id.as_str() != turn_id);
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
