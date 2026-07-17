mod compact;
mod lifecycle;
mod regular;
mod review;
mod user_shell;

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

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
use crate::config::ThreadStoreConfig;
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

const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TASK_COMPACT_METRIC: &str = "codex.task.compact";

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

fn emit_turn_tool_call_metric(
    session_telemetry: &SessionTelemetry,
    tool_call_count: u32,
    tmp_mem: (&str, &str),
) {
    session_telemetry.histogram(
        TURN_TOOL_CALL_METRIC,
        i64::from(tool_call_count),
        &[tmp_mem],
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
    coordinator: Arc<TurnTerminalCoordinator>,
    outcome: TurnTerminalOutcome,
    permit: Option<TurnTerminalPermit>,
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
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn
                .as_ref()
                .is_some_and(|active_turn| active_turn.task.is_none())
            {
                *active_turn = None;
            }
            return;
        }
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

        let pending_items = self.input_queue.get_pending_input(&self.active_turn).await;
        let turn_state = {
            let mut active = self.active_turn.lock().await;
            let turn = active.get_or_insert_with(ActiveTurn::default);
            debug_assert!(turn.task.is_none());
            Arc::clone(&turn.turn_state)
        };
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
        let agent_execution_guard = self.services.agent_control.execution_guard(
            turn_context.multi_agent_version,
            &turn_context.session_source,
        );
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
        let supervisor_turn_config = Arc::clone(&turn_context.config);
        let supervisor_handle = tokio::spawn(
            async move {
                let worker_result = worker_handle.await;
                supervisor_session
                    .schedule_rollout_compression_after_first_task(supervisor_turn_config.as_ref());
                supervisor_session
                    .on_task_finished(&supervisor_turn_id, worker_result)
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
        };
        turn.task = Some(running_task);
        turn.terminal = Some(terminal);
        drop(active);
        let _ = start_tx.send(());
    }

    fn schedule_rollout_compression_after_first_task(&self, config: &Config) {
        if !self
            .features()
            .enabled(Feature::LocalThreadStoreCompression)
            || !matches!(
                &config.experimental_thread_store,
                ThreadStoreConfig::Local
            )
            || self
                .rollout_compression_scheduled
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_err()
        {
            return;
        }
        codex_rollout::spawn_rollout_compression_worker(config.codex_home.to_path_buf());
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
            let (task, turn_state, permit, coordinator) = {
                let mut active = self.active_turn.lock().await;
                let Some(active_turn) = active.as_mut() else {
                    return TerminalSchedule::NotFound;
                };
                let Some(coordinator) = active_turn.terminal.as_ref().cloned() else {
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
                    coordinator: finalizer_coordinator,
                    outcome,
                    permit: Some(permit),
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

        let (turn_had_memory_citation, token_usage_at_turn_start) = {
            let ts = finalization.turn_state.lock().await;
            (ts.has_memory_citation, ts.token_usage_at_turn_start.clone())
        };
        let turn_tool_calls = turn_context.turn_timing_state.tool_call_count();
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
            emit_turn_tool_call_metric(&self.services.session_telemetry, turn_tool_calls, tmp_mem);
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
        let completion = if abort_reason.is_none() {
            self.services.task_evidence.completion_gate().await
        } else {
            None
        };
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
        let completed_at = timing_snapshot.completed_at_unix_secs;
        let duration_ms = timing_snapshot.duration_ms;
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
                completion,
                completed_at,
                duration_ms,
                time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                timing: Some(timing),
            })
        };
        self.send_event(turn_context.as_ref(), event).await;
        if let Some(permit) = finalization.permit.as_ref() {
            permit.mark_terminal_event_dispatched();
        }
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
                true
            } else {
                false
            }
        };
        if cleared_active_turn {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
        // Ordered append receipts place all regular items and this terminal event in JSONL order.
        // This is the turn's single durability barrier and therefore includes the terminal receipt.
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
                    completion: None,
                    completed_at: timing_snapshot.completed_at_unix_secs,
                    duration_ms: timing_snapshot.duration_ms,
                    time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                    timing: Some(timing),
                })
            };
            self.send_event(turn_context.as_ref(), event).await;
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_terminal_event_dispatched();
            }
        }

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

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
