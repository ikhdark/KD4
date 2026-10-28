use crate::agent::AgentStatus;
use crate::agent::agent_status_from_task;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
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
use crate::session_prefix::format_subagent_context_line;
use crate::session_prefix::format_subagent_notification_message;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadManagerState;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
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
pub(crate) const ANCESTOR_PROGRESS_REFUSAL: &str = "Sub-agents report to ancestors only at the end. Finish with one final answer, including any blockers; the host delivers the terminal result automatically. This progress message was not queued.";

/// Whether `reference` can only mean an ancestor of `author`: a parent or
/// root alias (`root` and `..` are reserved agent names), or an absolute path
/// above it.
pub(crate) fn is_ancestor_reference(author: &AgentPath, reference: &str) -> bool {
    let reference = reference.trim();
    let path = reference.trim_end_matches('/');
    matches!(reference, "parent" | "root" | ".." | "/")
        || (!path.is_empty()
            && author
                .as_str()
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/')))
}
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
    pub(crate) runtime_timeout: Option<tokio::time::Duration>,
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
    pub(crate) parent_turn_id: Option<String>,
    pub(crate) environments: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) captured_environments: Option<TurnEnvironmentSnapshot>,
    pub(crate) typed_task_binding: Option<codex_agent_task_store::AgentTaskBindingDraft>,
    pub(crate) agent_job_binding: Option<AgentJobBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct LiveAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) metadata: AgentMetadata,
    pub(crate) status: AgentStatus,
    pub(crate) execution_started_at: tokio::time::Instant,
}

/// Model usage for one thread, separated by token class.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ThreadUsageTotals {
    pub(crate) coverage: Option<codex_protocol::protocol::TokenUsageCoverage>,
    pub(crate) input_tokens: u64,
    pub(crate) cached_input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) total_tokens: u64,
}

impl Default for ThreadUsageTotals {
    fn default() -> Self {
        Self {
            coverage: codex_protocol::protocol::TokenUsage::empty_ledger().coverage,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 0,
        }
    }
}

impl ThreadUsageTotals {
    fn accumulate(&mut self, usage: &codex_protocol::protocol::TokenUsage) {
        *self = self.checked_sum(Self {
            coverage: usage.coverage,
            input_tokens: usage.input_tokens.max(0) as u64,
            cached_input_tokens: usage.cached_input_tokens.max(0) as u64,
            output_tokens: usage.output_tokens.max(0) as u64,
            reasoning_output_tokens: usage.reasoning_output_tokens.max(0) as u64,
            total_tokens: usage.total_tokens.max(0) as u64,
        });
    }

    fn checked_sum(self, other: Self) -> Self {
        let coverage = self.coverage.zip(other.coverage).map(|(mut left, right)| {
            left.add_assign(&right);
            left.saturated |= [
                (self.input_tokens, other.input_tokens),
                (self.cached_input_tokens, other.cached_input_tokens),
                (self.output_tokens, other.output_tokens),
                (self.reasoning_output_tokens, other.reasoning_output_tokens),
                (self.total_tokens, other.total_tokens),
            ]
            .into_iter()
            .any(|(left, right)| left.checked_add(right).is_none());
            left
        });
        Self {
            coverage,
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(other.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_add(other.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }

    pub(crate) fn known_classes(self) -> [(&'static str, Option<u64>); 5] {
        let known = |value, reported| {
            self.coverage.and_then(|coverage| {
                (!coverage.saturated && reported == Some(coverage.receipts)).then_some(value)
            })
        };
        [
            (
                "input",
                known(self.input_tokens, self.coverage.map(|c| c.input_tokens)),
            ),
            (
                "cached_input",
                known(
                    self.cached_input_tokens,
                    self.coverage.map(|c| c.cached_input_tokens),
                ),
            ),
            (
                "output",
                known(self.output_tokens, self.coverage.map(|c| c.output_tokens)),
            ),
            (
                "reasoning_output",
                known(
                    self.reasoning_output_tokens,
                    self.coverage.map(|c| c.reasoning_output_tokens),
                ),
            ),
            (
                "total",
                known(self.total_tokens, self.coverage.map(|c| c.total_tokens)),
            ),
        ]
    }
}

/// Aggregate readable usage for an agent and the subtree it spawned.
///
/// Root and descendant totals stay separate so delegation overhead is attributable:
/// a parent whose own usage is small but whose subtree is large has pushed cost into
/// children rather than removed it.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct SubtreeUsage {
    pub(crate) root: ThreadUsageTotals,
    pub(crate) descendants: ThreadUsageTotals,
    pub(crate) descendant_threads: u32,
    /// Subtree members whose usage could not be read. Nonzero means `combined()` is a
    /// lower bound rather than a complete total.
    pub(crate) threads_missing_usage: u32,
    /// False unless membership includes every spawn mode, not just durable edges.
    /// Ephemeral descendants can disappear from both retained and live graphs.
    pub(crate) membership_complete: bool,
}

impl SubtreeUsage {
    /// Total parent-plus-child usage.
    pub(crate) fn combined(&self) -> ThreadUsageTotals {
        self.root.checked_sum(self.descendants)
    }

    /// Whether every subtree member reported usage.
    pub(crate) fn is_complete(&self) -> bool {
        self.membership_complete && self.threads_missing_usage == 0
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedAgent {
    pub(crate) agent_name: String,
    pub(crate) agent_status: AgentStatus,
    pub(crate) last_task_message: Option<String>,
    pub(crate) runtime_loaded: bool,
}





#[derive(Clone, Debug)]
enum V2AgentLoadCompletion {
    Loading,
    Succeeded,
    CapacityExceeded { max_threads: usize },
    ThreadNotFound(ThreadId),
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
    pub(crate) inherited_validation_context: Option<super::task_coordinator::ValidationTurnContext>,
}

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(manager: Weak<ThreadManagerState>) -> Self {
        Self {
            manager,
            ..Default::default()
        }
    }

    pub(crate) fn registry(&self) -> &Arc<AgentRegistry> {
        &self.state
    }

    pub(crate) fn model_http_clients(&self) -> Option<Arc<crate::client::SharedModelHttpClients>> {
        self.manager
            .upgrade()
            .map(|manager| Arc::clone(&manager.model_http_clients))
    }

    pub(crate) fn with_session_id(mut self, session_id: SessionId, max_threads: usize) -> Self {
        self.session_id = session_id;
        self.agent_execution_limiter.initialize(max_threads);
        self.task_coordinator
            .initialize_metric_capacity(max_threads);
        self
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn task_coordinator(&self) -> &AgentTaskCoordinator {
        &self.task_coordinator
    }

    /// Mirror child admission facts into the owning root rollout. The caller
    /// holds the shared budget's checkpoint guard through local and root writes.
    /// Root callers persist their own context and need no second checkpoint.
    pub(crate) async fn persist_root_validation_checkpoint(
        &self,
        turn: &crate::session::turn_context::TurnContext,
    ) -> Result<(), String> {
        if !turn.session_source.is_non_root_agent() {
            return Ok(());
        }
        let root_id = ThreadId::from_string(&turn.validation_root_turn.0)
            .map_err(|error| format!("invalid validation root thread: {error}"))?;
        let manager = self.upgrade().map_err(|error| error.to_string())?;
        let root = manager.get_thread(root_id).await
            .map_err(|error| format!("validation root is unavailable: {error}"))?;
        let session = &root.codex.session;
        // Serialize with turn installation, but never hold active_turn across
        // storage I/O: interruption and terminal cleanup must remain available.
        let _start_guard = session.task_start_gate.acquire().await
            .map_err(|_| "validation root turn admission is closed".to_string())?;
        let item = {
            let active = session.active_turn.lock().await;
            let root_turn = active.as_ref().and_then(|active| active.task.as_ref())
                .map(|task| &task.turn_context)
                .ok_or_else(|| "validation root turn is no longer active".to_string())?;
            if root_turn.validation_root_turn != turn.validation_root_turn
                || !Arc::ptr_eq(&root_turn.full_suite_budget, &turn.full_suite_budget)
            {
                return Err("validation belongs to a different root turn".to_string());
            }
            root_turn.to_turn_context_item()
        };
        session.persist_rollout_items_durable(&[RolloutItem::TurnContext(item)]).await
            .map_err(|error| format!("failed to checkpoint root validation admissions: {error}"))
    }

    /// Whether this tree has ever had a live sub-agent.
    ///
    /// The tool surface uses this rather than the registry's live-graph view, so
    /// a graph that empties when children finish cannot retract tool schemas
    /// that sit in the cached prompt prefix. See
    /// [`AgentRegistry::ever_had_live_agents`].
    pub(crate) fn ever_had_live_agents(&self) -> bool {
        self.state.ever_had_live_agents()
    }

    /// Heartbeats a bound typed agent for as long as this loaded runtime lives. A finished turn
    /// idles instead of exiting, so follow-up and correction turns stay covered; a resumed or
    /// reloaded runtime gets its own watcher.
    pub(crate) fn start_typed_actor_heartbeat_watcher(
        &self,
        agent_path: AgentPath,
        thread_id: ThreadId,
        thread: &Arc<crate::CodexThread>,
    ) {
        let control = self.clone();
        let runtime = Arc::downgrade(thread);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(TYPED_ACTOR_HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let status = match runtime.upgrade() {
                    Some(thread) if thread.is_running() => thread.agent_status().await,
                    _ => break,
                };
                let progress = match typed_actor_heartbeat(&status) {
                    TypedActorHeartbeat::Renew { progress } => progress,
                    TypedActorHeartbeat::Idle => continue,
                    TypedActorHeartbeat::Stop => break,
                };
                let Some(binding) = control
                    .task_coordinator()
                    .binding_for_agent_path(&agent_path)
                else {
                    warn!(%thread_id, %agent_path, "live typed agent has no durable binding to heartbeat");
                    continue;
                };
                if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str()) {
                    warn!(%thread_id, %agent_path, "live typed agent binding points at a different thread");
                    continue;
                }
                match control
                    .task_coordinator()
                    .heartbeat_typed_actor_binding(&binding, progress)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => warn!(
                        %thread_id,
                        attempt_id = %binding.attempt_id,
                        "live typed actor heartbeat was rejected"
                    ),
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

    /// Watches a bound agent runtime this control just resumed or reloaded. Reattachment to a
    /// runtime that was already running keeps that runtime's existing watcher.
    fn watch_loaded_typed_actor(
        &self,
        agent_path: Option<AgentPath>,
        loaded: &crate::thread_manager::NewThread,
    ) {
        if loaded.was_already_running {
            return;
        }
        if let Some(agent_path) = agent_path
            && self
                .task_coordinator()
                .binding_for_agent_path(&agent_path)
                .is_some()
        {
            self.start_typed_actor_heartbeat_watcher(agent_path, loaded.thread_id, &loaded.thread);
        }
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
            if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str()) {
                continue;
            }
            let TypedActorHeartbeat::Renew { progress } =
                typed_actor_heartbeat(&self.get_status(thread_id).await)
            else {
                continue;
            };
            match self
                .task_coordinator()
                .heartbeat_typed_actor_binding_if_due(&binding, progress)
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




    /// Wait for the actual child loop before retiring its accounting identity.
    pub(crate) async fn wait_for_agent_termination(&self, agent_id: ThreadId) -> CodexResult<()> {
        let state = self.upgrade()?;
        match state.get_thread(agent_id).await {
            Ok(thread) => thread.wait_until_terminated().await,
            Err(CodexErr::ThreadNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        Ok(())
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
        self.send_input_with_preview(agent_id, state, input, execution_guard, last_task_message).await
    }

    async fn send_input_with_preview(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        input: Vec<UserInput>,
        execution_guard: Option<AgentExecutionGuard>,
        last_task_message: Option<String>,
    ) -> CodexResult<String> {
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
        self.validate_inter_agent_communication(&communication, &agent_communication_context)?;
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

    pub(crate) fn validate_inter_agent_communication(
        &self,
        communication: &InterAgentCommunication,
        agent_communication_context: &AgentCommunicationContext,
    ) -> CodexResult<()> {
        // Only the host's terminal publication may report up the agent tree.
        // Queue-only progress still interrupts an active parent's sampling;
        // reject it before loading a target, reserving capacity, or enqueuing.
        let reports_to_ancestor = communication.author.as_str()
            .strip_prefix(communication.recipient.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'));
        if reports_to_ancestor && !agent_communication_context.is_terminal_result() {
            return Err(CodexErr::InvalidRequest(ANCESTOR_PROGRESS_REFUSAL.into()));
        }
        Ok(())
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

    pub(crate) fn retained_input_queue(
        &self,
        id: ThreadId,
    ) -> Option<Arc<crate::session::InputQueue>> {
        self.state.retained_input_queue(id)
    }

    pub(crate) fn can_queue_inter_agent_communication(&self, id: ThreadId) -> bool {
        self.state.input_queue_for_delivery(id).is_some()
    }

    pub(crate) fn remember_input_queue(
        &self,
        id: ThreadId,
        queue: Arc<crate::session::InputQueue>,
    ) {
        self.state.remember_input_queue(id, queue);
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
        self.submit_inter_agent_communication_with_preview(
            agent_id, state, communication, context, execution_guard, last_task_message,
        ).await
    }

    async fn submit_inter_agent_communication_with_preview(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        execution_guard: Option<AgentExecutionGuard>,
        last_task_message: Option<String>,
    ) -> CodexResult<String> {
        let communication_log_metadata = crate::agent_communication::logging_enabled()
            .then(|| crate::agent_communication::agent_communication_log_metadata(&communication));
        let turn_owner = communication.trigger_turn.then(|| context.followup_turn_owner()).flatten();
        let queue = (!communication.trigger_turn)
            .then(|| self.state.input_queue_for_delivery(agent_id))
            .flatten();
        let result = if let Some(queue) = queue {
            state
                .enqueue_queue_only_communication(agent_id, &queue, communication)
                .await
        } else {
            self.handle_thread_request_result(
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
            .await
        };
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
            // Publish ownership only for accepted work, without an intervening
            // await or rewriting immutable creation-turn provenance.
            if let Some((parent, turn)) = turn_owner {
                self.state.record_turn_followup_owner(agent_id, parent, turn);
            }
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
            state.send_op_with_instance(agent_id, Op::Interrupt).await,
        )
        .await
    }

    async fn handle_thread_request_result(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        request: CodexResult<(Arc<crate::codex_thread::CodexThread>, CodexResult<String>)>,
    ) -> CodexResult<String> {
        let (expected_thread, result) = request?;
        if matches!(result, Err(CodexErr::InternalAgentDied)) {
            let drained = tokio::time::timeout(
                residency::RESIDENCY_ACQUISITION_TIMEOUT,
                async {
                    expected_thread.request_shutdown().await?;
                    expected_thread.wait_until_drained().await
                },
            ).await;
            if !matches!(drained, Ok(Ok(()))) {
                warn!(%agent_id, "failed agent runtime is still retiring; preserving registration and residency");
                return result;
            }
            let retain_cold_identity =
                expected_thread.multi_agent_version() == Some(MultiAgentVersion::V2);
            state
                .remove_thread_if_same_with_cleanup(&agent_id, &expected_thread, || {
                    self.forget_v2_residency(agent_id);
                    // V2 runtime death is a cold-residency transition, not a logical close.
                    // A loader may already own the retained identity while rebuilding it.
                    if !retain_cold_identity {
                        self.state.release_spawned_thread(agent_id);
                    }
                })
                .await;
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        if let Ok(thread) = state.get_thread(agent_id).await {
            return thread.agent_status().await;
        }
        // Completed actors may be unloaded while their identity and durable receipt remain.
        let Some(path) = self
            .state
            .agent_metadata_for_thread(agent_id)
            .and_then(|metadata| metadata.agent_path)
        else {
            return AgentStatus::NotFound;
        };
        let coordinator = self.task_coordinator();
        let Some(binding) = coordinator
            .binding_for_agent_path(&path)
            .filter(|binding| binding.thread_id.as_deref() == Some(agent_id.to_string().as_str()))
        else {
            return AgentStatus::NotFound;
        };
        match coordinator
            .get_agent_task(binding.assignment_id, Some(0))
            .await
        {
            Ok(task) if task.current_attempt.attempt_id == binding.attempt_id => {
                agent_status_from_task(&task, None).unwrap_or(AgentStatus::NotFound)
            }
            _ => AgentStatus::NotFound,
        }
    }

    /// Unlike get_status, durable Active state never proves a loaded producer.
    pub(crate) async fn has_running_agent(&self, agent_id: ThreadId) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return false;
        };
        thread.is_running()
            && matches!(
                thread.agent_status().await,
                AgentStatus::PendingInit | AgentStatus::Running
            )
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

    pub(crate) fn get_agent_id_for_path(&self, path: &AgentPath) -> Option<ThreadId> {
        self.state.agent_id_for_path(path)
    }

    pub(crate) fn ensure_agent_known(&self, agent_id: ThreadId) -> CodexResult<AgentMetadata> {
        self.state
            .agent_metadata_for_thread(agent_id)
            .ok_or(CodexErr::ThreadNotFound(agent_id))
    }

    /// Tool-facing admission includes retained closed children, but never another root's UUID.
    ///
    /// Once the tree root is registered, an ID that names no live or stored thread
    /// returns `ThreadNotFound`: it cannot belong to another tree, so tools keep their
    /// not-found result for it.
    pub(crate) async fn ensure_agent_in_tree(&self, agent_id: ThreadId) -> CodexResult<()> {
        if self.state.agent_metadata_for_thread(agent_id).is_some() {
            return Ok(());
        }
        let denied = || {
            CodexErr::UnsupportedOperation(format!(
                "agent {agent_id} is not part of this session tree"
            ))
        };
        let root = self
            .state
            .agent_id_for_path(&AgentPath::root())
            .ok_or_else(denied)?;
        let state = self.upgrade()?;
        let mut current = agent_id;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            if current == root || self.state.agent_metadata_for_thread(current).is_some() {
                return Ok(());
            }
            let stored = match state
                .read_stored_thread(ReadThreadParams {
                    thread_id: current,
                    include_archived: true,
                    include_history: false,
                })
                .await
            {
                Ok(stored) => stored,
                // A live but unstored thread has no verifiable lineage.
                Err(CodexErr::ThreadNotFound(_)) if current == agent_id => {
                    return Err(match state.get_thread(agent_id).await {
                        Ok(_) => denied(),
                        Err(_) => CodexErr::ThreadNotFound(agent_id),
                    });
                }
                Err(_) => return Err(denied()),
            };
            let parent = stored.parent_thread_id.ok_or_else(denied)?;
            if stored.source.parent_thread_id() != Some(parent) {
                return Err(denied());
            }
            current = parent;
        }
        Err(denied())
    }

    pub(crate) async fn list_live_agent_subtree_thread_ids(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut thread_ids = vec![agent_id];
        thread_ids.extend(self.live_thread_spawn_descendants(agent_id).await?);
        Ok(thread_ids)
    }

    pub(crate) async fn list_turn_owned_agent_subtree(
        &self,
        parent: ThreadId,
        turn_id: &str,
    ) -> CodexResult<Vec<ThreadId>> {
        let state = self.upgrade()?;
        let ephemeral_owners = self.state.turn_spawn_owners();
        let mut roots = ephemeral_owners.iter().filter_map(|(child, (owner, turn))| {
                (*owner == parent && turn == turn_id).then_some(*child)
            }).collect::<HashSet<_>>();
        let mut parents = self.state.known_spawn_edges().into_iter()
            .map(|(parent, child)| (child, parent)).collect::<HashMap<_, _>>();
        parents.extend(state.list_live_thread_spawn_edges().await.into_iter()
            .map(|(parent, child)| (child, parent)));
        let mut edges = parents.into_iter().map(|(child, parent)| (parent, child)).collect();
        let mut durable_allowed = None;
        if let Some(graph) = state.agent_graph_store() {
            let allowed = graph.list_turn_owned_thread_spawn_descendants(parent, turn_id, ephemeral_owners.clone()).await
                .map_err(|error| CodexErr::Fatal(format!("failed to read turn-owned children: {error}")))?
                .into_iter().collect::<HashSet<_>>();
            roots.extend(allowed.iter().copied());
            durable_allowed = Some(allowed);
            edges = graph.list_reconciled_thread_spawn_edges(parent, edges).await
                .map_err(|error| CodexErr::Fatal(format!("failed to reconcile turn-owned children: {error}")))?;
        }
        let mut children = HashMap::<ThreadId, Vec<ThreadId>>::new();
        for (owner, child) in edges {
            if durable_allowed.as_ref().is_some_and(|allowed| {
                !allowed.contains(&child) && !ephemeral_owners.contains_key(&child)
            }) {
                continue;
            }
            if owner != parent || roots.contains(&child) {
                children.entry(owner).or_default().push(child);
            }
        }
        let mut seen = HashSet::from([parent]);
        let mut pending = vec![parent];
        let mut result = Vec::new();
        while let Some(owner) = pending.pop() {
            for child in children.remove(&owner).unwrap_or_default() {
                if seen.insert(child) {
                    result.push(child);
                    pending.push(child);
                }
            }
        }
        Ok(result)
    }

    /// Sums readable model usage across `agent_id` and its known descendants.
    ///
    /// Per-thread accounting alone makes delegated work look free: a parent that spawns
    /// five children reports only its own tokens. This aggregates the subtree so parent
    /// and child cost can be compared against doing the same work locally.
    ///
    /// Retained spawn edges keep unloaded children in the membership count. Their
    /// unavailable session usage makes the totals a lower bound. Even with a
    /// retained graph, ephemeral children are not persisted, so complete membership
    /// cannot currently be certified.
    pub(crate) async fn aggregate_live_subtree_usage(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<SubtreeUsage> {
        self.aggregate_live_subtree_usage_from_snapshot(agent_id, &HashMap::new())
            .await
    }

    async fn aggregate_live_subtree_usage_from_snapshot(
        &self,
        agent_id: ThreadId,
        captured_threads: &HashMap<ThreadId, Arc<crate::CodexThread>>,
    ) -> CodexResult<SubtreeUsage> {
        let state = self.upgrade()?;
        let mut usage = SubtreeUsage::default();
        let thread_ids = state
            .reconciled_agent_subtree(agent_id, self.state.known_spawn_edges())
            .await?;
        for thread_id in thread_ids {
            // Membership is structural; a member without readable usage is still
            // a descendant and is reported separately as missing usage.
            if thread_id != agent_id {
                usage.descendant_threads = usage.descendant_threads.saturating_add(1);
            }
            let thread = match captured_threads.get(&thread_id) {
                Some(thread) => Some(Arc::clone(thread)),
                None => state.get_thread(thread_id).await.ok(),
            };
            let Some(thread) = thread else {
                usage.threads_missing_usage = usage.threads_missing_usage.saturating_add(1);
                continue;
            };
            let Some(info) = thread.token_usage_info().await else {
                usage.threads_missing_usage = usage.threads_missing_usage.saturating_add(1);
                continue;
            };
            let total = &info.total_token_usage;
            let target = if thread_id == agent_id {
                &mut usage.root
            } else {
                &mut usage.descendants
            };
            target.accumulate(total);
        }
        Ok(usage)
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

    /// Runtime grants remain owned by the live root, not copied into child config.
    pub(crate) async fn root_session_for_permissions(
        &self,
        caller_thread_id: ThreadId,
    ) -> Option<Arc<crate::session::session::Session>> {
        let root_thread_id = ThreadId::from(self.session_id);
        if root_thread_id == caller_thread_id {
            return None;
        }
        let root = self.upgrade().ok()?.get_thread(root_thread_id).await.ok()?;
        Some(Arc::clone(&root.codex.session))
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
        if let Some(binding) = self.task_coordinator().binding_for_agent_path(&agent_path)
            && let Some(thread_id) = binding
                .thread_id
                .as_deref()
                .and_then(|id| ThreadId::from_string(id).ok())
        {
            self.rehydrate_bound_agent(thread_id).await?;
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }

    /// Rebuild the open graph's identities without opening any descendant runtime.
    pub(crate) async fn restore_cold_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let Some(graph) = state.agent_graph_store() else {
            return Ok(());
        };
        let root_path = self
            .get_agent_metadata(root_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .unwrap_or_else(AgentPath::root);
        let mut queue = VecDeque::from([(root_thread_id, root_path)]);
        let mut seen = HashSet::from([root_thread_id]);
        while let Some((parent, parent_path)) = queue.pop_front() {
            let children = graph
                .list_thread_spawn_children(
                    parent,
                    Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Open),
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            for child in children {
                if !seen.insert(child) {
                    return Err(CodexErr::InvalidRequest(format!(
                        "repeated cold agent edge to {child}"
                    )));
                }
                // A just-published live child's rollout may not yet be flushed.
                // Its registered runtime is authoritative; do not read it back as cold.
                if let Ok(thread) = state.get_thread(child).await
                    && let Some(metadata) = self.get_agent_metadata(child)
                    && let Some(path) = metadata.agent_path
                {
                    let snapshot = thread.config_snapshot().await;
                    if snapshot.parent_thread_id != Some(parent)
                        || snapshot.session_source.parent_thread_id() != Some(parent)
                        || path.as_str().rsplit_once('/').map(|(prefix, _)| prefix)
                            != Some(parent_path.as_str())
                    {
                        return Err(CodexErr::InvalidRequest(format!(
                            "live agent `{path}` has inconsistent lineage"
                        )));
                    }
                    self.task_coordinator().record_unavailable_descendant(child, false);
                    queue.push_back((child, path));
                    continue;
                }
                let stored = match state
                    .read_stored_thread(ReadThreadParams {
                        thread_id: child,
                        include_archived: false,
                        include_history: false,
                    })
                    .await
                {
                    Ok(stored) => stored,
                    Err(CodexErr::ThreadNotFound(_)) => {
                        // Keep the graph edge and obligation; do not resurrect an
                        // archived worker or block healthy sibling restoration.
                        self.task_coordinator().record_unavailable_descendant(child, true);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                self.task_coordinator().record_unavailable_descendant(child, false);
                let path = stored.source.get_agent_path().ok_or_else(|| {
                    CodexErr::InvalidRequest(format!(
                        "retained agent {child} has no canonical path"
                    ))
                })?;
                if stored.parent_thread_id != Some(parent)
                    || stored.source.parent_thread_id() != Some(parent)
                    || path.as_str().rsplit_once('/').map(|(prefix, _)| prefix)
                        != Some(parent_path.as_str())
                {
                    return Err(CodexErr::InvalidRequest(format!(
                        "retained agent `{path}` has inconsistent lineage"
                    )));
                }
                if let Some(binding) = self.task_coordinator().binding_for_agent_path(&path) {
                    if binding.thread_id.as_deref() != Some(child.to_string().as_str()) {
                        return Err(CodexErr::InvalidRequest(format!(
                            "retained agent `{path}` has a conflicting binding"
                        )));
                    }
                    self.rehydrate_binding_with_stored(binding, Some(stored)).await?;
                } else {
                    self.state.restore_cold_agent(
                        AgentMetadata {
                            agent_id: Some(child),
                            agent_path: Some(path.clone()),
                            agent_nickname: stored.agent_nickname,
                            agent_role: stored.agent_role,
                            last_task_message: None,
                        },
                        parent,
                    )?;
                }
                queue.push_back((child, path));
            }
        }
        Ok(())
    }

    pub(crate) async fn rehydrate_bound_agent(&self, thread_id: ThreadId) -> CodexResult<()> {
        if self.state.agent_metadata_for_thread(thread_id).is_some() {
            return Ok(());
        }
        let binding = self
            .task_coordinator()
            .bindings_for_recovery()
            .into_iter()
            .find(|binding| binding.thread_id.as_deref() == Some(thread_id.to_string().as_str()))
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        self.rehydrate_binding(binding).await
    }

    async fn rehydrate_binding(
        &self,
        binding: codex_agent_task_store::AgentTaskBinding,
    ) -> CodexResult<()> {
        self.rehydrate_binding_with_stored(binding, None).await
    }

    // Only cold-tree traversal supplies a stored observation: it has already
    // checked this child's direct lineage. Authorization and edge status below
    // remain fresh; a traversal's earlier open-child list is not authority here.
    async fn rehydrate_binding_with_stored(
        &self,
        binding: codex_agent_task_store::AgentTaskBinding,
        validated_stored: Option<codex_thread_store::StoredThread>,
    ) -> CodexResult<()> {
        let thread_id = binding
            .thread_id
            .as_deref()
            .and_then(|id| ThreadId::from_string(id).ok())
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "retained agent `{}` has no valid thread id",
                    binding.agent_path
                ))
            })?;
        if self.state.agent_metadata_for_thread(thread_id).is_some() {
            return Ok(());
        }
        if let Some(stored) = validated_stored.as_ref() {
            let parent = stored.parent_thread_id.ok_or(CodexErr::ThreadNotFound(thread_id))?;
            if stored.thread_id != thread_id || stored.source.parent_thread_id() != Some(parent) {
                return Err(CodexErr::ThreadNotFound(thread_id));
            }
            self.ensure_agent_in_tree(parent).await?;
        } else {
            self.ensure_agent_in_tree(thread_id).await?;
        }
        let coordinator = self.task_coordinator();
        if coordinator.root_session_id().as_deref() != Some(binding.root_session_id.as_str()) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let current = coordinator
            .get_agent_task_authorization(binding.assignment_id)
            .await
            .map_err(|error| {
                if matches!(&error, codex_agent_task_store::StoreError::Json(_)
                    | codex_agent_task_store::StoreError::CorruptData(_)
                    | codex_agent_task_store::StoreError::Io(_)
                    | codex_agent_task_store::StoreError::AssignmentNotFound(_)
                    | codex_agent_task_store::StoreError::AttemptNotFound(_))
                {
                    tracing::warn!(assignment_id = %binding.assignment_id, %error, "retained task identity is unavailable");
                    CodexErr::UnsupportedOperation(format!("assignment {} is unavailable; its obligation remains unresolved", binding.assignment_id))
                } else {
                    CodexErr::Fatal(error.to_string())
                }
            })?;
        if current.current_attempt.attempt_id != binding.attempt_id {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let state = self.upgrade()?;
        let stored = match validated_stored {
            Some(stored) => stored,
            None => state
                .read_stored_thread(codex_thread_store::ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history: false,
                })
                .await?,
        };
        let parent = stored
            .parent_thread_id
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        let path = stored
            .source
            .get_agent_path()
            .ok_or(CodexErr::ThreadNotFound(thread_id))?;
        if path.as_str() != binding.agent_path || stored.source.parent_thread_id() != Some(parent) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        // Like cold-tree restore, a cold agent's membership comes only from a durable
        // open spawn edge. Without the graph no closure is recorded durably, so only a
        // runtime that is still loaded shows the agent was not closed: close unloads it.
        let open = match state.agent_graph_store() {
            Some(graph) => graph
                .has_thread_spawn_child(
                    parent,
                    thread_id,
                    codex_agent_graph_store::ThreadSpawnEdgeStatus::Open,
                )
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?,
            None => state.get_thread(thread_id).await.is_ok(),
        };
        if !open {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        if coordinator
            .binding_for_assignment(binding.assignment_id)
            .as_ref()
            != Some(&binding)
        {
            return Err(CodexErr::InvalidRequest(format!(
                "retained agent `{path}` binding changed during recovery"
            )));
        }
        self.state.restore_cold_agent(
            AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(path),
                agent_nickname: stored.agent_nickname,
                agent_role: stored.agent_role,
                last_task_message: None,
            },
            parent,
        )
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) fn subscribe_capacity_changes(&self) -> watch::Receiver<u64> {
        self.state.subscribe_capacity_changes()
    }

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
        self.list_agents_with_task_snapshots(current_session_source, path_prefix)
            .await.map(|(agents, _)| agents)
    }

    pub(crate) async fn list_agents_with_task_snapshots(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<(Vec<ListedAgent>, HashMap<codex_agent_task_store::AssignmentId, codex_agent_task_store::AgentTask>)> {
        let state = self.upgrade()?;
        let mut task_snapshots = HashMap::new();
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        // Hydrate identities only. No child runtime, bootstrap input or model call is started.
        let mut bindings = self.task_coordinator().bindings_for_recovery();
        bindings.sort_by(|left, right| left.agent_path.cmp(&right.agent_path));
        for binding in bindings {
            // An unstarted launch has no thread yet; its in-flight spawn owns the path.
            if binding.thread_id.is_none() {
                continue;
            }
            let path = AgentPath::try_from(binding.agent_path.as_str())
                .map_err(CodexErr::InvalidRequest)?;
            if resolved_prefix
                .as_ref()
                .is_some_and(|prefix| !agent_matches_prefix(Some(&path), prefix))
            {
                continue;
            }
            match self.rehydrate_binding(binding).await {
                Ok(()) => {}
                // Closed, superseded, archived, deleted or closing agents are not open
                // members. Omit them instead of failing the listing for every agent.
                Err(CodexErr::ThreadNotFound(_) | CodexErr::UnsupportedOperation(_)) => {}
                Err(error) => {
                    return Err(CodexErr::InvalidRequest(format!(
                        "retained agent `{path}` is unavailable: {error}"
                    )));
                }
            }
        }
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
                runtime_loaded: true,
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

            let runtime = state.get_thread(thread_id).await.ok();
            let runtime_loaded = runtime.is_some();
            let status = if let Some(thread) = runtime {
                thread.agent_status().await
            } else if let Some(binding) = metadata.agent_path.as_ref()
                .and_then(|path| self.task_coordinator().binding_for_agent_path(path))
                .filter(|binding| binding.thread_id.as_deref() == Some(thread_id.to_string().as_str()))
            {
                match self.task_coordinator().get_agent_task(binding.assignment_id, Some(0)).await {
                    Ok(task) if task.current_attempt.attempt_id == binding.attempt_id => {
                        let status = agent_status_from_task(&task, None).unwrap_or(AgentStatus::NotFound);
                        task_snapshots.insert(binding.assignment_id, task);
                        status
                    }
                    _ => AgentStatus::NotFound,
                }
            } else {
                AgentStatus::NotFound
            };
            let agent_name = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            let last_task_message = metadata.last_task_message.clone();
            agents.push(ListedAgent {
                agent_name,
                agent_status: status,
                runtime_loaded,
                last_task_message,
            });
        }

        Ok((agents, task_snapshots))
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
    ) -> Option<tokio::task::JoinHandle<()>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };
        let control = self.clone();
        let watched_binding = child_agent_path
            .as_ref()
            .and_then(|path| self.task_coordinator().binding_for_agent_path(path));
        Some(tokio::spawn(async move {
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
            let usage_threads = match state.as_ref() {
                Some(state) => state
                    .loaded_agent_subtree_snapshot(
                        child_thread_id,
                        control.state.known_spawn_edges(),
                    )
                    .await
                    .unwrap_or_else(|error| {
                        warn!(%child_thread_id, %error, "failed to retain delegation usage owners");
                        HashMap::new()
                    }),
                None => HashMap::new(),
            };
            // Keep accounting owned by this watcher while publishing the result
            // independently. Both branches finish before the watcher retires.
            let accounting_state = state.clone();
            let accounting_barrier = child_thread
                .as_ref()
                .map(|thread| thread.codex.session.terminal_tasks.token());
            let accounting = async {
                // Publication can also release the manager's last external
                // owner. Keep graph services alive through metrics emission.
                let _state_owner = accounting_state;
                let _terminal_owner = accounting_barrier;
                // Record aggregate cost for this delegation while its subtree is still
                // readable. Per-thread accounting alone would report the child's tokens
                // without the descendants it spawned, making deep delegation look cheap.
                if let Some(child_thread) = child_thread.as_ref() {
                    match control
                        .aggregate_live_subtree_usage_from_snapshot(child_thread_id, &usage_threads)
                        .await
                    {
                        Ok(usage) => {
                            crate::agent::task_metrics::emit_subtree_usage(
                                &child_thread.session_telemetry(),
                                &usage,
                            );
                        }
                        Err(error) => warn!(
                            child_thread_id = %child_thread_id,
                            %error,
                            "failed to aggregate delegation subtree usage"
                        ),
                    }
                }
            };
            let notification = async {
                let mut protected_host_summary = None;
                if let (Some(child_agent_path), Some(binding)) =
                    (child_agent_path.as_ref(), watched_binding.as_ref())
                {
                    let task_coordinator = control.task_coordinator();
                    let assignment_id = Some(binding.assignment_id);
                    match task_coordinator
                        .seal_missing_receipt_for_attempt(
                            child_agent_path,
                            child_thread_id,
                            &status,
                            binding.attempt_id,
                        )
                        .await
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
                                if watched_binding.as_ref().is_some_and(|binding| {
                                    binding.attempt_id != task.current_attempt.attempt_id
                                }) {
                                    return;
                                }
                                protected_host_summary =
                                    crate::agent::status::completion_diagnostics_from_task(&task);
                                if let Some(durable_status) = agent_status_from_task(
                                    &task,
                                    Some((binding.attempt_id, &status)),
                                ) {
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
                    let receipt = watched_binding.as_ref().map(|binding| format!(
                    "Receipt: get_agent_task({{\"assignment_id\":\"{}\"}})\nProducer attempt: {}",
                    binding.assignment_id, binding.attempt_id,
                ));
                    let Some(communication) =
                    crate::session_prefix::inter_agent_completion_communication_with_diagnostics(
                        parent_agent_path,
                        child_agent_path,
                        child_thread_id,
                        &status,
                        receipt.as_deref(),
                        protected_host_summary.as_deref(),
                    )
                else {
                    return;
                };
                    let context = AgentCommunicationContext::new(
                        AgentCommunicationKind::Result,
                        child_thread_id,
                    );
                    let _ = control
                        .send_inter_agent_communication(parent_thread_id, communication, context)
                        .await;
                    return;
                }
                let message =
                    format_subagent_notification_message(child_reference.as_str(), &status);
                let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
                    return;
                };
                parent_thread
                    .inject_user_message_without_turn(message)
                    .await;
            };
            tokio::join!(notification, accounting);
        }))
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
        let candidate_names = if preferred_agent_nickname.is_some() {
            Vec::new()
        } else {
            spawn::agent_nickname_candidates(config, agent_role.as_deref())
        };
        let agent_nickname = Some(reservation.reserve_agent_nickname_with_preference(
            &candidate_names,
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
        let mut children_by_parent = self
            .live_thread_spawn_children(Some(parent_thread_id))
            .await?;
        Ok(children_by_parent
            .remove(&parent_thread_id)
            .unwrap_or_default())
    }

    async fn live_thread_spawn_children(
        &self,
        requested_parent: Option<ThreadId>,
    ) -> CodexResult<HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::<ThreadId, Vec<(ThreadId, AgentMetadata)>>::new();

        let mut edges = state
            .list_live_thread_spawn_edges()
            .await
            .into_iter()
            .collect::<HashSet<_>>();
        edges.extend(self.state.known_spawn_edges());
        let edges = edges
            .into_iter()
            .filter(|(parent, _)| requested_parent.is_none_or(|requested| requested == *parent))
            .collect::<Vec<_>>();
        let child_ids = edges.iter().map(|(_, child)| *child).collect();
        let mut metadata = self.state.agent_metadata_for_threads(&child_ids);
        for (parent_thread_id, child_thread_id) in edges {
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push((
                    child_thread_id,
                    metadata.remove(&child_thread_id).unwrap_or(AgentMetadata {
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

    async fn live_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        Ok(self
            .upgrade()?
            .reconciled_agent_subtree(root_thread_id, self.state.known_spawn_edges())
            .await?
            .into_iter()
            .skip(1)
            .collect())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TypedActorHeartbeat {
    Renew { progress: bool },
    Idle,
    Stop,
}

/// A running turn is working even inside one long model call or command, so it defers
/// nonproductive recovery; pending and interrupted agents only stay live. A finished turn must not
/// renew a settled actor, but a follow-up turn may still start on the same runtime.
fn typed_actor_heartbeat(status: &AgentStatus) -> TypedActorHeartbeat {
    match status {
        AgentStatus::Running => TypedActorHeartbeat::Renew { progress: true },
        AgentStatus::PendingInit | AgentStatus::Interrupted => {
            TypedActorHeartbeat::Renew { progress: false }
        }
        AgentStatus::Completed(_)
        | AgentStatus::CompletedWithSurface { .. }
        | AgentStatus::Stopped { .. }
        | AgentStatus::Errored(_) => TypedActorHeartbeat::Idle,
        AgentStatus::Shutdown | AgentStatus::NotFound => TypedActorHeartbeat::Stop,
    }
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
    use std::fmt::Write as _;
    let mut preview = super::registry::TaskMessagePreview::default();
    for (index, item) in input.iter().enumerate() {
        if index != 0 && preview.write_str("\n").is_err() {
            break;
        }
        let result = match item {
            UserInput::Text { text, .. } => preview.write_str(text),
            UserInput::Image { .. } => preview.write_str("[image]"),
            UserInput::LocalImage { path, .. } => write!(preview, "[local_image:{}]", path.display()),
            UserInput::Skill { name, path, .. } => write!(preview, "[skill:${name}]({})", path.display()),
            UserInput::Mention { name, path, .. } => write!(preview, "[mention:${name}]({path})"),
            _ => preview.write_str("[input]"),
        };
        if result.is_err() {
            break;
        }
    }
    preview.finish()
}

fn last_task_message_from_communication(communication: &InterAgentCommunication) -> Option<String> {
    if communication.encrypted_content.is_some() {
        return None;
    }
    non_empty_task_message(super::registry::bounded_task_message_from_text(&communication.content))
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
