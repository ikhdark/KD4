mod compact;
#[path = "completion_review_v2.rs"]
pub(crate) mod completion_review;
mod lifecycle;
mod regular;
mod review;
mod user_shell;

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::WorkspaceFinalizationFence;
use codex_extension_api::ExtensionData;
use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::Span;
use tracing::field;
use tracing::info_span;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::hook_runtime::inspect_pending_input;
use crate::hook_runtime::record_additional_contexts;
use crate::hook_runtime::record_pending_input;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::state::TurnState;
use crate::state::TurnTerminalCoordinator;
use crate::state::TurnTerminalPermit;
use codex_analytics::TurnProfileFact;
use codex_analytics::TurnTokenUsageFact;
use codex_login::AuthManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_E2E_DURATION_METRIC;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_otel::TURN_TOKEN_USAGE_METRIC;
use codex_otel::TURN_TOOL_CALL_METRIC;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::WarningEvent;

use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
pub(crate) use compact::CompactTask;
pub(crate) use regular::RegularTask;
pub(crate) use review::ReviewTask;
pub(crate) use user_shell::UserShellCommandMode;
pub(crate) use user_shell::UserShellCommandTask;
pub(crate) use user_shell::execute_user_shell_command;

const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TASK_COMPACT_METRIC: &str = "codex.task.compact";
const WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON: &str =
    "the workspace finalization fence could not be sealed for terminal dispatch";

pub(crate) type SessionTaskResult = CodexResult<Option<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptedTurnHistoryMarker {
    Disabled,
    ContextualUser,
    Developer,
}

impl InterruptedTurnHistoryMarker {
    pub(crate) fn from_config_and_version(
        config: &Config,
        multi_agent_version: MultiAgentVersion,
    ) -> Self {
        if !config.agent_interrupt_message_enabled {
            return Self::Disabled;
        }
        if multi_agent_version == MultiAgentVersion::V2 {
            Self::Developer
        } else {
            Self::ContextualUser
        }
    }
}

/// Shared model-visible marker used by both the real interrupt path and
/// interrupted fork snapshots.
pub(crate) fn interrupted_turn_history_marker(
    marker: InterruptedTurnHistoryMarker,
) -> Option<ResponseItem> {
    match marker {
        InterruptedTurnHistoryMarker::Disabled => None,
        InterruptedTurnHistoryMarker::ContextualUser => Some(ContextualUserFragment::into(
            crate::context::TurnAborted::new(crate::context::TurnAborted::INTERRUPTED_GUIDANCE),
        )),
        InterruptedTurnHistoryMarker::Developer => {
            let marker = crate::context::TurnAborted::new(
                crate::context::TurnAborted::INTERRUPTED_DEVELOPER_GUIDANCE,
            );
            Some(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: marker.render(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            })
        }
    }
}

fn emit_turn_network_proxy_metric(
    session_telemetry: &SessionTelemetry,
    network_proxy_active: bool,
    tmp_mem: (&str, &str),
) {
    let active = if network_proxy_active {
        "true"
    } else {
        "false"
    };
    session_telemetry.counter(
        TURN_NETWORK_PROXY_METRIC,
        /*inc*/ 1,
        &[("active", active), tmp_mem],
    );
}

fn emit_turn_memory_metric(
    session_telemetry: &SessionTelemetry,
    feature_enabled: bool,
    config_enabled: bool,
    has_citations: bool,
) {
    let read_allowed = feature_enabled && config_enabled;
    session_telemetry.counter(
        TURN_MEMORY_METRIC,
        /*inc*/ 1,
        &[
            ("read_allowed", bool_tag(read_allowed)),
            ("feature_enabled", bool_tag(feature_enabled)),
            ("config_use_memories", bool_tag(config_enabled)),
            ("has_citations", bool_tag(has_citations)),
        ],
    );
}

pub(crate) fn emit_compact_metric(
    session_telemetry: &SessionTelemetry,
    compact_type: &'static str,
    manual: bool,
) {
    session_telemetry.counter(
        TASK_COMPACT_METRIC,
        /*inc*/ 1,
        &[("type", compact_type), ("manual", bool_tag(manual))],
    );
}

fn bool_tag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Thin wrapper that exposes the parts of [`Session`] task runners need.
#[derive(Clone)]
pub(crate) struct SessionTaskContext {
    session: Arc<Session>,
    turn_extension_data: Arc<ExtensionData>,
}

impl SessionTaskContext {
    pub(crate) fn new(session: Arc<Session>, turn_extension_data: Arc<ExtensionData>) -> Self {
        Self {
            session,
            turn_extension_data,
        }
    }

    pub(crate) fn clone_session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    pub(crate) fn turn_extension_data(&self) -> Arc<ExtensionData> {
        Arc::clone(&self.turn_extension_data)
    }

    pub(crate) fn auth_manager(&self) -> Arc<AuthManager> {
        Arc::clone(&self.session.services.auth_manager)
    }

    pub(crate) fn models_manager(&self) -> SharedModelsManager {
        Arc::clone(&self.session.services.models_manager)
    }
}

/// Async task that drives a [`Session`] turn.
///
/// Implementations encapsulate a specific Codex workflow (regular chat,
/// reviews, ghost snapshots, etc.). Each task instance is owned by a
/// [`Session`] and executed on a background Tokio task. The trait is
/// intentionally small: implementers identify themselves via
/// [`SessionTask::kind`], perform their work in [`SessionTask::run`], and may
/// release resources in [`SessionTask::abort`].
pub(crate) trait SessionTask: Send + Sync + 'static {
    /// Describes the type of work the task performs so the session can
    /// surface it in telemetry and UI.
    fn kind(&self) -> TaskKind;

    /// Returns the tracing name for a spawned task span.
    fn span_name(&self) -> &'static str;

    /// Executes the task until completion or cancellation.
    ///
    /// Implementations typically stream protocol events using `session` and
    /// `ctx`, returning an optional final agent message when finished. The
    /// provided `cancellation_token` is cancelled when the session requests an
    /// abort; implementers should watch for it and terminate quickly once it
    /// fires. Returning [`Some`] yields a final message that
    /// [`Session::on_task_finished`] will emit to the client. Returning
    /// [`CodexErr::TurnAborted`] completes the task through the aborted-turn
    /// lifecycle instead.
    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = SessionTaskResult> + Send;

    /// Gives the task a chance to perform cleanup after an abort.
    ///
    /// The default implementation is a no-op; override this if additional
    /// teardown or notifications are required once
    /// [`Session::abort_all_tasks`] cancels the task.
    fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let _ = (session, ctx);
        }
    }
}

pub(crate) trait AnySessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn span_name(&self) -> &'static str;

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult>;

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()>;
}

impl<T> AnySessionTask for T
where
    T: SessionTask,
{
    fn kind(&self) -> TaskKind {
        SessionTask::kind(self)
    }

    fn span_name(&self) -> &'static str {
        SessionTask::span_name(self)
    }

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult> {
        Box::pin(SessionTask::run(
            self,
            session,
            ctx,
            input,
            cancellation_token,
        ))
    }

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(SessionTask::abort(self, session, ctx))
    }
}

#[derive(Debug)]
enum TurnTerminalOutcome {
    Completed { last_agent_message: Option<String> },
    ReturnedError(CodexErr),
    Aborted(TurnAbortReason),
    WorkerJoinFailed(WorkerJoinFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkerJoinFailure {
    Cancelled,
    Panicked,
}

enum TerminalSchedule {
    Started(Arc<TurnTerminalCoordinator>),
    AlreadyRunning(Arc<TurnTerminalCoordinator>),
    NotFound,
}

impl TerminalSchedule {
    fn coordinator(&self) -> Option<&Arc<TurnTerminalCoordinator>> {
        match self {
            Self::Started(coordinator) | Self::AlreadyRunning(coordinator) => Some(coordinator),
            Self::NotFound => None,
        }
    }

    fn matched(&self) -> bool {
        !matches!(self, Self::NotFound)
    }
}

struct TerminalFinalization {
    task: RunningTask,
    turn_state: Arc<tokio::sync::Mutex<TurnState>>,
    reasoning_policy_recorder: Arc<crate::session::reasoning_governor::ReasoningPolicyRecorder>,
    coordinator: Arc<TurnTerminalCoordinator>,
    outcome: TurnTerminalOutcome,
    permit: Option<TurnTerminalPermit>,
    completion_finalization_permit: Option<crate::agent::control::CompletionFinalizationPermit>,
    workspace_finalization_guard: Option<WorkspaceFinalizationGuard>,
}

struct WorkspaceFinalizationGuard {
    store: Arc<dyn AgentTaskStore>,
    repo_root: PathBuf,
    fence: Option<WorkspaceFinalizationFence>,
    heartbeat_cancel: CancellationToken,
    heartbeat_task: Option<tokio::task::JoinHandle<()>>,
    healthy: Arc<AtomicBool>,
}

impl WorkspaceFinalizationGuard {
    fn new(
        store: Arc<dyn AgentTaskStore>,
        repo_root: PathBuf,
        fence: WorkspaceFinalizationFence,
    ) -> Self {
        let heartbeat_cancel = CancellationToken::new();
        let healthy = Arc::new(AtomicBool::new(true));
        let heartbeat_task = {
            let store = Arc::clone(&store);
            let repo_root = repo_root.clone();
            let fence_id = fence.fence_id.clone();
            let root_session_id = fence.root_session_id.clone();
            let cancel = heartbeat_cancel.clone();
            let healthy = Arc::clone(&healthy);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(30),
                    Duration::from_secs(30),
                );
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = interval.tick() => {
                            match store
                                .heartbeat_workspace_finalization(
                                    &repo_root,
                                    fence_id.clone(),
                                    root_session_id.clone(),
                                )
                                .await
                            {
                                Ok(true) => {}
                                Ok(false) | Err(_) => {
                                    healthy.store(false, Ordering::Release);
                                    break;
                                }
                            }
                        }
                    }
                }
            })
        };
        Self {
            store,
            repo_root,
            fence: Some(fence),
            heartbeat_cancel,
            heartbeat_task: Some(heartbeat_task),
            healthy,
        }
    }

    fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    async fn seal_for_terminal_dispatch(&mut self) -> Result<(), String> {
        if !self.is_healthy() {
            return Err("workspace finalization fence is unhealthy".to_string());
        }
        let Some(fence) = self.fence.clone() else {
            self.healthy.store(false, Ordering::Release);
            return Err("workspace finalization fence is missing".to_string());
        };
        match self
            .store
            .seal_workspace_finalization_dispatch(&self.repo_root, fence)
            .await
        {
            Ok(sealed_fence) => {
                self.fence = Some(sealed_fence);
                if self.is_healthy() {
                    Ok(())
                } else {
                    Err("workspace finalization fence became unhealthy while sealing".to_string())
                }
            }
            Err(error) => {
                self.healthy.store(false, Ordering::Release);
                Err(error.to_string())
            }
        }
    }

    async fn release(&mut self) -> Result<(), String> {
        self.heartbeat_cancel.cancel();
        if let Some(task) = self.heartbeat_task.take()
            && task.await.is_err()
        {
            self.healthy.store(false, Ordering::Release);
        }
        let Some(fence) = self.fence.take() else {
            return Ok(());
        };
        if let Err(error) = self
            .store
            .release_workspace_finalization(&self.repo_root, fence.clone())
            .await
        {
            self.fence = Some(fence);
            return Err(error.to_string());
        }
        Ok(())
    }
}

async fn seal_passed_completion_for_terminal_dispatch(
    completion: &mut Option<TaskCompletionGate>,
    guard: Option<&mut WorkspaceFinalizationGuard>,
) -> Option<&'static str> {
    if !completion
        .as_ref()
        .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
    {
        return None;
    }
    let guard = guard?;
    if let Err(error) = guard.seal_for_terminal_dispatch().await {
        warn!(%error, "failed to seal workspace finalization fence for terminal dispatch");
        if let Some(gate) = completion.as_mut() {
            gate.status = TaskCompletionStatus::Partial;
            gate.reasons
                .push(WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON.to_string());
            gate.reasons.sort();
            gate.reasons.dedup();
        }
        return Some(WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON);
    }
    None
}

impl Drop for WorkspaceFinalizationGuard {
    fn drop(&mut self) {
        self.heartbeat_cancel.cancel();
        let heartbeat_task = self.heartbeat_task.take();
        let Some(fence) = self.fence.take() else {
            return;
        };
        let store = Arc::clone(&self.store);
        let repo_root = self.repo_root.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(
                fence_id = %fence.fence_id,
                "workspace finalization fence will rely on lease expiry because no runtime is available"
            );
            return;
        };
        handle.spawn(async move {
            if let Some(task) = heartbeat_task {
                let _ = task.await;
            }
            if let Err(error) = store
                .release_workspace_finalization(&repo_root, fence.clone())
                .await
            {
                warn!(
                    fence_id = %fence.fence_id,
                    %error,
                    "failed to release workspace finalization fence during cleanup"
                );
            }
        });
    }
}

struct WorkerDoneNotifier(Arc<Notify>);

impl Drop for WorkerDoneNotifier {
    fn drop(&mut self) {
        // `notify_one` retains a permit when the abort finalizer has not started waiting yet.
        self.0.notify_one();
    }
}

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        self.start_task(turn_context, input, task).await;
    }

    pub(crate) async fn start_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        let taskless_placeholder = {
            let active_turn = self.active_turn.lock().await;
            active_turn.as_ref().and_then(|active_turn| {
                (active_turn.task.is_none() && active_turn.terminal.is_none())
                    .then(|| Arc::clone(&active_turn.turn_state))
            })
        };
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some(taskless_placeholder) = taskless_placeholder.as_ref() {
                self.clear_taskless_placeholder(taskless_placeholder).await;
            }
            return;
        }
        let agent_execution_guard = match self.services.agent_control.execution_guard_for_task(
            self.thread_id,
            &turn_context.sub_id,
            turn_context.multi_agent_version,
            &turn_context.session_source,
        ) {
            Ok(guard) => guard,
            Err(err) => {
                if let Some(taskless_placeholder) = taskless_placeholder.as_ref() {
                    self.clear_taskless_placeholder(taskless_placeholder).await;
                }
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(err.to_error_event(None)),
                )
                .await;
                return;
            }
        };
        let completion_activity_guard = match self
            .services
            .agent_control
            .default_child_completion_activity(&turn_context.session_source)
            .await
        {
            Ok(guard) => guard,
            Err(err) => {
                if let Some(taskless_placeholder) = taskless_placeholder.as_ref() {
                    self.clear_taskless_placeholder(taskless_placeholder).await;
                }
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(err.to_error_event(None)),
                )
                .await;
                return;
            }
        };
        let task: Arc<dyn AnySessionTask> = Arc::new(task);
        let task_kind = task.kind();
        let span_name = task.span_name();
        let turn_started_at_unix_ms = turn_context.turn_timing_state.mark_turn_started();
        turn_context
            .turn_metadata_state
            .set_turn_started_at_unix_ms(turn_started_at_unix_ms);
        let token_usage_at_turn_start = self.total_token_usage().await.unwrap_or_default();

        let cancellation_token = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let terminal = TurnTerminalCoordinator::new(turn_context.sub_id.clone());

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        let turn_state = {
            let mut active = self.active_turn.lock().await;
            let turn = active.get_or_insert_with(ActiveTurn::default);
            debug_assert!(turn.task.is_none());
            turn.reasoning_policy_recorder = Arc::new(
                crate::session::reasoning_governor::ReasoningPolicyRecorder::new(
                    turn_context.config.reasoning_phase_efforts.is_some(),
                ),
            );
            Arc::clone(&turn.turn_state)
        };
        let pending_items = self.input_queue.get_pending_input(&self.active_turn).await;
        turn_state.lock().await.token_usage_at_turn_start = token_usage_at_turn_start.clone();
        self.input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), pending_items)
            .await;
        self.emit_turn_start_lifecycle(turn_context.as_ref(), &token_usage_at_turn_start)
            .await;

        let turn_extension_data = Arc::clone(&turn_context.extension_data);
        let mut active = self.active_turn.lock().await;
        let turn = active.get_or_insert_with(ActiveTurn::default);
        debug_assert!(turn.task.is_none());
        let done_clone = Arc::clone(&done);
        let session_ctx = Arc::new(SessionTaskContext::new(
            Arc::clone(self),
            Arc::clone(&turn_extension_data),
        ));
        let ctx = Arc::clone(&turn_context);
        let task_for_run = Arc::clone(&task);
        let task_input = input;
        let task_cancellation_token = cancellation_token.child_token();
        let (start_tx, start_rx) = oneshot::channel::<()>();
        // Task-owned turn spans keep a core-owned span open for the
        // full task lifecycle after the submission dispatch span ends.
        let reasoning_effort = turn_context.effective_reasoning_effort_for_tracing();
        let task_span = info_span!(
            "turn",
            otel.name = span_name,
            thread.id = %self.thread_id,
            turn.id = %turn_context.sub_id,
            model = %turn_context.model_info.slug,
            codex.turn.reasoning_effort = %reasoning_effort,
            codex.turn.token_usage.input_tokens = field::Empty,
            codex.turn.token_usage.cached_input_tokens = field::Empty,
            codex.turn.token_usage.non_cached_input_tokens = field::Empty,
            codex.turn.token_usage.output_tokens = field::Empty,
            codex.turn.token_usage.reasoning_output_tokens = field::Empty,
            codex.turn.token_usage.total_tokens = field::Empty,
        );
        let worker_handle = tokio::spawn(
            async move {
                let _done_notifier = WorkerDoneNotifier(done_clone);
                // Do not let a fast worker finish before its RunningTask and terminal
                // coordinator are visible under the active-turn lock.
                let _ = start_rx.await;
                task_for_run
                    .run(
                        session_ctx,
                        ctx,
                        task_input,
                        task_cancellation_token.child_token(),
                    )
                    .instrument(trace_span!("session_task.run"))
                    .await
            }
            .instrument(task_span.clone()),
        );
        let worker_abort_handle = worker_handle.abort_handle();
        let supervisor_session = Arc::clone(self);
        let supervisor_turn_id = turn_context.sub_id.clone();
        let supervisor_handle = tokio::spawn(
            async move {
                supervisor_session
                    .on_task_finished(&supervisor_turn_id, worker_handle.await)
                    .await;
            }
            .instrument(task_span.clone()),
        );
        let running_task = RunningTask {
            done,
            kind: task_kind,
            task,
            cancellation_token,
            worker_abort_handle,
            _supervisor_handle: supervisor_handle,
            task_span,
            turn_context: Arc::clone(&turn_context),
            turn_extension_data,
            _agent_execution_guard: agent_execution_guard,
            _completion_activity_guard: completion_activity_guard,
        };
        turn.task = Some(running_task);
        turn.terminal = Some(terminal);
        drop(active);
        let _ = start_tx.send(());
    }

    async fn clear_taskless_placeholder(
        &self,
        expected_turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        let mut active_turn = self.active_turn.lock().await;
        if active_turn.as_ref().is_some_and(|active_turn| {
            active_turn.task.is_none()
                && active_turn.terminal.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, expected_turn_state)
        }) {
            *active_turn = None;
        }
    }

    async fn on_task_finished(
        self: &Arc<Self>,
        turn_id: &str,
        result: std::result::Result<SessionTaskResult, tokio::task::JoinError>,
    ) {
        let outcome = match result {
            Ok(Ok(last_agent_message)) => TurnTerminalOutcome::Completed { last_agent_message },
            Ok(Err(err)) => TurnTerminalOutcome::ReturnedError(err),
            Err(err) if err.is_cancelled() => {
                TurnTerminalOutcome::WorkerJoinFailed(WorkerJoinFailure::Cancelled)
            }
            Err(_) => TurnTerminalOutcome::WorkerJoinFailed(WorkerJoinFailure::Panicked),
        };
        let _ = self.schedule_turn_terminal(Some(turn_id), outcome).await;
    }

    /// Starts a regular turn when the session is idle and pending work is waiting.
    ///
    /// Pending work currently includes mailbox mail marked with `trigger_turn`.
    ///
    /// This helper generates a fresh sub-id for the synthetic turn before delegating to the
    /// explicit-sub-id variant.
    pub(crate) async fn maybe_start_turn_for_pending_work(self: &Arc<Self>) {
        self.maybe_start_turn_for_pending_work_with_sub_id(uuid::Uuid::new_v4().to_string())
            .await;
    }

    /// Starts a regular turn with the provided sub-id when pending work should wake an idle
    /// session.
    ///
    /// The turn is created only when there is mailbox mail marked with `trigger_turn`, and only
    /// if the session is currently idle.
    pub(crate) async fn maybe_start_turn_for_pending_work_with_sub_id(
        self: &Arc<Self>,
        sub_id: String,
    ) {
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
            return;
        }

        {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some()
                || self
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            *active_turn = Some(ActiveTurn::default());
        }

        let turn_context = self.new_default_turn_with_sub_id(sub_id).await;
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        self.start_task(turn_context, Vec::new(), RegularTask::new())
            .await;
    }

    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        let schedule = self
            .schedule_turn_terminal(None, TurnTerminalOutcome::Aborted(reason))
            .await;
        if let Some(coordinator) = schedule.coordinator() {
            coordinator.wait_completed().await;
        }
    }

    pub(crate) async fn abort_turn_if_active(
        self: &Arc<Self>,
        turn_id: &str,
        reason: TurnAbortReason,
    ) -> bool {
        let schedule = self
            .schedule_turn_terminal(Some(turn_id), TurnTerminalOutcome::Aborted(reason))
            .await;
        let matched = schedule.matched();
        if let Some(coordinator) = schedule.coordinator() {
            coordinator.wait_completed().await;
        }
        matched
    }

    fn schedule_turn_terminal<'a>(
        self: &'a Arc<Self>,
        expected_turn_id: Option<&'a str>,
        outcome: TurnTerminalOutcome,
    ) -> BoxFuture<'a, TerminalSchedule> {
        Box::pin(async move {
            let (task, turn_state, reasoning_policy_recorder, permit, coordinator) = {
                let mut active = self.active_turn.lock().await;
                let Some(active_turn) = active.as_mut() else {
                    return TerminalSchedule::NotFound;
                };
                let Some(coordinator) = active_turn.terminal.as_ref().cloned() else {
                    if expected_turn_id.is_none() && active_turn.task.is_none() {
                        *active = None;
                    }
                    return TerminalSchedule::NotFound;
                };
                if expected_turn_id.is_some_and(|turn_id| coordinator.turn_id() != turn_id) {
                    return TerminalSchedule::NotFound;
                }
                if active_turn.task.is_none() {
                    return TerminalSchedule::AlreadyRunning(coordinator);
                }
                let Some(permit) = coordinator.try_claim() else {
                    return TerminalSchedule::AlreadyRunning(coordinator);
                };
                let Some(task) = active_turn.task.take() else {
                    return TerminalSchedule::AlreadyRunning(coordinator);
                };
                (
                    task,
                    Arc::clone(&active_turn.turn_state),
                    active_turn.reasoning_policy_recorder.clone(),
                    permit,
                    coordinator,
                )
            };

            // From this point to `TaskTracker::spawn` there is no await: the permit moves
            // directly from the caller into a session-owned, non-cancellable supervisor task.
            let finalizer_span = task.task_span.clone();
            let session = Arc::clone(self);
            let terminal_turn_id = coordinator.turn_id().to_string();
            let finalizer_coordinator = Arc::clone(&coordinator);
            self.terminal_tasks.spawn(
            async move {
                let mut finalization = TerminalFinalization {
                    task,
                    turn_state,
                    reasoning_policy_recorder,
                    coordinator: finalizer_coordinator,
                    outcome,
                    permit: Some(permit),
                    completion_finalization_permit: None,
                    workspace_finalization_guard: None,
                };
                let result = AssertUnwindSafe(
                    session.finalize_turn_terminal(&mut finalization),
                )
                .catch_unwind()
                .await;
                if result.is_err() {
                    warn!(
                        turn_id = %terminal_turn_id,
                        "turn terminal finalizer panicked; running fail-safe terminal completion"
                    );
                    if AssertUnwindSafe(
                        session.finalize_turn_terminal_fail_safe(&mut finalization),
                    )
                    .catch_unwind()
                    .await
                    .is_err()
                    {
                        warn!(
                            turn_id = %terminal_turn_id,
                            "turn fail-safe terminal completion also panicked"
                        );
                    }
                }
                if let Some(permit) = finalization.permit.take() {
                    permit.complete();
                }
            }
            .instrument(finalizer_span),
        );

            TerminalSchedule::Started(coordinator)
        })
    }

    async fn finalize_turn_terminal(self: &Arc<Self>, finalization: &mut TerminalFinalization) {
        let turn_context = Arc::clone(&finalization.task.turn_context);
        self.services
            .command_execution
            .cancel_mutations_for_turn(&turn_context.sub_id)
            .await;
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();

        let requires_abort_cleanup = matches!(
            &finalization.outcome,
            TurnTerminalOutcome::Aborted(_) | TurnTerminalOutcome::WorkerJoinFailed(_)
        );
        if requires_abort_cleanup {
            #[cfg(test)]
            finalization
                .coordinator
                .panic_before_worker_cancellation_if_requested();
            trace!(
                task_kind = ?finalization.task.kind,
                sub_id = %turn_context.sub_id,
                "quiescing task before terminal finalization"
            );
            finalization.task.cancellation_token.cancel();
            tokio::select! {
                _ = finalization.task.done.notified() => {},
                _ = tokio::time::sleep(Duration::from_millis(GRACEFULL_INTERRUPTION_TIMEOUT_MS)) => {
                    warn!(
                        "task {} didn't complete gracefully after {}ms",
                        turn_context.sub_id,
                        GRACEFULL_INTERRUPTION_TIMEOUT_MS
                    );
                }
            }
            finalization.task.worker_abort_handle.abort();

            let session_task = Arc::clone(&finalization.task.task);
            let session_ctx = Arc::new(SessionTaskContext::new(
                Arc::clone(self),
                Arc::clone(&finalization.task.turn_extension_data),
            ));
            session_task
                .abort(session_ctx, Arc::clone(&turn_context))
                .await;
        }

        turn_context.turn_timing_state.begin_finalization();

        let explicit_abort_reason = match &finalization.outcome {
            TurnTerminalOutcome::Aborted(reason) => Some(reason.clone()),
            _ => None,
        };
        if explicit_abort_reason == Some(TurnAbortReason::Interrupted)
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    turn_context.config.as_ref(),
                    turn_context.multi_agent_version,
                ),
            )
        {
            self.record_conversation_items(turn_context.as_ref(), std::slice::from_ref(&marker))
                .await;
            if let Err(err) = self.flush_rollout().await {
                warn!("failed to flush interrupted-turn marker before terminal event: {err}");
            }
        }

        let (last_agent_message, abort_reason) = match &finalization.outcome {
            TurnTerminalOutcome::Completed { last_agent_message } => {
                (last_agent_message.clone(), None)
            }
            TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted) => {
                (None, Some(TurnAbortReason::Interrupted))
            }
            TurnTerminalOutcome::ReturnedError(err) => {
                warn!(%err, "session task returned an unexpected error");
                (None, None)
            }
            TurnTerminalOutcome::Aborted(reason) => (None, Some(reason.clone())),
            TurnTerminalOutcome::WorkerJoinFailed(_) => (None, None),
        };

        if requires_abort_cleanup {
            // Cancellation is observable before pending approvals are dropped, preventing an
            // in-flight approval wait from surfacing as a model-visible rejection first.
            self.input_queue
                .clear_pending_for_turn_state(finalization.turn_state.as_ref())
                .await;
        } else {
            let pending_input = self
                .input_queue
                .take_pending_input_for_turn_state(finalization.turn_state.as_ref())
                .await;
            for pending_input_item in pending_input {
                let hook_outcome =
                    inspect_pending_input(self, &turn_context, &pending_input_item).await;
                if hook_outcome.should_stop {
                    record_additional_contexts(
                        self,
                        &turn_context,
                        hook_outcome.additional_contexts,
                    )
                    .await;
                } else {
                    record_pending_input(
                        self,
                        &turn_context,
                        pending_input_item,
                        hook_outcome.additional_contexts,
                    )
                    .await;
                }
            }
        }

        let (
            turn_had_memory_citation,
            turn_tool_calls,
            token_usage_at_turn_start,
            mut completion_review_partial_reasons,
        ) = {
            let ts = finalization.turn_state.lock().await;
            (
                ts.has_memory_citation,
                ts.tool_calls,
                ts.token_usage_at_turn_start.clone(),
                ts.completion_review_partial_reasons(),
            )
        };
        // Emit token usage metrics.
        {
            // TODO(jif): drop this
            let tmp_mem = (
                "tmp_mem_enabled",
                if self.enabled(Feature::MemoryTool) {
                    "true"
                } else {
                    "false"
                },
            );
            let network_proxy = self.services.network_proxy.load_full();
            let network_proxy_active = match network_proxy.as_ref() {
                Some(started_network_proxy) => {
                    match started_network_proxy.proxy().current_cfg().await {
                        Ok(config) => config.enabled,
                        Err(err) => {
                            warn!(
                                "failed to read managed network proxy state for turn metrics: {err:#}"
                            );
                            false
                        }
                    }
                }
                None => false,
            };
            emit_turn_network_proxy_metric(
                &self.services.session_telemetry,
                network_proxy_active,
                tmp_mem,
            );
            self.services.session_telemetry.histogram(
                TURN_TOOL_CALL_METRIC,
                i64::try_from(turn_tool_calls).unwrap_or(i64::MAX),
                &[tmp_mem],
            );
            let total_token_usage = self.total_token_usage().await.unwrap_or_default();
            let turn_token_usage = TokenUsage {
                input_tokens: (total_token_usage.input_tokens
                    - token_usage_at_turn_start.input_tokens)
                    .max(0),
                cached_input_tokens: (total_token_usage.cached_input_tokens
                    - token_usage_at_turn_start.cached_input_tokens)
                    .max(0),
                output_tokens: (total_token_usage.output_tokens
                    - token_usage_at_turn_start.output_tokens)
                    .max(0),
                reasoning_output_tokens: (total_token_usage.reasoning_output_tokens
                    - token_usage_at_turn_start.reasoning_output_tokens)
                    .max(0),
                total_tokens: (total_token_usage.total_tokens
                    - token_usage_at_turn_start.total_tokens)
                    .max(0),
            };
            let current_span = Span::current();
            current_span.record(
                "codex.turn.token_usage.input_tokens",
                turn_token_usage.input_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.cached_input_tokens",
                turn_token_usage.cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.non_cached_input_tokens",
                turn_token_usage.non_cached_input(),
            );
            current_span.record(
                "codex.turn.token_usage.output_tokens",
                turn_token_usage.output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.reasoning_output_tokens",
                turn_token_usage.reasoning_output_tokens,
            );
            current_span.record(
                "codex.turn.token_usage.total_tokens",
                turn_token_usage.total_tokens,
            );
            self.services
                .analytics_events_client
                .track_turn_token_usage(TurnTokenUsageFact {
                    turn_id: turn_context.sub_id.clone(),
                    thread_id: self.thread_id.to_string(),
                    token_usage: turn_token_usage.clone(),
                });
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.total_tokens,
                &[("token_type", "total"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.input_tokens,
                &[("token_type", "input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.cached_input(),
                &[("token_type", "cached_input"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.output_tokens,
                &[("token_type", "output"), tmp_mem],
            );
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                turn_token_usage.reasoning_output_tokens,
                &[("token_type", "reasoning_output"), tmp_mem],
            );
        }
        emit_turn_memory_metric(
            &self.services.session_telemetry,
            turn_context.config.features.enabled(Feature::MemoryTool),
            turn_context.config.memories.use_memories,
            turn_had_memory_citation,
        );
        let mut atomically_persisted_completion = None;
        let mut terminal_authoritative_inputs = None;
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && turn_context
                .config
                .features
                .enabled(Feature::TaskCompletionReviewer)
            && completion_review_partial_reasons.is_empty()
        {
            let preliminary_authoritative =
                completion_review::refresh_authoritative_review_inputs(self).await;
            let preliminary_dossier = self
                .services
                .task_evidence
                .completion_review_dossier(
                    last_agent_message.as_deref(),
                    &preliminary_authoritative.typed_mutation_identities,
                    &preliminary_authoritative.typed_evidence,
                    &preliminary_authoritative.review_lens_selection_facts,
                    &preliminary_authoritative.partial_reasons,
                    preliminary_authoritative.typed_quiescent,
                    preliminary_authoritative.default_children_quiescent,
                )
                .await;
            if preliminary_dossier.as_ref().is_some_and(|dossier| {
                dossier.cycle_phase
                    == Some(crate::task_evidence::CompletionReviewCyclePhase::ProvisionalClean)
            }) {
                match self
                    .services
                    .agent_control
                    .begin_completion_finalization()
                    .await
                {
                    Ok(permit) => finalization.completion_finalization_permit = Some(permit),
                    Err(error) => completion_review_partial_reasons.push(format!(
                        "completion finalization admission could not be acquired: {error}"
                    )),
                }

                if completion_review_partial_reasons.is_empty() {
                    // The completion permit waits for admitted root/default-child work. Reconcile
                    // authoritative evidence once more before the workspace fence makes the typed
                    // store read-only.
                    let _ = completion_review::refresh_authoritative_review_inputs(self).await;
                    let coordinator = self.services.agent_control.task_coordinator();
                    match (
                        coordinator.store(),
                        coordinator.root_session_id(),
                        self.services.task_evidence.repository_root(),
                    ) {
                        (Some(store), Some(root_session_id), Some(repo_root)) => {
                            let repo_root = repo_root.to_path_buf();
                            match store
                                .begin_workspace_finalization(&repo_root, root_session_id)
                                .await
                            {
                                Ok(fence) => {
                                    finalization.workspace_finalization_guard = Some(
                                        WorkspaceFinalizationGuard::new(store, repo_root, fence),
                                    );
                                }
                                Err(error) => completion_review_partial_reasons.push(format!(
                                    "workspace finalization fence could not be acquired: {error}"
                                )),
                            }
                        }
                        (None, None, _) => {}
                        _ => completion_review_partial_reasons.push(
                            "typed-work finalization state was only partially initialized"
                                .to_string(),
                        ),
                    }
                }

                if completion_review_partial_reasons.is_empty() {
                    let authoritative =
                        completion_review::inspect_authoritative_review_inputs(self).await;
                    terminal_authoritative_inputs = Some(authoritative.clone());
                    let guarded_dossier = self
                        .services
                        .task_evidence
                        .completion_review_dossier(
                            last_agent_message.as_deref(),
                            &authoritative.typed_mutation_identities,
                            &authoritative.typed_evidence,
                            &authoritative.review_lens_selection_facts,
                            &authoritative.partial_reasons,
                            authoritative.typed_quiescent,
                            authoritative.default_children_quiescent,
                        )
                        .await;
                    match guarded_dossier {
                        Some(dossier)
                            if dossier.cycle_phase
                                == Some(
                                    crate::task_evidence::CompletionReviewCyclePhase::ProvisionalClean,
                                )
                                && finalization
                                    .workspace_finalization_guard
                                    .as_ref()
                                    .is_none_or(WorkspaceFinalizationGuard::is_healthy) =>
                        {
                            if !completion_review::user_sources_still_current(&dossier).await {
                                let _ = self
                                    .services
                                    .task_evidence
                                    .supersede_provisional_completion_review(&dossier)
                                    .await;
                                completion_review_partial_reasons.push(
                                    "a file-backed user source changed before terminal closure"
                                        .to_string(),
                                );
                            } else {
                                match self
                                    .services
                                    .task_evidence
                                    .finalize_completion_review(&dossier)
                                    .await
                                {
                                    crate::task_evidence::AtomicReviewTransition::Persisted(gate) => {
                                        atomically_persisted_completion = Some(gate);
                                    }
                                    crate::task_evidence::AtomicReviewTransition::Superseded => {
                                        let _ = self
                                            .services
                                            .task_evidence
                                            .supersede_provisional_completion_review(&dossier)
                                            .await;
                                        completion_review_partial_reasons.push(
                                            "the reviewed candidate changed during terminal finalization"
                                                .to_string(),
                                        );
                                    }
                                    crate::task_evidence::AtomicReviewTransition::Failed => {
                                        completion_review_partial_reasons.push(
                                            "the atomic completion-review terminal transition failed"
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                        }
                        Some(dossier) => {
                            let _ = self
                                .services
                                .task_evidence
                                .supersede_provisional_completion_review(&dossier)
                                .await;
                            completion_review_partial_reasons.push(
                                "the reviewed candidate was invalidated before terminal closure"
                                    .to_string(),
                            );
                        }
                        None => completion_review_partial_reasons.push(
                            "the guarded completion dossier could not be reconstructed".to_string(),
                        ),
                    }
                }
            }
        }
        if atomically_persisted_completion.is_some()
            && finalization
                .workspace_finalization_guard
                .as_ref()
                .is_some_and(|guard| !guard.is_healthy())
        {
            let reason = "the workspace finalization fence was lost before terminal emission";
            let _ = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(reason)
                .await;
            if let Some(gate) = atomically_persisted_completion.as_mut() {
                gate.status = TaskCompletionStatus::Partial;
                gate.reasons.push(reason.to_string());
                gate.reasons.sort();
                gate.reasons.dedup();
            }
            completion_review_partial_reasons.push(reason.to_string());
        }
        let mut completion = if abort_reason.is_none() {
            match atomically_persisted_completion {
                Some(gate) => Some(gate),
                None => self.services.task_evidence.completion_gate().await,
            }
        } else {
            None
        };
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && !turn_context
                .config
                .features
                .enabled(Feature::TaskCompletionReviewer)
        {
            let coordinator = self.services.agent_control.task_coordinator();
            let (quiescence_reason, quiescence_warnings) = if let (
                Some(store),
                Some(root_session_id),
            ) =
                (coordinator.store(), coordinator.root_session_id())
            {
                if let Err(error) = self
                    .services
                    .agent_control
                    .reconcile_live_typed_actor_heartbeats()
                    .await
                {
                    (
                        Some(format!(
                            "linked typed work liveness could not be reconciled: {error}"
                        )),
                        Vec::new(),
                    )
                } else {
                    match store.check_quiescence(root_session_id).await {
                        Ok(status) => {
                            let reason = (!status.quiescent).then(|| {
                                format!(
                                    "linked typed work is not quiescent: active assignments [{}]; running validations [{}]; pending gates [{}]; active claims [{}]; active mutation leases [{}]",
                                    status
                                        .active_assignment_ids
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    status.running_validation_call_ids.join(", "),
                                    status
                                        .pending_gate_assignment_ids
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    status
                                        .active_claim_assignment_ids
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    status.active_mutation_lease_ids.join(", "),
                                )
                            });
                            (reason, status.warnings)
                        }
                        Err(error) => (
                            Some(format!(
                                "linked typed-work quiescence could not be established: {error}"
                            )),
                            Vec::new(),
                        ),
                    }
                }
            } else {
                (None, Vec::new())
            };
            for warning in quiescence_warnings {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Warning(WarningEvent { message: warning }),
                )
                .await;
            }
            if let Some(reason) = quiescence_reason {
                match completion.as_mut() {
                    Some(gate) => {
                        gate.status = TaskCompletionStatus::Blocked;
                        gate.reasons.push(reason);
                    }
                    None => {
                        completion = Some(TaskCompletionGate {
                            status: TaskCompletionStatus::Blocked,
                            reasons: vec![reason],
                            evidence_path: None,
                        });
                    }
                }
            }
        }
        if abort_reason.is_none() {
            merge_completion_review_partial(&mut completion, completion_review_partial_reasons);
        }
        if let Some(reason) = abort_reason.as_ref() {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
        } else {
            self.emit_turn_stop_lifecycle(turn_context.extension_data.as_ref())
                .await;
        }

        if let TurnTerminalOutcome::WorkerJoinFailed(failure) = &finalization.outcome {
            let failure_kind = match failure {
                WorkerJoinFailure::Cancelled => "cancelled",
                WorkerJoinFailure::Panicked => "panicked",
            };
            self.send_event(
                turn_context.as_ref(),
                EventMsg::Error(ErrorEvent {
                    message: format!(
                        "The turn worker {failure_kind} before terminal bookkeeping completed."
                    ),
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
            )
            .await;
        }

        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout before completing turn: {err}");
            self.send_event(
                turn_context.as_ref(),
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Failed to save the conversation transcript; Codex will continue retrying. Error: {err}"
                    ),
                }),
            )
            .await;
        }

        let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
        if let Some(duration) = timing_snapshot.inclusive_duration() {
            turn_context
                .session_telemetry
                .record_duration(TURN_E2E_DURATION_METRIC, duration, &[]);
        }
        let timing = timing_snapshot.protocol_timing();
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
                turn_id: turn_context.sub_id.clone(),
                profile: timing_snapshot.legacy_profile.clone(),
                timing: Some(timing.clone()),
            });
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && turn_context
                .config
                .features
                .enabled(Feature::TaskCompletionReviewer)
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
        {
            let terminal_invalidation_reason = if finalization
                .workspace_finalization_guard
                .as_ref()
                .is_some_and(|guard| !guard.is_healthy())
            {
                Some("the workspace finalization fence was lost before terminal emission")
            } else if let Some(authoritative) = terminal_authoritative_inputs.as_ref() {
                let current_dossier = self
                    .services
                    .task_evidence
                    .completion_review_dossier(
                        last_agent_message.as_deref(),
                        &authoritative.typed_mutation_identities,
                        &authoritative.typed_evidence,
                        &authoritative.review_lens_selection_facts,
                        &authoritative.partial_reasons,
                        authoritative.typed_quiescent,
                        authoritative.default_children_quiescent,
                    )
                    .await;
                if let Some(dossier) = current_dossier
                    && self
                        .services
                        .task_evidence
                        .passed_completion_matches_dossier(&dossier)
                        .await
                {
                    None
                } else {
                    Some("the reviewed candidate drifted before terminal emission")
                }
            } else {
                Some("the final authoritative completion snapshot was unavailable")
            };
            let terminal_invalidation_reason = terminal_invalidation_reason.or_else(|| {
                finalization
                    .workspace_finalization_guard
                    .as_ref()
                    .is_some_and(|guard| !guard.is_healthy())
                    .then_some("the workspace finalization fence was lost before terminal emission")
            });
            if let Some(reason) = terminal_invalidation_reason {
                if let Some(gate) = completion.as_mut() {
                    gate.status = TaskCompletionStatus::Partial;
                    gate.reasons.push(reason.to_string());
                    gate.reasons.sort();
                    gate.reasons.dedup();
                }
                let _ = self
                    .services
                    .task_evidence
                    .invalidate_completion_after_terminal_emission_failure(reason)
                    .await;
            }
        }
        let completed_at = timing_snapshot.completed_at_unix_secs;
        let duration_ms = timing_snapshot.duration_ms;
        let error = turn_context.terminal_error.lock().await.clone();
        if abort_reason.is_none()
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
            && finalization
                .workspace_finalization_guard
                .as_ref()
                .is_some_and(|guard| !guard.is_healthy())
        {
            let reason = "the workspace finalization fence was lost before terminal emission";
            if let Some(gate) = completion.as_mut() {
                gate.status = TaskCompletionStatus::Partial;
                gate.reasons.push(reason.to_string());
                gate.reasons.sort();
                gate.reasons.dedup();
            }
            let _ = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(reason)
                .await;
        }
        if abort_reason.is_none()
            && let Some(reason) = seal_passed_completion_for_terminal_dispatch(
                &mut completion,
                finalization.workspace_finalization_guard.as_mut(),
            )
            .await
        {
            let _ = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(reason)
                .await;
        }
        let passed_root_completion = abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
        let event = if let Some(reason) = abort_reason.as_ref() {
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_context.sub_id.clone()),
                reason: reason.clone(),
                completed_at,
                duration_ms,
                timing: Some(timing),
            })
        } else {
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message,
                error,
                completion,
                completed_at,
                duration_ms,
                time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                timing: Some(timing),
            })
        };
        if let Some(summary) = finalization
            .reasoning_policy_recorder
            .take_summary(turn_context.sub_id.clone())
        {
            self.send_event(
                turn_context.as_ref(),
                EventMsg::ReasoningPolicySummary(summary),
            )
            .await;
        }
        self.send_event(turn_context.as_ref(), event).await;
        if let Some(permit) = finalization.permit.as_ref() {
            permit.mark_terminal_event_dispatched();
        }
        if passed_root_completion {
            self.set_last_passed_root_completion_turn_id(Some(turn_context.sub_id.clone()))
                .await;
        }
        if let Some(mut guard) = finalization.workspace_finalization_guard.take()
            && let Err(error) = guard.release().await
        {
            warn!(%error, "failed to release workspace finalization fence after terminal emission");
        }
        drop(finalization.completion_finalization_permit.take());
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        let cleared_active_turn = {
            let mut active = self.active_turn.lock().await;
            if let Some(active_turn) = active.as_ref()
                && active_turn.task.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
            {
                *active = None;
                drop(finalization.task._agent_execution_guard.take());
                drop(finalization.task._completion_activity_guard.take());
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }
        if cleared_active_turn && abort_reason == Some(TurnAbortReason::Interrupted) {
            self.maybe_start_turn_for_pending_work().await;
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn close_unified_exec_processes(&self) {
        self.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
    }

    pub(crate) async fn list_background_terminals(&self) -> Vec<BackgroundTerminalInfo> {
        self.services.unified_exec_manager.list_processes().await
    }

    pub(crate) async fn terminate_background_terminal(&self, process_id: i32) -> bool {
        self.services
            .unified_exec_manager
            .terminate_process(process_id)
            .await
    }

    async fn finalize_turn_terminal_fail_safe(
        self: &Arc<Self>,
        finalization: &mut TerminalFinalization,
    ) {
        let turn_context = Arc::clone(&finalization.task.turn_context);
        self.services
            .command_execution
            .cancel_mutations_for_turn(&turn_context.sub_id)
            .await;
        finalization.task.cancellation_token.cancel();
        finalization.task.worker_abort_handle.abort();
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        turn_context.turn_timing_state.begin_finalization();
        self.input_queue
            .clear_pending_for_turn_state(finalization.turn_state.as_ref())
            .await;

        let terminal_event_dispatched = finalization.coordinator.terminal_event_dispatched();
        if !terminal_event_dispatched {
            let _ = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(
                    "terminal emission failed after completion-review closure",
                )
                .await;
            self.send_event(
                turn_context.as_ref(),
                EventMsg::Error(ErrorEvent {
                    message:
                        "Turn terminal bookkeeping failed; emitted a fail-safe terminal outcome."
                            .to_string(),
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
            )
            .await;

            let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
            if let Some(duration) = timing_snapshot.inclusive_duration() {
                turn_context.session_telemetry.record_duration(
                    TURN_E2E_DURATION_METRIC,
                    duration,
                    &[],
                );
            }
            let timing = timing_snapshot.protocol_timing();
            self.services
                .analytics_events_client
                .track_turn_profile(TurnProfileFact {
                    turn_id: turn_context.sub_id.clone(),
                    profile: timing_snapshot.legacy_profile.clone(),
                    timing: Some(timing.clone()),
                });
            let abort_reason = match &finalization.outcome {
                TurnTerminalOutcome::Aborted(reason) => Some(reason.clone()),
                TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted) => {
                    Some(TurnAbortReason::Interrupted)
                }
                _ => None,
            };
            let event = if let Some(reason) = abort_reason {
                EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some(turn_context.sub_id.clone()),
                    reason,
                    completed_at: timing_snapshot.completed_at_unix_secs,
                    duration_ms: timing_snapshot.duration_ms,
                    timing: Some(timing),
                })
            } else {
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_context.sub_id.clone(),
                    last_agent_message: None,
                    error: None,
                    completion: None,
                    completed_at: timing_snapshot.completed_at_unix_secs,
                    duration_ms: timing_snapshot.duration_ms,
                    time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                    timing: Some(timing),
                })
            };
            if let Some(summary) = finalization
                .reasoning_policy_recorder
                .take_summary(turn_context.sub_id.clone())
            {
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::ReasoningPolicySummary(summary),
                )
                .await;
            }
            self.send_event(turn_context.as_ref(), event).await;
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_terminal_event_dispatched();
            }
        }

        if let Some(mut guard) = finalization.workspace_finalization_guard.take()
            && let Err(error) = guard.release().await
        {
            warn!(%error, "failed to release workspace finalization fence during fail-safe cleanup");
        }
        drop(finalization.completion_finalization_permit.take());

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);
        let cleared_active_turn = {
            let mut active = self.active_turn.lock().await;
            if let Some(active_turn) = active.as_ref()
                && active_turn.task.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
            {
                *active = None;
                drop(finalization.task._agent_execution_guard.take());
                drop(finalization.task._completion_activity_guard.take());
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after fail-safe terminal event: {err}");
        }
    }
}

fn merge_completion_review_partial(
    completion: &mut Option<TaskCompletionGate>,
    partial_reasons: Vec<String>,
) {
    if partial_reasons.is_empty() {
        return;
    }
    match completion.as_mut() {
        Some(gate) => {
            if gate.status == TaskCompletionStatus::Passed {
                gate.status = TaskCompletionStatus::Partial;
            }
            gate.reasons.extend(partial_reasons);
            gate.reasons.sort();
            gate.reasons.dedup();
        }
        None => {
            *completion = Some(TaskCompletionGate {
                status: TaskCompletionStatus::Partial,
                reasons: partial_reasons,
                evidence_path: None,
            });
        }
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
