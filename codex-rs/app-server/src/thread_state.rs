use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadGoal;
use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_core::CodexThread;
use codex_core::ThreadConfigSnapshot;
use codex_file_watcher::WatchRegistration;
use codex_protocol::ThreadId;
#[cfg(test)]
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_rollout::state_db::StateDbHandle;
use codex_utils_path_uri::LegacyAppPathString;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tracing::error;

type PendingInterruptQueue = Vec<ConnectionRequestId>;
const MAX_TRACKED_TURN_ORIGINS: usize = 256;

#[derive(Default)]
struct TurnOriginState {
    by_turn_id: HashMap<String, ConnectionId>,
    insertion_order: VecDeque<String>,
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
        if state
            .by_turn_id
            .insert(turn_id.clone(), connection_id)
            .is_none()
        {
            state.insertion_order.push_back(turn_id.clone());
        }
        while state.insertion_order.len() > MAX_TRACKED_TURN_ORIGINS {
            if let Some(expired_turn_id) = state.insertion_order.pop_front() {
                state.by_turn_id.remove(&expired_turn_id);
            }
        }
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
        let connection_id = state.by_turn_id.remove(turn_id);
        if connection_id.is_some() {
            state
                .insertion_order
                .retain(|candidate| candidate != turn_id);
        }
        connection_id
    }

    fn remove_if_matches(&self, turn_id: &str, connection_id: ConnectionId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.by_turn_id.get(turn_id) == Some(&connection_id) {
            state.by_turn_id.remove(turn_id);
            state
                .insertion_order
                .retain(|candidate| candidate != turn_id);
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

/// Per-conversation accumulation of the latest states e.g. error message while a turn runs.
#[derive(Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) started_at: Option<i64>,
    pub(crate) command_execution_started: HashSet<String>,
    pub(crate) last_error: Option<TurnError>,
    pub(crate) origin_connection_id: Option<ConnectionId>,
}

#[derive(Default)]
pub(crate) struct ThreadState {
    pub(crate) pending_interrupts: PendingInterruptQueue,
    pub(crate) pending_rollbacks: Option<ConnectionRequestId>,
    pub(crate) turn_summary: TurnSummary,
    pub(crate) last_terminal_turn_id: Option<String>,
    pub(crate) cancel_tx: Option<oneshot::Sender<()>>,
    pub(crate) listener_generation: u64,
    last_thread_settings: Option<ThreadSettings>,
    listener_command_tx: Option<mpsc::UnboundedSender<ThreadListenerCommand>>,
    current_turn_history: ThreadHistoryBuilder,
    turn_origin_tracker: TurnOriginTracker,
    listener_thread: Option<Weak<CodexThread>>,
    watch_registration: WatchRegistration,
}

impl ThreadState {
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
    ) -> (mpsc::UnboundedReceiver<ThreadListenerCommand>, u64) {
        if let Some(previous) = self.cancel_tx.replace(cancel_tx) {
            let _ = previous.send(());
        }
        self.listener_generation = self.listener_generation.wrapping_add(1);
        self.last_thread_settings = Some(thread_settings_baseline);
        let (listener_command_tx, listener_command_rx) = mpsc::unbounded_channel();
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

    pub(crate) fn listener_command_tx(
        &self,
    ) -> Option<mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.current_turn_history.active_turn_snapshot()
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
            self.last_terminal_turn_id = Some(event_turn_id.to_string());
            self.current_turn_history.reset();
        }
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
) {
    let (completion_tx, completion_rx) = oneshot::channel();
    let listener_command_tx = {
        let state = thread_state.lock().await;
        state.listener_command_tx()
    };
    let Some(listener_command_tx) = listener_command_tx else {
        error!("failed to remove pending client request: thread listener is not running");
        return;
    };

    if listener_command_tx
        .send(ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        })
        .is_err()
    {
        error!(
            "failed to remove pending client request: thread listener command channel is closed"
        );
        return;
    }

    if let Err(err) = completion_rx.await {
        error!("failed to remove pending client request: {err}");
    }
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
            multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
            personality: None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResolvedDeliveryFilter {
    pub(crate) experimental_api_enabled: bool,
    pub(crate) opted_out_notification_methods: Arc<HashSet<String>>,
}

impl ResolvedDeliveryFilter {
    pub(crate) fn new(
        experimental_api_enabled: bool,
        opted_out_notification_methods: HashSet<String>,
    ) -> Self {
        Self {
            experimental_api_enabled,
            opted_out_notification_methods: Arc::new(opted_out_notification_methods),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecipientDescriptor {
    pub(crate) connection_id: ConnectionId,
    pub(crate) filter_revision: u64,
    pub(crate) delivery_filter: ResolvedDeliveryFilter,
    pub(crate) raw_events_enabled: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecipientSnapshot {
    pub(crate) revision: u64,
    descriptors: Arc<[RecipientDescriptor]>,
    connection_ids: Arc<[ConnectionId]>,
}

impl RecipientSnapshot {
    #[cfg(test)]
    pub(crate) fn permissive(connection_ids: Vec<ConnectionId>) -> Self {
        let descriptors = connection_ids
            .iter()
            .copied()
            .map(|connection_id| RecipientDescriptor {
                connection_id,
                filter_revision: 0,
                delivery_filter: ResolvedDeliveryFilter {
                    experimental_api_enabled: true,
                    opted_out_notification_methods: Arc::default(),
                },
                raw_events_enabled: true,
            })
            .collect::<Vec<_>>()
            .into();
        Self {
            revision: 0,
            descriptors,
            connection_ids: connection_ids.into(),
        }
    }

    pub(crate) fn descriptors(&self) -> &[RecipientDescriptor] {
        &self.descriptors
    }

    pub(crate) fn connection_ids(&self) -> &[ConnectionId] {
        &self.connection_ids
    }

    pub(crate) fn raw_events_enabled_for(&self, connection_id: ConnectionId) -> bool {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.connection_id == connection_id)
            .is_some_and(|descriptor| descriptor.raw_events_enabled)
    }
}

struct ThreadEntry {
    state: Arc<Mutex<ThreadState>>,
    connection_ids: HashSet<ConnectionId>,
    raw_event_connection_ids: HashSet<ConnectionId>,
    recipient_filter_revisions: HashMap<ConnectionId, u64>,
    recipient_snapshot: RecipientSnapshot,
    has_connections_watcher: watch::Sender<bool>,
}

impl Default for ThreadEntry {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ThreadState::default())),
            connection_ids: HashSet::new(),
            raw_event_connection_ids: HashSet::new(),
            recipient_filter_revisions: HashMap::new(),
            recipient_snapshot: RecipientSnapshot::default(),
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

    fn rebuild_recipient_snapshot(
        &mut self,
        live_connections: &HashMap<ConnectionId, LiveConnection>,
    ) {
        let descriptors = self
            .connection_ids
            .iter()
            .filter_map(|connection_id| {
                let connection = live_connections.get(connection_id)?;
                Some(RecipientDescriptor {
                    connection_id: *connection_id,
                    filter_revision: self
                        .recipient_filter_revisions
                        .get(connection_id)
                        .copied()
                        .unwrap_or(connection.filter_revision),
                    delivery_filter: connection.capabilities.delivery_filter.clone(),
                    raw_events_enabled: self.raw_event_connection_ids.contains(connection_id),
                })
            })
            .collect::<Vec<_>>();
        let connection_ids = descriptors
            .iter()
            .map(|descriptor| descriptor.connection_id)
            .collect::<Vec<_>>();
        self.recipient_snapshot = RecipientSnapshot {
            revision: self.recipient_snapshot.revision.wrapping_add(1),
            descriptors: descriptors.into(),
            connection_ids: connection_ids.into(),
        };
    }

    fn initialize_recipient_filter_revision(
        &mut self,
        connection_id: ConnectionId,
        connection_filter_revision: u64,
    ) {
        self.recipient_filter_revisions
            .entry(connection_id)
            .or_insert(connection_filter_revision);
    }

    fn bump_recipient_filter_revision(&mut self, connection_id: ConnectionId) {
        let revision = self
            .recipient_filter_revisions
            .entry(connection_id)
            .or_default();
        *revision = revision.wrapping_add(1);
    }
}

#[derive(Default)]
struct ThreadStateManagerInner {
    live_connections: HashMap<ConnectionId, LiveConnection>,
    threads: HashMap<ThreadId, ThreadEntry>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<ThreadId>>,
}

impl ThreadStateManagerInner {
    fn rebuild_recipient_snapshot(&mut self, thread_id: ThreadId) {
        let Self {
            live_connections,
            threads,
            ..
        } = self;
        if let Some(thread_entry) = threads.get_mut(&thread_id) {
            thread_entry.rebuild_recipient_snapshot(live_connections);
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConnectionCapabilities {
    pub(crate) request_attestation: bool,
    pub(crate) delivery_filter: ResolvedDeliveryFilter,
}

struct LiveConnection {
    capabilities: ConnectionCapabilities,
    filter_revision: u64,
}

#[derive(Clone, Default)]
pub(crate) struct ThreadStateManager {
    state: Arc<Mutex<ThreadStateManagerInner>>,
    // Extension event sinks are synchronous, so they need an await-free way to
    // enqueue work on the active per-thread listener.
    listener_commands:
        Arc<StdMutex<HashMap<ThreadId, mpsc::UnboundedSender<ThreadListenerCommand>>>>,
}

impl ThreadStateManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn connection_initialized(
        &self,
        connection_id: ConnectionId,
        capabilities: ConnectionCapabilities,
    ) {
        let mut state = self.state.lock().await;
        let previous = state.live_connections.get(&connection_id);
        let filter_changed = previous
            .is_none_or(|previous| previous.capabilities.delivery_filter != capabilities.delivery_filter);
        let filter_revision = previous.map_or(0, |previous| {
            previous
                .filter_revision
                .wrapping_add(u64::from(filter_changed))
        });
        state.live_connections.insert(
            connection_id,
            LiveConnection {
                capabilities,
                filter_revision,
            },
        );
        if filter_changed {
            let thread_ids = state
                .thread_ids_by_connection
                .get(&connection_id)
                .cloned()
                .unwrap_or_default();
            for thread_id in thread_ids {
                if let Some(thread_entry) = state.threads.get_mut(&thread_id) {
                    thread_entry.bump_recipient_filter_revision(connection_id);
                }
                state.rebuild_recipient_snapshot(thread_id);
            }
        }
    }

    pub(crate) async fn first_attestation_capable_connection_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Option<ConnectionId> {
        let state = self.state.lock().await;
        state
            .threads
            .get(&thread_id)?
            .connection_ids
            .iter()
            .filter_map(|connection_id| {
                state
                    .live_connections
                    .get(connection_id)?
                    .capabilities
                    .request_attestation
                    .then_some(*connection_id)
            })
            .min_by_key(|connection_id| connection_id.0)
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
        self.recipient_snapshot(thread_id)
            .await
            .connection_ids()
            .to_vec()
    }

    pub(crate) async fn recipient_snapshot(&self, thread_id: ThreadId) -> RecipientSnapshot {
        self.state
            .lock()
            .await
            .threads
            .get(&thread_id)
            .map(|thread_entry| thread_entry.recipient_snapshot.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn thread_state(&self, thread_id: ThreadId) -> Arc<Mutex<ThreadState>> {
        let mut state = self.state.lock().await;
        state.threads.entry(thread_id).or_default().state.clone()
    }

    pub(crate) fn current_listener_command_tx(
        &self,
        thread_id: ThreadId,
    ) -> Option<mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn register_listener_command_tx(
        &self,
        thread_id: ThreadId,
        tx: mpsc::UnboundedSender<ThreadListenerCommand>,
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
                thread_entry
                    .raw_event_connection_ids
                    .remove(&connection_id);
                thread_entry
                    .recipient_filter_revisions
                    .remove(&connection_id);
                thread_entry.update_has_connections();
            }
            state.rebuild_recipient_snapshot(thread_id);
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
        let mut state = self.state.lock().await;
        let connection_filter_revision = state
            .live_connections
            .get(&connection_id)?
            .filter_revision;
        state
            .thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_entry = state.threads.entry(thread_id).or_default();
        let membership_changed = thread_entry.connection_ids.insert(connection_id);
        thread_entry.initialize_recipient_filter_revision(
            connection_id,
            connection_filter_revision,
        );
        let raw_filter_changed = experimental_raw_events
            && thread_entry.raw_event_connection_ids.insert(connection_id);
        if raw_filter_changed && !membership_changed {
            thread_entry.bump_recipient_filter_revision(connection_id);
        }
        thread_entry.update_has_connections();
        let thread_state = thread_entry.state.clone();
        if membership_changed || raw_filter_changed {
            state.rebuild_recipient_snapshot(thread_id);
        }
        Some(thread_state)
    }

    pub(crate) async fn try_add_connection_to_thread(
        &self,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(connection_filter_revision) = state
            .live_connections
            .get(&connection_id)
            .map(|connection| connection.filter_revision)
        else {
            return false;
        };
        state
            .thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id);
        let thread_entry = state.threads.entry(thread_id).or_default();
        let membership_changed = thread_entry.connection_ids.insert(connection_id);
        thread_entry.initialize_recipient_filter_revision(
            connection_id,
            connection_filter_revision,
        );
        thread_entry.update_has_connections();
        if membership_changed {
            state.rebuild_recipient_snapshot(thread_id);
        }
        true
    }

    pub(crate) async fn remove_connection(&self, connection_id: ConnectionId) -> Vec<ThreadId> {
        {
            let mut state = self.state.lock().await;
            state.live_connections.remove(&connection_id);
            let thread_ids = state
                .thread_ids_by_connection
                .remove(&connection_id)
                .unwrap_or_default();
            for thread_id in &thread_ids {
                if let Some(thread_entry) = state.threads.get_mut(thread_id) {
                    thread_entry.connection_ids.remove(&connection_id);
                    thread_entry
                        .raw_event_connection_ids
                        .remove(&connection_id);
                    thread_entry
                        .recipient_filter_revisions
                        .remove(&connection_id);
                    thread_entry.update_has_connections();
                }
                state.rebuild_recipient_snapshot(*thread_id);
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

#[cfg(test)]
mod recipient_snapshot_tests {
    use super::*;

    fn descriptor(
        snapshot: &RecipientSnapshot,
        connection_id: ConnectionId,
    ) -> &RecipientDescriptor {
        snapshot
            .descriptors()
            .iter()
            .find(|descriptor| descriptor.connection_id == connection_id)
            .expect("recipient descriptor")
    }

    #[tokio::test]
    async fn snapshot_revisions_track_filter_and_raw_changes() {
        let manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        let initial_filter = ResolvedDeliveryFilter::new(
            false,
            HashSet::from(["thread/ignored".to_string()]),
        );
        manager
            .connection_initialized(
                connection_id,
                ConnectionCapabilities {
                    request_attestation: false,
                    delivery_filter: initial_filter.clone(),
                },
            )
            .await;
        manager
            .try_ensure_connection_subscribed(thread_id, connection_id, false)
            .await
            .expect("subscription");

        let initial = manager.recipient_snapshot(thread_id).await;
        let initial_descriptor = descriptor(&initial, connection_id);
        assert!(!initial_descriptor.raw_events_enabled);
        assert_eq!(initial_descriptor.delivery_filter, initial_filter);

        manager
            .try_ensure_connection_subscribed(thread_id, connection_id, true)
            .await
            .expect("raw subscription upgrade");
        let raw_enabled = manager.recipient_snapshot(thread_id).await;
        let raw_descriptor = descriptor(&raw_enabled, connection_id);
        assert!(raw_enabled.revision > initial.revision);
        assert!(raw_descriptor.filter_revision > initial_descriptor.filter_revision);
        assert!(raw_descriptor.raw_events_enabled);
        assert!(!descriptor(&initial, connection_id).raw_events_enabled);

        let updated_filter = ResolvedDeliveryFilter::new(true, HashSet::new());
        manager
            .connection_initialized(
                connection_id,
                ConnectionCapabilities {
                    request_attestation: false,
                    delivery_filter: updated_filter.clone(),
                },
            )
            .await;
        let updated = manager.recipient_snapshot(thread_id).await;
        let updated_descriptor = descriptor(&updated, connection_id);
        assert!(updated.revision > raw_enabled.revision);
        assert!(updated_descriptor.filter_revision > raw_descriptor.filter_revision);
        assert_eq!(updated_descriptor.delivery_filter, updated_filter);

        manager
            .try_ensure_connection_subscribed(thread_id, connection_id, false)
            .await
            .expect("false does not downgrade raw delivery");
        assert_eq!(manager.recipient_snapshot(thread_id).await, updated);
    }

    #[tokio::test]
    async fn snapshot_keeps_per_connection_raw_state_and_removes_departed_recipients() {
        let manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let raw_connection = ConnectionId(1);
        let filtered_connection = ConnectionId(2);
        for connection_id in [raw_connection, filtered_connection] {
            manager
                .connection_initialized(connection_id, ConnectionCapabilities::default())
                .await;
        }
        manager
            .try_ensure_connection_subscribed(thread_id, raw_connection, true)
            .await
            .expect("raw subscription");
        manager
            .try_ensure_connection_subscribed(thread_id, filtered_connection, false)
            .await
            .expect("filtered subscription");

        let both = manager.recipient_snapshot(thread_id).await;
        assert!(descriptor(&both, raw_connection).raw_events_enabled);
        assert!(!descriptor(&both, filtered_connection).raw_events_enabled);

        assert!(
            manager
                .unsubscribe_connection_from_thread(thread_id, raw_connection)
                .await
        );
        let one = manager.recipient_snapshot(thread_id).await;
        assert_eq!(one.descriptors().len(), 1);
        assert_eq!(one.descriptors()[0].connection_id, filtered_connection);

        manager.remove_connection(filtered_connection).await;
        assert!(
            manager
                .recipient_snapshot(thread_id)
                .await
                .descriptors()
                .is_empty()
        );
    }
}
