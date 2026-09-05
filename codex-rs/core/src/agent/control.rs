use crate::agent::AgentStatus;
use crate::agent::agent_status_from_task;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
use crate::agent::registry::AgentTreeClosingGuard;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
use crate::agent::status::is_final;
use crate::agent::task_coordinator::AgentTaskCoordinator;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::emit_subagent_session_started;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::session_prefix::format_subagent_context_line;
use crate::session_prefix::format_subagent_notification_message;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadManagerState;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
use codex_agent_task_store::AgentTaskBinding;
use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::user_input::UserInput;
use codex_thread_store::ReadThreadParams;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::watch;
use tracing::warn;

pub(crate) use self::execution::AgentExecutionGuard;
use self::execution::AgentExecutionLimiter;
use self::residency::V2Residency;

const ROOT_LAST_TASK_MESSAGE: &str = "Main thread";
const TYPED_ACTOR_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
mod execution;
mod legacy;
mod residency;
mod spawn;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnAgentForkMode {
    TaskCapsule,
    FullHistory,
    LastNTurns(usize),
}

#[derive(Clone)]
pub(crate) struct AgentJobBinding {
    pub(crate) state_db: Arc<codex_state::StateRuntime>,
    pub(crate) job_id: String,
    pub(crate) item_id: String,
}

impl std::fmt::Debug for AgentJobBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentJobBinding")
            .field("job_id", &self.job_id)
            .field("item_id", &self.item_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnAgentOptions {
    pub(crate) fork_parent_spawn_call_id: Option<String>,
    pub(crate) fork_mode: Option<SpawnAgentForkMode>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) environments: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) typed_task_binding: Option<codex_agent_task_store::AgentTaskBindingDraft>,
    pub(crate) agent_job_binding: Option<AgentJobBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct LiveAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) metadata: AgentMetadata,
    pub(crate) status: AgentStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedAgent {
    pub(crate) agent_name: String,
    pub(crate) agent_status: AgentStatus,
    pub(crate) last_task_message: Option<String>,
}

#[cfg(test)]
pub(crate) struct AgentControlTestBarrier {
    visits: std::sync::atomic::AtomicUsize,
    reached: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Default for AgentControlTestBarrier {
    fn default() -> Self {
        Self {
            visits: std::sync::atomic::AtomicUsize::new(0),
            reached: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[cfg(test)]
impl AgentControlTestBarrier {
    async fn pause(&self) {
        self.visits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.reached.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test barrier should remain open")
            .forget();
    }

    pub(crate) async fn wait_until_reached(&self) {
        self.reached
            .acquire()
            .await
            .expect("test barrier should remain open")
            .forget();
    }

    pub(crate) fn release_one(&self) {
        self.release.add_permits(1);
    }

    fn visits(&self) -> usize {
        self.visits.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
#[derive(Default)]
struct AgentControlTestHooks {
    before_initial_submission: std::sync::Mutex<Option<Arc<AgentControlTestBarrier>>>,
    before_v2_cold_load: std::sync::Mutex<Option<Arc<AgentControlTestBarrier>>>,
    after_execution_reservation: std::sync::Mutex<Option<Arc<AgentControlTestBarrier>>>,
    after_terminal_tree_freeze: std::sync::Mutex<Option<Arc<AgentControlTestBarrier>>>,
}

#[derive(Clone, Debug)]
enum V2AgentLoadCompletion {
    Loading,
    Succeeded,
    Failed(Arc<str>),
    Cancelled,
}

/// Control-plane handle for multi-agent operations.
/// `AgentControl` is held by each session (via `SessionServices`). It provides capability to
/// spawn new agents and the inter-agent communication layer.
/// An `AgentControl` instance is intended to be created at most once per root thread/session
/// tree. That same `AgentControl` is then shared with every sub-agent spawned from that root,
/// which keeps the registry scoped to that root thread rather than the entire `ThreadManager`.
#[derive(Clone, Default)]
pub(crate) struct AgentControl {
    /// ID shared by the whole agent control session. This means every sub-agents from a common
    /// root share the same session ID.
    session_id: SessionId,
    /// Durable task aggregation follows the original session lineage across resume and fork.
    /// Keep this separate from the runtime session ID so forked threads retain their own identity.
    task_lineage_id: String,
    /// Manager-issued root whose complete task lineage must be quiescent before this session can
    /// publish terminal success. Forks retain the original root rather than narrowing the check to
    /// the fork's newer thread ID.
    terminal_quiescence_root_thread_id: Option<ThreadId>,
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    manager: Weak<ThreadManagerState>,
    state: Arc<AgentRegistry>,
    v2_residency: Arc<V2Residency>,
    v2_load_flights:
        Arc<std::sync::Mutex<HashMap<ThreadId, Arc<watch::Sender<V2AgentLoadCompletion>>>>>,
    agent_execution_limiter: Arc<AgentExecutionLimiter>,
    /// Durable typed-task state shared by the root thread and all of its sub-agents.
    task_coordinator: AgentTaskCoordinator,
    /// Fresh typed reviewer admissions exist only in this live control tree. Durable task rows
    /// and resumed session-source strings cannot reconstruct this authority.
    fresh_typed_reviews: Arc<
        std::sync::Mutex<HashMap<codex_agent_task_store::AttemptId, FreshTypedReviewAdmission>>,
    >,
    #[cfg(test)]
    test_hooks: Arc<AgentControlTestHooks>,
}

/// A single-use, process-private proof of the exact freshly admitted reviewer contract.
/// It is deliberately neither serializable nor cloneable.
pub(crate) struct FreshTypedReviewAdmission {
    assignment: codex_agent_task_store::Assignment,
    binding: AgentTaskBinding,
    runtime_session_id: SessionId,
}

impl FreshTypedReviewAdmission {
    pub(crate) fn assignment(&self) -> &codex_agent_task_store::Assignment {
        &self.assignment
    }
}

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(manager: Weak<ThreadManagerState>) -> Self {
        Self {
            manager,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_session_id(self, session_id: SessionId, max_threads: usize) -> Self {
        let task_lineage_id = session_id.to_string();
        self.with_session_and_task_lineage(session_id, task_lineage_id, max_threads)
    }

    pub(crate) fn with_session_and_task_lineage(
        mut self,
        session_id: SessionId,
        task_lineage_id: String,
        max_threads: usize,
    ) -> Self {
        self.session_id = session_id;
        self.task_lineage_id = task_lineage_id;
        self.agent_execution_limiter.initialize(max_threads);
        self.task_coordinator
            .initialize_metric_capacity(max_threads);
        self
    }

    pub(crate) fn with_terminal_quiescence_root_thread_id(
        mut self,
        root_thread_id: ThreadId,
    ) -> Self {
        self.terminal_quiescence_root_thread_id = Some(root_thread_id);
        self
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn task_lineage_id(&self) -> &str {
        &self.task_lineage_id
    }

    pub(crate) fn task_coordinator(&self) -> &AgentTaskCoordinator {
        &self.task_coordinator
    }

    pub(crate) fn take_fresh_historical_review_admission(
        &self,
        binding: &AgentTaskBinding,
        reviewer: &codex_agent_task_store::AgentTask,
        live_thread_id: ThreadId,
    ) -> Result<FreshTypedReviewAdmission, String> {
        let mut admissions = self
            .fresh_typed_reviews
            .lock()
            .map_err(|_| "fresh reviewer admission state is unavailable".to_string())?;
        let admission = admissions.get(&binding.attempt_id).ok_or_else(|| {
            "historical acceptance requires a fresh live typed reviewer admission; restored task rows and replayed bindings are not authority".to_string()
        })?;
        let admitted = &admission.binding;
        if admission.runtime_session_id != self.session_id
            || admission.assignment != reviewer.assignment
            || reviewer.current_attempt.attempt_id != admitted.attempt_id
            || reviewer.current_attempt.amendment.is_some()
            || binding.assignment_id != admitted.assignment_id
            || binding.attempt_id != admitted.attempt_id
            || binding.root_session_id != admitted.root_session_id
            || binding.root_session_id != self.task_lineage_id
            || binding.agent_path != admitted.agent_path
            || binding.task_name != admitted.task_name
            || binding.thread_id != admitted.thread_id
            || admitted.thread_id.as_deref() != Some(live_thread_id.to_string().as_str())
        {
            return Err(
                "the live reviewer binding or assignment differs from its fresh typed admission"
                    .to_string(),
            );
        }
        admissions
            .remove(&binding.attempt_id)
            .ok_or_else(|| "the fresh reviewer admission was already consumed".to_string())
    }

    pub(crate) fn has_live_agents(&self) -> bool {
        self.state.has_live_agents()
    }

    /// Freeze spawn admission for the complete in-memory tree and reject terminal publication
    /// while any descendant can still produce work. The returned guard must remain alive until
    /// successful terminal persistence has finished.
    pub(crate) async fn begin_terminal_publication(
        &self,
        current_thread_id: ThreadId,
    ) -> CodexResult<AgentTreeClosingGuard> {
        let root_thread_id = self
            .terminal_quiescence_root_thread_id
            .unwrap_or(current_thread_id);
        let mut publication_roots = vec![root_thread_id];
        if current_thread_id != root_thread_id {
            publication_roots.push(current_thread_id);
        }
        let publication_root_ids = publication_roots.iter().copied().collect::<HashSet<_>>();
        let mut closing_guard = self
            .state
            .begin_closing_registered_agent_tree(root_thread_id);
        closing_guard.mark_threads(publication_roots.iter().copied());
        #[cfg(test)]
        {
            let barrier = self
                .test_hooks
                .after_terminal_tree_freeze
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(barrier) = barrier {
                barrier.pause().await;
            }
        }
        let mut known_thread_ids = publication_root_ids.clone();
        loop {
            let mut newly_discovered = Vec::new();
            for publication_root in &publication_roots {
                newly_discovered.extend(
                    self.live_thread_spawn_descendants(*publication_root)
                        .await?
                        .into_iter()
                        .filter(|thread_id| known_thread_ids.insert(*thread_id)),
                );
            }
            if newly_discovered.is_empty() {
                break;
            }
            closing_guard.mark_threads(newly_discovered);
        }

        let persisted_topologies = self
            .persisted_thread_spawn_readiness(&publication_roots)
            .await?;
        let mut persisted_open_descendants = HashSet::new();
        let mut persisted_descendants = HashSet::new();
        for (_, persisted_topology) in persisted_topologies {
            persisted_open_descendants.extend(persisted_topology.open_descendants);
            let mut newly_discovered = Vec::new();
            for thread_id in persisted_topology.descendants {
                if publication_root_ids.contains(&thread_id)
                    || !persisted_descendants.insert(thread_id)
                {
                    return Err(CodexErr::Fatal(format!(
                        "persisted thread-spawn topology is ambiguous: thread {thread_id} appears in multiple terminal publication roots"
                    )));
                }
                if known_thread_ids.insert(thread_id) {
                    newly_discovered.push(thread_id);
                }
            }
            closing_guard.mark_threads(newly_discovered);
        }

        let mut active_descendants = Vec::new();
        for thread_id in known_thread_ids
            .into_iter()
            .filter(|thread_id| !publication_root_ids.contains(thread_id))
        {
            let status = self.get_status(thread_id).await;
            if persisted_open_descendants.contains(&thread_id) {
                active_descendants.push(format!("{thread_id} (persisted open)"));
            } else if !is_final(&status) {
                active_descendants.push(format!("{thread_id} ({status:?})"));
            }
        }
        if !active_descendants.is_empty() {
            active_descendants.sort();
            return Err(CodexErr::UnsupportedOperation(format!(
                "terminal completion is waiting for active child agents: {}",
                active_descendants.join(", ")
            )));
        }
        Ok(closing_guard)
    }

    pub(crate) fn start_typed_actor_heartbeat_watcher(
        &self,
        binding: AgentTaskBinding,
        thread_id: ThreadId,
    ) {
        if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str()) {
            warn!(
                %thread_id,
                agent_path = %binding.agent_path,
                attempt_id = %binding.attempt_id,
                "live typed agent binding points at a different thread"
            );
            return;
        }
        let control = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TYPED_ACTOR_HEARTBEAT_INTERVAL);
            loop {
                interval.tick().await;
                if !matches!(
                    control.get_status(thread_id).await,
                    AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
                ) {
                    break;
                }
                match control
                    .task_coordinator()
                    .heartbeat_typed_actor_binding(&binding)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        warn!(
                            %thread_id,
                            attempt_id = %binding.attempt_id,
                            "live typed actor heartbeat was rejected"
                        );
                        break;
                    }
                    Err(error) => warn!(
                        %thread_id,
                        attempt_id = %binding.attempt_id,
                        %error,
                        "live typed actor heartbeat failed"
                    ),
                }
            }
        });
    }

    pub(crate) async fn reconcile_live_typed_actor_heartbeats(
        &self,
    ) -> codex_agent_task_store::StoreResult<()> {
        for metadata in self.state.live_agents() {
            let (Some(agent_path), Some(thread_id)) = (metadata.agent_path, metadata.agent_id)
            else {
                continue;
            };
            let Some(binding) = self.task_coordinator().binding_for_agent_path(&agent_path) else {
                continue;
            };
            if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str())
                || !matches!(
                    self.get_status(thread_id).await,
                    AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
                )
            {
                continue;
            }
            match self
                .task_coordinator()
                .heartbeat_typed_actor_binding(&binding)
                .await
            {
                Ok(true) => {}
                Ok(false) => warn!(
                    %thread_id,
                    attempt_id = %binding.attempt_id,
                    "live typed actor heartbeat was rejected during reconciliation"
                ),
                Err(error) => warn!(
                    %thread_id,
                    attempt_id = %binding.attempt_id,
                    %error,
                    "live typed actor heartbeat failed during reconciliation"
                ),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_before_initial_submission_barrier(
        &self,
        barrier: Option<Arc<AgentControlTestBarrier>>,
    ) {
        *self
            .test_hooks
            .before_initial_submission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = barrier;
    }

    #[cfg(test)]
    pub(crate) fn set_after_terminal_tree_freeze_barrier(
        &self,
        barrier: Option<Arc<AgentControlTestBarrier>>,
    ) {
        *self
            .test_hooks
            .after_terminal_tree_freeze
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = barrier;
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let execution_guard = self
            .reserve_execution_capacity_for_turn_start(agent_id, /*starts_turn*/ true)
            .await?;
        #[cfg(test)]
        if execution_guard.is_some() {
            let barrier = self
                .test_hooks
                .after_execution_reservation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(barrier) = barrier {
                barrier.pause().await;
            }
        }
        self.send_input_after_capacity_check(agent_id, &state, input, execution_guard)
            .await
    }

    async fn send_input_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        input: Vec<UserInput>,
        execution_guard: Option<AgentExecutionGuard>,
    ) -> CodexResult<String> {
        let last_task_message = non_empty_task_message(render_input_preview(&input));
        let result = self
            .handle_thread_request_result(
                agent_id,
                state,
                state
                    .send_op_with_execution_guard(agent_id, input.into(), execution_guard)
                    .await,
            )
            .await;
        if result.is_ok() {
            match last_task_message {
                Some(last_task_message) => self
                    .state
                    .update_last_task_message(agent_id, last_task_message),
                None => self.state.clear_last_task_message(agent_id),
            }
        }
        result
    }

    pub(crate) async fn send_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let execution_guard = self
            .reserve_execution_capacity_for_turn_start(agent_id, communication.trigger_turn)
            .await?;
        self.send_inter_agent_communication_after_capacity_check(
            agent_id,
            &state,
            communication,
            agent_communication_context,
            execution_guard,
        )
        .await
    }

    pub(crate) async fn send_typed_child_completion_with_admission(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
    ) -> CodexResult<String> {
        if communication.trigger_turn {
            return Err(CodexErr::InvalidRequest(
                "typed child completion cannot trigger a parent turn".to_string(),
            ));
        }
        let state = self.upgrade()?;
        let last_task_message = last_task_message_from_communication(&communication);
        let communication_log_metadata = crate::agent_communication::logging_enabled()
            .then(|| crate::agent_communication::agent_communication_log_metadata(&communication));
        let result = self
            .handle_thread_request_result(
                agent_id,
                &state,
                state
                    .send_typed_child_completion_with_admission(agent_id, communication)
                    .await,
            )
            .await;
        if let (Some(metadata), Ok(communication_id)) =
            (communication_log_metadata, result.as_ref())
        {
            crate::agent_communication::emit_agent_communication_send(
                communication_id,
                &context,
                metadata,
                agent_id,
            );
        }
        if result.is_ok() {
            match last_task_message {
                Some(last_task_message) => self
                    .state
                    .update_last_task_message(agent_id, last_task_message),
                None => self.state.clear_last_task_message(agent_id),
            }
        }
        result
    }

    async fn send_inter_agent_communication_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        execution_guard: Option<AgentExecutionGuard>,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication(
            agent_id,
            state,
            communication,
            context,
            execution_guard,
        )
        .await
    }

    async fn submit_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        execution_guard: Option<AgentExecutionGuard>,
    ) -> CodexResult<String> {
        let last_task_message = last_task_message_from_communication(&communication);
        let communication_log_metadata = crate::agent_communication::logging_enabled()
            .then(|| crate::agent_communication::agent_communication_log_metadata(&communication));
        let result = self
            .handle_thread_request_result(
                agent_id,
                state,
                state
                    .send_op_with_execution_guard(
                        agent_id,
                        Op::InterAgentCommunication { communication },
                        execution_guard,
                    )
                    .await,
            )
            .await;
        if let (Some(metadata), Ok(communication_id)) =
            (communication_log_metadata, result.as_ref())
        {
            crate::agent_communication::emit_agent_communication_send(
                communication_id,
                &context,
                metadata,
                agent_id,
            );
        }
        if result.is_ok() {
            match last_task_message {
                Some(last_task_message) => self
                    .state
                    .update_last_task_message(agent_id, last_task_message),
                None => self.state.clear_last_task_message(agent_id),
            }
        }
        result
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        self.handle_thread_request_result(
            agent_id,
            &state,
            state.send_op(agent_id, Op::Interrupt).await,
        )
        .await
    }

    async fn handle_thread_request_result(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        result: CodexResult<String>,
    ) -> CodexResult<String> {
        if matches!(result, Err(CodexErr::InternalAgentDied)) {
            let _ = state.remove_thread(&agent_id).await;
            self.forget_v2_residency(agent_id);
            self.state.release_spawned_thread(agent_id);
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    pub(crate) fn register_session_root(
        &self,
        current_thread_id: ThreadId,
        current_parent_thread_id: Option<ThreadId>,
    ) {
        if current_parent_thread_id.is_none() {
            self.state.register_root_thread(current_thread_id);
        }
    }

    pub(crate) fn get_agent_metadata(&self, agent_id: ThreadId) -> Option<AgentMetadata> {
        self.state.agent_metadata_for_thread(agent_id)
    }

    pub(crate) fn ensure_agent_known(&self, agent_id: ThreadId) -> CodexResult<AgentMetadata> {
        self.state
            .agent_metadata_for_thread(agent_id)
            .ok_or(CodexErr::ThreadNotFound(agent_id))
    }

    pub(crate) async fn list_live_agent_subtree_thread_ids(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut thread_ids = vec![agent_id];
        thread_ids.extend(self.live_thread_spawn_descendants(agent_id).await?);
        Ok(thread_ids)
    }

    pub(crate) async fn get_agent_config_snapshot(
        &self,
        agent_id: ThreadId,
    ) -> Option<ThreadConfigSnapshot> {
        let Ok(state) = self.upgrade() else {
            return None;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return None;
        };
        Some(thread.config_snapshot().await)
    }

    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.state.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<watch::Receiver<AgentStatus>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.subscribe_status())
    }

    pub(crate) async fn format_environment_context_subagents(
        &self,
        parent_thread_id: ThreadId,
    ) -> String {
        let Ok(agents) = self.open_thread_spawn_children(parent_thread_id).await else {
            return String::new();
        };

        agents
            .into_iter()
            .map(|(thread_id, metadata)| {
                let reference = metadata
                    .agent_path
                    .as_ref()
                    .map(|agent_path| agent_path.name().to_string())
                    .unwrap_or_else(|| thread_id.to_string());
                format_subagent_context_line(reference.as_str(), metadata.agent_nickname.as_deref())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) async fn list_agents(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<Vec<ListedAgent>> {
        let state = self.upgrade()?;
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        let mut live_agents = self.state.live_agents();
        live_agents.sort_by(|left, right| {
            left.agent_path
                .as_deref()
                .unwrap_or_default()
                .cmp(right.agent_path.as_deref().unwrap_or_default())
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let root_path = AgentPath::root();
        let mut agents = Vec::with_capacity(live_agents.len().saturating_add(1));
        if resolved_prefix
            .as_ref()
            .is_none_or(|prefix| agent_matches_prefix(Some(&root_path), prefix))
            && let Some(root_thread_id) = self.state.agent_id_for_path(&root_path)
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            agents.push(ListedAgent {
                agent_name: root_path.to_string(),
                agent_status: root_thread.agent_status().await,
                last_task_message: Some(ROOT_LAST_TASK_MESSAGE.to_string()),
            });
        }

        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            if resolved_prefix
                .as_ref()
                .is_some_and(|prefix| !agent_matches_prefix(metadata.agent_path.as_ref(), prefix))
            {
                continue;
            }

            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let agent_name = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            let last_task_message = metadata.last_task_message.clone();
            agents.push(ListedAgent {
                agent_name,
                agent_status: thread.agent_status().await,
                last_task_message,
            });
        }

        Ok(agents)
    }

    /// Starts a detached watcher for sub-agents spawned from another thread.
    ///
    /// This is only enabled for `SubAgentSource::ThreadSpawn`, where a parent thread exists and
    /// can receive completion notifications.
    fn maybe_start_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
    ) {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let mut status = match control.subscribe_status(child_thread_id).await {
                Ok(mut status_rx) => {
                    let mut status = status_rx.borrow().clone();
                    while !is_final(&status) {
                        if status_rx.changed().await.is_err() {
                            status = control.get_status(child_thread_id).await;
                            break;
                        }
                        status = status_rx.borrow().clone();
                    }
                    status
                }
                Err(_) => control.get_status(child_thread_id).await,
            };
            if !is_final(&status) {
                return;
            }

            let state = control.upgrade().ok();
            let child_thread = match state.as_ref() {
                Some(state) => state.get_thread(child_thread_id).await.ok(),
                None => None,
            };
            if let Some(child_agent_path) = child_agent_path.as_ref() {
                let task_coordinator = control.task_coordinator();
                let binding = task_coordinator.binding_for_agent_path(child_agent_path);
                let assignment_id = binding.as_ref().map(|binding| binding.assignment_id);
                match match binding.as_ref() {
                    Some(binding) => task_coordinator
                        .seal_missing_receipt(
                            binding,
                        format!(
                            "typed agent {child_agent_path} finished with status {status:?} without submitting a receipt"
                        ),
                    )
                        .await,
                    None => Ok(None),
                }
                {
                    Ok(Some(receipt)) => {
                        if let Some(child_thread) = child_thread.as_ref() {
                            let session_telemetry = child_thread.session_telemetry();
                            task_coordinator
                                .maybe_emit_terminal_metrics(
                                    receipt.assignment_id,
                                    &session_telemetry,
                                )
                                .await;
                        } else {
                            warn!(
                                agent_path = %child_agent_path,
                                "could not emit missing-receipt typed-task metrics because the child thread was unavailable"
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        warn!(
                            agent_path = %child_agent_path,
                            %error,
                            "failed to seal missing typed-agent receipt"
                        );
                    }
                }
                if let Some(assignment_id) = assignment_id {
                    match task_coordinator.get_agent_task(assignment_id, None).await {
                        Ok(task) => {
                            if let Some(durable_status) = agent_status_from_task(&task) {
                                status = durable_status;
                            }
                        }
                        Err(error) => warn!(
                            %assignment_id,
                            %error,
                            "failed to load durable typed-agent outcome for completion watcher"
                        ),
                    }
                }
            }

            let Some(state) = state else {
                return;
            };
            let child_uses_multi_agent_v2 = match child_thread.as_ref() {
                Some(child_thread) => {
                    child_thread.multi_agent_version() == Some(MultiAgentVersion::V2)
                }
                None => true,
            };
            if child_agent_path.is_some() && child_uses_multi_agent_v2 {
                let Some(child_agent_path) = child_agent_path.clone() else {
                    return;
                };
                let Some(parent_agent_path) = child_agent_path
                    .as_str()
                    .rsplit_once('/')
                    .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
                else {
                    return;
                };
                let Some(message) = format_inter_agent_completion_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    &status,
                ) else {
                    return;
                };
                let communication = InterAgentCommunication::new(
                    child_agent_path,
                    parent_agent_path,
                    Vec::new(),
                    message,
                    /*trigger_turn*/ false,
                );
                let context =
                    AgentCommunicationContext::new(AgentCommunicationKind::Result, child_thread_id);
                let _ = control
                    .send_inter_agent_communication(parent_thread_id, communication, context)
                    .await;
                return;
            }
            let message = format_subagent_notification_message(child_reference.as_str(), &status);
            let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                return;
            };
            parent_thread
                .inject_user_message_without_turn(message)
                .await;
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_thread_spawn(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &Config,
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<(SessionSource, AgentMetadata)> {
        if depth == 1 {
            self.state.register_root_thread(parent_thread_id);
        }
        reservation.reserve_parent_thread(parent_thread_id)?;
        if let Some(agent_path) = agent_path.as_ref() {
            reservation.reserve_agent_path(agent_path)?;
        }
        let candidate_names = spawn::agent_nickname_candidates(config, agent_role.as_deref());
        let candidate_name_refs: Vec<&str> = candidate_names.iter().map(String::as_str).collect();
        let agent_nickname = Some(reservation.reserve_agent_nickname_with_preference(
            &candidate_name_refs,
            preferred_agent_nickname.as_deref(),
        )?);
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: agent_path.clone(),
            agent_nickname: agent_nickname.clone(),
            agent_role: agent_role.clone(),
        });
        let agent_metadata = AgentMetadata {
            agent_id: None,
            agent_path,
            agent_nickname,
            agent_role,
            last_task_message: None,
        };
        Ok((session_source, agent_metadata))
    }

    fn upgrade(&self) -> CodexResult<Arc<ThreadManagerState>> {
        self.manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))
    }

    async fn inherited_environments_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
    ) -> Option<TurnEnvironmentSnapshot> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        Some(
            parent_thread
                .codex
                .session
                .services
                .turn_environments
                .snapshot()
                .await,
        )
    }

    async fn inherited_exec_policy_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
        child_config: &Config,
    ) -> Option<Arc<crate::exec_policy::ExecPolicyManager>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        let parent_config = parent_thread.codex.session.get_config().await;
        if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, child_config) {
            return None;
        }

        Some(Arc::clone(
            &parent_thread.codex.session.services.exec_policy,
        ))
    }

    async fn open_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> CodexResult<Vec<(ThreadId, AgentMetadata)>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        Ok(children_by_parent
            .remove(&parent_thread_id)
            .unwrap_or_default())
    }

    async fn live_thread_spawn_children(
        &self,
    ) -> CodexResult<HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::<ThreadId, Vec<(ThreadId, AgentMetadata)>>::new();

        for (parent_thread_id, child_thread_id) in state.list_live_thread_spawn_edges().await {
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push((
                    child_thread_id,
                    self.state
                        .agent_metadata_for_thread(child_thread_id)
                        .unwrap_or(AgentMetadata {
                            agent_id: Some(child_thread_id),
                            ..Default::default()
                        }),
                ));
        }

        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| {
                left.1
                    .agent_path
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(right.1.agent_path.as_deref().unwrap_or_default())
                    .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
            });
        }

        Ok(children_by_parent)
    }

    async fn persist_thread_spawn_edge_for_source(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
    ) -> CodexResult<()> {
        let Some(parent_thread_id) = session_source.and_then(SessionSource::parent_thread_id)
        else {
            return Ok(());
        };
        if child_thread.config_snapshot().await.ephemeral {
            return Ok(());
        }
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };
        agent_graph_store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Open,
            )
            .await
            .map_err(|err| CodexErr::Fatal(format!("failed to persist thread-spawn edge: {err}")))
    }

    pub(crate) async fn close_persisted_thread_spawn_edge(
        &self,
        child_thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(());
        };
        agent_graph_store
            .set_thread_spawn_edge_status(
                child_thread_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed,
            )
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!(
                    "failed to close persisted thread-spawn edge for {child_thread_id}: {err}"
                ))
            })
    }

    async fn live_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        let mut descendants = Vec::new();
        let mut stack = children_by_parent
            .remove(&root_thread_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(child_thread_id, _)| child_thread_id)
            .rev()
            .collect::<Vec<_>>();

        while let Some(thread_id) = stack.pop() {
            descendants.push(thread_id);
            if let Some(children) = children_by_parent.remove(&thread_id) {
                for (child_thread_id, _) in children.into_iter().rev() {
                    stack.push(child_thread_id);
                }
            }
        }

        Ok(descendants)
    }

    /// Read persisted legacy topology without filtering traversal by edge status. Filtering the
    /// traversal itself would miss an open grandchild whose parent edge is already closed.
    async fn persisted_thread_spawn_readiness(
        &self,
        root_thread_ids: &[ThreadId],
    ) -> CodexResult<Vec<(ThreadId, PersistedThreadSpawnTopology)>> {
        let state = self.upgrade()?;
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return Ok(root_thread_ids
                .iter()
                .copied()
                .map(|thread_id| (thread_id, PersistedThreadSpawnTopology::default()))
                .collect());
        };

        let mut topologies = Vec::with_capacity(root_thread_ids.len());
        for root_thread_id in root_thread_ids {
            topologies.push((
                *root_thread_id,
                read_persisted_thread_spawn_topology(agent_graph_store.as_ref(), *root_thread_id)
                    .await?,
            ));
        }
        let mut confirmed_topologies = Vec::with_capacity(root_thread_ids.len());
        for root_thread_id in root_thread_ids {
            confirmed_topologies.push((
                *root_thread_id,
                read_persisted_thread_spawn_topology(agent_graph_store.as_ref(), *root_thread_id)
                    .await?,
            ));
        }
        if topologies != confirmed_topologies {
            return Err(CodexErr::Fatal(
                "persisted thread-spawn topology changed while terminal publication readiness was read"
                    .to_string(),
            ));
        }

        Ok(topologies)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PersistedThreadSpawnTopology {
    descendants: Vec<ThreadId>,
    open_descendants: HashSet<ThreadId>,
    edges: Vec<(
        ThreadId,
        ThreadId,
        codex_agent_graph_store::ThreadSpawnEdgeStatus,
    )>,
}

async fn read_persisted_thread_spawn_topology(
    agent_graph_store: &dyn codex_agent_graph_store::AgentGraphStore,
    root_thread_id: ThreadId,
) -> CodexResult<PersistedThreadSpawnTopology> {
    use codex_agent_graph_store::ThreadSpawnEdgeStatus;

    let mut descendants = agent_graph_store
        .list_thread_spawn_descendants(root_thread_id, None)
        .await
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to load persisted thread-spawn descendants for terminal publication: {err}"
            ))
        })?;
    let descendant_ids = descendants.iter().copied().collect::<HashSet<_>>();
    if descendant_ids.len() != descendants.len() || descendant_ids.contains(&root_thread_id) {
        return Err(CodexErr::Fatal(
            "persisted thread-spawn topology is ambiguous: descendant traversal returned duplicate or cyclic thread IDs"
                .to_string(),
        ));
    }

    let mut parent_thread_ids = Vec::with_capacity(descendants.len() + 1);
    parent_thread_ids.push(root_thread_id);
    parent_thread_ids.extend(descendants.iter().copied());
    let mut parent_by_child = HashMap::with_capacity(descendants.len());
    let mut open_descendants = HashSet::new();
    let mut edges = Vec::with_capacity(descendants.len());

    for parent_thread_id in parent_thread_ids {
        let all_children =
            read_persisted_thread_spawn_children(agent_graph_store, parent_thread_id, None).await?;
        let open_children = read_persisted_thread_spawn_children(
            agent_graph_store,
            parent_thread_id,
            Some(ThreadSpawnEdgeStatus::Open),
        )
        .await?;
        let closed_children = read_persisted_thread_spawn_children(
            agent_graph_store,
            parent_thread_id,
            Some(ThreadSpawnEdgeStatus::Closed),
        )
        .await?;

        let all_child_ids = unique_persisted_child_ids(parent_thread_id, "all", all_children)?;
        let open_child_ids = unique_persisted_child_ids(parent_thread_id, "open", open_children)?;
        let closed_child_ids =
            unique_persisted_child_ids(parent_thread_id, "closed", closed_children)?;
        if !open_child_ids.is_disjoint(&closed_child_ids)
            || open_child_ids
                .union(&closed_child_ids)
                .copied()
                .collect::<HashSet<_>>()
                != all_child_ids
        {
            return Err(CodexErr::Fatal(format!(
                "persisted thread-spawn topology is ambiguous: status projections disagree for parent {parent_thread_id}"
            )));
        }

        for child_thread_id in all_child_ids {
            if !descendant_ids.contains(&child_thread_id) {
                return Err(CodexErr::Fatal(format!(
                    "persisted thread-spawn topology is ambiguous: child {child_thread_id} of parent {parent_thread_id} was absent from descendant traversal"
                )));
            }
            if let Some(previous_parent) = parent_by_child.insert(child_thread_id, parent_thread_id)
            {
                return Err(CodexErr::Fatal(format!(
                    "persisted thread-spawn topology is ambiguous: child {child_thread_id} has parents {previous_parent} and {parent_thread_id}"
                )));
            }
            let status = if open_child_ids.contains(&child_thread_id) {
                open_descendants.insert(child_thread_id);
                ThreadSpawnEdgeStatus::Open
            } else {
                ThreadSpawnEdgeStatus::Closed
            };
            edges.push((parent_thread_id, child_thread_id, status));
        }
    }

    if parent_by_child.len() != descendant_ids.len() {
        return Err(CodexErr::Fatal(
            "persisted thread-spawn topology is ambiguous: descendant traversal and direct edges disagree"
                .to_string(),
        ));
    }
    descendants.sort_by_key(std::string::ToString::to_string);
    edges.sort_by_key(|(parent, child, status)| {
        (
            parent.to_string(),
            child.to_string(),
            matches!(status, ThreadSpawnEdgeStatus::Closed),
        )
    });
    Ok(PersistedThreadSpawnTopology {
        descendants,
        open_descendants,
        edges,
    })
}

async fn read_persisted_thread_spawn_children(
    agent_graph_store: &dyn codex_agent_graph_store::AgentGraphStore,
    parent_thread_id: ThreadId,
    status_filter: Option<codex_agent_graph_store::ThreadSpawnEdgeStatus>,
) -> CodexResult<Vec<ThreadId>> {
    agent_graph_store
        .list_thread_spawn_children(parent_thread_id, status_filter)
        .await
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "failed to load persisted thread-spawn children for terminal publication: {err}"
            ))
        })
}

fn unique_persisted_child_ids(
    parent_thread_id: ThreadId,
    projection: &str,
    child_thread_ids: Vec<ThreadId>,
) -> CodexResult<HashSet<ThreadId>> {
    let child_count = child_thread_ids.len();
    let child_ids = child_thread_ids.into_iter().collect::<HashSet<_>>();
    if child_ids.len() != child_count {
        return Err(CodexErr::Fatal(format!(
            "persisted thread-spawn topology is ambiguous: {projection} child projection for parent {parent_thread_id} returned duplicate thread IDs"
        )));
    }
    Ok(child_ids)
}

fn agent_matches_prefix(agent_path: Option<&AgentPath>, prefix: &AgentPath) -> bool {
    if prefix.is_root() {
        return true;
    }

    agent_path.is_some_and(|agent_path| {
        agent_path == prefix
            || agent_path
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

pub(crate) fn render_input_preview(input: &[UserInput]) -> String {
    input
        .iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Image { .. } => "[image]".to_string(),
            UserInput::LocalImage { path, .. } => {
                format!("[local_image:{}]", path.display())
            }
            UserInput::Skill { name, path, .. } => {
                format!("[skill:${name}]({})", path.display())
            }
            UserInput::Mention { name, path, .. } => format!("[mention:${name}]({path})"),
            _ => "[input]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn last_task_message_from_communication(communication: &InterAgentCommunication) -> Option<String> {
    if communication.encrypted_content.is_some() {
        return None;
    }
    non_empty_task_message(communication.content.clone())
}

fn non_empty_task_message(message: String) -> Option<String> {
    (!message.is_empty()).then_some(message)
}

fn thread_spawn_depth(session_source: &SessionSource) -> Option<i32> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => Some(*depth),
        _ => None,
    }
}
#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
