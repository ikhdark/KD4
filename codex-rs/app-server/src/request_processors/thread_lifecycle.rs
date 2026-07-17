use super::*;
use codex_protocol::config_types::MultiAgentMode;

pub(super) const THREAD_UNLOADING_DELAY: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub(crate) struct ThreadLifecycleCoordinator {
    published_states:
        std::sync::Mutex<HashMap<ThreadId, std::sync::Weak<ThreadLifecycleState>>>,
}

#[derive(Default)]
struct ThreadLifecycleState {
    operation_lock: Arc<Mutex<()>>,
    subscription_state: Arc<Mutex<ThreadSubscriptionState>>,
}

#[derive(Default)]
struct ThreadSubscriptionState {
    unloading: bool,
}

pub(crate) struct ThreadLifecycleGuards {
    _states: Vec<Arc<ThreadLifecycleState>>,
    _guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

pub(super) struct ThreadSubscriptionGuard {
    state: Arc<ThreadLifecycleState>,
    guard: tokio::sync::OwnedMutexGuard<ThreadSubscriptionState>,
}

pub(super) struct ThreadUnloadToken {
    state: Arc<ThreadLifecycleState>,
}

impl ThreadLifecycleCoordinator {
    fn state(&self, thread_id: ThreadId) -> Arc<ThreadLifecycleState> {
        let mut states = self
            .published_states
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(state) = states.get(&thread_id).and_then(|state| state.upgrade()) {
            return state;
        }
        if states.len() >= 1024 {
            states.retain(|_, state| state.strong_count() > 0);
        }
        let state = Arc::new(ThreadLifecycleState::default());
        states.insert(thread_id, Arc::downgrade(&state));
        state
    }

    pub(crate) async fn lock_thread(&self, thread_id: ThreadId) -> ThreadLifecycleGuards {
        self.lock_threads([thread_id]).await
    }

    pub(super) async fn lock_threads(
        &self,
        thread_ids: impl IntoIterator<Item = ThreadId>,
    ) -> ThreadLifecycleGuards {
        let mut thread_ids = thread_ids.into_iter().collect::<Vec<_>>();
        thread_ids.sort_by_key(ToString::to_string);
        thread_ids.dedup();

        // Publish or resolve every keyed state while holding only the short
        // synchronous registry lock inside `state`, then await the per-thread
        // locks in a stable order without retaining that publication lock.
        let states = thread_ids
            .into_iter()
            .map(|thread_id| self.state(thread_id))
            .collect::<Vec<_>>();
        let mut guards = Vec::with_capacity(states.len());
        for state in &states {
            guards.push(Arc::clone(&state.operation_lock).lock_owned().await);
        }
        ThreadLifecycleGuards {
            _states: states,
            _guards: guards,
        }
    }

    pub(super) async fn subscription_guard(
        &self,
        thread_id: ThreadId,
    ) -> ThreadSubscriptionGuard {
        let state = self.state(thread_id);
        let guard = Arc::clone(&state.subscription_state).lock_owned().await;
        ThreadSubscriptionGuard { state, guard }
    }

    pub(super) async fn is_unloading(&self, thread_id: ThreadId) -> bool {
        self.subscription_guard(thread_id).await.is_unloading()
    }

    pub(super) async fn clear_unloading(&self, thread_id: ThreadId) {
        let mut subscription_guard = self.subscription_guard(thread_id).await;
        subscription_guard.guard.unloading = false;
    }
}

impl ThreadSubscriptionGuard {
    pub(super) fn is_unloading(&self) -> bool {
        self.guard.unloading
    }

    pub(super) fn mark_unloading(&mut self) -> ThreadUnloadToken {
        self.guard.unloading = true;
        ThreadUnloadToken {
            state: Arc::clone(&self.state),
        }
    }
}

impl ThreadUnloadToken {
    pub(super) async fn clear(self) {
        let mut subscription_state = self.state.subscription_state.lock().await;
        subscription_state.unloading = false;
    }
}

#[derive(Clone)]
pub(super) struct ListenerTaskContext {
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) thread_state_manager: ThreadStateManager,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) thread_lifecycle: Arc<ThreadLifecycleCoordinator>,
    pub(super) thread_watch_manager: ThreadWatchManager,
    pub(super) fallback_model_provider: String,
    pub(super) codex_home: PathBuf,
    pub(super) skills_watcher: Arc<SkillsWatcher>,
}

struct UnloadingState {
    delay: Duration,
    has_subscribers_rx: watch::Receiver<bool>,
    has_subscribers: (bool, Instant),
    thread_status_rx: watch::Receiver<ThreadStatus>,
    is_active: (bool, Instant),
}

impl UnloadingState {
    async fn new(
        listener_task_context: &ListenerTaskContext,
        thread_id: ThreadId,
        delay: Duration,
    ) -> Option<Self> {
        let has_subscribers_rx = listener_task_context
            .thread_state_manager
            .subscribe_to_has_connections(thread_id)
            .await?;
        let thread_status_rx = listener_task_context
            .thread_watch_manager
            .subscribe(thread_id)
            .await?;
        let has_subscribers = (*has_subscribers_rx.borrow(), Instant::now());
        let is_active = (
            matches!(*thread_status_rx.borrow(), ThreadStatus::Active { .. }),
            Instant::now(),
        );
        Some(Self {
            delay,
            has_subscribers_rx,
            has_subscribers,
            thread_status_rx,
            is_active,
        })
    }

    fn unloading_target(&self) -> Option<Instant> {
        match (self.has_subscribers, self.is_active) {
            ((false, has_no_subscribers_since), (false, is_inactive_since)) => {
                Some(std::cmp::max(has_no_subscribers_since, is_inactive_since) + self.delay)
            }
            _ => None,
        }
    }

    fn sync_receiver_values(&mut self) {
        let has_subscribers = *self.has_subscribers_rx.borrow();
        if self.has_subscribers.0 != has_subscribers {
            self.has_subscribers = (has_subscribers, Instant::now());
        }

        let is_active = matches!(*self.thread_status_rx.borrow(), ThreadStatus::Active { .. });
        if self.is_active.0 != is_active {
            self.is_active = (is_active, Instant::now());
        }
    }

    fn should_unload_now(&mut self) -> bool {
        self.sync_receiver_values();
        self.unloading_target()
            .is_some_and(|target| target <= Instant::now())
    }

    fn note_thread_activity_observed(&mut self) {
        if !self.is_active.0 {
            self.is_active = (false, Instant::now());
        }
    }

    async fn wait_for_unloading_trigger(&mut self) -> bool {
        loop {
            self.sync_receiver_values();
            let unloading_target = self.unloading_target();
            if let Some(target) = unloading_target
                && target <= Instant::now()
            {
                return true;
            }
            let unloading_sleep = async {
                if let Some(target) = unloading_target {
                    tokio::time::sleep_until(target.into()).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = unloading_sleep => return true,
                changed = self.has_subscribers_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
                changed = self.thread_status_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
            }
        }
    }
}

pub(super) enum ThreadShutdownResult {
    Complete,
    SubmitFailed,
    TimedOut,
}

pub(super) enum EnsureConversationListenerResult {
    Attached,
    ConnectionClosed,
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "listener subscription must be serialized against this thread's pending unload"
)]
pub(super) async fn ensure_conversation_listener(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
    let conversation = match listener_task_context
        .thread_manager
        .get_thread(conversation_id)
        .await
    {
        Ok(conv) => conv,
        Err(_) => {
            return Err(invalid_request(format!(
                "thread not found: {conversation_id}"
            )));
        }
    };
    let thread_state = {
        let subscription_guard = listener_task_context
            .thread_lifecycle
            .subscription_guard(conversation_id)
            .await;
        if subscription_guard.is_unloading() {
            return Err(invalid_request(format!(
                "thread {conversation_id} is closing; retry after the thread is closed"
            )));
        }
        let Some(thread_state) = listener_task_context
            .thread_state_manager
            .try_ensure_connection_subscribed(conversation_id, connection_id, raw_events_enabled)
            .await
        else {
            return Ok(EnsureConversationListenerResult::ConnectionClosed);
        };
        thread_state
    };
    if let Err(error) = ensure_listener_task_running(
        listener_task_context.clone(),
        conversation_id,
        conversation,
        thread_state,
    )
    .await
    {
        let _ = listener_task_context
            .thread_state_manager
            .unsubscribe_connection_from_thread(conversation_id, connection_id)
            .await;
        return Err(error);
    }
    Ok(EnsureConversationListenerResult::Attached)
}

pub(super) fn log_listener_attach_result(
    result: Result<EnsureConversationListenerResult, JSONRPCErrorError>,
    thread_id: ThreadId,
    connection_id: ConnectionId,
    thread_kind: &'static str,
) {
    match result {
        Ok(EnsureConversationListenerResult::Attached) => {}
        Ok(EnsureConversationListenerResult::ConnectionClosed) => {
            tracing::debug!(
                thread_id = %thread_id,
                connection_id = ?connection_id,
                "skipping auto-attach for closed connection"
            );
        }
        Err(err) => {
            tracing::warn!(
                "failed to attach listener for {thread_kind} {thread_id}: {message}",
                message = err.message
            );
        }
    }
}

pub(super) async fn ensure_listener_task_running(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
    thread_state: Arc<Mutex<ThreadState>>,
) -> Result<(), JSONRPCErrorError> {
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    let Some(mut unloading_state) = UnloadingState::new(
        &listener_task_context,
        conversation_id,
        THREAD_UNLOADING_DELAY,
    )
    .await
    else {
        return Err(invalid_request(format!(
            "thread {conversation_id} is closing; retry after the thread is closed"
        )));
    };
    let config = conversation.config().await;
    let config_snapshot = conversation.config_snapshot().await;
    let watch_registration = listener_task_context
        .skills_watcher
        .register_thread_config(
            config.as_ref(),
            listener_task_context.thread_manager.as_ref(),
            &config_snapshot,
        )
        .await;
    let thread_settings_baseline = thread_settings_from_config_snapshot(&config_snapshot);
    let (mut listener_command_rx, listener_generation) = {
        let mut thread_state = thread_state.lock().await;
        if thread_state.listener_matches(&conversation) {
            return Ok(());
        }
        let (listener_command_rx, listener_generation) = thread_state.set_listener(
            cancel_tx,
            &conversation,
            watch_registration,
            thread_settings_baseline,
        );
        let Some(listener_command_tx) = thread_state.listener_command_tx() else {
            tracing::warn!(
                "thread listener command sender missing immediately after listener registration"
            );
            return Ok(());
        };
        listener_task_context
            .thread_state_manager
            .register_listener_command_tx(conversation_id, listener_command_tx);
        (listener_command_rx, listener_generation)
    };
    let ListenerTaskContext {
        outgoing,
        thread_manager,
        thread_state_manager,
        thread_lifecycle,
        thread_watch_manager,
        fallback_model_provider,
        codex_home,
        ..
    } = listener_task_context;
    let outgoing_for_task = Arc::clone(&outgoing);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut cancel_rx => {
                    // Listener was superseded or the thread is being torn down.
                    break;
                }
                listener_command = listener_command_rx.recv() => {
                    let Some(listener_command) = listener_command else {
                        break;
                    };
                    handle_thread_listener_command(
                        conversation_id,
                        &conversation,
                        codex_home.as_path(),
                        &thread_state_manager,
                        &thread_state,
                        &thread_watch_manager,
                        &outgoing_for_task,
                        &thread_lifecycle,
                        listener_command,
                    )
                    .await;
                }
                event = conversation.next_event() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(err) => {
                            tracing::warn!("thread.next_event() failed with: {err}");
                            break;
                        }
                    };

                    // Track the event before freezing its recipient snapshot.
                    // Snapshot acquisition is this event's delivery
                    // linearization point.
                    {
                        let mut thread_state = thread_state.lock().await;
                        thread_state.track_current_turn_event(&event.id, &event.msg);
                    }
                    let recipient_snapshot = thread_state_manager
                        .recipient_snapshot(conversation_id)
                        .await;
                    let thread_outgoing = ThreadScopedOutgoingMessageSender::from_snapshot(
                        outgoing_for_task.clone(),
                        recipient_snapshot,
                        conversation_id,
                    );

                    apply_bespoke_event_handling(
                        event,
                        conversation_id,
                        conversation.clone(),
                        thread_manager.clone(),
                        thread_outgoing,
                        thread_state.clone(),
                        thread_watch_manager.clone(),
                        Arc::clone(&thread_lifecycle),
                        fallback_model_provider.clone(),
                    )
                    .await;
                }
                unloading_watchers_open = unloading_state.wait_for_unloading_trigger() => {
                    if !unloading_watchers_open {
                        break;
                    }
                    if !unloading_state.should_unload_now() {
                        continue;
                    }
                    if matches!(conversation.agent_status().await, AgentStatus::Running) {
                        unloading_state.note_thread_activity_observed();
                        continue;
                    }
                    let lifecycle_guard = thread_lifecycle.lock_thread(conversation_id).await;
                    let unload_token = {
                        let mut subscription_guard = thread_lifecycle
                            .subscription_guard(conversation_id)
                            .await;
                        if subscription_guard.is_unloading() {
                            continue;
                        }
                        if !unloading_state.should_unload_now() {
                            continue;
                        }
                        subscription_guard.mark_unloading()
                    };
                    unload_thread_without_subscribers(
                        thread_manager.clone(),
                        outgoing_for_task.clone(),
                        lifecycle_guard,
                        unload_token,
                        thread_state_manager.clone(),
                        thread_watch_manager.clone(),
                        conversation_id,
                        conversation.clone(),
                    )
                    .await;
                    break;
                }
            }
        }

        let mut thread_state = thread_state.lock().await;
        if thread_state.listener_generation == listener_generation {
            thread_state_manager.unregister_listener_command_tx(conversation_id);
            thread_state.clear_listener();
        }
    });
    Ok(())
}

pub(super) async fn wait_for_thread_shutdown(thread: &Arc<CodexThread>) -> ThreadShutdownResult {
    match tokio::time::timeout(Duration::from_secs(10), thread.shutdown_and_wait()).await {
        Ok(Ok(())) => ThreadShutdownResult::Complete,
        Ok(Err(_)) => ThreadShutdownResult::SubmitFailed,
        Err(_) => ThreadShutdownResult::TimedOut,
    }
}

pub(super) async fn unload_thread_without_subscribers(
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    lifecycle_guard: ThreadLifecycleGuards,
    unload_token: ThreadUnloadToken,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) {
    info!("thread {thread_id} has no subscribers and is idle; shutting down");

    // Any pending app-server -> client requests for this thread can no longer be
    // answered; cancel their callbacks before shutdown/unload.
    outgoing
        .cancel_requests_for_thread(thread_id, /*error*/ None)
        .await;
    thread_state_manager.remove_thread_state(thread_id).await;

    tokio::spawn(async move {
        let _lifecycle_guard = lifecycle_guard;
        match wait_for_thread_shutdown(&thread).await {
            ThreadShutdownResult::Complete => {
                if thread_manager.remove_thread(&thread_id).await.is_none() {
                    info!("thread {thread_id} was already removed before teardown finalized");
                    thread_watch_manager
                        .remove_thread(&thread_id.to_string())
                        .await;
                } else {
                    thread_watch_manager
                        .remove_thread(&thread_id.to_string())
                        .await;
                    let notification = ThreadClosedNotification {
                        thread_id: thread_id.to_string(),
                    };
                    outgoing
                        .send_server_notification(ServerNotification::ThreadClosed(notification))
                        .await;
                }
            }
            ThreadShutdownResult::SubmitFailed => {
                warn!("failed to submit Shutdown to thread {thread_id}");
            }
            ThreadShutdownResult::TimedOut => {
                warn!("thread {thread_id} shutdown timed out; leaving thread loaded");
            }
        }
        unload_token.clear().await;
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_thread_listener_command(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    codex_home: &Path,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_lifecycle: &Arc<ThreadLifecycleCoordinator>,
    listener_command: ThreadListenerCommand,
) {
    match listener_command {
        ThreadListenerCommand::SendThreadResumeResponse(resume_request) => {
            handle_pending_thread_resume_request(
                conversation_id,
                conversation,
                codex_home,
                thread_state_manager,
                thread_state,
                thread_watch_manager,
                outgoing,
                thread_lifecycle,
                *resume_request,
            )
            .await;
        }
        ThreadListenerCommand::EmitThreadGoalUpdated { turn_id, goal } => {
            outgoing
                .send_server_notification(ServerNotification::ThreadGoalUpdated(
                    ThreadGoalUpdatedNotification {
                        thread_id: conversation_id.to_string(),
                        turn_id,
                        goal,
                    },
                ))
                .await;
        }
        ThreadListenerCommand::EmitThreadGoalCleared => {
            outgoing
                .send_server_notification(ServerNotification::ThreadGoalCleared(
                    ThreadGoalClearedNotification {
                        thread_id: conversation_id.to_string(),
                    },
                ))
                .await;
        }
        ThreadListenerCommand::EmitThreadGoalSnapshot { state_db } => {
            send_thread_goal_snapshot_notification(outgoing, conversation_id, &state_db).await;
        }
        ThreadListenerCommand::ResolveServerRequest {
            request_id,
            completion_tx,
        } => {
            resolve_pending_server_request(
                conversation_id,
                thread_state_manager,
                outgoing,
                request_id,
            )
            .await;
            let _ = completion_tx.send(());
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "running-thread resume subscription must be serialized against this thread's pending unload"
)]
pub(super) async fn handle_pending_thread_resume_request(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    _codex_home: &Path,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_lifecycle: &Arc<ThreadLifecycleCoordinator>,
    pending: crate::thread_state::PendingThreadResumeRequest,
) {
    let active_turn = {
        let state = thread_state.lock().await;
        state.active_turn_snapshot()
    };
    tracing::debug!(
        thread_id = %conversation_id,
        request_id = ?pending.request_id,
        active_turn_present = active_turn.is_some(),
        active_turn_id = ?active_turn.as_ref().map(|turn| turn.id.as_str()),
        active_turn_status = ?active_turn.as_ref().map(|turn| &turn.status),
        "composing running thread resume response"
    );
    let has_live_in_progress_turn =
        matches!(conversation.agent_status().await, AgentStatus::Running)
            || active_turn
                .as_ref()
                .is_some_and(|turn| matches!(turn.status, TurnStatus::InProgress));

    let request_id = pending.request_id;
    let connection_id = request_id.connection_id;
    let mut thread = pending.thread_summary;
    if pending.include_turns {
        populate_thread_turns_from_history(
            &mut thread,
            &pending.history_items,
            active_turn.as_ref(),
        );
    }

    let thread_status = thread_watch_manager
        .loaded_status_for_thread(&thread.id)
        .await;

    set_thread_status_and_interrupt_stale_turns(
        &mut thread,
        thread_status,
        has_live_in_progress_turn,
    );
    let token_usage_thread = pending.include_turns.then(|| thread.clone());
    let mut initial_turns_page = if let Some(params) = pending.initial_turns_page.as_ref() {
        match super::thread_processor::build_thread_resume_initial_turns_page(
            &pending.history_items,
            thread.status.clone(),
            has_live_in_progress_turn,
            active_turn,
            params,
        ) {
            Ok(page) => Some(page),
            Err(error) => {
                outgoing.send_error(request_id, error).await;
                return;
            }
        }
    } else {
        None
    };
    if pending.redact_resume_payloads {
        redact_thread_resume_payloads(&mut thread.turns);
        if let Some(initial_turns_page) = initial_turns_page.as_mut() {
            redact_thread_resume_payloads(&mut initial_turns_page.data);
        }
    }

    {
        let subscription_guard = thread_lifecycle
            .subscription_guard(conversation_id)
            .await;
        if subscription_guard.is_unloading() {
            drop(subscription_guard);
            outgoing
                .send_error(
                    request_id,
                    invalid_request(format!(
                        "thread {conversation_id} is closing; retry thread/resume after the thread is closed"
                    )),
                )
                .await;
            return;
        }
        if !thread_state_manager
            .try_add_connection_to_thread(conversation_id, connection_id)
            .await
        {
            tracing::debug!(
                thread_id = %conversation_id,
                connection_id = ?connection_id,
                "skipping running thread resume for closed connection"
            );
            return;
        }
    }

    let config_snapshot = pending.config_snapshot;
    let cwd = config_snapshot.cwd().clone();
    let ThreadConfigSnapshot {
        model,
        model_provider_id,
        service_tier,
        approval_policy,
        approvals_reviewer,
        permission_profile,
        active_permission_profile,
        workspace_roots,
        reasoning_effort,
        originator,
        ..
    } = config_snapshot;
    let instruction_sources = pending.instruction_sources;
    let sandbox = thread_response_sandbox_policy(&permission_profile, cwd.as_path());
    let active_permission_profile =
        thread_response_active_permission_profile(active_permission_profile);
    let session_id = conversation.session_configured().session_id.to_string();
    thread.session_id = session_id;

    let response = ThreadResumeResponse {
        thread,
        model,
        model_provider: model_provider_id,
        service_tier,
        cwd,
        runtime_workspace_roots: workspace_roots,
        instruction_sources,
        approval_policy: approval_policy.into(),
        approvals_reviewer: approvals_reviewer.into(),
        sandbox,
        active_permission_profile,
        reasoning_effort,
        multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
        initial_turns_page,
    };
    outgoing
        .send_response_with_thread_originator(request_id, response, originator)
        .await;
    // Match cold resume: metadata-only resume should attach the listener without
    // paying the cost of turn reconstruction for historical usage replay.
    if let Some(token_usage_thread) = token_usage_thread {
        let token_usage_turn_id = latest_token_usage_turn_id_from_rollout_items(
            &pending.history_items,
            token_usage_thread.turns.as_slice(),
        );
        // Rejoining a loaded thread has the same UI contract as a cold resume, but
        // uses the live conversation state instead of reconstructing a new session.
        send_thread_token_usage_update_to_connection(
            outgoing,
            connection_id,
            conversation_id,
            &token_usage_thread,
            conversation.as_ref(),
            token_usage_turn_id,
        )
        .await;
    }
    if pending.emit_thread_goal_update {
        if let Some(state_db) = pending.thread_goal_state_db {
            send_thread_goal_snapshot_notification(outgoing, conversation_id, &state_db).await;
        } else {
            tracing::warn!(
                thread_id = %conversation_id,
                "state db unavailable when reading thread goal for running thread resume"
            );
        }
    }
    outgoing
        .replay_requests_to_connection_for_thread(connection_id, conversation_id)
        .await;
    // App-server owns resume response and snapshot ordering, so wait until
    // replay completes before letting extensions react to the idle thread.
    if pending.emit_thread_goal_update {
        conversation.emit_thread_idle_lifecycle_if_idle().await;
    }
}

pub(super) async fn send_thread_goal_snapshot_notification(
    outgoing: &Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
    state_db: &StateDbHandle,
) {
    match state_db.thread_goals().get_thread_goal(thread_id).await {
        Ok(Some(goal)) => {
            outgoing
                .send_server_notification(ServerNotification::ThreadGoalUpdated(
                    ThreadGoalUpdatedNotification {
                        thread_id: thread_id.to_string(),
                        turn_id: None,
                        goal: api_thread_goal_from_state(goal),
                    },
                ))
                .await;
        }
        Ok(None) => {
            outgoing
                .send_server_notification(ServerNotification::ThreadGoalCleared(
                    ThreadGoalClearedNotification {
                        thread_id: thread_id.to_string(),
                    },
                ))
                .await;
        }
        Err(err) => {
            tracing::warn!(
                thread_id = %thread_id,
                "failed to read thread goal for resume snapshot: {err}"
            );
        }
    }
}

pub(crate) fn populate_thread_turns_from_history(
    thread: &mut Thread,
    items: &[RolloutItem],
    active_turn: Option<&Turn>,
) {
    let mut turns = build_legacy_api_turns_from_rollout_items(items);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn.clone());
    }
    thread.turns = turns;
}

pub(super) async fn resolve_pending_server_request(
    conversation_id: ThreadId,
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    request_id: RequestId,
) {
    let thread_id = conversation_id.to_string();
    let recipient_snapshot = thread_state_manager
        .recipient_snapshot(conversation_id)
        .await;
    let outgoing = ThreadScopedOutgoingMessageSender::from_snapshot(
        outgoing.clone(),
        recipient_snapshot,
        conversation_id,
    );
    outgoing
        .send_server_notification(ServerNotification::ServerRequestResolved(
            ServerRequestResolvedNotification {
                thread_id,
                request_id,
            },
        ))
        .await;
}

pub(super) fn merge_turn_history_with_active_turn(turns: &mut Vec<Turn>, active_turn: Turn) {
    turns.retain(|turn| turn.id != active_turn.id);
    turns.push(active_turn);
}

pub(super) fn set_thread_status_and_interrupt_stale_turns(
    thread: &mut Thread,
    loaded_status: ThreadStatus,
    has_live_in_progress_turn: bool,
) {
    let status = resolve_thread_status(loaded_status, has_live_in_progress_turn);
    if !matches!(status, ThreadStatus::Active { .. }) {
        for turn in &mut thread.turns {
            if matches!(turn.status, TurnStatus::InProgress) {
                turn.status = TurnStatus::Interrupted;
            }
        }
    }
    thread.status = status;
}

#[cfg(test)]
#[path = "thread_lifecycle_tests.rs"]
mod tests;
