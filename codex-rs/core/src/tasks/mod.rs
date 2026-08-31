mod compact;
mod lifecycle;
mod regular;
mod review;
mod user_shell;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
use crate::hook_runtime::run_turn_interrupt_hooks;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::state::TurnState;
use crate::state::TurnTerminalCoordinator;
use crate::state::TurnTerminalPermit;
use crate::tools::context::RequiredToolTerminal;
use codex_analytics::TurnProfileFact;
use codex_analytics::TurnTokenUsageFact;
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
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;

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

const GRACEFUL_INTERRUPTION_MARGIN: Duration = Duration::from_millis(250);
const GRACEFUL_INTERRUPTION_TIMEOUT: Duration =
    crate::tools::parallel::TOOL_RUNTIME_CLEANUP_DEADLINE
        .saturating_add(GRACEFUL_INTERRUPTION_MARGIN);
const TASK_COMPACT_METRIC: &str = "codex.task.compact";

#[derive(Clone, Debug, Default)]
pub(crate) struct TurnTaskResult {
    pub(crate) last_agent_message: Option<String>,
    pub(crate) surfaced_result: Option<codex_protocol::protocol::SurfacedToolResult>,
    pub(crate) required_tool_terminal: Option<RequiredToolTerminal>,
    /// Preserve already-accepted pending input for a fresh turn instead of folding it into a
    /// terminal turn that no longer has a model-generation budget.
    pub(crate) defer_pending_input: bool,
}

pub(crate) type SessionTaskResult = CodexResult<TurnTaskResult>;

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
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, SessionTaskResult>;

    /// Gives the task a chance to perform cleanup after an abort.
    ///
    /// The default implementation is a no-op; override this if additional
    /// teardown or notifications are required once
    /// [`Session::abort_all_tasks`] cancels the task.
    fn abort<'a>(&'a self, session: Arc<Session>, ctx: Arc<TurnContext>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = (session, ctx);
        })
    }
}

#[derive(Debug)]
enum TurnTerminalOutcome {
    Completed { result: TurnTaskResult },
    ReturnedError(CodexErr),
    Aborted(TurnAbortReason),
    WorkerJoinFailed(WorkerJoinFailure),
}

impl TurnTerminalOutcome {
    fn abort_reason(&self) -> Option<TurnAbortReason> {
        match self {
            Self::Aborted(reason) => Some(reason.clone()),
            Self::ReturnedError(CodexErr::TurnAborted) => Some(TurnAbortReason::Interrupted),
            Self::WorkerJoinFailed(_) => Some(TurnAbortReason::InternalError),
            _ => None,
        }
    }
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
    worker_failure_reported: bool,
}

struct WorkerDoneNotifier {
    notify: Arc<Notify>,
    worker_done: Arc<AtomicBool>,
}

impl Drop for WorkerDoneNotifier {
    fn drop(&mut self) {
        self.worker_done.store(true, Ordering::Release);
        self.notify.notify_waiters();
        self.notify.notify_one();
    }
}

async fn wait_for_worker_done(task: &RunningTask) {
    loop {
        let notified = task.done.notified();
        if task.worker_done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn drain_auxiliary_tasks(tasks: &mut tokio::task::JoinSet<()>) {
    while let Some(result) = tasks.join_next().await {
        if let Err(err) = result
            && !err.is_cancelled()
        {
            warn!(%err, "turn auxiliary task failed while terminalizing");
        }
    }
}

async fn quiesce_turn_auxiliary_tasks(task: &mut RunningTask) {
    task.auxiliary_cancellation_token.cancel();
    if tokio::time::timeout(
        GRACEFUL_INTERRUPTION_TIMEOUT,
        drain_auxiliary_tasks(&mut task.auxiliary_tasks),
    )
    .await
    .is_err()
    {
        task.auxiliary_tasks.abort_all();
        drain_auxiliary_tasks(&mut task.auxiliary_tasks).await;
    }
}

pub(crate) struct TasklessTurnStartupGuard {
    session: Arc<Session>,
    turn_state: Arc<tokio::sync::Mutex<TurnState>>,
    armed: bool,
}

impl TasklessTurnStartupGuard {
    pub(crate) fn new(
        session: &Arc<Session>,
        turn_state: Arc<tokio::sync::Mutex<TurnState>>,
    ) -> Self {
        Self {
            session: Arc::clone(session),
            turn_state,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TasklessTurnStartupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let session = Arc::clone(&self.session);
        let turn_state = Arc::clone(&self.turn_state);
        self.session.terminal_tasks.spawn(async move {
            session
                .recover_cancelled_taskless_placeholder(&turn_state)
                .await;
        });
    }
}

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        let Ok(_task_start_permit) = self.task_start_gate.acquire().await else {
            unreachable!("session-owned task-start semaphore is never closed");
        };
        self.abort_all_tasks(TurnAbortReason::Replaced).await;
        self.clear_connector_selection().await;
        let _ = self.start_task_locked(turn_context, input, task).await;
    }

    #[cfg(test)]
    pub(crate) async fn start_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) {
        let Ok(_task_start_permit) = self.task_start_gate.acquire().await else {
            unreachable!("session-owned task-start semaphore is never closed");
        };
        let _ = self.start_task_locked(turn_context, input, task).await;
    }

    pub(crate) async fn start_task_with_admission<T: SessionTask>(
        self: &Arc<Self>,
        _task_start_permit: &tokio::sync::SemaphorePermit<'_>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) -> CodexResult<()> {
        self.start_task_locked(turn_context, input, task).await
    }

    async fn start_task_locked<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<TurnInput>,
        task: T,
    ) -> CodexResult<()> {
        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            match active_turn.as_ref() {
                Some(active_turn)
                    if active_turn.task.is_some() || active_turn.terminal.is_some() =>
                {
                    return Err(CodexErr::InvalidRequest(
                        "a turn is already active".to_string(),
                    ));
                }
                Some(active_turn) => Arc::clone(&active_turn.turn_state),
                None => Arc::clone(&active_turn.insert(ActiveTurn::default()).turn_state),
            }
        };
        let mut startup_guard = TasklessTurnStartupGuard::new(self, Arc::clone(&turn_state));
        if self.terminal_interaction_pending.load(Ordering::Acquire)
            || self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
        {
            self.recover_cancelled_taskless_placeholder(&turn_state)
                .await;
            startup_guard.disarm();
            return Err(CodexErr::InvalidRequest(
                "the thread is shutting down".to_string(),
            ));
        }
        let agent_execution_guard = match self.services.agent_control.execution_guard_for_task(
            self.thread_id,
            &turn_context.sub_id,
            turn_context.multi_agent_version,
            &turn_context.session_source,
        ) {
            Ok(guard) => guard,
            Err(err) => {
                self.recover_cancelled_taskless_placeholder(&turn_state)
                    .await;
                startup_guard.disarm();
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(err.to_error_event(None)),
                )
                .await;
                return Err(err);
            }
        };
        let task: Arc<dyn SessionTask> = Arc::new(task);
        let task_kind = task.kind();
        let span_name = task.span_name();
        let turn_started_at_unix_ms = turn_context.turn_timing_state.mark_turn_started();
        turn_context
            .turn_metadata_state
            .set_turn_started_at_unix_ms(turn_started_at_unix_ms);
        let token_usage_at_turn_start = self.total_token_usage().await.unwrap_or_default();

        let cancellation_token = CancellationToken::new();
        let auxiliary_cancellation_token = cancellation_token.child_token();
        let done = Arc::new(Notify::new());
        let worker_done = Arc::new(AtomicBool::new(false));
        let terminal = TurnTerminalCoordinator::new_with_tool_call_acceptance(
            turn_context.sub_id.clone(),
            Arc::clone(&turn_context.tool_call_acceptance),
        );

        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);

        let reservation_is_current = {
            let mut active = self.active_turn.lock().await;
            active.as_mut().is_some_and(|turn| {
                turn.task.is_none()
                    && turn.terminal.is_none()
                    && Arc::ptr_eq(&turn.turn_state, &turn_state)
            })
        };
        if !reservation_is_current {
            self.recover_cancelled_taskless_placeholder(&turn_state)
                .await;
            startup_guard.disarm();
            return Err(CodexErr::Fatal(
                "turn start reservation was lost before task installation".to_string(),
            ));
        }
        let pending_items = self.input_queue.get_pending_input(&self.active_turn).await;
        turn_state.lock().await.token_usage_at_turn_start = token_usage_at_turn_start.clone();
        self.input_queue
            .restore_transferred_input_for_turn_state(turn_state.as_ref(), pending_items)
            .await;

        let start_tx = {
            let mut active = self.active_turn.lock().await;
            let reservation_is_current = active.as_ref().is_some_and(|turn| {
                turn.task.is_none()
                    && turn.terminal.is_none()
                    && Arc::ptr_eq(&turn.turn_state, &turn_state)
                    && !self.terminal_interaction_pending.load(Ordering::Acquire)
                    && !self
                        .shutting_down
                        .load(std::sync::atomic::Ordering::Acquire)
            });
            if !reservation_is_current {
                None
            } else {
                let Some(turn) = active.as_mut() else {
                    unreachable!("validated taskless turn reservation must remain present");
                };
                turn.reasoning_policy_recorder = Arc::new(
                    crate::session::reasoning_governor::ReasoningPolicyRecorder::new(
                        turn_context.config.reasoning_phase_efforts.is_some(),
                    ),
                );
                let done_clone = Arc::clone(&done);
                let worker_done_clone = Arc::clone(&worker_done);
                let session = Arc::clone(self);
                let ctx = Arc::clone(&turn_context);
                let task_for_run = Arc::clone(&task);
                let task_input = input;
                let task_cancellation_token = cancellation_token.child_token();
                let lifecycle_token_usage = token_usage_at_turn_start.clone();
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
                        let _done_notifier = WorkerDoneNotifier {
                            notify: done_clone,
                            worker_done: worker_done_clone,
                        };
                        // Do not let a fast worker finish before its RunningTask and terminal
                        // coordinator are visible under the active-turn lock.
                        let _ = start_rx.await;
                        session
                            .emit_turn_start_lifecycle(ctx.as_ref(), &lifecycle_token_usage)
                            .await;
                        task_for_run
                            .run(
                                session,
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
                    worker_done,
                    kind: task_kind,
                    task,
                    cancellation_token,
                    auxiliary_cancellation_token,
                    auxiliary_tasks: tokio::task::JoinSet::new(),
                    worker_abort_handle,
                    _supervisor_handle: supervisor_handle,
                    task_span,
                    turn_context: Arc::clone(&turn_context),
                    _agent_execution_guard: agent_execution_guard,
                };
                turn.task = Some(running_task);
                turn.terminal = Some(terminal);
                Some(start_tx)
            }
        };
        let Some(start_tx) = start_tx else {
            self.recover_cancelled_taskless_placeholder(&turn_state)
                .await;
            startup_guard.disarm();
            return Err(CodexErr::Fatal(
                "turn start reservation was lost before task installation".to_string(),
            ));
        };
        startup_guard.disarm();
        let _ = start_tx.send(());
        Ok(())
    }

    pub(crate) async fn spawn_active_turn_auxiliary<F, Fut>(&self, start: F) -> bool
    where
        F: FnOnce(Arc<TurnContext>, CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut active_turn = self.active_turn.lock().await;
        let Some(task) = active_turn
            .as_mut()
            .and_then(|active_turn| active_turn.task.as_mut())
        else {
            return false;
        };
        let turn_context = Arc::clone(&task.turn_context);
        let cancellation_token = task.auxiliary_cancellation_token.child_token();
        task.auxiliary_tasks
            .spawn(start(turn_context, cancellation_token));
        true
    }

    pub(crate) async fn clear_taskless_placeholder(
        &self,
        expected_turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        self.recover_cancelled_taskless_placeholder(expected_turn_state)
            .await;
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "taskless placeholder identity and pending input extraction must remain atomic"
    )]
    async fn recover_cancelled_taskless_placeholder(
        &self,
        expected_turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        let recovered_input = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none()
                    && active_turn.terminal.is_none()
                    && Arc::ptr_eq(&active_turn.turn_state, expected_turn_state)
            }) {
                let recovered_input = self
                    .input_queue
                    .take_pending_input_for_turn_state(expected_turn_state.as_ref())
                    .await;
                *active_turn = None;
                Some(recovered_input)
            } else if active_turn.as_ref().is_some_and(|active_turn| {
                Arc::ptr_eq(&active_turn.turn_state, expected_turn_state)
            }) {
                None
            } else {
                Some(
                    self.input_queue
                        .take_pending_input_for_turn_state(expected_turn_state.as_ref())
                        .await,
                )
            }
        };
        let Some(recovered_input) = recovered_input else {
            return;
        };
        let recovered_work = !recovered_input.is_empty();
        self.input_queue
            .restore_transferred_startup_input(recovered_input)
            .await;
        if !recovered_work {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
    }

    async fn on_task_finished(
        self: &Arc<Self>,
        turn_id: &str,
        result: std::result::Result<SessionTaskResult, tokio::task::JoinError>,
    ) {
        let outcome = match result {
            Ok(Ok(result)) => TurnTerminalOutcome::Completed { result },
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
            || self.terminal_interaction_pending.load(Ordering::Acquire)
        {
            return;
        }
        if !self.input_queue.has_pending_turn_start_work().await {
            return;
        }

        // All turn-start paths reserve the active slot while holding the same
        // admission guard. This keeps the pending-work path from publishing a
        // taskless placeholder ahead of an already admitted client start.
        let Ok(task_start_permit) = self.task_start_gate.acquire().await else {
            unreachable!("session-owned task-start semaphore is never closed");
        };
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
            || self.terminal_interaction_pending.load(Ordering::Acquire)
            || !self.input_queue.has_pending_turn_start_work().await
        {
            return;
        }

        let turn_state = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some()
                || self.terminal_interaction_pending.load(Ordering::Acquire)
                || self
                    .shutting_down
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            let active_turn = active_turn.insert(ActiveTurn::default());
            Arc::clone(&active_turn.turn_state)
        };
        let mut startup_guard = TasklessTurnStartupGuard::new(self, Arc::clone(&turn_state));

        let turn_context = self.new_default_turn_with_sub_id(sub_id).await;
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        let _ = self
            .start_task_with_admission(
                &task_start_permit,
                turn_context,
                Vec::new(),
                RegularTask::new(),
            )
            .await;
        startup_guard.disarm();
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
            let scheduling = {
                let mut active = self.active_turn.lock().await;
                let Some(active_turn) = active.as_mut() else {
                    return TerminalSchedule::NotFound;
                };
                match active_turn.terminal.as_ref().cloned() {
                    Some(coordinator) => {
                        if expected_turn_id.is_some_and(|turn_id| coordinator.turn_id() != turn_id)
                        {
                            return TerminalSchedule::NotFound;
                        }
                        let Some(permit) = coordinator.try_claim() else {
                            return TerminalSchedule::AlreadyRunning(coordinator);
                        };
                        let Some(task) = active_turn.task.take() else {
                            return TerminalSchedule::AlreadyRunning(coordinator);
                        };
                        coordinator.seal_tool_call_acceptance(&task.turn_context.turn_timing_state);
                        self.terminal_interaction_pending
                            .store(true, Ordering::Release);
                        Ok((
                            task,
                            Arc::clone(&active_turn.turn_state),
                            active_turn.reasoning_policy_recorder.clone(),
                            permit,
                            coordinator,
                        ))
                    }
                    None => Err((expected_turn_id.is_none() && active_turn.task.is_none())
                        .then(|| Arc::clone(&active_turn.turn_state))),
                }
            };
            let scheduling = match scheduling {
                Ok(scheduling) => scheduling,
                Err(taskless_turn_state) => {
                    if let Some(taskless_turn_state) = taskless_turn_state {
                        self.recover_cancelled_taskless_placeholder(&taskless_turn_state)
                            .await;
                    }
                    return TerminalSchedule::NotFound;
                }
            };

            let (task, turn_state, reasoning_policy_recorder, permit, coordinator) = scheduling;
            let finalizer_span = task.task_span.clone();
            let terminal_turn_id = coordinator.turn_id().to_string();
            let finalizer_coordinator = Arc::clone(&coordinator);
            let session = Arc::clone(self);
            self.terminal_tasks.spawn(
                async move {
                    let mut finalization = TerminalFinalization {
                        task,
                        turn_state,
                        reasoning_policy_recorder,
                        coordinator: finalizer_coordinator,
                        outcome,
                        permit: Some(permit),
                        worker_failure_reported: false,
                    };
                    if AssertUnwindSafe(session.finalize_turn_terminal(&mut finalization))
                        .catch_unwind()
                        .await
                        .is_err()
                    {
                        warn!(
                            turn_id = %terminal_turn_id,
                            "turn terminal finalizer panicked; running ordinary fail-safe cleanup"
                        );
                        let _ = AssertUnwindSafe(
                            session.finalize_turn_terminal_fail_safe(&mut finalization),
                        )
                        .catch_unwind()
                        .await;
                    }
                    if !finalization.coordinator.interaction_released() {
                        let _ = session.detach_terminal_turn(&mut finalization).await;
                    }
                    if let Some(permit) = finalization.permit.take() {
                        permit.complete_cleanup();
                    }
                }
                .instrument(finalizer_span),
            );

            TerminalSchedule::Started(coordinator)
        })
    }

    async fn detach_terminal_turn(&self, finalization: &mut TerminalFinalization) -> bool {
        drop(finalization.task._agent_execution_guard.take());
        let mut active = self.active_turn.lock().await;
        if active.as_ref().is_some_and(|active_turn| {
            active_turn.task.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
        }) {
            *active = None;
        }
        let detached = active.as_ref().is_none_or(|active_turn| {
            !Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
        });
        self.terminal_interaction_pending
            .store(false, Ordering::Release);
        if let Some(permit) = finalization.permit.as_ref() {
            permit.mark_interaction_released();
        }
        detached
    }

    async fn emit_post_terminal_metrics(
        &self,
        turn_context: &TurnContext,
        turn_had_memory_citation: bool,
        turn_tool_calls: u64,
        token_usage_at_turn_start: &TokenUsage,
    ) {
        let memory_feature_enabled = turn_context.config.features.enabled(Feature::MemoryTool);
        let tmp_mem = (
            "tmp_mem_enabled",
            if memory_feature_enabled {
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
            input_tokens: (total_token_usage.input_tokens - token_usage_at_turn_start.input_tokens)
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
            total_tokens: (total_token_usage.total_tokens - token_usage_at_turn_start.total_tokens)
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
        for (token_type, value) in [
            ("total", turn_token_usage.total_tokens),
            ("input", turn_token_usage.input_tokens),
            ("cached_input", turn_token_usage.cached_input()),
            ("output", turn_token_usage.output_tokens),
            ("reasoning_output", turn_token_usage.reasoning_output_tokens),
        ] {
            self.services.session_telemetry.histogram(
                TURN_TOKEN_USAGE_METRIC,
                value,
                &[("token_type", token_type), tmp_mem],
            );
        }
        emit_turn_memory_metric(
            &self.services.session_telemetry,
            memory_feature_enabled,
            turn_context.config.memories.use_memories,
            turn_had_memory_citation,
        );
    }

    async fn emit_worker_join_failure_before_terminal(
        &self,
        finalization: &mut TerminalFinalization,
        turn_context: &TurnContext,
    ) {
        if finalization.worker_failure_reported {
            return;
        }
        let TurnTerminalOutcome::WorkerJoinFailed(failure) = &finalization.outcome else {
            return;
        };
        let failure_kind = match failure {
            WorkerJoinFailure::Cancelled => "cancelled",
            WorkerJoinFailure::Panicked => "panicked",
        };
        self.send_event(
            turn_context,
            EventMsg::Error(ErrorEvent {
                message: format!(
                    "The turn worker {failure_kind} before terminal bookkeeping completed."
                ),
                codex_error_info: Some(CodexErrorInfo::InternalServerError),
            }),
        )
        .await;
        finalization.worker_failure_reported = true;
    }

    async fn finalize_turn_terminal(self: &Arc<Self>, finalization: &mut TerminalFinalization) {
        let turn_context = Arc::clone(&finalization.task.turn_context);
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();

        let requires_abort_cleanup = matches!(
            finalization.outcome,
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
        }

        quiesce_turn_auxiliary_tasks(&mut finalization.task).await;
        self.services
            .command_execution
            .finish_turn_before_terminal(&turn_context.sub_id)
            .await;

        if requires_abort_cleanup {
            if tokio::time::timeout(
                GRACEFUL_INTERRUPTION_TIMEOUT,
                wait_for_worker_done(&finalization.task),
            )
            .await
            .is_err()
            {
                warn!(
                    "task {} didn't complete gracefully after {}ms",
                    turn_context.sub_id,
                    GRACEFUL_INTERRUPTION_TIMEOUT.as_millis()
                );
                finalization.task.worker_abort_handle.abort();
            }
            wait_for_worker_done(&finalization.task).await;
            Arc::clone(&finalization.task.task)
                .abort(Arc::clone(self), Arc::clone(&turn_context))
                .await;
        }

        self.services
            .code_mode_service
            .finish_turn(&turn_context.sub_id);
        if let Err(err) = self
            .persist_missing_call_outputs_durable(&turn_context)
            .await
        {
            warn!(
                turn_id = %turn_context.sub_id,
                "failed to persist missing tool outputs before terminal event: {err}"
            );
        }
        if let Err(err) = self
            .flush_rollout_after_ordered_commits(&turn_context)
            .await
        {
            warn!(
                turn_id = %turn_context.sub_id,
                "failed to flush rollout before terminal event: {err}"
            );
        }
        turn_context.turn_timing_state.begin_finalization();

        let abort_reason = finalization.outcome.abort_reason();
        self.emit_worker_join_failure_before_terminal(finalization, turn_context.as_ref())
            .await;

        if abort_reason == Some(TurnAbortReason::Interrupted)
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    turn_context.config.as_ref(),
                    turn_context.multi_agent_version,
                ),
            )
            && let Err(err) = self
                .record_conversation_items_durable(&turn_context, std::slice::from_ref(&marker))
                .await
        {
            warn!(
                turn_id = %turn_context.sub_id,
                "failed to persist interrupted-turn marker before terminal event: {err}"
            );
        }

        let restart_for_pending_input = if requires_abort_cleanup {
            self.input_queue
                .clear_pending_for_turn_state(finalization.turn_state.as_ref())
                .await;
            false
        } else {
            let pending_input = self
                .input_queue
                .take_pending_input_for_turn_state(finalization.turn_state.as_ref())
                .await;
            let restart = !pending_input.is_empty();
            self.input_queue
                .restore_transferred_startup_input(pending_input)
                .await;
            restart
        };

        if abort_reason == Some(TurnAbortReason::Interrupted) {
            run_turn_interrupt_hooks(self, &turn_context).await;
        }
        if let Some(reason) = abort_reason.as_ref() {
            self.emit_turn_abort_lifecycle(reason.clone(), turn_context.extension_data.as_ref())
                .await;
        } else {
            self.emit_turn_stop_lifecycle(turn_context.extension_data.as_ref())
                .await;
        }

        let (turn_had_memory_citation, turn_tool_calls, token_usage_at_turn_start) = {
            let state = finalization.turn_state.lock().await;
            (
                state.has_memory_citation,
                state.tool_calls,
                state.token_usage_at_turn_start.clone(),
            )
        };

        let repaired_tool_timings = turn_context
            .turn_timing_state
            .repair_terminal_tool_timing_after_durable_projection();
        if repaired_tool_timings > 0 {
            warn!(
                turn_id = %turn_context.sub_id,
                repaired_tool_timings,
                "repaired terminal tool timing pairs from durable output projections"
            );
        }
        if tokio::time::timeout(
            Duration::from_secs(1),
            turn_context
                .turn_timing_state
                .wait_for_tool_closure_after_seal(),
        )
        .await
        .is_err()
        {
            warn!(
                turn_id = %turn_context.sub_id,
                "tool closure did not complete before terminal publication"
            );
        }

        let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
        if let Some(duration) = timing_snapshot.inclusive_duration() {
            turn_context
                .session_telemetry
                .record_duration(TURN_E2E_DURATION_METRIC, duration, &[]);
        }
        let timing = timing_snapshot.protocol_timing();
        if finalization
            .permit
            .as_ref()
            .is_some_and(TurnTerminalPermit::try_claim_analytics_emission)
        {
            self.services
                .analytics_events_client
                .track_turn_profile(TurnProfileFact {
                    turn_id: turn_context.sub_id.clone(),
                    profile: timing_snapshot.legacy_profile.clone(),
                    timing: Some(timing.clone()),
                });
        }

        let (last_agent_message, surfaced_result, required_tool_terminal, defer_pending_input) =
            match &finalization.outcome {
                TurnTerminalOutcome::Completed { result } => (
                    result.last_agent_message.clone(),
                    result.surfaced_result.clone(),
                    result.required_tool_terminal.clone(),
                    result.defer_pending_input,
                ),
                _ => (None, None, None, false),
            };
        let error = match &finalization.outcome {
            TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted) => None,
            TurnTerminalOutcome::ReturnedError(err) => Some(err.to_error_event(None)),
            TurnTerminalOutcome::WorkerJoinFailed(_) => Some(ErrorEvent {
                message: "The turn worker failed before completing normally.".to_string(),
                codex_error_info: Some(CodexErrorInfo::InternalServerError),
            }),
            _ => turn_context.terminal_error.lock().await.clone(),
        };
        let last_agent_message = if last_agent_message.is_none()
            && surfaced_result.is_none()
            && turn_context.final_output_json_schema.is_none()
        {
            required_tool_terminal
                .as_ref()
                .map(|terminal| terminal.message.clone())
        } else {
            last_agent_message
        };
        let event = if let Some(reason) = abort_reason.clone() {
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
                last_agent_message,
                surfaced_result,
                error,
                completed_at: timing_snapshot.completed_at_unix_secs,
                duration_ms: timing_snapshot.duration_ms,
                time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                timing: Some(timing),
            })
        };

        let cleared_active_turn = self.detach_terminal_turn(finalization).await;
        self.send_event(turn_context.as_ref(), event).await;
        self.services
            .command_execution
            .persist_cache_after_terminal()
            .await;
        self.emit_post_terminal_metrics(
            turn_context.as_ref(),
            turn_had_memory_citation,
            turn_tool_calls,
            &token_usage_at_turn_start,
        )
        .await;
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
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);
        if let Err(err) = self.flush_rollout().await {
            warn!("failed to flush rollout after emitting terminal turn event: {err}");
        }

        if cleared_active_turn {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        if cleared_active_turn
            && (abort_reason == Some(TurnAbortReason::Interrupted)
                || required_tool_terminal.is_some()
                || defer_pending_input
                || restart_for_pending_input)
        {
            self.maybe_start_turn_for_pending_work().await;
        }
    }

    async fn finalize_turn_terminal_fail_safe(
        self: &Arc<Self>,
        finalization: &mut TerminalFinalization,
    ) {
        let turn_context = Arc::clone(&finalization.task.turn_context);
        finalization.task.cancellation_token.cancel();
        finalization.task.worker_abort_handle.abort();
        wait_for_worker_done(&finalization.task).await;
        self.services
            .command_execution
            .finish_turn(&turn_context.sub_id)
            .await;
        self.services
            .code_mode_service
            .finish_turn(&turn_context.sub_id);
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        turn_context.turn_timing_state.begin_finalization();
        let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
        let timing = timing_snapshot.protocol_timing();
        let event = if let Some(reason) = finalization.outcome.abort_reason() {
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(turn_context.sub_id.clone()),
                reason,
                completed_at: timing_snapshot.completed_at_unix_secs,
                duration_ms: timing_snapshot.duration_ms,
                timing: Some(timing),
            })
        } else {
            let message =
                "The turn finalizer failed before ordinary terminal cleanup completed.".to_string();
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                last_agent_message: Some(message.clone()),
                surfaced_result: None,
                error: Some(ErrorEvent {
                    message,
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
                completed_at: timing_snapshot.completed_at_unix_secs,
                duration_ms: timing_snapshot.duration_ms,
                time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                timing: Some(timing),
            })
        };
        let cleared_active_turn = self.detach_terminal_turn(finalization).await;
        self.send_event(turn_context.as_ref(), event).await;
        self.input_queue
            .clear_pending_for_turn_state(finalization.turn_state.as_ref())
            .await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&turn_context.sub_id);
        let _ = self.flush_rollout().await;
        if cleared_active_turn {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
    }

    pub(crate) async fn begin_shutdown(&self) {
        let Ok(_task_start_permit) = self.task_start_gate.acquire().await else {
            unreachable!("session-owned task-start semaphore is never closed");
        };
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

    pub(crate) async fn terminate_background_terminal(&self, process_id: u32) -> bool {
        self.services
            .unified_exec_manager
            .terminate_process(process_id)
            .await
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
