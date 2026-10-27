use super::token_usage_replay::TokenUsageReplaySnapshot;
use super::*;
use crate::thread_status::ThreadStatusSubscription;
use tokio::sync::mpsc;

pub(super) const THREAD_UNLOADING_DELAY: Duration = Duration::from_secs(5 * 60);
const THREAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INACTIVE_THREADS: usize = 8;

async fn release_turn_start_for_event(
    thread_state_manager: &ThreadStateManager,
    thread_id: ThreadId,
    event_turn_id: &str,
    event: &EventMsg,
    active_turn_id: Option<&str>,
) {
    let is_terminal = matches!(event, EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_));
    let failed_before_start =
        matches!(event, EventMsg::Error(_)) && active_turn_id != Some(event_turn_id);
    if is_terminal || failed_before_start {
        thread_state_manager
            .release_turn_start(thread_id, event_turn_id)
            .await;
    }
}

struct EligibleThread {
    sequence: u64,
    listener_generation: u64,
    evict_tx: mpsc::UnboundedSender<EvictionRequest>,
}

enum ThreadConnectionAdmission<T> {
    Admitted(T),
    ConnectionClosed,
    ThreadClosing,
}

#[derive(Default)]
struct EvictionRequest {
    completion: Option<oneshot::Sender<()>>,
}

pub(super) struct EvictionCompletion(Option<oneshot::Sender<()>>);

impl Drop for EvictionCompletion {
    fn drop(&mut self) {
        if let Some(completion) = self.0.take() {
            let _ = completion.send(());
        }
    }
}

#[derive(Default)]
struct ThreadUnloadAuthorityState {
    unloading: HashSet<ThreadId>,
    eligible: HashMap<ThreadId, EligibleThread>,
    eligibility_owners: HashMap<ThreadId, u64>,
    next_sequence: u64,
}

pub(crate) struct PendingThreadUnloads {
    state: Mutex<ThreadUnloadAuthorityState>,
    admission_gates: std::sync::Mutex<HashMap<ThreadId, std::sync::Weak<Semaphore>>>,
}

impl Default for PendingThreadUnloads {
    fn default() -> Self {
        Self {
            state: Mutex::new(ThreadUnloadAuthorityState::default()),
            admission_gates: Default::default(),
        }
    }
}

impl PendingThreadUnloads {
    fn admission_gate(&self, thread_id: ThreadId) -> Arc<Semaphore> {
        let mut gates = self
            .admission_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&thread_id).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Semaphore::new(1));
        gates.insert(thread_id, Arc::downgrade(&gate));
        gate
    }

    pub(super) async fn begin(&self, thread_id: ThreadId) -> bool {
        let gate = self.admission_gate(thread_id);
        let Ok(_admission_permit) = gate.acquire().await else {
            return false;
        };
        let mut state = self.state.lock().await;
        state.eligible.remove(&thread_id);
        state.eligibility_owners.remove(&thread_id);
        state.unloading.insert(thread_id)
    }

    pub(super) async fn finish(&self, thread_id: &ThreadId) {
        self.state.lock().await.unloading.remove(thread_id);
    }

    pub(super) async fn contains(&self, thread_id: &ThreadId) -> bool {
        self.state.lock().await.unloading.contains(thread_id)
    }

    async fn admit_listener_connection(
        &self,
        thread_state_manager: &ThreadStateManager,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> ThreadConnectionAdmission<Arc<Mutex<ThreadState>>> {
        let gate = self.admission_gate(thread_id);
        let Ok(_admission_permit) = gate.acquire().await else {
            return ThreadConnectionAdmission::ThreadClosing;
        };
        if self.state.lock().await.unloading.contains(&thread_id) {
            return ThreadConnectionAdmission::ThreadClosing;
        }
        let thread_state = thread_state_manager
            .try_ensure_connection_subscribed(thread_id, connection_id, raw_events_enabled)
            .await;
        match thread_state {
            Some(thread_state) => ThreadConnectionAdmission::Admitted(thread_state),
            None => ThreadConnectionAdmission::ConnectionClosed,
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Retain the listener generation guard through connection admission and response enqueue"
    )]
    async fn admit_resume_connection<'a>(
        &self,
        thread_state_manager: &ThreadStateManager,
        thread_id: ThreadId,
        connection_id: ConnectionId,
        thread_state: &'a Mutex<ThreadState>,
        listener_generation: u64,
    ) -> Result<
        ThreadConnectionAdmission<tokio::sync::MutexGuard<'a, ThreadState>>,
        JSONRPCErrorError,
    > {
        let gate = self.admission_gate(thread_id);
        let Ok(_admission_permit) = gate.acquire().await else {
            return Ok(ThreadConnectionAdmission::ThreadClosing);
        };
        if self.state.lock().await.unloading.contains(&thread_id) {
            return Ok(ThreadConnectionAdmission::ThreadClosing);
        }
        // Match listener admission's gate -> thread-state lock order. Keep the
        // generation guard through response enqueue, after releasing this gate.
        let state = thread_state.lock().await;
        if state.listener_generation != listener_generation || state.listener_command_tx().is_none()
        {
            return Err(resume_listener_changed_error(thread_id));
        }
        let added = thread_state_manager
            .try_add_connection_to_thread(thread_id, connection_id)
            .await;
        if added {
            Ok(ThreadConnectionAdmission::Admitted(state))
        } else {
            Ok(ThreadConnectionAdmission::ConnectionClosed)
        }
    }

    async fn set_eligible(
        &self,
        thread_id: ThreadId,
        listener_generation: u64,
        eligible: bool,
        evict_tx: &mpsc::UnboundedSender<EvictionRequest>,
    ) {
        let evict_tx = {
            let mut state = self.state.lock().await;
            let closed = state
                .eligible
                .iter()
                .filter(|(_, entry)| entry.evict_tx.is_closed())
                .map(|(thread_id, _)| *thread_id)
                .collect::<Vec<_>>();
            for closed_thread_id in closed {
                state.eligible.remove(&closed_thread_id);
                state.eligibility_owners.remove(&closed_thread_id);
            }
            if state.unloading.contains(&thread_id) {
                state.eligible.remove(&thread_id);
                state.eligibility_owners.remove(&thread_id);
                return;
            }
            match state.eligibility_owners.get(&thread_id).copied() {
                Some(owner_generation) if owner_generation == listener_generation => {}
                Some(owner_generation)
                    if listener_generation_is_newer(listener_generation, owner_generation) =>
                {
                    state
                        .eligibility_owners
                        .insert(thread_id, listener_generation);
                }
                Some(_) => return,
                None => {
                    state
                        .eligibility_owners
                        .insert(thread_id, listener_generation);
                }
            }
            if !eligible {
                state.eligible.remove(&thread_id);
                return;
            }
            if let Some(entry) = state.eligible.get_mut(&thread_id) {
                entry.listener_generation = listener_generation;
                entry.evict_tx = evict_tx.clone();
            } else {
                let sequence = state.next_sequence;
                state.next_sequence = state.next_sequence.wrapping_add(1);
                state.eligible.insert(
                    thread_id,
                    EligibleThread {
                        sequence,
                        listener_generation,
                        evict_tx: evict_tx.clone(),
                    },
                );
            }
            // Failed/uncertain retirements still occupy memory and residency;
            // their surviving owners remain counted until actual cleanup.
            if state.eligible.len().saturating_add(state.unloading.len()) <= MAX_INACTIVE_THREADS {
                return;
            }
            let oldest = state
                .eligible
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(thread_id, _)| *thread_id);
            oldest.and_then(|thread_id| {
                state
                    .eligible
                    .remove(&thread_id)
                    .map(|entry| (thread_id, entry))
            })
        };
        if let Some((thread_id, entry)) = evict_tx
            && entry.evict_tx.send(EvictionRequest::default()).is_err()
        {
            self.unregister_eligibility(thread_id, entry.listener_generation)
                .await;
        }
    }

    async fn unregister_eligibility(&self, thread_id: ThreadId, listener_generation: u64) {
        let mut state = self.state.lock().await;
        if state.eligibility_owners.get(&thread_id) == Some(&listener_generation) {
            state.eligibility_owners.remove(&thread_id);
            state.eligible.remove(&thread_id);
        }
    }

    pub(crate) async fn evict_one_eligible_and_wait(&self) {
        let entry = {
            let mut state = self.state.lock().await;
            let closed = state
                .eligible
                .iter()
                .filter(|(_, entry)| entry.evict_tx.is_closed())
                .map(|(thread_id, _)| *thread_id)
                .collect::<Vec<_>>();
            for closed_thread_id in closed {
                state.eligible.remove(&closed_thread_id);
                state.eligibility_owners.remove(&closed_thread_id);
            }
            let oldest = state
                .eligible
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(thread_id, _)| *thread_id);
            oldest.and_then(|thread_id| {
                state
                    .eligible
                    .remove(&thread_id)
                    .map(|entry| (thread_id, entry))
            })
        };
        let Some((thread_id, entry)) = entry else {
            return;
        };
        let (completion_tx, completion_rx) = oneshot::channel();
        if entry
            .evict_tx
            .send(EvictionRequest {
                completion: Some(completion_tx),
            })
            .is_ok()
        {
            let _ = completion_rx.await;
        } else {
            self.unregister_eligibility(thread_id, entry.listener_generation)
                .await;
        }
    }

}

fn listener_generation_is_newer(candidate: u64, current: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u64 << 63)
}

#[derive(Clone)]
pub(super) struct ListenerTaskContext {
    pub(super) thread_queue_processor: Option<ThreadQueueRequestProcessor>,
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) thread_state_manager: ThreadStateManager,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) pending_thread_unloads: Arc<PendingThreadUnloads>,
    pub(super) thread_watch_manager: ThreadWatchManager,
    pub(super) thread_list_state_permit: Arc<Semaphore>,
    pub(super) fallback_model_provider: String,
    pub(super) codex_home: PathBuf,
    pub(super) skills_watcher: Arc<SkillsWatcher>,
}

struct UnloadingState {
    thread_id: ThreadId,
    authority: Arc<PendingThreadUnloads>,
    delay: Duration,
    has_subscribers_rx: watch::Receiver<bool>,
    has_subscribers: (bool, Instant),
    thread_status_rx: ThreadStatusSubscription,
    is_active: (bool, Instant),
    evict_tx: mpsc::UnboundedSender<EvictionRequest>,
    evict_rx: mpsc::UnboundedReceiver<EvictionRequest>,
    listener_generation: Option<u64>,
}

enum UnloadingTrigger {
    IdleTimeout,
    LruPressure(EvictionCompletion),
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
        let (evict_tx, evict_rx) = mpsc::unbounded_channel();
        let state = Self {
            thread_id,
            authority: listener_task_context.pending_thread_unloads.clone(),
            delay,
            has_subscribers_rx,
            has_subscribers,
            thread_status_rx,
            is_active,
            evict_tx,
            evict_rx,
            listener_generation: None,
        };
        Some(state)
    }

    async fn register_listener(&mut self, listener_generation: u64) {
        self.listener_generation = Some(listener_generation);
        self.sync_eligibility().await;
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

    fn is_unload_eligible(&mut self) -> bool {
        self.sync_receiver_values();
        !self.has_subscribers.0 && !self.is_active.0
    }

    async fn sync_eligibility(&self) {
        let Some(listener_generation) = self.listener_generation else {
            return;
        };
        self.authority
            .set_eligible(
                self.thread_id,
                listener_generation,
                !self.has_subscribers.0 && !self.is_active.0,
                &self.evict_tx,
            )
            .await;
    }

    async fn unregister(&self) {
        if let Some(listener_generation) = self.listener_generation {
            self.authority
                .unregister_eligibility(self.thread_id, listener_generation)
                .await;
        }
    }

    fn note_thread_activity_observed(&mut self) {
        if !self.is_active.0 {
            self.is_active = (false, Instant::now());
        }
    }

    async fn wait_for_unloading_trigger(&mut self) -> Option<UnloadingTrigger> {
        loop {
            self.sync_receiver_values();
            self.sync_eligibility().await;
            let unloading_target = self.unloading_target();
            if let Some(target) = unloading_target
                && target <= Instant::now()
            {
                return Some(UnloadingTrigger::IdleTimeout);
            }
            let unloading_sleep = async {
                if let Some(target) = unloading_target {
                    tokio::time::sleep_until(target.into()).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = unloading_sleep => return Some(UnloadingTrigger::IdleTimeout),
                evict = self.evict_rx.recv() => {
                    let request = evict?;
                    return Some(UnloadingTrigger::LruPressure(EvictionCompletion(
                        request.completion,
                    )));
                },
                changed = self.has_subscribers_rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    self.sync_receiver_values();
                },
                changed = self.thread_status_rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    self.sync_receiver_values();
                },
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ThreadShutdownResult {
    Complete,
    SubmitFailed,
    TimedOut,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum IdleThreadShutdownResult {
    ReadyForColdResume,
    Closing,
}

pub(super) enum EnsureConversationListenerResult {
    Attached,
    ConnectionClosed,
}

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
    ensure_conversation_listener_for_instance(
        listener_task_context,
        conversation_id,
        conversation,
        connection_id,
        raw_events_enabled,
    )
    .await
}

pub(super) async fn ensure_conversation_listener_for_instance(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
    connection_id: ConnectionId,
    raw_events_enabled: bool,
) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
    let thread_state = match listener_task_context
        .pending_thread_unloads
        .admit_listener_connection(
            &listener_task_context.thread_state_manager,
            conversation_id,
            connection_id,
            raw_events_enabled,
        )
        .await
    {
        ThreadConnectionAdmission::Admitted(thread_state) => thread_state,
        ThreadConnectionAdmission::ConnectionClosed => {
            return Ok(EnsureConversationListenerResult::ConnectionClosed);
        }
        ThreadConnectionAdmission::ThreadClosing => {
            return Err(invalid_request(format!(
                "thread {conversation_id} is closing; retry after the thread is closed"
            )));
        }
    };
    // The private history index needs input provenance even for clients that
    // did not request experimental raw notifications. Tool outputs stay off.
    conversation.request_raw_user_turn_boundaries();
    if raw_events_enabled {
        // Tell the producer a consumer exists before the listener starts, so no raw event is
        // suppressed for a subscriber that asked for them.
        conversation.request_raw_response_items();
    }
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

/// Starts the listener that owns idle unload for a created thread with no
/// connection to subscribe. Subscription normally starts it, so a creator that
/// disconnects before attaching would otherwise leave the loaded thread
/// unsupervised until the server exits.
pub(super) async fn supervise_unsubscribed_thread(
    listener_task_context: ListenerTaskContext,
    conversation_id: ThreadId,
    conversation: Arc<CodexThread>,
) -> Result<(), JSONRPCErrorError> {
    if listener_task_context
        .pending_thread_unloads
        .contains(&conversation_id)
        .await
    {
        return Ok(());
    }
    let thread_state = listener_task_context
        .thread_state_manager
        .thread_state(conversation_id)
        .await;
    ensure_listener_task_running(
        listener_task_context,
        conversation_id,
        conversation,
        thread_state,
    )
    .await
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
    let listener_cancellation = CancellationToken::new();
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
    {
        let thread_state = thread_state.lock().await;
        if thread_state.listener_matches(&conversation) {
            return Ok(());
        }
    }
    let config = conversation.config().await;
    let environments = conversation.environment_selections().await;
    let watch_registration = listener_task_context
        .skills_watcher
        .register_thread_config(
            config.as_ref(),
            listener_task_context.thread_manager.as_ref(),
            &environments,
        )
        .await
        .map_err(|err| internal_error(format!("failed to register skills watcher: {err}")))?;
    let thread_settings_baseline =
        thread_settings_from_config_snapshot(&conversation.config_snapshot().await);
    let (mut listener_command_rx, listener_generation, listener_overflow) = {
        let mut thread_state = thread_state.lock().await;
        if thread_state.listener_matches(&conversation) {
            return Ok(());
        }
        let (listener_command_rx, listener_generation) = thread_state.set_listener(
            listener_cancellation.clone(),
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
        let listener_overflow = listener_task_context
            .thread_state_manager
            .register_listener_command_tx(conversation_id, listener_command_tx);
        (listener_command_rx, listener_generation, listener_overflow)
    };
    let event_context = listener_task_context.clone();
    let ListenerTaskContext {
        outgoing,
        thread_manager,
        thread_state_manager,
        pending_thread_unloads,
        thread_watch_manager,
        thread_queue_processor,
        ..
    } = listener_task_context;
    let outgoing_for_task = Arc::clone(&outgoing);
    // Publication and spawn must have no intervening await: attachment cancellation
    // must not leave a registered command sender without its owning listener task.
    tokio::spawn(async move {
        unloading_state.register_listener(listener_generation).await;
        let mut shutdown_observed = false;
        loop {
            tokio::select! {
                biased;
                _ = listener_cancellation.cancelled() => {
                    // Listener was superseded or the thread is being torn down.
                    break;
                }
                _ = listener_overflow.cancelled() => {
                    // A listener command was not admitted. Disconnect subscribers
                    // so a subsequent resume can restore an ordered snapshot.
                    for connection_id in thread_state_manager.subscribed_connection_ids(conversation_id).await {
                        outgoing_for_task.fail_connection_delivery(connection_id).await;
                    }
                    break;
                }
                listener_command = listener_command_rx.recv() => {
                    let Some(listener_command) = listener_command else {
                        break;
                    };
                    handle_thread_listener_command(
                        conversation_id,
                        &conversation,
                        &thread_state_manager,
                        &thread_state,
                        &thread_watch_manager,
                        &outgoing_for_task,
                        &pending_thread_unloads,
                        listener_command,
                    )
                    .await;
                }
                event = conversation.next_event() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(err) => {
                            if !shutdown_observed {
                                tracing::warn!("thread.next_event() failed with: {err}");
                                for connection_id in thread_state_manager.subscribed_connection_ids(conversation_id).await {
                                    outgoing_for_task.fail_connection_delivery(connection_id).await;
                                }
                            }
                            // EOF follows actual terminal-producer drain on a
                            // normal shutdown. On failure, core intake is fenced;
                            // retain an exact-instance shutdown/reap owner before
                            // removing anything from the runtime registry.
                            if pending_thread_unloads.begin(conversation_id).await {
                                unload_thread_without_subscribers(
                                    thread_manager.clone(),
                                    outgoing_for_task.clone(),
                                    pending_thread_unloads.clone(),
                                    thread_state_manager.clone(),
                                    thread_watch_manager.clone(),
                                    thread_queue_processor.clone(),
                                    conversation_id,
                                    conversation.clone(),
                                    EvictionCompletion(None),
                                ).await;
                            }
                            break;
                        }
                    };
                    shutdown_observed |= matches!(&event.msg, EventMsg::ShutdownComplete);

                    process_thread_listener_event(
                        &event_context,
                        conversation_id,
                        &conversation,
                        &thread_state,
                        event,
                    ).await;
                }
                unloading_trigger = unloading_state.wait_for_unloading_trigger() => {
                    let Some(unloading_trigger) = unloading_trigger else {
                        break;
                    };
                    if !unloading_state.is_unload_eligible()
                        || matches!(&unloading_trigger, UnloadingTrigger::IdleTimeout)
                            && !unloading_state.should_unload_now()
                    {
                        continue;
                    }
                    if matches!(conversation.agent_status().await, AgentStatus::Running) {
                        unloading_state.note_thread_activity_observed();
                        continue;
                    }
                    if !pending_thread_unloads.begin(conversation_id).await {
                        continue;
                    }
                    if !unloading_state.is_unload_eligible() {
                        pending_thread_unloads.finish(&conversation_id).await;
                        continue;
                    }
                    let eviction_completion = match unloading_trigger {
                        UnloadingTrigger::IdleTimeout => EvictionCompletion(None),
                        UnloadingTrigger::LruPressure(completion) => completion,
                    };
                    unload_thread_without_subscribers(
                        thread_manager.clone(),
                        outgoing_for_task.clone(),
                        pending_thread_unloads.clone(),
                        thread_state_manager.clone(),
                        thread_watch_manager.clone(),
                        thread_queue_processor.clone(),
                        conversation_id,
                        conversation.clone(),
                        eviction_completion,
                    )
                    .await;
                    break;
                }
            }
        }

        unloading_state.unregister().await;
        let mut thread_state = thread_state.lock().await;
        if thread_state.listener_generation == listener_generation {
            if let Some(tx) = thread_state.listener_command_tx().as_ref() {
                thread_state_manager.unregister_listener_command_tx(conversation_id, tx);
            }
            thread_state.clear_listener();
        }
    });
    Ok(())
}

pub(super) async fn wait_for_thread_shutdown(thread: &Arc<CodexThread>) -> ThreadShutdownResult {
    wait_for_thread_shutdown_with_timeout(thread, THREAD_SHUTDOWN_TIMEOUT).await
}

/// Processes one core event in the listener subscription order.
async fn process_thread_listener_event(
    context: &ListenerTaskContext,
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    thread_state: &Arc<Mutex<ThreadState>>,
    event: codex_protocol::protocol::Event,
) {
    let (is_current_turn, summary_failed) = {
        let state = thread_state.lock().await;
        (
            state.is_current_summary_turn(&event.id),
            state
                .turn_summary_for(&event.id)
                .is_some_and(|summary| summary.last_error.is_some()),
        )
    };
    let queue_completed = is_current_turn && matches!(&event.msg, EventMsg::TurnComplete(_));
    // Core emits the legacy user-message event after recording the input in
    // either history format. Reconcile custody off the listener's critical path.
    if matches!(&event.msg, EventMsg::UserMessage(_))
        && let Some(queue) = &context.thread_queue_processor
    {
        queue.input_recorded(conversation_id);
    }
    let queue_paused = is_current_turn
        && (matches!(&event.msg, EventMsg::TurnAborted(_))
            || matches!(&event.msg, EventMsg::TurnComplete(completed)
            if completed.error.is_some())
            || queue_completed && summary_failed);
    if queue_paused && let Some(queue) = &context.thread_queue_processor {
        queue.pause(conversation_id).await;
    }
    let (active_turn_id, raw_events_enabled, reconciled_wait_items) = {
        let mut state = thread_state.lock().await;
        let reconciled_wait_items =
            state.track_current_turn_event_with_reconciled_wait_items(&event.id, &event.msg);
        (
            state.open_turn_id().map(str::to_owned),
            state.experimental_raw_events,
            reconciled_wait_items,
        )
    };
    release_turn_start_for_event(
        &context.thread_state_manager,
        conversation_id,
        &event.id,
        &event.msg,
        active_turn_id.as_deref(),
    )
    .await;
    if matches!(&event.msg, EventMsg::RawResponseItem(_)) && !raw_events_enabled {
        return;
    }
    let (subscribed_connection_ids, experimental_api_connection_ids) = context
        .thread_state_manager
        .connection_ids_for_thread(conversation_id)
        .await;
    let thread_outgoing = ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
        Arc::clone(&context.outgoing),
        subscribed_connection_ids,
        experimental_api_connection_ids,
        conversation_id,
    );
    for change in reconciled_wait_items {
        thread_outgoing
            .send_server_notification(ServerNotification::ItemCompleted(
                codex_app_server_protocol::ItemCompletedNotification {
                    thread_id: conversation_id.to_string(),
                    turn_id: change.turn_id,
                    completed_at_ms: chrono::Utc::now().timestamp_millis(),
                    item: change.item,
                },
            ))
            .await;
    }
    apply_bespoke_event_handling(
        event,
        conversation_id,
        Arc::clone(conversation),
        Arc::clone(&context.thread_manager),
        thread_outgoing,
        Arc::clone(thread_state),
        context.thread_watch_manager.clone(),
        Arc::clone(&context.thread_list_state_permit),
        context.fallback_model_provider.clone(),
    )
    .await;
    if queue_completed
        && !queue_paused
        && let Some(queue) = &context.thread_queue_processor
    {
        queue.kick(conversation_id);
    }
}

async fn wait_for_thread_shutdown_with_timeout(
    thread: &Arc<CodexThread>,
    shutdown_timeout: Duration,
) -> ThreadShutdownResult {
    match tokio::time::timeout(shutdown_timeout, async {
        thread.shutdown_and_wait().await?;
        thread.wait_until_drained().await
    })
    .await
    {
        Ok(Ok(())) => ThreadShutdownResult::Complete,
        Ok(Err(_)) => ThreadShutdownResult::SubmitFailed,
        Err(_) => ThreadShutdownResult::TimedOut,
    }
}

/// An unload attempt is irreversible once shutdown is admitted. Keep its exact
/// runtime and closing marker until actual accepted owners and persistence drain.
async fn wait_for_owned_thread_retirement(thread_id: ThreadId, thread: &Arc<CodexThread>) {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        match async {
            thread.request_shutdown().await?;
            thread.wait_until_drained().await
        }.await {
            Ok(()) => return,
            Err(error) => warn!(%thread_id, %error, "thread retirement failed; retaining owner for retry"),
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_thread_unload(
    thread_manager: &Arc<ThreadManager>,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    thread_state_manager: &ThreadStateManager,
    thread_watch_manager: &ThreadWatchManager,
    thread_queue_processor: Option<&ThreadQueueRequestProcessor>,
    thread_id: ThreadId,
    expected_thread: &Arc<CodexThread>,
    emit_thread_closed: bool,
) -> bool {
    thread_manager
        .retire_thread_with_cleanup(thread_id, expected_thread, |removed_expected, replacement_present| async move {
            // The manager retains the existing per-ID publication lease through
            // every await, including release of the old unload marker. No global
            // thread-map lock is held while consumers or transports retire.
            if !replacement_present {
                if let Some(queue) = thread_queue_processor {
                    queue.retire_thread(thread_id).await;
                }
                outgoing.forget_dynamic_tool_owner(thread_id).await;
                outgoing.cancel_requests_for_thread(thread_id, /*error*/ None).await;
                thread_state_manager.remove_thread_state(thread_id).await;
                let notifications = async {
                    thread_watch_manager.remove_thread(&thread_id.to_string()).await;
                    if removed_expected && emit_thread_closed {
                        outgoing.send_server_notification(ServerNotification::ThreadClosed(
                            ThreadClosedNotification { thread_id: thread_id.to_string() },
                        )).await;
                    }
                };
                if tokio::time::timeout(
                    crate::outgoing_message::RESOURCE_DELIVERY_TIMEOUT,
                    notifications,
                ).await.is_err() {
                    warn!("timed out publishing retired thread state for {thread_id}; releasing unload authority");
                }
            } else {
                warn!("thread {thread_id} was replaced before teardown finalized; preserving and rearming the replacement");
            }
            pending_thread_unloads.finish(&thread_id).await;
        })
        .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn unload_thread_without_subscribers(
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<PendingThreadUnloads>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_queue_processor: Option<ThreadQueueRequestProcessor>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    eviction_completion: EvictionCompletion,
) {
    unload_thread_without_subscribers_with_timeout(
        thread_manager,
        outgoing,
        pending_thread_unloads,
        thread_state_manager,
        thread_watch_manager,
        thread_queue_processor,
        thread_id,
        thread,
        eviction_completion,
        THREAD_SHUTDOWN_TIMEOUT,
        /*shutdown_result_tx*/ None,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn unload_thread_without_subscribers_with_timeout(
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<PendingThreadUnloads>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_queue_processor: Option<ThreadQueueRequestProcessor>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    eviction_completion: EvictionCompletion,
    shutdown_timeout: Duration,
    shutdown_result_tx: Option<oneshot::Sender<ThreadShutdownResult>>,
) {
    info!("thread {thread_id} has no subscribers and is idle; shutting down");

    // Any pending app-server -> client requests for this thread can no longer be
    // answered; cancel their callbacks before shutdown/unload.
    outgoing
        .cancel_requests_for_thread(thread_id, /*error*/ None)
        .await;
    tokio::spawn(async move {
        let shutdown_result =
            wait_for_thread_shutdown_with_timeout(&thread, shutdown_timeout).await;
        if let Some(shutdown_result_tx) = shutdown_result_tx {
            let _ = shutdown_result_tx.send(shutdown_result);
        }
        if shutdown_result != ThreadShutdownResult::Complete {
            warn!(%thread_id, ?shutdown_result, "idle shutdown incomplete; retaining unload authority");
            wait_for_owned_thread_retirement(thread_id, &thread).await;
        }
        finish_thread_unload(
            &thread_manager,
            &outgoing,
            &pending_thread_unloads,
            &thread_state_manager,
            &thread_watch_manager,
            thread_queue_processor.as_ref(),
            thread_id,
            &thread,
            /*emit_thread_closed*/ true,
        ).await;
        drop(eviction_completion);
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn shutdown_idle_thread_for_resume(
    thread_manager: &Arc<ThreadManager>,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    thread_state_manager: &ThreadStateManager,
    thread_watch_manager: &ThreadWatchManager,
    thread_queue_processor: Option<&ThreadQueueRequestProcessor>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) -> IdleThreadShutdownResult {
    shutdown_idle_thread_for_resume_with_timeout(
        thread_manager,
        outgoing,
        pending_thread_unloads,
        thread_state_manager,
        thread_watch_manager,
        thread_queue_processor,
        thread_id,
        thread,
        THREAD_SHUTDOWN_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn shutdown_idle_thread_for_resume_with_timeout(
    thread_manager: &Arc<ThreadManager>,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    thread_state_manager: &ThreadStateManager,
    thread_watch_manager: &ThreadWatchManager,
    thread_queue_processor: Option<&ThreadQueueRequestProcessor>,
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    shutdown_timeout: Duration,
) -> IdleThreadShutdownResult {
    if !pending_thread_unloads.begin(thread_id).await {
        return IdleThreadShutdownResult::Closing;
    }
    let thread_manager = Arc::clone(thread_manager);
    let outgoing = Arc::clone(outgoing);
    let pending_thread_unloads = Arc::clone(pending_thread_unloads);
    let thread_state_manager = thread_state_manager.clone();
    let thread_watch_manager = thread_watch_manager.clone();
    let thread_queue_processor = thread_queue_processor.cloned();
    // Once the closing marker is published, shutdown and cleanup must outlive
    // cancellation of the resume request that initiated them.
    tokio::spawn(async move {
        match wait_for_thread_shutdown_with_timeout(&thread, shutdown_timeout).await {
            ThreadShutdownResult::Complete => {
                if finish_thread_unload(
                    &thread_manager,
                    &outgoing,
                    &pending_thread_unloads,
                    &thread_state_manager,
                    &thread_watch_manager,
                    thread_queue_processor.as_ref(),
                    thread_id,
                    &thread,
                    /*emit_thread_closed*/ false,
                )
                .await
                {
                    IdleThreadShutdownResult::ReadyForColdResume
                } else {
                    IdleThreadShutdownResult::Closing
                }
            }
            ThreadShutdownResult::SubmitFailed | ThreadShutdownResult::TimedOut => {
                warn!("thread {thread_id} shutdown incomplete; retaining retirement owner");
                outgoing
                    .cancel_requests_for_thread(thread_id, /*error*/ None)
                    .await;
                let thread_manager = Arc::clone(&thread_manager);
                let outgoing = Arc::clone(&outgoing);
                let pending_thread_unloads = Arc::clone(&pending_thread_unloads);
                let thread_state_manager = thread_state_manager.clone();
                let thread_watch_manager = thread_watch_manager.clone();
                tokio::spawn(async move {
                    wait_for_owned_thread_retirement(thread_id, &thread).await;
                    finish_thread_unload(
                        &thread_manager,
                        &outgoing,
                        &pending_thread_unloads,
                        &thread_state_manager,
                        &thread_watch_manager,
                        thread_queue_processor.as_ref(),
                        thread_id,
                        &thread,
                        /*emit_thread_closed*/ true,
                    )
                    .await;
                });
                IdleThreadShutdownResult::Closing
            }
        }
    })
    .await
    .unwrap_or_else(|error| {
        warn!("idle-resume shutdown owner failed for thread {thread_id}: {error}");
        IdleThreadShutdownResult::Closing
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_thread_listener_command(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    listener_command: ThreadListenerCommand,
) {
    match listener_command {
        ThreadListenerCommand::SendThreadResumeResponse(resume_request) => {
            handle_pending_thread_resume_request(
                conversation_id,
                conversation,
                thread_state_manager,
                thread_state,
                thread_watch_manager,
                outgoing,
                pending_thread_unloads,
                *resume_request,
            )
            .await;
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
pub(super) async fn handle_pending_thread_resume_request(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    pending: crate::thread_state::PendingThreadResumeRequest,
) {
    let needs_stream_prefix = !pending.redact_resume_payloads
        && !thread_state_manager
            .subscribed_connection_ids(conversation_id)
            .await
            .contains(&pending.request_id.connection_id);
    let mut stream_prefix = Vec::new();
    let mut resume_turns = None;
    let mut indexed_initial_page = None;
    let active_turn = {
        let mut state = thread_state.lock().await;
        if state.listener_generation != pending.listener_generation
            || state.listener_command_tx().is_none()
        {
            Err(resume_listener_changed_error(conversation_id))
        } else if let Some(history_items) = pending.history_items.as_deref() {
            if !state.seed_resume_history_for_listener(
                history_items,
                pending.listener_generation,
                pending.history_generation,
            ) {
                Err(resume_listener_changed_error(conversation_id))
            } else {
                if needs_stream_prefix {
                    stream_prefix = state.active_stream_replay();
                }
                Ok(state.active_turn_snapshot())
            }
        } else if !state.resume_history_is_seeded_for_current_listener() {
            Err(internal_error(format!(
                "thread {conversation_id} resume history is not initialized for the active listener"
            )))
        } else {
            if needs_stream_prefix {
                stream_prefix = state.active_stream_replay();
            }
            Ok(state.active_turn_snapshot())
        }
    };
    let active_turn = match active_turn {
        Ok(active_turn) => active_turn,
        Err(error) => {
            outgoing.send_error(pending.request_id, error).await;
            return;
        }
    };
    let history_items = pending.history_items.as_deref().unwrap_or(&[]);
    {
        // No listener event can interleave this capture and subscription commit.
        // The index preserves completed turns/items that active-prefix replay omits.
        let state = thread_state.lock().await;
        if pending.include_turns {
            resume_turns = state.resume_turns_snapshot(usize::MAX, SortDirection::Asc, TurnItemsView::Full)
                .map(|page| page.turns);
        } else if let Some(params) = pending.initial_turns_page.as_ref() {
            indexed_initial_page = state.resume_turns_snapshot(
                super::thread_processor::thread_page_size(params.limit),
                params.sort_direction.unwrap_or(SortDirection::Desc),
                params.items_view.unwrap_or(TurnItemsView::Summary),
            );
        }
    }
    tracing::debug!(
        thread_id = %conversation_id,
        request_id = ?pending.request_id,
        active_turn_present = active_turn.is_some(),
        active_turn_id = ?active_turn.as_ref().map(|turn| turn.id.as_str()),
        active_turn_status = ?active_turn.as_ref().map(|turn| &turn.status),
        "composing running thread resume response"
    );
    // The active-turn snapshot is reconstructed from retained history above and may describe a
    // turn that was interrupted by process shutdown. The live agent status is the authority for
    // whether that persisted in-progress turn is still running.
    let has_live_in_progress_turn =
        matches!(conversation.agent_status().await, AgentStatus::Running);
    let live_turn_id = conversation.active_turn_id().await;
    if !has_live_in_progress_turn
        || live_turn_id.as_deref() != active_turn.as_ref().map(|turn| turn.id.as_str())
    {
        stream_prefix.clear();
    }

    let request_id = pending.request_id;
    let connection_id = request_id.connection_id;
    let mut thread = pending.thread_summary;
    let token_usage_snapshot = if pending.include_turns {
        let usage = populate_thread_turns_from_history_with_token_usage(
            &mut thread,
            history_items,
            active_turn.as_ref(),
        );
        if let Some(turns) = resume_turns {
            thread.turns = turns;
        }
        usage
    } else {
        None
    };

    let thread_status = thread_watch_manager
        .loaded_status_for_thread(&thread.id)
        .await;

    set_thread_status_and_interrupt_stale_turns(
        &mut thread,
        thread_status,
        has_live_in_progress_turn,
    );
    let mut initial_turns_page = if let Some(mut page) = indexed_initial_page {
        super::thread_processor::normalize_thread_turns_status(&mut page.turns, thread.status.clone(), has_live_in_progress_turn);
        let cursors = (|| {
            let backwards_cursor = page.turns.first()
                .map(|turn| super::thread_processor::serialize_thread_turns_cursor(&turn.id, true))
                .transpose()?;
            let next_cursor = if page.more_turns_available {
                page.turns.last()
                    .map(|turn| super::thread_processor::serialize_thread_turns_cursor(&turn.id, false))
                    .transpose()?
            } else { None };
            Ok::<_, JSONRPCErrorError>((next_cursor, backwards_cursor))
        })();
        let (next_cursor, backwards_cursor) = match cursors {
            Ok(cursors) => cursors,
            Err(error) => { outgoing.send_error(request_id, error).await; return; }
        };
        Some(codex_app_server_protocol::TurnsPage { data: page.turns, next_cursor, backwards_cursor })
    } else if let Some(params) = pending.initial_turns_page.as_ref() {
        let page = if pending.include_turns {
            super::thread_processor::build_thread_resume_initial_turns_page(&thread.turns, params)
        } else {
            super::thread_processor::build_thread_resume_initial_turns_page_from_history(
                history_items,
                thread.status.clone(),
                has_live_in_progress_turn,
                active_turn,
                params,
            )
        };
        match page {
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

    let config_snapshot = pending.config_snapshot;
    let selected_environment =
        super::thread_processor::selected_thread_environment(&config_snapshot);
    let cwd = config_snapshot.cwd().clone();
    let ThreadConfigSnapshot {
        model,
        model_provider_id,
        service_tier,
        approval_policy,
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
        selected_environment,
        runtime_workspace_roots: workspace_roots,
        instruction_sources,
        approval_policy: approval_policy.into(),
        sandbox,
        permission_profile: Some(permission_profile),
        active_permission_profile,
        reasoning_effort,
        initial_turns_page,
    };
    let committed = outgoing
        .send_resume_response_with_commit(request_id, response, originator, async {
            match pending_thread_unloads
                .admit_resume_connection(
                    thread_state_manager,
                    conversation_id,
                    connection_id,
                    thread_state,
                    pending.listener_generation,
                )
                .await?
            {
                ThreadConnectionAdmission::Admitted(guard) => Ok(Some(guard)),
                ThreadConnectionAdmission::ThreadClosing => Err(invalid_request(format!(
                    "thread {conversation_id} is closing; retry thread/resume after the thread is closed"
                ))),
                ThreadConnectionAdmission::ConnectionClosed => {
                    tracing::debug!(
                        thread_id = %conversation_id,
                        connection_id = ?connection_id,
                        "skipping running thread resume for closed connection"
                    );
                    Ok(None)
                }
            }
        })
        .await;
    if !committed {
        return;
    }
    // Still on the listener command owner: enqueue the active prefix before
    // consuming any future suffix. These are live starts/deltas, not completed
    // history items. Existing subscribers already own their prefix.
    for notification in stream_prefix {
        outgoing
            .send_server_notification_to_connections(&[connection_id], notification)
            .await;
    }
    // Match cold resume: metadata-only resume should attach the listener without
    // paying the cost of turn reconstruction for historical usage replay.
    if let Some(token_usage_snapshot) = token_usage_snapshot {
        // Rejoining a loaded thread has the same UI contract as a cold resume, but
        // keeps the usage and its owner from the same loaded history snapshot.
        send_thread_token_usage_update_to_connection(
            outgoing,
            connection_id,
            conversation_id,
            token_usage_snapshot,
        )
        .await;
    }
    let experimental_api_enabled = thread_state_manager
        .connection_supports_experimental_api(connection_id)
        .await;
    outgoing
        .replay_requests_to_connection_for_thread(
            connection_id,
            conversation_id,
            experimental_api_enabled,
        )
        .await;
    // App-server owns resume response and snapshot ordering, so wait until
    // replay completes before letting extensions react to the idle thread.
    conversation.emit_thread_idle_lifecycle_if_idle().await;
}

fn resume_listener_changed_error(conversation_id: ThreadId) -> JSONRPCErrorError {
    internal_error(format!(
        "thread {conversation_id} listener changed while composing resume response"
    ))
}

pub(crate) fn populate_thread_turns_from_history(
    thread: &mut Thread,
    items: &[RolloutItem],
    active_turn: Option<&Turn>,
) -> std::result::Result<(), String> {
    codex_rollout::ensure_rollout_source_integrity(items).map_err(|error| error.to_string())?;
    let mut turns = build_legacy_api_turns_from_rollout_items(items);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn.clone());
    }
    thread.turns = turns;
    Ok(())
}

pub(super) fn populate_thread_turns_from_history_with_token_usage(
    thread: &mut Thread,
    items: &[RolloutItem],
    active_turn: Option<&Turn>,
) -> Option<TokenUsageReplaySnapshot> {
    let (mut turns, token_usage_replay) =
        super::token_usage_replay::build_turns_with_token_usage_replay(items);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn.clone());
    }
    let token_usage_snapshot = token_usage_replay.into_snapshot(&turns);
    thread.turns = turns;
    token_usage_snapshot
}

pub(super) async fn resolve_pending_server_request(
    conversation_id: ThreadId,
    thread_state_manager: &ThreadStateManager,
    outgoing: &Arc<OutgoingMessageSender>,
    request_id: RequestId,
) {
    let thread_id = conversation_id.to_string();
    let subscribed_connection_ids = thread_state_manager
        .subscribed_connection_ids(conversation_id)
        .await;
    let outgoing = ThreadScopedOutgoingMessageSender::new(
        outgoing.clone(),
        subscribed_connection_ids,
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
    let loaded_status = match loaded_status {
        ThreadStatus::Active { active_flags }
            if !has_live_in_progress_turn && active_flags.is_empty() =>
        {
            ThreadStatus::Idle
        }
        status => status,
    };
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

