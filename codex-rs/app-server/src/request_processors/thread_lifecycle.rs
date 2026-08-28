use super::*;
use crate::thread_state::TerminalEventDisposition;
use crate::thread_state::acknowledge_terminal_notification;
use crate::thread_status::ThreadStatusSubscription;
use tokio::sync::Notify;
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
    admission_gate: Semaphore,
    changed: Notify,
}

impl Default for PendingThreadUnloads {
    fn default() -> Self {
        Self {
            state: Mutex::new(ThreadUnloadAuthorityState::default()),
            admission_gate: Semaphore::new(1),
            changed: Notify::new(),
        }
    }
}

impl PendingThreadUnloads {
    pub(super) async fn begin(&self, thread_id: ThreadId) -> bool {
        let Ok(_admission_permit) = self.admission_gate.acquire().await else {
            return false;
        };
        let mut state = self.state.lock().await;
        state.eligible.remove(&thread_id);
        state.eligibility_owners.remove(&thread_id);
        state.unloading.insert(thread_id)
    }

    pub(super) async fn finish(&self, thread_id: &ThreadId) {
        if self.state.lock().await.unloading.remove(thread_id) {
            self.changed.notify_waiters();
        }
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
        let Ok(_admission_permit) = self.admission_gate.acquire().await else {
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

    async fn admit_resume_connection(
        &self,
        thread_state_manager: &ThreadStateManager,
        thread_id: ThreadId,
        connection_id: ConnectionId,
    ) -> ThreadConnectionAdmission<()> {
        let Ok(_admission_permit) = self.admission_gate.acquire().await else {
            return ThreadConnectionAdmission::ThreadClosing;
        };
        if self.state.lock().await.unloading.contains(&thread_id) {
            return ThreadConnectionAdmission::ThreadClosing;
        }
        let added = thread_state_manager
            .try_add_connection_to_thread(thread_id, connection_id)
            .await;
        if added {
            ThreadConnectionAdmission::Admitted(())
        } else {
            ThreadConnectionAdmission::ConnectionClosed
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
            if state.eligible.len() <= MAX_INACTIVE_THREADS {
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

    pub(super) async fn wait_until_finished(&self, thread_id: &ThreadId) {
        loop {
            let changed = self.changed.notified();
            if !self.contains(thread_id).await {
                return;
            }
            changed.await;
        }
    }
}

fn listener_generation_is_newer(candidate: u64, current: u64) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < (1_u64 << 63)
}

#[derive(Clone)]
pub(super) struct ListenerTaskContext {
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
    RejoinLoaded,
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
        .await;
    let thread_settings_baseline =
        thread_settings_from_config_snapshot(&conversation.config_snapshot().await);
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
    unloading_state.register_listener(listener_generation).await;
    let ListenerTaskContext {
        outgoing,
        thread_manager,
        thread_state_manager,
        pending_thread_unloads,
        thread_watch_manager,
        thread_list_state_permit,
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
                        &pending_thread_unloads,
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

                    let subscribed_connection_ids = thread_state_manager
                        .subscribed_connection_ids(conversation_id)
                        .await;
                    let durable_terminal_fingerprint = if matches!(
                        &event.msg,
                        EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
                    ) {
                        conversation
                            .durably_acknowledged_terminal_fingerprint(&event.id)
                            .await
                    } else {
                        None
                    };
                    let terminal_disposition = thread_state
                        .lock()
                        .await
                        .classify_terminal_event_with_durable_acknowledgement(
                            &event.id,
                            &event.msg,
                            &subscribed_connection_ids,
                            durable_terminal_fingerprint.as_deref(),
                        );
                    match terminal_disposition {
                        TerminalEventDisposition::RetryNotification(replay) => {
                            let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
                                outgoing_for_task.clone(),
                                subscribed_connection_ids,
                                conversation_id,
                            );
                            let dispatch = thread_outgoing
                                .retry_terminal_notification_with_receipts(
                                    replay.notification.clone(),
                                    replay.origin_connection_id,
                                    replay.target_connection_ids,
                                )
                                .await;
                            let accepted = thread_state.lock().await.record_terminal_notification_attempt(
                                &event.id,
                                &replay.fingerprint,
                                replay.notification,
                                replay.origin_connection_id,
                                &dispatch.targeted_connection_ids,
                                &dispatch.accepted_connection_ids,
                            );
                            if accepted {
                                acknowledge_terminal_notification(
                                    conversation.as_ref(),
                                    &thread_state,
                                    &event.id,
                                    &replay.fingerprint,
                                )
                                .await;
                            }
                            continue;
                        }
                        TerminalEventDisposition::ProjectNotification { fingerprint } => {
                            crate::bespoke_event_handling::project_terminal_notification_only(
                                event.clone(),
                                conversation_id,
                                conversation.clone(),
                                ThreadScopedOutgoingMessageSender::new(
                                    outgoing_for_task.clone(),
                                    subscribed_connection_ids,
                                    conversation_id,
                                ),
                                thread_state.clone(),
                                &fingerprint,
                            )
                            .await;
                            continue;
                        }
                        TerminalEventDisposition::Acknowledge { fingerprint } => {
                            acknowledge_terminal_notification(
                                conversation.as_ref(),
                                &thread_state,
                                &event.id,
                                &fingerprint,
                            )
                            .await;
                            continue;
                        }
                        TerminalEventDisposition::SuppressAcknowledged => {
                            continue;
                        }
                        TerminalEventDisposition::RejectConflict => {
                            tracing::error!(
                                turn_id = %event.id,
                                "rejected conflicting terminal event for an already terminal turn"
                            );
                            continue;
                        }
                        TerminalEventDisposition::RejectStale => {
                            tracing::warn!(
                                turn_id = %event.id,
                                "suppressed stale terminal event while a newer turn is active"
                            );
                            continue;
                        }
                        TerminalEventDisposition::Apply { fingerprint } => {
                            let mut state = thread_state.lock().await;
                            state.track_current_turn_event(&event.id, &event.msg);
                            state.mark_terminal_state_reduced(&event.id, &fingerprint);
                        }
                        TerminalEventDisposition::NotTerminal => {
                            thread_state
                                .lock()
                                .await
                                .track_current_turn_event(&event.id, &event.msg);
                        }
                    }
                    let active_turn_id = thread_state
                        .lock()
                        .await
                        .open_turn_id()
                        .map(str::to_owned);
                    release_turn_start_for_event(
                        &thread_state_manager,
                        conversation_id,
                        &event.id,
                        &event.msg,
                        active_turn_id.as_deref(),
                    )
                    .await;
                    let raw_events_enabled = thread_state.lock().await.experimental_raw_events;
                    if matches!(&event.msg, EventMsg::RawResponseItem(_)) && !raw_events_enabled {
                        continue;
                    }
                    let experimental_api_connection_ids = thread_state_manager
                        .experimental_api_connection_ids(conversation_id)
                        .await;
                    let thread_outgoing =
                        ThreadScopedOutgoingMessageSender::new_with_experimental_api_connections(
                        outgoing_for_task.clone(),
                        subscribed_connection_ids,
                        experimental_api_connection_ids,
                        conversation_id,
                    );

                    apply_bespoke_event_handling(
                        event.clone(),
                        conversation_id,
                        conversation.clone(),
                        thread_manager.clone(),
                        thread_outgoing,
                        thread_state.clone(),
                        thread_watch_manager.clone(),
                        thread_list_state_permit.clone(),
                        fallback_model_provider.clone(),
                    )
                    .await;
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
            thread_state_manager.unregister_listener_command_tx(conversation_id);
            thread_state.clear_listener();
        }
    });
    Ok(())
}

pub(super) async fn wait_for_thread_shutdown(thread: &Arc<CodexThread>) -> ThreadShutdownResult {
    wait_for_thread_shutdown_with_timeout(thread, THREAD_SHUTDOWN_TIMEOUT).await
}

async fn wait_for_thread_shutdown_with_timeout(
    thread: &Arc<CodexThread>,
    shutdown_timeout: Duration,
) -> ThreadShutdownResult {
    if thread.request_shutdown().await.is_err() {
        return ThreadShutdownResult::SubmitFailed;
    }
    match tokio::time::timeout(shutdown_timeout, thread.wait_until_terminated()).await {
        Ok(()) => ThreadShutdownResult::Complete,
        Err(_) => ThreadShutdownResult::TimedOut,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_thread_unload(
    thread_manager: &Arc<ThreadManager>,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    thread_state_manager: &ThreadStateManager,
    thread_watch_manager: &ThreadWatchManager,
    thread_id: ThreadId,
    expected_thread: &Arc<CodexThread>,
    emit_thread_closed: bool,
) -> bool {
    let removed_expected = thread_manager
        .remove_thread_if_same(&thread_id, expected_thread)
        .await;
    let ready_for_cold_resume = if removed_expected {
        outgoing
            .cancel_requests_for_thread(thread_id, /*error*/ None)
            .await;
        thread_state_manager.remove_thread_state(thread_id).await;
        thread_watch_manager
            .remove_thread(&thread_id.to_string())
            .await;
        if emit_thread_closed {
            let notification = ThreadClosedNotification {
                thread_id: thread_id.to_string(),
            };
            outgoing
                .send_server_notification(ServerNotification::ThreadClosed(notification))
                .await;
        }
        true
    } else {
        match thread_manager.get_thread(thread_id).await {
            Err(_) => {
                info!("thread {thread_id} was already removed before teardown finalized");
                thread_state_manager.remove_thread_state(thread_id).await;
                thread_watch_manager
                    .remove_thread(&thread_id.to_string())
                    .await;
                true
            }
            Ok(_) => {
                warn!(
                    "thread {thread_id} was replaced before teardown finalized; preserving the replacement"
                );
                false
            }
        }
    };
    pending_thread_unloads.finish(&thread_id).await;
    ready_for_cold_resume
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn unload_thread_without_subscribers(
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<PendingThreadUnloads>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
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
        match shutdown_result {
            ThreadShutdownResult::Complete => {
                finish_thread_unload(
                    &thread_manager,
                    &outgoing,
                    &pending_thread_unloads,
                    &thread_state_manager,
                    &thread_watch_manager,
                    thread_id,
                    &thread,
                    /*emit_thread_closed*/ true,
                )
                .await;
            }
            ThreadShutdownResult::SubmitFailed => {
                pending_thread_unloads.finish(&thread_id).await;
                warn!("failed to submit Shutdown to thread {thread_id}");
            }
            ThreadShutdownResult::TimedOut => {
                warn!("thread {thread_id} shutdown timed out; waiting for late termination");
                thread.wait_until_terminated().await;
                finish_thread_unload(
                    &thread_manager,
                    &outgoing,
                    &pending_thread_unloads,
                    &thread_state_manager,
                    &thread_watch_manager,
                    thread_id,
                    &thread,
                    /*emit_thread_closed*/ true,
                )
                .await;
            }
        }
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
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
) -> IdleThreadShutdownResult {
    shutdown_idle_thread_for_resume_with_timeout(
        thread_manager,
        outgoing,
        pending_thread_unloads,
        thread_state_manager,
        thread_watch_manager,
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
    thread_id: ThreadId,
    thread: Arc<CodexThread>,
    shutdown_timeout: Duration,
) -> IdleThreadShutdownResult {
    if !pending_thread_unloads.begin(thread_id).await {
        return IdleThreadShutdownResult::Closing;
    }
    match wait_for_thread_shutdown_with_timeout(&thread, shutdown_timeout).await {
        ThreadShutdownResult::Complete => {
            if finish_thread_unload(
                thread_manager,
                outgoing,
                pending_thread_unloads,
                thread_state_manager,
                thread_watch_manager,
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
        ThreadShutdownResult::SubmitFailed => {
            pending_thread_unloads.finish(&thread_id).await;
            warn!("failed to submit Shutdown to thread {thread_id}");
            IdleThreadShutdownResult::RejoinLoaded
        }
        ThreadShutdownResult::TimedOut => {
            warn!("thread {thread_id} shutdown timed out; waiting for late termination");
            outgoing
                .cancel_requests_for_thread(thread_id, /*error*/ None)
                .await;
            let thread_manager = Arc::clone(thread_manager);
            let outgoing = Arc::clone(outgoing);
            let pending_thread_unloads = Arc::clone(pending_thread_unloads);
            let thread_state_manager = thread_state_manager.clone();
            let thread_watch_manager = thread_watch_manager.clone();
            tokio::spawn(async move {
                thread.wait_until_terminated().await;
                finish_thread_unload(
                    &thread_manager,
                    &outgoing,
                    &pending_thread_unloads,
                    &thread_state_manager,
                    &thread_watch_manager,
                    thread_id,
                    &thread,
                    /*emit_thread_closed*/ true,
                )
                .await;
            });
            IdleThreadShutdownResult::Closing
        }
    }
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
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
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
                pending_thread_unloads,
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
pub(super) async fn handle_pending_thread_resume_request(
    conversation_id: ThreadId,
    conversation: &Arc<CodexThread>,
    _codex_home: &Path,
    thread_state_manager: &ThreadStateManager,
    thread_state: &Arc<Mutex<ThreadState>>,
    thread_watch_manager: &ThreadWatchManager,
    outgoing: &Arc<OutgoingMessageSender>,
    pending_thread_unloads: &Arc<PendingThreadUnloads>,
    pending: crate::thread_state::PendingThreadResumeRequest,
) {
    if let Some(history_items) = pending.history_items.as_deref() {
        let replay_fingerprints = conversation
            .terminal_events_requiring_app_server_acknowledgement()
            .await;
        if !thread_state.lock().await.seed_resume_history_for_listener(
            history_items,
            pending.listener_generation,
            replay_fingerprints.as_ref(),
        ) {
            outgoing
                .send_error(
                    pending.request_id,
                    internal_error(format!(
                        "thread {conversation_id} listener changed while composing resume response"
                    )),
                )
                .await;
            return;
        }
    } else if !thread_state
        .lock()
        .await
        .resume_history_is_seeded_for_current_listener()
    {
        outgoing
            .send_error(
                pending.request_id,
                internal_error(format!(
                    "thread {conversation_id} resume history is not initialized for the active listener"
                )),
            )
            .await;
        return;
    }
    let history_items = pending.history_items.as_deref().unwrap_or(&[]);
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
    // The active-turn snapshot is reconstructed from retained history above and may describe a
    // turn that was interrupted by process shutdown. The live agent status is the authority for
    // whether that persisted in-progress turn is still running.
    let has_live_in_progress_turn =
        matches!(conversation.agent_status().await, AgentStatus::Running);

    let request_id = pending.request_id;
    let connection_id = request_id.connection_id;
    let mut thread = pending.thread_summary;
    let token_usage_turn_id = if pending.include_turns {
        Some(populate_thread_turns_from_history_with_token_usage(
            &mut thread,
            history_items,
            active_turn.as_ref(),
        ))
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
    let mut initial_turns_page = if let Some(page) = pending.prepared_initial_turns_page {
        Some(page)
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

    match pending_thread_unloads
        .admit_resume_connection(thread_state_manager, conversation_id, connection_id)
        .await
    {
        ThreadConnectionAdmission::Admitted(()) => {}
        ThreadConnectionAdmission::ThreadClosing => {
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
        ThreadConnectionAdmission::ConnectionClosed => {
            tracing::debug!(
                thread_id = %conversation_id,
                connection_id = ?connection_id,
                "skipping running thread resume for closed connection"
            );
            return;
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
        selected_environment,
        runtime_workspace_roots: workspace_roots,
        instruction_sources,
        approval_policy: approval_policy.into(),
        approvals_reviewer: approvals_reviewer.into(),
        sandbox,
        permission_profile: Some(permission_profile),
        active_permission_profile,
        reasoning_effort,
        initial_turns_page,
    };
    outgoing
        .send_response_with_thread_originator(request_id, response, originator)
        .await;
    // Match cold resume: metadata-only resume should attach the listener without
    // paying the cost of turn reconstruction for historical usage replay.
    if let Some(token_usage_turn_id) = token_usage_turn_id {
        // Rejoining a loaded thread has the same UI contract as a cold resume, but
        // uses the live conversation state instead of reconstructing a new session.
        send_thread_token_usage_update_to_connection(
            outgoing,
            connection_id,
            conversation_id,
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

pub(super) fn populate_thread_turns_from_history_with_token_usage(
    thread: &mut Thread,
    items: &[RolloutItem],
    active_turn: Option<&Turn>,
) -> String {
    let (mut turns, token_usage_replay) =
        super::token_usage_replay::build_turns_with_token_usage_replay(items);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn.clone());
    }
    let token_usage_turn_id = token_usage_replay.into_turn_id(&turns);
    thread.turns = turns;
    token_usage_turn_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use core_test_support::load_default_config_for_test;
    use tempfile::TempDir;

    #[tokio::test]
    async fn pre_start_error_releases_claim_but_in_turn_error_retains_it() {
        let error = EventMsg::Error(codex_protocol::protocol::ErrorEvent {
            message: "injected rejection".to_string(),
            codex_error_info: None,
        });

        let rejected_manager = ThreadStateManager::new();
        let rejected_thread = ThreadId::new();
        assert_eq!(
            rejected_manager
                .claim_turn_start(Some("retryable"), rejected_thread, "turn-rejected")
                .await,
            crate::thread_state::TurnStartClaim::Claimed
        );
        release_turn_start_for_event(
            &rejected_manager,
            rejected_thread,
            "turn-rejected",
            &error,
            None,
        )
        .await;
        assert_eq!(
            rejected_manager
                .claim_turn_start(Some("retryable"), rejected_thread, "turn-retry")
                .await,
            crate::thread_state::TurnStartClaim::Claimed
        );

        let running_manager = ThreadStateManager::new();
        let running_thread = ThreadId::new();
        assert_eq!(
            running_manager
                .claim_turn_start(Some("running"), running_thread, "turn-running")
                .await,
            crate::thread_state::TurnStartClaim::Claimed
        );
        let open_turn_id = {
            let mut state = ThreadState::default();
            state.track_current_turn_event(
                "turn-running",
                &EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                    turn_id: "turn-running".to_string(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                }),
            );
            state.track_current_turn_event("turn-running", &error);
            state.open_turn_id().map(str::to_owned)
        };
        release_turn_start_for_event(
            &running_manager,
            running_thread,
            "turn-running",
            &error,
            open_turn_id.as_deref(),
        )
        .await;
        assert!(matches!(
            running_manager
                .claim_turn_start(Some("running"), ThreadId::new(), "turn-other")
                .await,
            crate::thread_state::TurnStartClaim::IdenticalTask(_)
        ));
    }

    struct LateShutdownFixture {
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        outgoing_rx: mpsc::Receiver<OutgoingEnvelope>,
        pending_thread_unloads: Arc<PendingThreadUnloads>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
        original_thread_state: Arc<Mutex<ThreadState>>,
        release_shutdown: Option<oneshot::Sender<()>>,
        _codex_home: TempDir,
    }

    impl LateShutdownFixture {
        async fn new() -> Self {
            let codex_home = TempDir::new().expect("create temp Codex home");
            let config = load_default_config_for_test(&codex_home).await;
            let thread_manager = Arc::new(
                codex_core::test_support::thread_manager_with_models_provider_and_home(
                    CodexAuth::create_dummy_chatgpt_auth_for_testing(),
                    config.model_provider.clone(),
                    config.codex_home.to_path_buf(),
                    Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
                ),
            );
            let codex_core::NewThread {
                thread_id, thread, ..
            } = thread_manager
                .start_thread(config)
                .await
                .expect("start test thread");
            let release_shutdown =
                codex_core::test_support::block_thread_terminal_tasks(thread.as_ref());
            let (outgoing_tx, outgoing_rx) = mpsc::channel(8);
            let outgoing = Arc::new(OutgoingMessageSender::new(
                outgoing_tx,
                AnalyticsEventsClient::disabled(),
            ));
            let pending_thread_unloads = Arc::new(PendingThreadUnloads::default());
            let thread_state_manager = ThreadStateManager::new();
            let original_thread_state = thread_state_manager.thread_state(thread_id).await;
            let thread_watch_manager = ThreadWatchManager::new();
            let thread_id_string = thread_id.to_string();
            thread_watch_manager
                .note_turn_started(&thread_id_string)
                .await;
            thread_watch_manager
                .note_turn_completed(&thread_id_string, /*failed*/ false)
                .await;
            Self {
                thread_manager,
                outgoing,
                outgoing_rx,
                pending_thread_unloads,
                thread_state_manager,
                thread_watch_manager,
                thread_id,
                thread,
                original_thread_state,
                release_shutdown: Some(release_shutdown),
                _codex_home: codex_home,
            }
        }

        async fn assert_late_cleanup_pending(&self) {
            assert!(
                self.pending_thread_unloads.contains(&self.thread_id).await,
                "late cleanup must retain pending unload authority"
            );
            let loaded = self
                .thread_manager
                .get_thread(self.thread_id)
                .await
                .expect("closing thread must stay loaded until termination");
            assert!(Arc::ptr_eq(&loaded, &self.thread));
            assert!(Arc::ptr_eq(
                &self.thread_state_manager.thread_state(self.thread_id).await,
                &self.original_thread_state
            ));
            assert_eq!(
                self.thread_watch_manager
                    .loaded_status_for_thread(&self.thread_id.to_string())
                    .await,
                ThreadStatus::Idle
            );
        }

        async fn release_and_assert_closed(&mut self) {
            self.release_shutdown
                .take()
                .expect("shutdown release should be present")
                .send(())
                .expect("blocked terminal task should still be waiting");
            tokio::time::timeout(
                Duration::from_secs(5),
                self.pending_thread_unloads
                    .wait_until_finished(&self.thread_id),
            )
            .await
            .expect("late cleanup should finish after termination");

            assert!(
                self.thread_manager
                    .get_thread(self.thread_id)
                    .await
                    .is_err(),
                "terminated thread must be removed"
            );
            let recreated_state = self.thread_state_manager.thread_state(self.thread_id).await;
            assert!(
                !Arc::ptr_eq(&recreated_state, &self.original_thread_state),
                "thread state ownership must be cleared"
            );
            assert_eq!(
                self.thread_watch_manager
                    .loaded_status_for_thread(&self.thread_id.to_string())
                    .await,
                ThreadStatus::NotLoaded
            );
            let envelope = tokio::time::timeout(Duration::from_secs(2), self.outgoing_rx.recv())
                .await
                .expect("ThreadClosed should be delivered")
                .expect("outgoing channel should stay open");
            let message = match envelope {
                OutgoingEnvelope::Broadcast { message }
                | OutgoingEnvelope::ToConnection { message, .. } => message,
            };
            assert!(matches!(
                message,
                OutgoingMessage::AppServerNotification(ServerNotification::ThreadClosed(
                    notification
                )) if notification.thread_id == self.thread_id.to_string()
            ));
        }
    }

    #[tokio::test]
    async fn existing_listener_skips_thread_skill_registration_and_watcher_is_lazy() {
        let fixture = LateShutdownFixture::new().await;
        let config = fixture.thread.config().await;
        let skills_watcher = SkillsWatcher::new(
            fixture.thread_manager.skills_service(),
            Arc::clone(&fixture.outgoing),
        );
        let context = ListenerTaskContext {
            thread_manager: Arc::clone(&fixture.thread_manager),
            thread_state_manager: fixture.thread_state_manager.clone(),
            outgoing: Arc::clone(&fixture.outgoing),
            pending_thread_unloads: Arc::clone(&fixture.pending_thread_unloads),
            thread_watch_manager: fixture.thread_watch_manager.clone(),
            thread_list_state_permit: Arc::new(Semaphore::new(1)),
            fallback_model_provider: config.model_provider_id.clone(),
            codex_home: config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&skills_watcher),
        };
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        let thread_settings =
            thread_settings_from_config_snapshot(&fixture.thread.config_snapshot().await);
        fixture.original_thread_state.lock().await.set_listener(
            cancel_tx,
            &fixture.thread,
            codex_file_watcher::WatchRegistration::default(),
            thread_settings,
        );

        ensure_listener_task_running(
            context,
            fixture.thread_id,
            Arc::clone(&fixture.thread),
            Arc::clone(&fixture.original_thread_state),
        )
        .await
        .expect("matching listener should already be running");

        assert_eq!(skills_watcher.thread_config_registration_count(), 0);
        skills_watcher.register_runtime_extra_roots(&[]);
        assert!(
            !skills_watcher.is_initialized(),
            "construction and empty roots must not allocate a skills file watcher"
        );
        skills_watcher.register_runtime_extra_roots(std::slice::from_ref(&config.cwd));
        skills_watcher.register_runtime_extra_roots(std::slice::from_ref(&config.cwd));
        assert!(skills_watcher.is_initialized());
        assert_eq!(skills_watcher.initialization_count(), 1);
    }

    #[tokio::test]
    async fn resume_waiter_is_released_only_after_unload_finishes() {
        let pending = Arc::new(PendingThreadUnloads::default());
        let thread_id = ThreadId::new();
        assert!(pending.begin(thread_id).await);

        let waiter_pending = Arc::clone(&pending);
        let waiter = tokio::spawn(async move {
            waiter_pending.wait_until_finished(&thread_id).await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        pending.finish(&thread_id).await;
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("resume waiter should continue after teardown")
            .expect("resume waiter should not panic");
    }

    #[tokio::test]
    // The held thread-state guard is the contention this test uses to keep admission in flight.
    #[allow(clippy::await_holding_invalid_type)]
    async fn unload_begin_waits_for_listener_connection_admission() {
        let authority = Arc::new(PendingThreadUnloads::default());
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(1);
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        let thread_state = thread_state_manager
            .try_ensure_connection_subscribed(thread_id, connection_id, false)
            .await
            .expect("initialized connection subscribes");
        assert!(
            thread_state_manager
                .unsubscribe_connection_from_thread(thread_id, connection_id)
                .await
        );

        let thread_state_guard = thread_state.lock().await;
        let admission_authority = Arc::clone(&authority);
        let admission_manager = thread_state_manager.clone();
        let admission = tokio::spawn(async move {
            admission_authority
                .admit_listener_connection(&admission_manager, thread_id, connection_id, true)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !thread_state_manager.has_subscribers(thread_id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("admission should subscribe before updating per-thread state");

        let begin_authority = Arc::clone(&authority);
        let begin = tokio::spawn(async move { begin_authority.begin(thread_id).await });
        tokio::task::yield_now().await;
        assert!(
            !begin.is_finished(),
            "unload begin must wait until subscription admission is complete"
        );

        drop(thread_state_guard);
        assert!(matches!(
            admission.await.expect("admission task should not panic"),
            ThreadConnectionAdmission::Admitted(_)
        ));
        assert!(begin.await.expect("unload begin task should not panic"));
        assert!(
            thread_state_manager.has_subscribers(thread_id).await,
            "the unload listener's final eligibility check must observe the admitted connection"
        );
        authority.finish(&thread_id).await;
    }

    #[test]
    fn inactive_thread_unload_delay_is_five_minutes() {
        assert_eq!(THREAD_UNLOADING_DELAY, Duration::from_secs(5 * 60));
    }

    #[tokio::test]
    async fn inactive_thread_lru_selects_only_the_oldest_above_eight() {
        let authority = PendingThreadUnloads::default();
        let mut receivers = Vec::new();
        for _ in 0..=MAX_INACTIVE_THREADS {
            let thread_id = ThreadId::new();
            let (evict_tx, evict_rx) = mpsc::unbounded_channel();
            authority.set_eligible(thread_id, 1, true, &evict_tx).await;
            receivers.push(evict_rx);
        }

        let eviction = receivers[0]
            .try_recv()
            .expect("oldest inactive task should be selected");
        assert!(eviction.completion.is_none());
        for receiver in &mut receivers[1..] {
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        assert_eq!(
            authority.state.lock().await.eligible.len(),
            MAX_INACTIVE_THREADS
        );
    }

    #[tokio::test]
    async fn stale_listener_cannot_remove_replacement_lru_eligibility() {
        let authority = PendingThreadUnloads::default();
        let thread_id = ThreadId::new();
        let (old_evict_tx, _old_evict_rx) = mpsc::unbounded_channel();
        let (replacement_evict_tx, _replacement_evict_rx) = mpsc::unbounded_channel();

        authority
            .set_eligible(thread_id, 1, true, &old_evict_tx)
            .await;
        authority
            .set_eligible(thread_id, 2, true, &replacement_evict_tx)
            .await;
        authority.unregister_eligibility(thread_id, 1).await;

        let state = authority.state.lock().await;
        assert_eq!(state.eligibility_owners.get(&thread_id), Some(&2));
        assert!(
            state
                .eligible
                .get(&thread_id)
                .is_some_and(|entry| entry.evict_tx.same_channel(&replacement_evict_tx)),
            "stale listener cleanup must preserve the replacement listener's eviction channel"
        );
    }

    #[tokio::test]
    async fn admission_eviction_waits_for_unload_completion() {
        let authority = Arc::new(PendingThreadUnloads::default());
        let thread_id = ThreadId::new();
        let (evict_tx, mut evict_rx) = mpsc::unbounded_channel();
        authority.set_eligible(thread_id, 1, true, &evict_tx).await;

        let eviction_authority = Arc::clone(&authority);
        let eviction = tokio::spawn(async move {
            eviction_authority.evict_one_eligible_and_wait().await;
        });
        let request = evict_rx
            .recv()
            .await
            .expect("eligible task should receive admission-pressure eviction");
        let completion = EvictionCompletion(request.completion);
        assert!(!eviction.is_finished());

        drop(completion);
        tokio::time::timeout(Duration::from_secs(1), eviction)
            .await
            .expect("admission retry should continue after unload completion")
            .expect("admission eviction should not panic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_unload_retains_authority_and_emits_thread_closed() {
        let mut fixture = LateShutdownFixture::new().await;
        assert!(
            fixture
                .pending_thread_unloads
                .begin(fixture.thread_id)
                .await
        );
        let (shutdown_result_tx, shutdown_result_rx) = oneshot::channel();
        unload_thread_without_subscribers_with_timeout(
            Arc::clone(&fixture.thread_manager),
            Arc::clone(&fixture.outgoing),
            Arc::clone(&fixture.pending_thread_unloads),
            fixture.thread_state_manager.clone(),
            fixture.thread_watch_manager.clone(),
            fixture.thread_id,
            Arc::clone(&fixture.thread),
            EvictionCompletion(None),
            Duration::ZERO,
            Some(shutdown_result_tx),
        )
        .await;

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), shutdown_result_rx)
                .await
                .expect("short shutdown deadline should be observed")
                .expect("unload owner should report its shutdown result"),
            ThreadShutdownResult::TimedOut
        );
        fixture.assert_late_cleanup_pending().await;
        fixture.release_and_assert_closed().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_override_resume_stays_closing_until_late_cleanup() {
        let mut fixture = LateShutdownFixture::new().await;
        let result = shutdown_idle_thread_for_resume_with_timeout(
            &fixture.thread_manager,
            &fixture.outgoing,
            &fixture.pending_thread_unloads,
            &fixture.thread_state_manager,
            &fixture.thread_watch_manager,
            fixture.thread_id,
            Arc::clone(&fixture.thread),
            Duration::ZERO,
        )
        .await;

        assert_eq!(result, IdleThreadShutdownResult::Closing);
        fixture.assert_late_cleanup_pending().await;
        assert_eq!(
            shutdown_idle_thread_for_resume_with_timeout(
                &fixture.thread_manager,
                &fixture.outgoing,
                &fixture.pending_thread_unloads,
                &fixture.thread_state_manager,
                &fixture.thread_watch_manager,
                fixture.thread_id,
                Arc::clone(&fixture.thread),
                Duration::ZERO,
            )
            .await,
            IdleThreadShutdownResult::Closing,
            "a retry must not rejoin the closing thread"
        );
        assert!(
            matches!(
                fixture.outgoing_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "ThreadClosed must wait for actual termination"
        );

        fixture.release_and_assert_closed().await;
    }
}
