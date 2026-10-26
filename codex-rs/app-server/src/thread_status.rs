use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::ThreadStatusChangedNotification;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TurnAbortReason;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct ThreadWatchManager {
    state: Arc<Mutex<ThreadWatchState>>,
    publications: Option<mpsc::Sender<ThreadStatusChangedNotification>>,
    publication_failure: tokio_util::sync::CancellationToken,
    running_turn_count_tx: watch::Sender<usize>,
}

pub(crate) struct ThreadWatchActiveGuard {
    manager: ThreadWatchManager,
    thread_id: String,
    guard_type: ThreadWatchActiveGuardType,
    lifecycle: Arc<()>,
}

pub(crate) struct ThreadStatusSubscription {
    receiver: Option<watch::Receiver<ThreadStatus>>,
    state: Arc<Mutex<ThreadWatchState>>,
    thread_id: String,
}

impl ThreadStatusSubscription {
    fn new(
        receiver: watch::Receiver<ThreadStatus>,
        state: Arc<Mutex<ThreadWatchState>>,
        thread_id: String,
    ) -> Self {
        Self {
            receiver: Some(receiver),
            state,
            thread_id,
        }
    }

    pub(crate) fn borrow(&self) -> watch::Ref<'_, ThreadStatus> {
        match self.receiver.as_ref() {
            Some(receiver) => receiver.borrow(),
            None => unreachable!("thread status subscription receiver is only removed on drop"),
        }
    }

    pub(crate) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.changed().await,
            None => unreachable!("thread status subscription receiver is only removed on drop"),
        }
    }
}

impl Drop for ThreadStatusSubscription {
    #[expect(
        clippy::expect_used,
        reason = "A poisoned canonical status lock cannot safely update watcher ownership"
    )]
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.state
            .lock()
            .expect("thread watch state poisoned")
            .remove_status_watcher_if_unused(&self.thread_id);
    }
}

impl ThreadWatchActiveGuard {
    fn new(
        manager: ThreadWatchManager,
        thread_id: String,
        guard_type: ThreadWatchActiveGuardType,
        lifecycle: Arc<()>,
    ) -> Self {
        Self {
            manager,
            thread_id,
            guard_type,
            lifecycle,
        }
    }
}

impl Drop for ThreadWatchActiveGuard {
    fn drop(&mut self) {
        self.manager
            .note_active_guard_released(&self.thread_id, self.guard_type, &self.lifecycle);
    }
}

#[derive(Clone, Copy)]
enum ThreadWatchActiveGuardType {
    Permission,
    UserInput,
}

impl Default for ThreadWatchManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadWatchManager {
    pub(crate) fn new() -> Self {
        let (running_turn_count_tx, _running_turn_count_rx) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(ThreadWatchState::default())),
            publications: None,
            publication_failure: tokio_util::sync::CancellationToken::new(),
            running_turn_count_tx,
        }
    }

    pub(crate) fn new_with_outgoing(outgoing: Arc<OutgoingMessageSender>) -> Self {
        let (running_turn_count_tx, _running_turn_count_rx) = watch::channel(0);
        let (publications, mut receiver) = mpsc::channel(1024);
        let publication_failure = tokio_util::sync::CancellationToken::new();
        tokio::spawn({
            let failure = publication_failure.clone();
            async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = failure.cancelled() => {
                            outgoing.fail_all_connection_delivery().await;
                            break;
                        }
                        next = receiver.recv() => {
                            let Some(notification) = next else { break };
                            tokio::select! {
                                _ = failure.cancelled() => {
                                    outgoing.fail_all_connection_delivery().await;
                                    break;
                                }
                                _ = outgoing.send_server_notification(ServerNotification::ThreadStatusChanged(notification)) => {}
                            }
                        }
                    }
                }
            }
        });
        Self {
            state: Arc::new(Mutex::new(ThreadWatchState::default())),
            publications: Some(publications),
            publication_failure,
            running_turn_count_tx,
        }
    }

    pub(crate) async fn upsert_thread(&self, thread: &Thread) {
        self.mutate_and_publish(move |state| {
            state.upsert_thread(thread.id.clone(), /*emit_notification*/ true)
        })
        .await;
    }

    pub(crate) async fn upsert_thread_silently(&self, thread: &Thread) {
        self.mutate_and_publish(move |state| {
            state.upsert_thread(thread.id.clone(), /*emit_notification*/ false)
        })
        .await;
    }

    pub(crate) async fn remove_thread(&self, thread_id: &str) {
        let thread_id = thread_id.to_string();
        self.mutate_and_publish(move |state| state.remove_thread(&thread_id))
            .await;
    }

    #[expect(
        clippy::expect_used,
        reason = "A poisoned canonical status lock may contain a partial mutation and must not publish it"
    )]
    pub(crate) async fn loaded_status_for_thread(&self, thread_id: &str) -> ThreadStatus {
        self.state
            .lock()
            .expect("thread watch state poisoned")
            .loaded_status_for_thread(thread_id)
    }

    #[expect(
        clippy::expect_used,
        reason = "A poisoned canonical status lock may contain a partial mutation and must not publish it"
    )]
    pub(crate) async fn loaded_statuses_for_threads(
        &self,
        thread_ids: Vec<String>,
    ) -> HashMap<String, ThreadStatus> {
        let state = self.state.lock().expect("thread watch state poisoned");
        thread_ids
            .into_iter()
            .map(|thread_id| {
                let status = state.loaded_status_for_thread(&thread_id);
                (thread_id, status)
            })
            .collect()
    }


    pub(crate) fn subscribe_running_turn_count(&self) -> watch::Receiver<usize> {
        self.running_turn_count_tx.subscribe()
    }

    pub(crate) async fn note_turn_started(&self, thread_id: &str) {
        self.update_runtime_for_thread(thread_id, |runtime| {
            runtime.harness_phases.clear();
            runtime.is_loaded = true;
            runtime.running = true;
            runtime.has_system_error = false;
        })
        .await;
    }

    pub(crate) async fn note_turn_completed(&self, thread_id: &str, failed: bool) {
        self.clear_active_state(thread_id, Some(failed)).await;
    }

    pub(crate) async fn note_harness_phases(
        &self,
        thread_id: &str,
        phases: Vec<codex_protocol::protocol::HarnessPhase>,
    ) {
        self.update_runtime_for_thread(thread_id, move |runtime| {
            if runtime.running {
                runtime.harness_phases = phases;
            }
        })
        .await;
    }

    pub(crate) async fn note_turn_aborted(&self, thread_id: &str, reason: &TurnAbortReason) {
        let failed = matches!(
            reason,
            TurnAbortReason::InternalError | TurnAbortReason::ProcessLost
        )
        .then_some(true);
        self.clear_active_state(thread_id, failed).await;
    }

    pub(crate) async fn note_thread_shutdown(&self, thread_id: &str) {
        self.update_runtime_for_thread(thread_id, |runtime| {
            runtime.running = false;
            runtime.pending_permission_requests = 0;
            runtime.pending_user_input_requests = 0;
            runtime.lifecycle = Arc::new(());
            runtime.is_loaded = false;
        })
        .await;
    }

    pub(crate) async fn note_system_error(&self, thread_id: &str) {
        self.update_runtime_for_thread(thread_id, |runtime| {
            runtime.running = false;
            runtime.pending_permission_requests = 0;
            runtime.pending_user_input_requests = 0;
            runtime.lifecycle = Arc::new(());
            runtime.has_system_error = true;
        })
        .await;
    }

    async fn clear_active_state(&self, thread_id: &str, failed: Option<bool>) {
        self.update_runtime_for_thread(thread_id, move |runtime| {
            runtime.running = false;
            runtime.pending_permission_requests = 0;
            runtime.pending_user_input_requests = 0;
            runtime.lifecycle = Arc::new(());
            if let Some(failed) = failed {
                runtime.has_system_error = failed;
            }
        })
        .await;
    }

    pub(crate) async fn note_permission_requested(
        &self,
        thread_id: &str,
    ) -> ThreadWatchActiveGuard {
        self.note_pending_request(thread_id, ThreadWatchActiveGuardType::Permission)
            .await
    }

    pub(crate) async fn note_user_input_requested(
        &self,
        thread_id: &str,
    ) -> ThreadWatchActiveGuard {
        self.note_pending_request(thread_id, ThreadWatchActiveGuardType::UserInput)
            .await
    }

    async fn note_pending_request(
        &self,
        thread_id: &str,
        guard_type: ThreadWatchActiveGuardType,
    ) -> ThreadWatchActiveGuard {
        let mut lifecycle = Arc::new(());
        let notification = self.mutate_state(|state| {
            state.update_runtime(thread_id, |runtime| {
                lifecycle = Arc::clone(&runtime.lifecycle);
                runtime.is_loaded = true;
                let counter = Self::pending_counter(runtime, guard_type);
                *counter = counter.saturating_add(1);
            })
        });
        // Own the decrement before notification delivery can suspend or be canceled.
        let guard =
            ThreadWatchActiveGuard::new(self.clone(), thread_id.to_string(), guard_type, lifecycle);
        let _ = notification;
        guard
    }

    #[expect(
        clippy::expect_used,
        reason = "A poisoned canonical status lock may contain a partial mutation and must not publish it"
    )]
    fn mutate_state<F>(&self, mutate: F) -> Option<ThreadStatusChangedNotification>
    where
        F: FnOnce(&mut ThreadWatchState) -> Option<ThreadStatusChangedNotification>,
    {
        let mut state = self.state.lock().expect("thread watch state poisoned");
        let notification = mutate(&mut state);
        // Retain the count for late subscribers and publish under the state
        // lock so an older mutation cannot overwrite a newer count.
        self.running_turn_count_tx.send_if_modified(|count| {
            if *count == state.running_turn_count {
                return false;
            }
            *count = state.running_turn_count;
            true
        });
        // Enqueue under the canonical-state lock, including guard drops, so
        // an older snapshot can never be published after a newer transition.
        if let Some(notification) = &notification
            && let Some(publications) = &self.publications
            && publications.try_send(notification.clone()).is_err()
        {
            self.publication_failure.cancel();
        }
        notification
    }

    async fn mutate_and_publish<F>(&self, mutate: F)
    where
        F: FnOnce(&mut ThreadWatchState) -> Option<ThreadStatusChangedNotification>,
    {
        let notification = self.mutate_state(mutate);
        let _ = notification;
    }

    #[expect(
        clippy::expect_used,
        reason = "A poisoned canonical status lock may contain a partial mutation and must not publish it"
    )]
    pub(crate) async fn subscribe(&self, thread_id: ThreadId) -> Option<ThreadStatusSubscription> {
        let thread_id = thread_id.to_string();
        let receiver = self
            .state
            .lock()
            .expect("thread watch state poisoned")
            .subscribe(thread_id.clone());
        Some(ThreadStatusSubscription::new(
            receiver,
            Arc::clone(&self.state),
            thread_id,
        ))
    }

    fn note_active_guard_released(
        &self,
        thread_id: &str,
        guard_type: ThreadWatchActiveGuardType,
        lifecycle: &Arc<()>,
    ) -> Option<ThreadStatusChangedNotification> {
        self.mutate_state(|state| {
            let previous_status = state.status_for(thread_id);
            let runtime = state.runtime_by_thread_id.get_mut(thread_id)?;
            if !Arc::ptr_eq(&runtime.lifecycle, lifecycle) {
                return None;
            }
            let counter = Self::pending_counter(runtime, guard_type);
            *counter = counter.saturating_sub(1);
            state.update_status_watcher_for_thread(thread_id);
            state.status_changed_notification(thread_id.to_string(), previous_status)
        })
    }

    async fn update_runtime_for_thread<F>(&self, thread_id: &str, update: F)
    where
        F: FnOnce(&mut RuntimeFacts),
    {
        let thread_id = thread_id.to_string();
        self.mutate_and_publish(move |state| state.update_runtime(&thread_id, update))
            .await;
    }

    fn pending_counter(
        runtime: &mut RuntimeFacts,
        guard_type: ThreadWatchActiveGuardType,
    ) -> &mut u32 {
        match guard_type {
            ThreadWatchActiveGuardType::Permission => &mut runtime.pending_permission_requests,
            ThreadWatchActiveGuardType::UserInput => &mut runtime.pending_user_input_requests,
        }
    }
}

pub(crate) fn resolve_thread_status(
    status: ThreadStatus,
    has_in_progress_turn: bool,
) -> ThreadStatus {
    // Running-turn events can arrive before the watch runtime state is observed by
    // the listener loop. In that window we prefer to reflect a real active turn as
    // `Active` instead of `Idle`/`NotLoaded`.
    if has_in_progress_turn && matches!(status, ThreadStatus::Idle | ThreadStatus::NotLoaded) {
        return ThreadStatus::Active {
            active_flags: Vec::new(),
        };
    }

    status
}

#[derive(Default)]
struct ThreadWatchState {
    running_turn_count: usize,
    runtime_by_thread_id: HashMap<String, RuntimeFacts>,
    status_watcher_by_thread_id: HashMap<String, watch::Sender<ThreadStatus>>,
}

impl ThreadWatchState {
    fn upsert_thread(
        &mut self,
        thread_id: String,
        emit_notification: bool,
    ) -> Option<ThreadStatusChangedNotification> {
        let previous_status = self.status_for(&thread_id);
        let runtime = self
            .runtime_by_thread_id
            .entry(thread_id.clone())
            .or_default();
        runtime.is_loaded = true;
        self.update_status_watcher_for_thread(&thread_id);
        if emit_notification {
            self.status_changed_notification(thread_id, previous_status)
        } else {
            None
        }
    }

    fn remove_thread(&mut self, thread_id: &str) -> Option<ThreadStatusChangedNotification> {
        let previous_status = self.status_for(thread_id);
        if let Some(runtime) = self.runtime_by_thread_id.remove(thread_id) {
            self.running_turn_count -= usize::from(runtime.running);
        }
        self.update_status_watcher(thread_id, &ThreadStatus::NotLoaded);
        if previous_status.is_some() && previous_status != Some(ThreadStatus::NotLoaded) {
            Some(ThreadStatusChangedNotification {
                thread_id: thread_id.to_string(),
                status: ThreadStatus::NotLoaded,
            })
        } else {
            None
        }
    }

    fn update_runtime<F>(
        &mut self,
        thread_id: &str,
        mutate: F,
    ) -> Option<ThreadStatusChangedNotification>
    where
        F: FnOnce(&mut RuntimeFacts),
    {
        let previous_status = self.status_for(thread_id);
        let runtime = self
            .runtime_by_thread_id
            .entry(thread_id.to_string())
            .or_default();
        runtime.is_loaded = true;
        let was_running = runtime.running;
        mutate(runtime);
        self.running_turn_count =
            self.running_turn_count - usize::from(was_running) + usize::from(runtime.running);
        self.update_status_watcher_for_thread(thread_id);
        self.status_changed_notification(thread_id.to_string(), previous_status)
    }

    fn status_for(&self, thread_id: &str) -> Option<ThreadStatus> {
        self.runtime_by_thread_id
            .get(thread_id)
            .map(loaded_thread_status)
    }

    fn loaded_status_for_thread(&self, thread_id: &str) -> ThreadStatus {
        self.status_for(thread_id)
            .unwrap_or(ThreadStatus::NotLoaded)
    }

    fn subscribe(&mut self, thread_id: String) -> watch::Receiver<ThreadStatus> {
        let status = self.loaded_status_for_thread(&thread_id);
        let sender = self
            .status_watcher_by_thread_id
            .entry(thread_id)
            .or_insert_with(|| watch::channel(status.clone()).0);
        sender.subscribe()
    }

    fn update_status_watcher_for_thread(&mut self, thread_id: &str) {
        let status = self.loaded_status_for_thread(thread_id);
        self.update_status_watcher(thread_id, &status);
    }

    fn update_status_watcher(&mut self, thread_id: &str, status: &ThreadStatus) {
        if let Some(sender) = self.status_watcher_by_thread_id.get(thread_id) {
            let status = status.clone();
            let _ = sender.send_if_modified(|current| {
                if *current == status {
                    false
                } else {
                    *current = status;
                    true
                }
            });
        }
        self.remove_status_watcher_if_unused(thread_id);
    }

    fn remove_status_watcher_if_unused(&mut self, thread_id: &str) {
        if self
            .status_watcher_by_thread_id
            .get(thread_id)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            self.status_watcher_by_thread_id.remove(thread_id);
        }
    }

    fn status_changed_notification(
        &self,
        thread_id: String,
        previous_status: Option<ThreadStatus>,
    ) -> Option<ThreadStatusChangedNotification> {
        let status = self.status_for(&thread_id)?;

        if previous_status.as_ref() == Some(&status) {
            return None;
        }

        Some(ThreadStatusChangedNotification { thread_id, status })
    }
}

#[derive(Clone, Default)]
struct RuntimeFacts {
    harness_phases: Vec<codex_protocol::protocol::HarnessPhase>,
    lifecycle: Arc<()>,
    is_loaded: bool,
    running: bool,
    pending_permission_requests: u32,
    pending_user_input_requests: u32,
    has_system_error: bool,
}

fn loaded_thread_status(runtime: &RuntimeFacts) -> ThreadStatus {
    if !runtime.is_loaded {
        return ThreadStatus::NotLoaded;
    }

    let mut active_flags = Vec::new();
    if runtime.running {
        active_flags.extend(runtime.harness_phases.iter().map(|phase| {
            use codex_protocol::protocol::HarnessPhase;
            match phase {
                HarnessPhase::ResourceResolution => ThreadActiveFlag::ResolvingResources,
                HarnessPhase::DiffTracker => ThreadActiveFlag::WaitingOnDiffTracker,
                HarnessPhase::WorkspaceGate => ThreadActiveFlag::WaitingOnWorkspaceGate,
                HarnessPhase::EvidenceTracker => ThreadActiveFlag::WaitingOnEvidenceTracker,
                HarnessPhase::Delivery => ThreadActiveFlag::WaitingOnDelivery,
                HarnessPhase::ProcessCleanup => ThreadActiveFlag::WaitingOnProcessCleanup,
                HarnessPhase::ReadyToSample => ThreadActiveFlag::ReadyToSample,
            }
        }));
    }
    if runtime.pending_permission_requests > 0 {
        active_flags.push(ThreadActiveFlag::WaitingOnApproval);
    }
    if runtime.pending_user_input_requests > 0 {
        active_flags.push(ThreadActiveFlag::WaitingOnUserInput);
    }

    if runtime.running || !active_flags.is_empty() {
        return ThreadStatus::Active { active_flags };
    }

    if runtime.has_system_error {
        return ThreadStatus::SystemError;
    }

    ThreadStatus::Idle
}

