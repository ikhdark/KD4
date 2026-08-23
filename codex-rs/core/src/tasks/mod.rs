mod compact;
#[path = "completion_review_v2.rs"]
pub(crate) mod completion_review;
mod lifecycle;
mod regular;
mod review;
mod user_shell;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_extension_api::ExtensionData;
use futures::FutureExt;
use futures::future::BoxFuture;
use sha2::Digest;
use sha2::Sha256;
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
use crate::state::TerminalDeliveryState as CoordinatorDeliveryState;
use crate::state::TurnState;
use crate::state::TurnTerminalCoordinator;
use crate::state::TurnTerminalPermit;
use crate::task_evidence::AuthoritativeTerminalEventV1;
use crate::task_evidence::CandidateDiffSnapshotV1;
use crate::task_evidence::CompletionCheckpointV1;
use crate::task_evidence::FinalProofSealInputV1;
use crate::task_evidence::FinalProofSealResultV1;
use crate::task_evidence::TerminalClaimResult;
use crate::task_evidence::TerminalDecisionClaim;
use crate::task_evidence::TerminalDeliveryState as DurableDeliveryState;
use crate::task_evidence::TerminalInteractionUpdate;
use crate::task_evidence::TerminalRecoveryState;
use crate::task_evidence::TerminalRolloutRepairV1;
use crate::task_evidence::TerminalizationReceiptSnapshot;
use crate::terminal_event_fingerprint;
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
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TerminalizationDeliveryState;
use codex_protocol::protocol::TerminalizationRecoveryState;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnTerminalizationCompleteEvent;
use codex_protocol::protocol::TurnTerminalizationReceipt;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingTerminalization;

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
    crate::tools::parallel::TOOL_RUNTIME_CANCELLATION_GRACE
        .saturating_add(GRACEFUL_INTERRUPTION_MARGIN);
const TERMINAL_MUTATION_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(1);
// Sealing is the only terminal phase that may need to collect and persist the
// complete proof bundle. Give it a little more room without lengthening every
// terminal cleanup operation.
const FINAL_PROOF_CANDIDATE_SEAL_TIMEOUT: Duration = Duration::from_secs(5);
// This correctness budget begins after the admission/lock fence. Fence wait is recorded as the
// initial terminal phase, but does not consume the post-admission terminalization budget.
const TERMINALIZATION_DEADLINE: Duration = Duration::from_secs(5);
const TERMINAL_REPAIR_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const TERMINAL_REPAIR_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const TERMINAL_REPAIR_MAX_BACKOFF: Duration = Duration::from_millis(800);
const TERMINAL_REPAIR_MAX_ATTEMPTS: usize = 5;
const TASK_COMPACT_METRIC: &str = "codex.task.compact";

#[derive(Debug, Default)]
struct TerminalRepairRetry {
    failed_attempts: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TerminalRepairFailure {
    Transient,
    Permanent,
}

impl TerminalRepairRetry {
    fn retry_delay_after_failure(&mut self, failure: TerminalRepairFailure) -> Option<Duration> {
        if failure == TerminalRepairFailure::Permanent {
            return None;
        }
        self.failed_attempts = self.failed_attempts.saturating_add(1);
        if self.failed_attempts >= TERMINAL_REPAIR_MAX_ATTEMPTS {
            return None;
        }
        let shift = u32::try_from(self.failed_attempts.saturating_sub(1)).unwrap_or(u32::MAX);
        Some(
            TERMINAL_REPAIR_INITIAL_BACKOFF
                .saturating_mul(1_u32.checked_shl(shift).unwrap_or(u32::MAX))
                .min(TERMINAL_REPAIR_MAX_BACKOFF),
        )
    }
}

fn classify_terminal_repair_io_error(error: &std::io::Error) -> TerminalRepairFailure {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::Unsupported
        | std::io::ErrorKind::NotADirectory
        | std::io::ErrorKind::IsADirectory => TerminalRepairFailure::Permanent,
        _ => TerminalRepairFailure::Transient,
    }
}

fn pending_terminal_recovery_state(delivery_state: DurableDeliveryState) -> TerminalRecoveryState {
    match delivery_state {
        DurableDeliveryState::Delivered => TerminalRecoveryState::None,
        DurableDeliveryState::NotAttempted
        | DurableDeliveryState::Claimed
        | DurableDeliveryState::DeliveryFailed => TerminalRecoveryState::Pending,
    }
}

fn protocol_terminalization_receipt(
    snapshot: TerminalizationReceiptSnapshot,
) -> TurnTerminalizationReceipt {
    TurnTerminalizationReceipt {
        terminal_identity: snapshot.terminal_identity,
        terminalization: snapshot.terminalization,
        delivery_state: match snapshot.delivery_state {
            DurableDeliveryState::NotAttempted => TerminalizationDeliveryState::NotAttempted,
            DurableDeliveryState::Claimed => TerminalizationDeliveryState::Claimed,
            DurableDeliveryState::Delivered => TerminalizationDeliveryState::Delivered,
            DurableDeliveryState::DeliveryFailed => TerminalizationDeliveryState::DeliveryFailed,
        },
        active_turn_detached: snapshot.active_turn_detached,
        terminal_interaction_released: snapshot.terminal_interaction_released,
        recovery_state: match snapshot.recovery_state {
            TerminalRecoveryState::None => TerminalizationRecoveryState::None,
            TerminalRecoveryState::Pending => TerminalizationRecoveryState::Pending,
            TerminalRecoveryState::Recovered => TerminalizationRecoveryState::Recovered,
        },
        deadline_exhausted_phase: snapshot.deadline_exhausted_phase,
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TurnTaskResult {
    pub(crate) last_agent_message: Option<String>,
    pub(crate) surfaced_result: Option<codex_protocol::protocol::SurfacedToolResult>,
}

pub(crate) type SessionTaskResult = CodexResult<TurnTaskResult>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalWaitError {
    OperationTimedOut,
    DeadlineExhausted,
}

#[derive(Clone)]
struct TerminalDeadline {
    started: tokio::time::Instant,
    deadline: tokio::time::Instant,
    exhausted_phase: Arc<std::sync::Mutex<Option<String>>>,
    phase_timings_ns: Arc<std::sync::Mutex<BTreeMap<String, u64>>>,
}

impl TerminalDeadline {
    fn start() -> Self {
        let started = tokio::time::Instant::now();
        Self {
            started,
            deadline: started + TERMINALIZATION_DEADLINE,
            exhausted_phase: Arc::new(std::sync::Mutex::new(None)),
            phase_timings_ns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    fn start_with_initial_phase(phase: &'static str, phase_started: tokio::time::Instant) -> Self {
        let deadline_started = tokio::time::Instant::now();
        let deadline = Self {
            started: phase_started,
            deadline: deadline_started + TERMINALIZATION_DEADLINE,
            exhausted_phase: Arc::new(std::sync::Mutex::new(None)),
            phase_timings_ns: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        };
        deadline.record_elapsed(
            phase,
            deadline_started.saturating_duration_since(phase_started),
        );
        deadline
    }

    fn remaining(&self, per_operation_limit: Duration) -> Option<Duration> {
        let now = tokio::time::Instant::now();
        (now < self.deadline).then(|| per_operation_limit.min(self.deadline - now))
    }

    async fn run<T, F>(
        &self,
        phase: &'static str,
        per_operation_limit: Duration,
        future: F,
    ) -> Result<T, TerminalWaitError>
    where
        F: Future<Output = T>,
    {
        let Some(limit) = self.remaining(per_operation_limit) else {
            self.record_exhausted(phase);
            return Err(TerminalWaitError::DeadlineExhausted);
        };
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(limit, future).await;
        let elapsed = tokio::time::Instant::now().saturating_duration_since(started);
        let elapsed_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let mut timings = self
            .phase_timings_ns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timings
            .entry(phase.to_string())
            .and_modify(|value| *value = value.saturating_add(elapsed_ns))
            .or_insert(elapsed_ns);
        drop(timings);
        match result {
            Ok(value) => Ok(value),
            Err(_) if tokio::time::Instant::now() >= self.deadline => {
                self.record_exhausted(phase);
                Err(TerminalWaitError::DeadlineExhausted)
            }
            Err(_) => Err(TerminalWaitError::OperationTimedOut),
        }
    }

    fn record_exhausted(&self, phase: &'static str) {
        let mut exhausted = self
            .exhausted_phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if exhausted.is_none() {
            *exhausted = Some(phase.to_string());
        }
    }

    fn record_elapsed(&self, phase: &'static str, elapsed: Duration) {
        let elapsed_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let mut timings = self
            .phase_timings_ns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timings
            .entry(phase.to_string())
            .and_modify(|value| *value = value.saturating_add(elapsed_ns))
            .or_insert(elapsed_ns);
    }

    fn finish_unclassified(&self) {
        let total_ns = u64::try_from(
            tokio::time::Instant::now()
                .saturating_duration_since(self.started)
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);
        let mut timings = self
            .phase_timings_ns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let classified_ns = timings
            .iter()
            .filter(|(phase, _)| phase.as_str() != "unclassified")
            .fold(0_u64, |total, (_, elapsed)| total.saturating_add(*elapsed));
        timings.insert(
            "unclassified".to_string(),
            total_ns.saturating_sub(classified_ns),
        );
    }

    fn exhausted_phase(&self) -> Option<String> {
        self.exhausted_phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn phase_timings_ns(&self) -> BTreeMap<String, u64> {
        self.phase_timings_ns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn apply_terminal_phase_timings(event: &mut EventMsg, phases: &BTreeMap<String, u64>) {
    let timing = match event {
        EventMsg::TurnComplete(event) => event.timing.as_mut(),
        EventMsg::TurnAborted(event) => event.timing.as_mut(),
        _ => None,
    };
    let Some(timing) = timing else {
        return;
    };
    apply_terminal_phase_timings_to_timing(timing, phases);
}

fn apply_terminal_phase_timings_to_timing(timing: &mut TurnTiming, phases: &BTreeMap<String, u64>) {
    let terminalization = &mut timing.terminalization;
    terminalization.final_mutation_to_seal_ns = phases
        .get("hooks_quiescence")
        .copied()
        .unwrap_or_default()
        .saturating_add(phases.get("fence").copied().unwrap_or_default())
        .saturating_add(phases.get("final_proof_gate").copied().unwrap_or_default());
    terminalization.completion_gate_ns =
        phases.get("final_proof_gate").copied().unwrap_or_default();
    terminalization.review_preflight_ns = terminalization
        .review_preflight_ns
        .max(phases.get("review_preflight").copied().unwrap_or_default());
    terminalization.review_ns = terminalization
        .review_ns
        .max(phases.get("review").copied().unwrap_or_default());
    terminalization.terminal_memo_hit_count = terminalization
        .terminal_memo_hit_count
        .max(u32::from(phases.contains_key("terminal_memo_hit")));
    terminalization.diff_refresh_count = terminalization
        .diff_refresh_count
        .max(u32::from(phases.contains_key("diff_refresh")));
    terminalization.preparation_ns = phases.get("preparation").copied().unwrap_or_default();
    terminalization.hooks_quiescence_ns =
        phases.get("hooks_quiescence").copied().unwrap_or_default();
    terminalization.fence_ns = phases.get("fence").copied().unwrap_or_default();
    terminalization.freshness_ns = phases.get("freshness").copied().unwrap_or_default();
    terminalization.gate_ns = phases.get("gate").copied().unwrap_or_default();
    terminalization.durable_commit_ns = phases.get("durable_commit").copied().unwrap_or_default();
    terminalization.delivery_attempt_ns =
        phases.get("delivery_attempt").copied().unwrap_or_default();
    terminalization.interaction_release_ns = phases
        .get("interaction_release")
        .copied()
        .unwrap_or_default();
    terminalization.post_cleanup_ns = phases.get("post_cleanup").copied().unwrap_or_default();
    terminalization.unclassified_ns = phases.get("unclassified").copied().unwrap_or_default();
}

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
    Completed { result: TurnTaskResult },
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
    deadline: TerminalDeadline,
    mutation_quiescent: bool,
    pending_turn_profile: Option<TurnProfileFact>,
}

struct TerminalInteractionMilestone {
    live_attempted: bool,
    live_delivered: bool,
    cleared_active_turn: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurableSideEffectStep {
    RunSideEffect,
    PersistReceipt,
    Complete,
}

fn durable_side_effect_step(
    side_effect_completed: bool,
    receipt_persisted: bool,
) -> DurableSideEffectStep {
    if receipt_persisted {
        DurableSideEffectStep::Complete
    } else if side_effect_completed {
        DurableSideEffectStep::PersistReceipt
    } else {
        DurableSideEffectStep::RunSideEffect
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalPublicationDecision {
    Publish,
    DeferForRolloutRepair,
}

fn terminal_publication_decision(rollout_structure_ready: bool) -> TerminalPublicationDecision {
    if rollout_structure_ready {
        TerminalPublicationDecision::Publish
    } else {
        TerminalPublicationDecision::DeferForRolloutRepair
    }
}

fn terminal_rollout_structure_ready(
    missing_call_output_repair_succeeded: bool,
    required_rollout_repairs_complete: bool,
) -> bool {
    missing_call_output_repair_succeeded && required_rollout_repairs_complete
}

fn downgrade_for_required_finalization_memo(
    gate: &mut TaskCompletionGate,
    memo_persisted: bool,
    failure_reason: &str,
) -> bool {
    if gate.status != TaskCompletionStatus::Passed || memo_persisted {
        return false;
    }
    gate.status = TaskCompletionStatus::Partial;
    gate.reasons.push(failure_reason.to_string());
    gate.reasons.sort();
    gate.reasons.dedup();
    true
}

fn atomic_review_transition_persisted<T>(
    transition: &Result<crate::task_evidence::AtomicReviewTransition<T>, TerminalWaitError>,
) -> bool {
    matches!(
        transition,
        Ok(crate::task_evidence::AtomicReviewTransition::Persisted(_))
    )
}

fn select_terminal_authority<T>(durable: Option<T>, candidate: T) -> (T, bool) {
    match durable {
        Some(authority) => (authority, true),
        None => (candidate, false),
    }
}

async fn seal_terminal_final_proof(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    last_agent_message: Option<&str>,
    child_gate_state: Vec<String>,
) -> Option<FinalProofSealResultV1> {
    if !session.services.task_evidence.allows_kd4_completion() {
        return None;
    }
    let authoritative = completion_review::inspect_authoritative_review_inputs(session).await;
    let dossier = session
        .services
        .task_evidence
        .completion_review_dossier(
            last_agent_message,
            &authoritative.typed_mutation_identities,
            &authoritative.typed_evidence,
            &authoritative.review_lens_selection_facts,
            &authoritative.partial_reasons,
            authoritative.typed_quiescent,
            authoritative.default_children_quiescent,
        )
        .await?;
    let identity_snapshot = session
        .services
        .task_evidence
        .final_proof_identity_snapshot()
        .await?;
    let workspace_path_snapshot_identity =
        identity_snapshot.workspace_path_snapshot_identity.clone();
    let workspace_epoch = session
        .services
        .command_execution
        .observe_repository_revision(
            &turn_context.sub_id,
            identity_snapshot.host_mutation_revision,
        )
        .await;
    let capture =
        crate::git_workspace::capture_candidate_diff(turn_context.config.cwd.as_path()).await;
    let (head_identity, index_identity, worktree_identity, changed_paths, raw_diff) =
        if let Some(capture) = capture {
            (
                capture.head_identity,
                capture.index_identity,
                capture.worktree_identity,
                capture.changed_paths,
                capture.raw_diff,
            )
        } else {
            (
                None,
                None,
                None,
                identity_snapshot.changed_paths,
                Vec::new(),
            )
        };
    let raw_artifact_digest = format!("{:x}", Sha256::digest(&raw_diff));
    let raw_artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
        turn_context.config.codex_home.as_path(),
        &session.thread_id.to_string(),
        &raw_diff,
    )
    .await;
    let raw_artifact_ref = raw_artifact
        .model_projection()
        .0
        .map(|artifact_id| artifact_id.to_string());
    let diff_identity = final_proof_hash(
        "KD4_CANDIDATE_DIFF_IDENTITY_V1",
        &serde_json::json!({
            "head": &head_identity,
            "index": &index_identity,
            "worktree": &worktree_identity,
            "changed_paths": &changed_paths,
            "raw_artifact_digest": &raw_artifact_digest,
            "workspace_path_snapshots": &workspace_path_snapshot_identity,
        }),
    );
    let workspace_manifest_identity = final_proof_hash(
        "KD4_FINAL_PROOF_WORKSPACE_MANIFEST_V1",
        &serde_json::json!({
            "head": &head_identity,
            "index": &index_identity,
            "worktree": &worktree_identity,
            "changed_paths": &changed_paths,
            "workspace_path_snapshots": &workspace_path_snapshot_identity,
        }),
    );
    let bounded_hunks = String::from_utf8_lossy(&raw_diff).into_owned();
    let context_window = turn_context
        .model_context_window()
        .unwrap_or_default()
        .max(0);
    let used_tokens = session.get_total_token_usage().await.max(0);
    let reserved_tokens = 6_144_i64.saturating_add(context_window / 5);
    let checkpoint_token_budget = usize::try_from(
        context_window
            .saturating_sub(used_tokens)
            .saturating_sub(reserved_tokens)
            .min(10_000),
    )
    .unwrap_or_default();
    let environment_identity = final_proof_hash(
        "KD4_FINAL_PROOF_ENVIRONMENT_V1",
        &format!("{:?}", turn_context.environments),
    );
    let toolchain_identity = final_proof_hash(
        "KD4_FINAL_PROOF_TOOLCHAIN_V1",
        &serde_json::json!({
            "rustup_toolchain": std::env::var("RUSTUP_TOOLCHAIN").ok(),
            "target": std::env::var("CARGO_BUILD_TARGET").ok(),
        }),
    );
    let features_identity = final_proof_hash(
        "KD4_FINAL_PROOF_FEATURES_V1",
        &format!("{:?}", turn_context.config.features.get()),
    );
    let configuration_identity = final_proof_hash(
        "KD4_FINAL_PROOF_CONFIGURATION_V1",
        &serde_json::json!({
            "cwd": &turn_context.config.cwd,
            "approval_policy": format!("{:?}", turn_context.approval_policy.value()),
            "sandbox_policy": format!("{:?}", turn_context.sandbox_policy()),
            "model": &turn_context.model_info.slug,
            "output_schema": &turn_context.final_output_json_schema,
        }),
    );
    session
        .services
        .task_evidence
        .seal_final_proof_candidate(FinalProofSealInputV1 {
            implementation_identity: dossier.implementation_identity_hash,
            source_identity: dossier.user_source_ledger_hash,
            requirement_identity: dossier.requirement_manifest_hash,
            workspace_epoch,
            workspace_manifest_identity,
            environment_identity,
            toolchain_identity,
            features_identity,
            configuration_identity,
            child_gate_state,
            reviewer_configuration_identity:
                completion_review::completion_review_configuration_identity(turn_context),
            typed_validation_proofs: authoritative.typed_validation_proofs,
            diff_snapshot: CandidateDiffSnapshotV1 {
                candidate_id: String::new(),
                diff_identity,
                head_identity,
                index_identity,
                worktree_identity,
                changed_paths,
                bounded_hunks,
                raw_artifact_digest,
                raw_artifact_ref,
                workspace_epoch,
            },
            checkpoint_token_budget,
        })
        .await
}

fn final_proof_hash(label: &str, value: &impl serde::Serialize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(value).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

struct WorkerDoneNotifier(Arc<Notify>);

impl Drop for WorkerDoneNotifier {
    fn drop(&mut self) {
        // `notify_one` retains a permit when the abort finalizer has not started waiting yet.
        self.0.notify_one();
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
        if self.terminal_interaction_pending.load(Ordering::Acquire)
            || self
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
        let mut startup_guard = TasklessTurnStartupGuard::new(self, Arc::clone(&turn_state));
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
        };
        turn.task = Some(running_task);
        turn.terminal = Some(terminal);
        drop(active);
        startup_guard.disarm();
        let _ = start_tx.send(());
    }

    pub(crate) async fn clear_taskless_placeholder(
        &self,
        expected_turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        let cleared = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.as_ref().is_some_and(|active_turn| {
                active_turn.task.is_none()
                    && active_turn.terminal.is_none()
                    && Arc::ptr_eq(&active_turn.turn_state, expected_turn_state)
            }) {
                *active_turn = None;
                true
            } else {
                false
            }
        };
        if cleared {
            self.emit_thread_idle_lifecycle_if_idle().await;
        }
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
            } else {
                None
            }
        };
        let Some(recovered_input) = recovered_input else {
            return;
        };
        let recovered_work = !recovered_input.is_empty();
        self.input_queue
            .recover_cancelled_startup_input(recovered_input)
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
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
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
        self.start_task(turn_context, Vec::new(), RegularTask::new())
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
            let terminal_fence_started = tokio::time::Instant::now();
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
                self.terminal_interaction_pending
                    .store(true, Ordering::Release);
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
            // The supervisor has accepted the final task result at this point. Preserve the
            // admission-fence timing origin, but start the correctness deadline here so lock
            // contention cannot shorten the finalizer's five-second window.
            let terminal_deadline =
                TerminalDeadline::start_with_initial_phase("fence", terminal_fence_started);
            self.terminal_tasks.spawn(
            async move {
                let mut finalization = TerminalFinalization {
                    task,
                    turn_state,
                    reasoning_policy_recorder,
                    coordinator: finalizer_coordinator,
                    outcome,
                    permit: Some(permit),
                    deadline: terminal_deadline,
                    mutation_quiescent: true,
                    pending_turn_profile: None,
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
                if !finalization.coordinator.interaction_released()
                    && AssertUnwindSafe(session.detach_terminal_turn(&mut finalization))
                        .catch_unwind()
                        .await
                        .is_err()
                {
                    warn!(
                        turn_id = %terminal_turn_id,
                        "emergency active-turn detachment panicked"
                    );
                    // Last-resort interaction release: do not leave admission permanently
                    // closed merely because secondary fail-safe bookkeeping also failed.
                    session
                        .terminal_interaction_pending
                        .store(false, Ordering::Release);
                    if let Some(permit) = finalization.permit.as_ref() {
                        permit.mark_interaction_released();
                    }
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
        let started = tokio::time::Instant::now();
        drop(finalization.task._agent_execution_guard.take());
        let active_turn_detached = {
            let mut active = self.active_turn.lock().await;
            if let Some(active_turn) = active.as_ref()
                && active_turn.task.is_none()
                && Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
            {
                *active = None;
            }
            let detached = active.as_ref().is_none_or(|active_turn| {
                !Arc::ptr_eq(&active_turn.turn_state, &finalization.turn_state)
            });
            // Open admission before notifying waiters that interaction release completed.
            // A caller may submit the next turn immediately after `TurnComplete`; waking its
            // `abort_all_tasks` waiter while this fence is still set makes `start_task` discard
            // that already-accepted turn.
            self.terminal_interaction_pending
                .store(false, Ordering::Release);
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_interaction_released();
            }
            detached
        };
        finalization.deadline.record_elapsed(
            "interaction_release",
            tokio::time::Instant::now().saturating_duration_since(started),
        );
        active_turn_detached
    }

    fn emit_pending_turn_profile(
        &self,
        finalization: &mut TerminalFinalization,
    ) -> Option<TurnTimingTerminalization> {
        let phases = finalization.deadline.phase_timings_ns();
        let pending = finalization.pending_turn_profile.as_mut()?;
        if let Some(timing) = pending.timing.as_mut() {
            apply_terminal_phase_timings_to_timing(timing, &phases);
        }
        let terminalization = pending
            .timing
            .as_ref()
            .map(|timing| timing.terminalization.clone());
        let Some(permit) = finalization.permit.as_ref() else {
            return terminalization;
        };
        if !permit.try_claim_analytics_emission() {
            return terminalization;
        }
        let pending = finalization.pending_turn_profile.take()?;
        self.services
            .analytics_events_client
            .track_turn_profile(pending);
        terminalization
    }

    async fn publish_terminal_interaction_milestone(
        self: &Arc<Self>,
        finalization: &mut TerminalFinalization,
        turn_context: &TurnContext,
        event: &mut EventMsg,
        durable_outcome: String,
        durable_success_established: bool,
        rollout_structure_ready: bool,
        rollout_repair_items: Vec<ResponseItem>,
    ) -> TerminalInteractionMilestone {
        let terminal_identity = format!("{}:{}", self.thread_id, turn_context.sub_id);
        apply_terminal_phase_timings(event, &finalization.deadline.phase_timings_ns());
        let Some(candidate_fingerprint) = terminal_event_fingerprint(event) else {
            warn!(turn_id = %turn_context.sub_id, "refusing to publish a non-terminal event through terminalization");
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        };
        let final_proof_identity = self
            .services
            .task_evidence
            .current_finalization_memo_identity()
            .await;
        if terminal_publication_decision(rollout_structure_ready)
            == TerminalPublicationDecision::DeferForRolloutRepair
        {
            let candidate_authority = AuthoritativeTerminalEventV1 {
                version: 1,
                terminal_identity: terminal_identity.clone(),
                turn_id: turn_context.sub_id.clone(),
                event: event.clone(),
                fingerprint: candidate_fingerprint.clone(),
                semantic_outcome: durable_outcome.clone(),
                final_proof_identity: final_proof_identity.clone(),
                rollout_repair: TerminalRolloutRepairV1 {
                    items: rollout_repair_items,
                    repair_missing_call_outputs: true,
                },
            };
            let claim = TerminalDecisionClaim {
                authoritative_event: candidate_authority.clone(),
                deadline_exhausted_phase: finalization.deadline.exhausted_phase(),
                mutation_quiescent: finalization.mutation_quiescent,
                durable_success_established,
                retained_ownership: Vec::new(),
                phase_timings_ns: finalization.deadline.phase_timings_ns(),
            };
            let claim_result = finalization
                .deadline
                .run(
                    "durable_commit",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .task_evidence
                        .commit_terminal_decision_and_claim(claim.clone()),
                )
                .await;
            let authority = match claim_result {
                Ok(TerminalClaimResult::Claimed(authority))
                | Ok(TerminalClaimResult::AlreadyClaimed(authority))
                    if authority.fingerprint == candidate_fingerprint =>
                {
                    Some(authority)
                }
                Ok(TerminalClaimResult::Conflict {
                    authoritative: Some(authority),
                    ..
                }) => Some(authority),
                _ => None,
            };
            if let Some(authority) = authority {
                self.schedule_terminal_after_rollout_repair(
                    Arc::clone(&finalization.task.turn_context),
                    authority,
                );
            } else {
                warn!(turn_id = %turn_context.sub_id, "could not durably bind terminal candidate before rollout repair deferral");
                self.schedule_terminal_claim_then_rollout_repair(
                    Arc::clone(&finalization.task.turn_context),
                    candidate_authority,
                    claim,
                );
                return TerminalInteractionMilestone {
                    live_attempted: false,
                    live_delivered: false,
                    // Retain the active turn fence while the exact candidate exists only
                    // in memory. A successful retry performs the normal recovered handoff.
                    cleared_active_turn: false,
                };
            }
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        }
        let candidate_authority = AuthoritativeTerminalEventV1 {
            version: 1,
            terminal_identity: terminal_identity.clone(),
            turn_id: turn_context.sub_id.clone(),
            event: event.clone(),
            fingerprint: candidate_fingerprint.clone(),
            semantic_outcome: durable_outcome,
            final_proof_identity,
            rollout_repair: TerminalRolloutRepairV1::default(),
        };
        let claim = TerminalDecisionClaim {
            authoritative_event: candidate_authority.clone(),
            deadline_exhausted_phase: finalization.deadline.exhausted_phase(),
            mutation_quiescent: true,
            durable_success_established,
            retained_ownership: Vec::new(),
            phase_timings_ns: finalization.deadline.phase_timings_ns(),
        };
        let claim_result = finalization
            .deadline
            .run(
                "durable_commit",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services
                    .task_evidence
                    .commit_terminal_decision_and_claim(claim.clone()),
            )
            .await;
        let mut candidate_rollout_fallback_allowed = true;
        let mut rollout_mirrored = false;
        let mut authority = match claim_result {
            Ok(TerminalClaimResult::Claimed(authority))
            | Ok(TerminalClaimResult::AlreadyClaimed(authority)) => Some(authority),
            Ok(TerminalClaimResult::Conflict {
                authoritative: Some(authority),
                candidate_fingerprint,
            }) => {
                warn!(
                    turn_id = %turn_context.sub_id,
                    authoritative_fingerprint = %authority.fingerprint,
                    %candidate_fingerprint,
                    "rejected conflicting terminal outcome; preserving the first authoritative event"
                );
                Some(authority)
            }
            Ok(TerminalClaimResult::Conflict {
                authoritative: None,
                candidate_fingerprint,
            }) => {
                warn!(turn_id = %turn_context.sub_id, %candidate_fingerprint, "legacy terminal claim has no exact event; refusing to guess an outcome");
                candidate_rollout_fallback_allowed = false;
                None
            }
            Ok(TerminalClaimResult::Failed)
            | Err(TerminalWaitError::OperationTimedOut)
            | Err(TerminalWaitError::DeadlineExhausted) => None,
        };

        // Task evidence is the preferred authority store. If it was unavailable, a successful
        // rollout append establishes the exact same candidate as authority. Never append a
        // degraded correction after either store accepted the candidate.
        if authority.is_none() && candidate_rollout_fallback_allowed {
            if let Some(existing) = self
                .reconcile_terminal_authority_from_rollout(&candidate_authority)
                .await
            {
                authority = Some(existing);
                rollout_mirrored = true;
            } else {
                let rollout = finalization
                    .deadline
                    .run(
                        "durable_commit",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.persist_terminal_event_for_dispatch(
                            &candidate_authority.event,
                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        ),
                    )
                    .await;
                if matches!(rollout, Ok(Ok(()))) {
                    authority = Some(candidate_authority.clone());
                    rollout_mirrored = true;
                } else {
                    authority = self
                        .reconcile_terminal_authority_from_rollout(&candidate_authority)
                        .await;
                    rollout_mirrored = authority.is_some();
                }
            }
        } else if let Some(authoritative) = authority.as_ref() {
            if let Some(existing) = self
                .reconcile_terminal_authority_from_rollout(authoritative)
                .await
            {
                rollout_mirrored = existing.fingerprint == authoritative.fingerprint;
                if !rollout_mirrored {
                    warn!(turn_id = %turn_context.sub_id, "rollout contains a conflicting terminal event; refusing to append another outcome");
                }
            } else {
                let rollout = finalization
                    .deadline
                    .run(
                        "durable_commit",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.persist_terminal_event_for_dispatch(
                            &authoritative.event,
                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        ),
                    )
                    .await;
                if !matches!(rollout, Ok(Ok(()))) {
                    warn!(turn_id = %turn_context.sub_id, "rollout mirroring failed after the terminal outcome became authoritative");
                } else {
                    rollout_mirrored = true;
                }
            }
        }

        if authority.is_none() && !candidate_rollout_fallback_allowed {
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_durable_commit(true);
            }
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        }
        let (authority, foreground_durable_authority) =
            select_terminal_authority(authority, candidate_authority);
        if !foreground_durable_authority {
            warn!(
                turn_id = %turn_context.sub_id,
                "terminal authority persistence missed the foreground deadline; preserving the exact candidate and scheduling durable recovery"
            );
            self.schedule_terminal_authority_persistence(authority.clone(), claim);
        }

        *event = authority.event.clone();
        let mut permit_delivery_claimed = false;
        if let Some(permit) = finalization.permit.as_ref() {
            // This exact candidate now owns the process-local terminal slot. Treat the
            // terminal milestone as committed so fail-safe cleanup does not invalidate the
            // valid result while the background task mirrors that same authority durably.
            permit.mark_durable_commit(true);
            permit_delivery_claimed = permit.mark_delivery_claimed();
        }
        if !self
            .register_authoritative_terminal_delivery(
                terminal_identity.clone(),
                authority.fingerprint.clone(),
            )
            .await
        {
            warn!(turn_id = %turn_context.sub_id, "process-local terminal registry rejected a conflicting fingerprint");
        }

        // Release the completed turn before its terminal event becomes observable. A client may
        // submit its next turn as soon as it receives `TurnComplete`; leaving the old turn
        // attached until after live delivery makes that input look steerable, then drops it when
        // terminal detachment clears the old turn.
        let cleared_active_turn = self.detach_terminal_turn(finalization).await;

        let delivery_started = tokio::time::Instant::now();
        let live_delivered = self
            .dispatch_terminal_event_live(
                turn_context,
                authority.event.clone(),
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            )
            .await;
        finalization.deadline.record_elapsed(
            "delivery_attempt",
            tokio::time::Instant::now().saturating_duration_since(delivery_started),
        );
        if permit_delivery_claimed && let Some(permit) = finalization.permit.as_ref() {
            permit.mark_delivery_attempted(live_delivered);
        }
        let requires_app_server_ack = turn_context.app_server_client_name.is_some();
        let redelivery = (requires_app_server_ack || !live_delivered)
            .then(|| (authority.clone(), requires_app_server_ack));

        let delivery_state = if live_delivered {
            DurableDeliveryState::Delivered
        } else {
            DurableDeliveryState::DeliveryFailed
        };
        let update = TerminalInteractionUpdate {
            terminal_identity,
            delivery_state,
            app_server_acknowledged: false,
            runtime_status_converged: true,
            rollout_mirrored,
            parent_notification_completed: false,
            post_terminal_cleanup_completed: false,
            active_turn_detached: cleared_active_turn,
            terminal_interaction_released: cleared_active_turn,
            recovery_state: TerminalRecoveryState::None,
            phase_timings_ns: finalization.deadline.phase_timings_ns(),
            terminalization: None,
        };
        match tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.services
                .task_evidence
                .update_terminal_interaction(update.clone()),
        )
        .await
        {
            Ok(true) => {
                if let Some((authority, require_app_server_ack)) = redelivery {
                    self.schedule_terminal_redelivery(authority, require_app_server_ack);
                }
            }
            Ok(false) | Err(_) => {
                warn!(
                    turn_id = %turn_context.sub_id,
                    "terminal interaction receipt remains pending; retrying persistence before any redelivery"
                );
                self.schedule_terminal_interaction_persistence_retry(
                    Arc::clone(&finalization.task.turn_context),
                    update,
                    redelivery,
                );
            }
        }

        TerminalInteractionMilestone {
            live_attempted: true,
            live_delivered,
            cleared_active_turn,
        }
    }

    /// Re-reads rollout storage after an ambiguous append failure. The first exact-turn terminal
    /// event in durable history wins, even when it conflicts with the in-memory candidate.
    async fn reconcile_terminal_authority_from_rollout(
        &self,
        candidate: &AuthoritativeTerminalEventV1,
    ) -> Option<AuthoritativeTerminalEventV1> {
        let history = self
            .live_thread()?
            .load_history(/*include_archived*/ true)
            .await
            .ok()?;
        history.items.into_iter().find_map(|item| {
            let RolloutItem::EventMsg(event) = item else {
                return None;
            };
            let event_turn_id = match &event {
                EventMsg::TurnComplete(event) => Some(event.turn_id.as_str()),
                EventMsg::TurnAborted(event) => event.turn_id.as_deref(),
                _ => return None,
            };
            if event_turn_id != Some(candidate.turn_id.as_str()) {
                return None;
            }
            let fingerprint = terminal_event_fingerprint(&event)?;
            if fingerprint != candidate.fingerprint {
                warn!(
                    turn_id = %candidate.turn_id,
                    authoritative_fingerprint = %fingerprint,
                    candidate_fingerprint = %candidate.fingerprint,
                    "rollout reconciliation preserved an earlier conflicting terminal event"
                );
            }
            Some(AuthoritativeTerminalEventV1 {
                version: 1,
                terminal_identity: candidate.terminal_identity.clone(),
                turn_id: candidate.turn_id.clone(),
                semantic_outcome: semantic_terminal_outcome(&event),
                event,
                fingerprint,
                final_proof_identity: candidate.final_proof_identity.clone(),
                rollout_repair: TerminalRolloutRepairV1::default(),
            })
        })
    }

    async fn ensure_terminal_authority_mirrored_in_rollout(
        &self,
        authority: &AuthoritativeTerminalEventV1,
    ) -> bool {
        match self
            .reconcile_terminal_authority_from_rollout(authority)
            .await
        {
            Some(existing) => existing.fingerprint == authority.fingerprint,
            None => self
                .persist_terminal_event_for_dispatch(
                    &authority.event,
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                )
                .await
                .is_ok(),
        }
    }

    fn schedule_terminal_interaction_persistence_retry(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        update: TerminalInteractionUpdate,
        redelivery: Option<(AuthoritativeTerminalEventV1, bool)>,
    ) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    warn!(turn_id = %turn_context.sub_id, "terminal receipt persistence stopped at shutdown");
                    return;
                }
                let persisted = tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .update_terminal_interaction(update.clone()),
                )
                .await;
                if matches!(persisted, Ok(true)) {
                    session
                        .emit_terminalization_receipt(
                            turn_context.as_ref(),
                            &update.terminal_identity,
                        )
                        .await;
                    if let Some((authority, require_app_server_ack)) = redelivery {
                        session.schedule_terminal_redelivery(authority, require_app_server_ack);
                    }
                    return;
                }
                let Some(delay) =
                    retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!(turn_id = %turn_context.sub_id, "terminal receipt persistence retry budget exhausted");
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    fn schedule_completion_invalidation_retry(self: &Arc<Self>, reason: String) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    warn!(%reason, "completion invalidation persistence stopped at shutdown");
                    return;
                }
                let persisted = tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .invalidate_completion_after_terminal_emission_failure(&reason),
                )
                .await;
                if matches!(
                    persisted,
                    Ok(crate::task_evidence::AtomicReviewTransition::Persisted(()))
                ) {
                    return;
                }
                let Some(delay) = retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!(%reason, "completion invalidation persistence retry budget exhausted");
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    fn schedule_provisional_review_supersession_retry(self: &Arc<Self>) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    warn!("provisional completion-review supersession stopped at shutdown");
                    return;
                }
                let persisted = tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .supersede_current_provisional_completion_review(),
                )
                .await;
                if matches!(
                    persisted,
                    Ok(crate::task_evidence::AtomicReviewTransition::Persisted(()))
                ) {
                    return;
                }
                let Some(delay) = retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!("provisional completion-review supersession retry budget exhausted");
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    fn schedule_terminal_after_rollout_repair(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        authority: AuthoritativeTerminalEventV1,
    ) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            debug_assert_eq!(turn_context.sub_id, authority.turn_id);
            session
                .recover_authoritative_terminal_delivery(
                    authority, /*require_app_server_ack*/ false,
                )
                .await;
        });
    }

    fn schedule_terminal_claim_then_rollout_repair(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        candidate: AuthoritativeTerminalEventV1,
        claim: TerminalDecisionClaim,
    ) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    return;
                }
                let result = tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .commit_terminal_decision_and_claim(claim.clone()),
                )
                .await;
                let authority = match result {
                    Ok(TerminalClaimResult::Claimed(authority))
                    | Ok(TerminalClaimResult::AlreadyClaimed(authority))
                        if authority.fingerprint == candidate.fingerprint =>
                    {
                        Some(authority)
                    }
                    Ok(TerminalClaimResult::Conflict {
                        authoritative: Some(authority),
                        ..
                    }) => Some(authority),
                    _ => None,
                };
                if let Some(authority) = authority {
                    debug_assert_eq!(turn_context.sub_id, authority.turn_id);
                    session
                        .recover_authoritative_terminal_delivery(
                            authority,
                            /*require_app_server_ack*/ false,
                        )
                        .await;
                    return;
                }
                let Some(delay) =
                    retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!(turn_id = %candidate.turn_id, "terminal claim retry budget exhausted; retaining the active turn because no durable candidate exists");
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    pub(crate) fn schedule_terminal_redelivery(
        self: &Arc<Self>,
        authority: AuthoritativeTerminalEventV1,
        require_app_server_ack: bool,
    ) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            {
                let mut registry = session.terminal_delivery_registry.lock().await;
                let Some(entry) = registry.by_identity.get_mut(&authority.terminal_identity) else {
                    return;
                };
                if entry.redelivery_scheduled {
                    return;
                }
                entry.redelivery_scheduled = true;
            }
            let mut retry = TerminalRepairRetry::default();
            let mut retry_budget_exhausted = false;
            let mut live_side_effect_completed = false;
            let mut receipt_persisted = false;
            let mut pending_update = None;
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                match durable_side_effect_step(live_side_effect_completed, receipt_persisted) {
                    DurableSideEffectStep::RunSideEffect => {
                        if require_app_server_ack
                            && session
                                .terminal_delivery_acknowledged(
                                    &authority.terminal_identity,
                                    &authority.fingerprint,
                                )
                                .await
                        {
                            break;
                        }
                        if session.try_send_live_event_accepted(Event {
                            id: authority.turn_id.clone(),
                            msg: authority.event.clone(),
                        }) {
                            let released = session
                                .release_terminal_turn_after_live_handoff(&authority.turn_id)
                                .await;
                            pending_update = Some(TerminalInteractionUpdate {
                                terminal_identity: authority.terminal_identity.clone(),
                                delivery_state: DurableDeliveryState::Delivered,
                                app_server_acknowledged: false,
                                runtime_status_converged: true,
                                rollout_mirrored: false,
                                parent_notification_completed: false,
                                post_terminal_cleanup_completed: false,
                                active_turn_detached: released,
                                terminal_interaction_released: released,
                                recovery_state: TerminalRecoveryState::Recovered,
                                phase_timings_ns: BTreeMap::new(),
                                terminalization: None,
                            });
                            live_side_effect_completed = true;
                        }
                    }
                    DurableSideEffectStep::PersistReceipt => {
                        let Some(update) = pending_update.as_ref() else {
                            live_side_effect_completed = false;
                            continue;
                        };
                        receipt_persisted = session
                            .services
                            .task_evidence
                            .update_terminal_interaction(update.clone())
                            .await;
                    }
                    DurableSideEffectStep::Complete => {
                        let turn_context = session
                            .new_default_turn_with_sub_id(authority.turn_id.clone())
                            .await;
                        session
                            .emit_terminalization_receipt(
                                turn_context.as_ref(),
                                &authority.terminal_identity,
                            )
                            .await;
                        if !require_app_server_ack {
                            break;
                        }
                        live_side_effect_completed = false;
                        receipt_persisted = false;
                        pending_update = None;
                    }
                }
                let Some(delay) =
                    retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    retry_budget_exhausted = true;
                    break;
                };
                tokio::time::sleep(delay).await;
            }
            if let Some(entry) = session
                .terminal_delivery_registry
                .lock()
                .await
                .by_identity
                .get_mut(&authority.terminal_identity)
            {
                entry.redelivery_scheduled = false;
            }
            if retry_budget_exhausted {
                warn!(
                    turn_id = %authority.turn_id,
                    "terminal redelivery retry budget exhausted; durable delivery remains pending for reconnect or restart"
                );
            }
        });
    }

    fn schedule_terminal_authority_persistence(
        self: &Arc<Self>,
        authority: AuthoritativeTerminalEventV1,
        claim: TerminalDecisionClaim,
    ) {
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    return;
                }
                let claim_result = tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .commit_terminal_decision_and_claim(claim.clone()),
                )
                .await;
                let claim_matches = match claim_result {
                    Err(_) => false,
                    Ok(result) => match result {
                    TerminalClaimResult::Claimed(existing)
                    | TerminalClaimResult::AlreadyClaimed(existing) => {
                        existing.fingerprint == authority.fingerprint
                    }
                    TerminalClaimResult::Conflict {
                        authoritative: Some(existing),
                        ..
                    } => {
                        if existing.fingerprint != authority.fingerprint {
                            warn!(
                                turn_id = %authority.turn_id,
                                "durable terminal recovery found a conflicting authoritative event"
                            );
                        }
                        return;
                    }
                    TerminalClaimResult::Conflict {
                        authoritative: None,
                        ..
                    } => return,
                    TerminalClaimResult::Failed => false,
                    },
                };
                let rollout_mirrored = matches!(
                    tokio::time::timeout(
                        TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                        session.ensure_terminal_authority_mirrored_in_rollout(&authority),
                    )
                    .await,
                    Ok(true)
                );
                if claim_matches && rollout_mirrored {
                    return;
                }
                let Some(delay) =
                    retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!(turn_id = %authority.turn_id, "terminal authority persistence retry budget exhausted");
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    fn schedule_parent_terminal_notification_retry(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        event: EventMsg,
    ) {
        let Some(fingerprint) = terminal_event_fingerprint(&event) else {
            return;
        };
        let terminal_identity = format!("{}:{}", self.thread_id, turn_context.sub_id);
        let session = Arc::clone(self);
        self.terminal_tasks.spawn(async move {
            let mut retry = TerminalRepairRetry::default();
            let mut notification_completed = false;
            let mut receipt_persisted = false;
            loop {
                if session.shutting_down.load(Ordering::Acquire) {
                    return;
                }
                match durable_side_effect_step(notification_completed, receipt_persisted) {
                    DurableSideEffectStep::RunSideEffect => {
                        if !session
                            .register_authoritative_terminal_delivery(
                                terminal_identity.clone(),
                                fingerprint.clone(),
                            )
                            .await
                        {
                            return;
                        }
                        notification_completed = session
                            .maybe_notify_parent_of_terminal_turn(turn_context.as_ref(), &event)
                            .await;
                    }
                    DurableSideEffectStep::PersistReceipt => {
                        receipt_persisted = session
                            .services
                            .task_evidence
                            .update_terminal_interaction(TerminalInteractionUpdate {
                                terminal_identity: terminal_identity.clone(),
                                delivery_state: DurableDeliveryState::Claimed,
                                app_server_acknowledged: false,
                                runtime_status_converged: true,
                                rollout_mirrored: false,
                                parent_notification_completed: true,
                                post_terminal_cleanup_completed: false,
                                active_turn_detached: false,
                                terminal_interaction_released: false,
                                recovery_state: TerminalRecoveryState::None,
                                phase_timings_ns: BTreeMap::new(),
                                terminalization: None,
                            })
                            .await;
                    }
                    DurableSideEffectStep::Complete => {
                        session
                            .emit_terminalization_receipt(turn_context.as_ref(), &terminal_identity)
                            .await;
                        return;
                    }
                }
                let Some(delay) =
                    retry.retry_delay_after_failure(TerminalRepairFailure::Transient)
                else {
                    warn!(
                        turn_id = %turn_context.sub_id,
                        "parent terminal notification retry budget exhausted; durable notification remains pending for restart"
                    );
                    return;
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    async fn release_terminal_turn_after_live_handoff(&self, turn_id: &str) -> bool {
        let mut active = self.active_turn.lock().await;
        let matches_terminal = active
            .as_ref()
            .and_then(|active_turn| active_turn.terminal.as_ref())
            .is_some_and(|terminal| terminal.turn_id() == turn_id);
        if matches_terminal {
            if let Some(terminal) = active
                .as_ref()
                .and_then(|active_turn| active_turn.terminal.as_ref())
            {
                terminal.mark_interaction_released();
            }
            *active = None;
            self.terminal_interaction_pending
                .store(false, Ordering::Release);
            true
        } else {
            active.is_none()
        }
    }

    pub(crate) async fn resume_pending_terminal_deliveries(self: &Arc<Self>) {
        for authority in self
            .services
            .task_evidence
            .pending_authoritative_terminal_events()
            .await
        {
            self.recover_authoritative_terminal_delivery(
                authority, /*require_app_server_ack*/ true,
            )
            .await;
        }
    }

    async fn recover_authoritative_terminal_delivery(
        self: &Arc<Self>,
        authority: AuthoritativeTerminalEventV1,
        require_app_server_ack: bool,
    ) {
        let turn_context = self
            .new_default_turn_with_sub_id(authority.turn_id.clone())
            .await;
        let rollout_repaired = self
            .repair_recovered_terminal_rollout(&turn_context, &authority)
            .await;
        if !rollout_repaired {
            warn!(
                turn_id = %authority.turn_id,
                "terminal rollout repair budget exhausted; delivering the durable terminal with recovery still pending"
            );
        }
        let rollout_mirrored = rollout_repaired
            && self
                .ensure_terminal_authority_mirrored_in_rollout(&authority)
                .await;
        if !rollout_mirrored {
            warn!(turn_id = %authority.turn_id, "terminal rollout remains pending for restart recovery");
        }
        if !self
            .register_authoritative_terminal_delivery(
                authority.terminal_identity.clone(),
                authority.fingerprint.clone(),
            )
            .await
        {
            warn!(
                turn_id = %authority.turn_id,
                "skipping conflicting recovered terminal delivery"
            );
            return;
        }
        let live_delivered = self.try_send_live_event_accepted(Event {
            id: authority.turn_id.clone(),
            msg: authority.event.clone(),
        });
        let parent_notification_completed = self
            .maybe_notify_parent_of_terminal_turn(turn_context.as_ref(), &authority.event)
            .await;
        self.services
            .guardian_rejection_circuit_breaker
            .lock()
            .await
            .clear_turn(&authority.turn_id);
        let released = self
            .release_terminal_turn_after_live_handoff(&authority.turn_id)
            .await;
        let update = TerminalInteractionUpdate {
            terminal_identity: authority.terminal_identity.clone(),
            delivery_state: if live_delivered {
                DurableDeliveryState::Delivered
            } else {
                DurableDeliveryState::DeliveryFailed
            },
            // A non-app-server client accepts the live channel handoff directly; there is no
            // later protocol acknowledgement to wait for before restart recovery is complete.
            app_server_acknowledged: !require_app_server_ack,
            runtime_status_converged: true,
            rollout_mirrored,
            parent_notification_completed,
            post_terminal_cleanup_completed: true,
            active_turn_detached: released,
            terminal_interaction_released: released,
            recovery_state: if rollout_mirrored {
                TerminalRecoveryState::Recovered
            } else {
                TerminalRecoveryState::Pending
            },
            phase_timings_ns: BTreeMap::new(),
            terminalization: None,
        };
        let redelivery = (require_app_server_ack || !live_delivered)
            .then(|| (authority.clone(), require_app_server_ack));
        let updated = self
            .services
            .task_evidence
            .update_terminal_interaction(update.clone())
            .await;
        if updated {
            self.emit_terminalization_receipt(turn_context.as_ref(), &authority.terminal_identity)
                .await;
            if let Some((authority, require_app_server_ack)) = redelivery {
                self.schedule_terminal_redelivery(authority, require_app_server_ack);
            }
        } else {
            self.schedule_terminal_interaction_persistence_retry(
                Arc::clone(&turn_context),
                update,
                redelivery,
            );
        }
        if !parent_notification_completed {
            self.schedule_parent_terminal_notification_retry(
                Arc::clone(&turn_context),
                authority.event.clone(),
            );
        }
    }

    async fn repair_recovered_terminal_rollout(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        authority: &AuthoritativeTerminalEventV1,
    ) -> bool {
        let mut repair_items = authority.rollout_repair.items.clone();
        let mut retry = TerminalRepairRetry::default();
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                return false;
            }
            let mut failure = TerminalRepairFailure::Transient;
            let items_repaired = if repair_items.is_empty() {
                true
            } else {
                match tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    self.record_conversation_items_durable(turn_context, &repair_items),
                )
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(error)) => {
                        failure = classify_terminal_repair_io_error(&error);
                        false
                    }
                    Err(_) => false,
                }
            };
            if items_repaired {
                repair_items.clear();
            }
            let missing_outputs_repaired = if !authority.rollout_repair.repair_missing_call_outputs
            {
                true
            } else {
                match tokio::time::timeout(
                    TERMINAL_REPAIR_ATTEMPT_TIMEOUT,
                    self.persist_missing_call_outputs_durable(turn_context),
                )
                .await
                {
                    Ok(Ok(_)) => true,
                    Ok(Err(error)) => {
                        if classify_terminal_repair_io_error(&error)
                            == TerminalRepairFailure::Permanent
                        {
                            failure = TerminalRepairFailure::Permanent;
                        }
                        false
                    }
                    Err(_) => false,
                }
            };
            if repair_items.is_empty() && missing_outputs_repaired {
                return true;
            }
            let Some(delay) = retry.retry_delay_after_failure(failure) else {
                return false;
            };
            tokio::time::sleep(delay).await;
        }
    }

    pub(crate) async fn adopt_rollout_terminal_authority(&self, items: &[RolloutItem]) {
        let Some(authority) = last_terminal_authority_from_rollout(self.thread_id, items) else {
            return;
        };
        if self
            .services
            .task_evidence
            .authoritative_terminal_event(&authority.terminal_identity)
            .await
            .is_some()
        {
            return;
        }
        match self
            .services
            .task_evidence
            .commit_terminal_decision_and_claim(TerminalDecisionClaim {
                authoritative_event: authority,
                deadline_exhausted_phase: None,
                mutation_quiescent: false,
                durable_success_established: false,
                retained_ownership: Vec::new(),
                phase_timings_ns: BTreeMap::new(),
            })
            .await
        {
            TerminalClaimResult::Claimed(_) | TerminalClaimResult::AlreadyClaimed(_) => {}
            TerminalClaimResult::Conflict { .. } | TerminalClaimResult::Failed => {
                warn!("could not bind the exact retained terminal event into recovery evidence");
            }
        }
    }

    pub(crate) async fn recover_bound_terminal_intent(self: &Arc<Self>, turn_id: String) {
        if let Some(authority) = self
            .services
            .task_evidence
            .authoritative_terminal_event(&format!("{}:{turn_id}", self.thread_id))
            .await
        {
            self.recover_authoritative_terminal_delivery(
                authority, /*require_app_server_ack*/ false,
            )
            .await;
            return;
        }
        let Some(intent) = self
            .services
            .task_evidence
            .completion_recovery_intent(&turn_id)
            .await
        else {
            // Legacy or unbound final-proof evidence is deliberately not associated with the
            // open turn.
            return;
        };
        let turn_context = self.new_default_turn_with_sub_id(turn_id.clone()).await;
        let mut failure_reasons = Vec::new();

        self.services.command_execution.finish_turn(&turn_id).await;
        let inputs = completion_review::refresh_authoritative_review_inputs(self).await;
        failure_reasons.extend(inputs.partial_reasons.iter().cloned());
        if !inputs.typed_quiescent {
            failure_reasons
                .push("typed task ownership was not quiescent during recovery".to_string());
        }
        if !inputs.default_children_quiescent {
            failure_reasons
                .push("default child ownership was not quiescent during recovery".to_string());
        }
        let authoritative_inputs = Some(inputs);

        let mut completion_gate = None;
        if failure_reasons.is_empty() {
            let child_gate_state = authoritative_inputs
                .as_ref()
                .map(|inputs| inputs.typed_evidence.clone())
                .unwrap_or_default();
            match seal_terminal_final_proof(
                self,
                &turn_context,
                Some(&intent.final_message),
                child_gate_state,
            )
            .await
            {
                Some(FinalProofSealResultV1::Memoized { gate, .. })
                    if gate.status == TaskCompletionStatus::Passed =>
                {
                    completion_gate = Some(gate);
                }
                Some(FinalProofSealResultV1::Memoized { gate, .. }) => {
                    failure_reasons.extend(gate.reasons);
                }
                Some(FinalProofSealResultV1::Sealed { .. }) => failure_reasons.push(
                    "restart recovery identities no longer match the sealed memo".to_string(),
                ),
                Some(FinalProofSealResultV1::PreflightFailed(gate)) => {
                    failure_reasons.extend(gate.reasons);
                }
                None => failure_reasons
                    .push("restart recovery could not rebuild the final proof".to_string()),
            }
            if self
                .services
                .task_evidence
                .current_finalization_memo_identity()
                .await
                .as_deref()
                != Some(intent.memo_identity.as_str())
            {
                completion_gate = None;
                failure_reasons.push(
                    "restart recovery finalization memo identity changed during revalidation"
                        .to_string(),
                );
            }
        }

        if failure_reasons.is_empty()
            && let Some(inputs) = authoritative_inputs.as_ref()
        {
            let dossier = self
                .services
                .task_evidence
                .completion_review_dossier(
                    Some(&intent.final_message),
                    &inputs.typed_mutation_identities,
                    &inputs.typed_evidence,
                    &inputs.review_lens_selection_facts,
                    &inputs.partial_reasons,
                    inputs.typed_quiescent,
                    inputs.default_children_quiescent,
                )
                .await;
            let dossier_matches = if let Some(dossier) = dossier.as_ref() {
                self.services
                    .task_evidence
                    .passed_completion_matches_dossier(dossier)
                    .await
            } else {
                false
            };
            if !dossier_matches {
                completion_gate = None;
                failure_reasons.push(
                    "restart recovery completion dossier no longer matches the sealed proof"
                        .to_string(),
                );
            }
        }

        failure_reasons.sort();
        failure_reasons.dedup();
        let (event, outcome) = if failure_reasons.is_empty() {
            (
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_id.clone(),
                    last_agent_message: Some(intent.final_message),
                    surfaced_result: None,
                    error: None,
                    completion: completion_gate,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                }),
                "passed_recovered".to_string(),
            )
        } else {
            let message = failure_reasons.join("; ");
            (
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_id.clone(),
                    last_agent_message: Some(format!("Task completion is partial: {message}")),
                    surfaced_result: None,
                    error: Some(ErrorEvent {
                        message,
                        codex_error_info: None,
                    }),
                    completion: Some(TaskCompletionGate {
                        status: TaskCompletionStatus::Partial,
                        reasons: failure_reasons,
                        evidence_path: None,
                    }),
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                }),
                "partial_recovery".to_string(),
            )
        };
        self.establish_recovered_terminal_event(&turn_context, event, outcome, true)
            .await;
    }

    async fn establish_recovered_terminal_event(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        event: EventMsg,
        semantic_outcome: String,
        mutation_quiescent: bool,
    ) {
        let Some(fingerprint) = terminal_event_fingerprint(&event) else {
            return;
        };
        let terminal_identity = format!("{}:{}", self.thread_id, turn_context.sub_id);
        let candidate = AuthoritativeTerminalEventV1 {
            version: 1,
            terminal_identity: terminal_identity.clone(),
            turn_id: turn_context.sub_id.clone(),
            event,
            fingerprint,
            semantic_outcome,
            final_proof_identity: self
                .services
                .task_evidence
                .current_finalization_memo_identity()
                .await,
            rollout_repair: TerminalRolloutRepairV1::default(),
        };
        let claim = self
            .services
            .task_evidence
            .commit_terminal_decision_and_claim(TerminalDecisionClaim {
                authoritative_event: candidate.clone(),
                deadline_exhausted_phase: None,
                mutation_quiescent,
                durable_success_established: candidate.semantic_outcome == "passed_recovered",
                retained_ownership: Vec::new(),
                phase_timings_ns: BTreeMap::new(),
            })
            .await;
        let authority = match claim {
            TerminalClaimResult::Claimed(authority)
            | TerminalClaimResult::AlreadyClaimed(authority) => authority,
            TerminalClaimResult::Conflict {
                authoritative: Some(authority),
                ..
            } => authority,
            TerminalClaimResult::Conflict {
                authoritative: None,
                ..
            } => return,
            TerminalClaimResult::Failed => {
                if let Some(existing) = self
                    .reconcile_terminal_authority_from_rollout(&candidate)
                    .await
                {
                    existing
                } else if !self
                    .ensure_terminal_authority_mirrored_in_rollout(&candidate)
                    .await
                {
                    self.try_send_live_event(Event {
                        id: candidate.turn_id,
                        msg: candidate.event,
                    });
                    return;
                } else {
                    candidate
                }
            }
        };
        let rollout_mirrored = self
            .ensure_terminal_authority_mirrored_in_rollout(&authority)
            .await;
        if !self
            .register_authoritative_terminal_delivery(
                authority.terminal_identity.clone(),
                authority.fingerprint.clone(),
            )
            .await
        {
            return;
        }
        let live_delivered = self.try_send_live_event_accepted(Event {
            id: authority.turn_id.clone(),
            msg: authority.event.clone(),
        });
        let requires_app_server_ack = turn_context.app_server_client_name.is_some();
        let redelivery = (requires_app_server_ack || !live_delivered)
            .then(|| (authority.clone(), requires_app_server_ack));
        let parent_notification_completed = self
            .finish_terminal_event_dispatch(turn_context, &authority.event)
            .await;
        if !parent_notification_completed {
            self.schedule_parent_terminal_notification_retry(
                Arc::clone(turn_context),
                authority.event.clone(),
            );
        }
        let update = TerminalInteractionUpdate {
            terminal_identity,
            delivery_state: if live_delivered {
                DurableDeliveryState::Delivered
            } else {
                DurableDeliveryState::DeliveryFailed
            },
            app_server_acknowledged: false,
            runtime_status_converged: true,
            rollout_mirrored,
            parent_notification_completed,
            post_terminal_cleanup_completed: true,
            active_turn_detached: true,
            terminal_interaction_released: true,
            recovery_state: TerminalRecoveryState::Recovered,
            phase_timings_ns: BTreeMap::new(),
            terminalization: None,
        };
        let updated = self
            .services
            .task_evidence
            .update_terminal_interaction(update.clone())
            .await;
        if updated {
            self.emit_terminalization_receipt(turn_context.as_ref(), &authority.terminal_identity)
                .await;
            if let Some((authority, require_app_server_ack)) = redelivery {
                self.schedule_terminal_redelivery(authority, require_app_server_ack);
            }
        } else {
            self.schedule_terminal_interaction_persistence_retry(
                Arc::clone(turn_context),
                update,
                redelivery,
            );
        }
    }

    async fn emit_terminalization_receipt(
        &self,
        turn_context: &TurnContext,
        terminal_identity: &str,
    ) {
        let Some(snapshot) = self
            .services
            .task_evidence
            .terminalization_receipt_snapshot(terminal_identity)
            .await
        else {
            return;
        };
        if snapshot.recovery_state == TerminalRecoveryState::Recovered
            && !self
                .claim_terminal_recovery_notification(terminal_identity)
                .await
        {
            return;
        }
        self.send_event(
            turn_context,
            EventMsg::TurnTerminalizationComplete(TurnTerminalizationCompleteEvent {
                turn_id: turn_context.sub_id.clone(),
                receipt: protocol_terminalization_receipt(snapshot),
            }),
        )
        .await;
    }

    async fn emit_post_terminal_metrics(
        &self,
        turn_context: &TurnContext,
        turn_had_memory_citation: bool,
        turn_tool_calls: u64,
        token_usage_at_turn_start: &TokenUsage,
    ) {
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
            turn_context.config.features.enabled(Feature::MemoryTool),
            turn_context.config.memories.use_memories,
            turn_had_memory_citation,
        );
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
        }

        self.services
            .command_execution
            .finish_turn(&turn_context.sub_id)
            .await;
        if requires_abort_cleanup {
            tokio::select! {
                _ = finalization.task.done.notified() => {},
                _ = tokio::time::sleep(GRACEFUL_INTERRUPTION_TIMEOUT) => {
                    warn!(
                        "task {} didn't complete gracefully after {}ms",
                        turn_context.sub_id,
                        GRACEFUL_INTERRUPTION_TIMEOUT.as_millis()
                    );
                }
            }
            finalization.task.worker_abort_handle.abort();

            let session_task = Arc::clone(&finalization.task.task);
            let session_ctx = Arc::new(SessionTaskContext::new(
                Arc::clone(self),
                Arc::clone(&finalization.task.turn_extension_data),
            ));
            if finalization
                .deadline
                .run(
                    "hooks_quiescence",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    session_task.abort(session_ctx, Arc::clone(&turn_context)),
                )
                .await
                .is_err()
            {
                warn!(turn_id = %turn_context.sub_id, "timed out running task-specific abort cleanup");
            }
        }

        let missing_call_output_repair_failure = match finalization
            .deadline
            .run(
                "preparation",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.persist_missing_call_outputs_durable(&turn_context),
            )
            .await
        {
            Ok(Ok(0)) => None,
            Ok(Ok(output_count)) => {
                warn!(
                    turn_id = %turn_context.sub_id,
                    output_count,
                    "persisted missing tool outputs before terminal event"
                );
                None
            }
            Ok(Err(err)) => {
                let reason =
                    format!("missing tool-output repair failed before terminal publication: {err}");
                warn!(turn_id = %turn_context.sub_id, %reason);
                Some(reason)
            }
            Err(_) => {
                let reason =
                    "missing tool-output repair timed out before terminal publication".to_string();
                warn!(turn_id = %turn_context.sub_id, %reason);
                Some(reason)
            }
        };

        turn_context.turn_timing_state.begin_finalization();

        let explicit_abort_reason = match &finalization.outcome {
            TurnTerminalOutcome::Aborted(reason) => Some(reason.clone()),
            _ => None,
        };
        let mut interrupted_marker_repair = Vec::new();
        if explicit_abort_reason == Some(TurnAbortReason::Interrupted)
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    turn_context.config.as_ref(),
                    turn_context.multi_agent_version,
                ),
            )
        {
            match finalization
                .deadline
                .run(
                    "preparation",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.record_conversation_items_durable(
                        &turn_context,
                        std::slice::from_ref(&marker),
                    ),
                )
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!("failed to persist interrupted-turn marker before terminal event: {err}");
                    interrupted_marker_repair.push(marker);
                }
                Err(_) => {
                    warn!(turn_id = %turn_context.sub_id, "timed out persisting interrupted-turn marker before terminal event");
                    interrupted_marker_repair.push(marker);
                }
            }
        }

        let (mut last_agent_message, surfaced_result, abort_reason) = match &finalization.outcome {
            TurnTerminalOutcome::Completed { result } => (
                result.last_agent_message.clone(),
                result.surfaced_result.clone(),
                None,
            ),
            TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted) => {
                (None, None, Some(TurnAbortReason::Interrupted))
            }
            TurnTerminalOutcome::ReturnedError(err) => {
                warn!(%err, "session task returned an unexpected error");
                (None, None, None)
            }
            TurnTerminalOutcome::Aborted(reason) => (None, None, Some(reason.clone())),
            TurnTerminalOutcome::WorkerJoinFailed(_) => (None, None, None),
        };

        let pending_input_result = finalization
            .deadline
            .run(
                "hooks_quiescence",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                async {
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
                                inspect_pending_input(self, &turn_context, &pending_input_item)
                                    .await;
                            if hook_outcome.should_stop {
                                crate::hook_runtime::emit_hook_stop_reason(
                                    self,
                                    &turn_context,
                                    "UserPromptSubmit",
                                    hook_outcome.stop_reason.as_deref(),
                                )
                                .await;
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
                },
            )
            .await;
        if pending_input_result.is_err() {
            warn!(turn_id = %turn_context.sub_id, "pending-input terminal hooks did not quiesce before the deadline");
        }

        // Extension lifecycle callbacks have no restrictive effects contract. They therefore
        // remain mutation-capable and must finish before final freshness and gate evaluation.
        let terminal_lifecycle_result = finalization
            .deadline
            .run(
                "hooks_quiescence",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                async {
                    if let Some(reason) = explicit_abort_reason.as_ref() {
                        self.emit_turn_abort_lifecycle(
                            reason.clone(),
                            turn_context.extension_data.as_ref(),
                        )
                        .await;
                    } else {
                        self.emit_turn_stop_lifecycle(turn_context.extension_data.as_ref())
                            .await;
                    }
                },
            )
            .await;
        if terminal_lifecycle_result.is_err() {
            warn!(turn_id = %turn_context.sub_id, "mutation-capable terminal lifecycle hook did not quiesce");
        }
        let terminal_hooks_complete =
            pending_input_result.is_ok() && terminal_lifecycle_result.is_ok();
        finalization.mutation_quiescent = true;

        let (
            turn_had_memory_citation,
            turn_tool_calls,
            token_usage_at_turn_start,
            mut completion_review_partial_reasons,
        ) = match finalization
            .deadline
            .run(
                "preparation",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                async {
                    let ts = finalization.turn_state.lock().await;
                    (
                        ts.has_memory_citation,
                        ts.tool_calls,
                        ts.token_usage_at_turn_start.clone(),
                        ts.completion_review_partial_reasons(),
                    )
                },
            )
            .await
        {
            Ok(values) => values,
            Err(_) => (
                false,
                0,
                TokenUsage::default(),
                vec!["terminal preparation timed out before state capture".to_string()],
            ),
        };
        if !terminal_hooks_complete {
            completion_review_partial_reasons
                .push("terminal hooks did not finish before completion review".to_string());
        }
        if let Some(reason) = missing_call_output_repair_failure.as_ref() {
            completion_review_partial_reasons.push(reason.clone());
        }
        let mut atomically_persisted_completion = None;
        let mut terminal_authoritative_inputs = None;
        if abort_reason.is_none()
            && turn_context
                .config
                .features
                .enabled(Feature::TaskCompletionReviewer)
            && completion_review_partial_reasons.is_empty()
        {
            let completion_review_result = finalization
                .deadline
                .run(
                    "gate",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    async {
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
                if completion_review_partial_reasons.is_empty() {
                    // Reconcile authoritative evidence immediately before reviewing the current
                    // workspace snapshot. Writers remain live throughout completion review.
                    let refreshed =
                        completion_review::refresh_authoritative_review_inputs(self).await;
                    completion_review_partial_reasons
                        .extend(refreshed.partial_reasons.iter().cloned());
                    if !refreshed.typed_quiescent {
                        completion_review_partial_reasons.push(
                            "typed task ownership was not quiescent after completion admission"
                                .to_string(),
                        );
                    }
                    if !refreshed.default_children_quiescent {
                        completion_review_partial_reasons.push(
                            "default child ownership was not quiescent after completion admission"
                                .to_string(),
                        );
                    }
                    completion_review_partial_reasons.sort();
                    completion_review_partial_reasons.dedup();
                    terminal_authoritative_inputs = Some(refreshed);
                }

                if completion_review_partial_reasons.is_empty() {
                    let authoritative = match terminal_authoritative_inputs.as_ref() {
                        Some(authoritative) => authoritative.clone(),
                        None => completion_review::refresh_authoritative_review_inputs(self).await,
                    };
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
                                ) =>
                        {
                            if !completion_review::user_sources_still_current(&dossier).await {
                                let superseded = self
                                    .services
                                    .task_evidence
                                    .supersede_provisional_completion_review(&dossier)
                                    .await;
                                if !matches!(
                                    superseded,
                                    crate::task_evidence::AtomicReviewTransition::Persisted(())
                                ) {
                                    self.schedule_provisional_review_supersession_retry();
                                }
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
                                        let retry_authoritative = completion_review::refresh_authoritative_review_inputs(self).await;
                                        completion_review_partial_reasons.extend(
                                            retry_authoritative.partial_reasons.iter().cloned(),
                                        );
                                        if !retry_authoritative.typed_quiescent {
                                            completion_review_partial_reasons.push(
                                                "typed task ownership was not quiescent after completion-review persistence changed"
                                                    .to_string(),
                                            );
                                        }
                                        if !retry_authoritative.default_children_quiescent {
                                            completion_review_partial_reasons.push(
                                                "default child ownership was not quiescent after completion-review persistence changed"
                                                    .to_string(),
                                            );
                                        }
                                        completion_review_partial_reasons.sort();
                                        completion_review_partial_reasons.dedup();
                                        terminal_authoritative_inputs =
                                            Some(retry_authoritative.clone());
                                        let retry_dossier = if completion_review_partial_reasons
                                            .is_empty()
                                        {
                                            self.services
                                                .task_evidence
                                                .completion_review_dossier(
                                                    last_agent_message.as_deref(),
                                                    &retry_authoritative
                                                        .typed_mutation_identities,
                                                    &retry_authoritative.typed_evidence,
                                                    &retry_authoritative
                                                        .review_lens_selection_facts,
                                                    &retry_authoritative.partial_reasons,
                                                    retry_authoritative.typed_quiescent,
                                                    retry_authoritative
                                                        .default_children_quiescent,
                                                )
                                                .await
                                        } else {
                                            None
                                        };
                                        let retry = match retry_dossier.as_ref() {
                                            Some(retry_dossier)
                                                if retry_dossier.cycle_phase
                                                    == Some(
                                                        crate::task_evidence::CompletionReviewCyclePhase::ProvisionalClean,
                                                    )
                                                    && completion_review::user_sources_still_current(
                                                        retry_dossier,
                                                    )
                                                    .await =>
                                            {
                                                self.services
                                                    .task_evidence
                                                    .finalize_completion_review(retry_dossier)
                                                    .await
                                            }
                                            _ => crate::task_evidence::AtomicReviewTransition::Superseded,
                                        };
                                        match retry {
                                            crate::task_evidence::AtomicReviewTransition::Persisted(gate) => {
                                                atomically_persisted_completion = Some(gate);
                                            }
                                            crate::task_evidence::AtomicReviewTransition::Superseded
                                            | crate::task_evidence::AtomicReviewTransition::Failed => {
                                                if let Some(retry_dossier) = retry_dossier.as_ref() {
                                                    let superseded = self
                                                        .services
                                                        .task_evidence
                                                        .supersede_provisional_completion_review(
                                                            retry_dossier,
                                                        )
                                                        .await;
                                                    if !matches!(
                                                        superseded,
                                                        crate::task_evidence::AtomicReviewTransition::Persisted(())
                                                    ) {
                                                        self.schedule_provisional_review_supersession_retry();
                                                    }
                                                }
                                                completion_review_partial_reasons.push(
                                                    "the reviewed candidate changed during terminal finalization"
                                                        .to_string(),
                                                );
                                            }
                                        }
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
                            let superseded = self
                                .services
                                .task_evidence
                                .supersede_provisional_completion_review(&dossier)
                                .await;
                            if !matches!(
                                superseded,
                                crate::task_evidence::AtomicReviewTransition::Persisted(())
                            ) {
                                self.schedule_provisional_review_supersession_retry();
                            }
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
                    },
                )
                .await;
            if completion_review_result.is_err() {
                let reason = "completion finalization timed out before terminal dispatch";
                warn!(turn_id = %turn_context.sub_id, %reason);
                completion_review_partial_reasons.push(reason.to_string());
                if let Some(gate) = atomically_persisted_completion.as_mut() {
                    gate.status = TaskCompletionStatus::Partial;
                    gate.reasons.push(reason.to_string());
                    gate.reasons.sort();
                    gate.reasons.dedup();
                    self.schedule_completion_invalidation_retry(reason.to_string());
                }
            }
        }
        let completion_was_review_finalized = atomically_persisted_completion.is_some();
        let mut completion = if abort_reason.is_none() {
            match atomically_persisted_completion {
                Some(gate) => Some(gate),
                None => match finalization
                    .deadline
                    .run(
                        "gate",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.services.task_evidence.completion_gate(),
                    )
                    .await
                {
                    Ok(gate) => gate,
                    Err(_) => {
                        completion_review_partial_reasons
                            .push("timed out loading persisted completion evidence".to_string());
                        None
                    }
                },
            }
        } else {
            None
        };
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && !completion_was_review_finalized
        {
            let coordinator = self.services.agent_control.task_coordinator();
            let quiescence_result = finalization
                .deadline
                .run(
                    "hooks_quiescence",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    async {
            if let (
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
                                    "linked typed work is not quiescent: active assignments [{}]; running validations [{}]; pending gates [{}]",
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
            }
                    },
                )
                .await;
            let (quiescence_reason, quiescence_warnings) = match quiescence_result {
                Ok(result) => result,
                Err(_) => (
                    Some("linked typed-work quiescence check timed out".to_string()),
                    Vec::new(),
                ),
            };
            for warning in quiescence_warnings {
                warn!(turn_id = %turn_context.sub_id, message = %warning, "typed-work quiescence warning");
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
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && self.services.task_evidence.allows_kd4_completion()
        {
            let mut final_proof_child_gate_state = completion_review_partial_reasons.clone();
            if let Some(gate) = completion
                .as_ref()
                .filter(|gate| gate.status != TaskCompletionStatus::Passed)
            {
                final_proof_child_gate_state.extend(gate.reasons.iter().cloned());
            }
            final_proof_child_gate_state.sort();
            final_proof_child_gate_state.dedup();

            finalization
                .deadline
                .record_elapsed("diff_refresh", Duration::ZERO);
            let sealed = finalization
                .deadline
                .run(
                    "final_proof_gate",
                    FINAL_PROOF_CANDIDATE_SEAL_TIMEOUT,
                    seal_terminal_final_proof(
                        self,
                        &turn_context,
                        last_agent_message.as_deref(),
                        final_proof_child_gate_state,
                    ),
                )
                .await;
            match sealed {
                Ok(Some(result)) => {
                    let freshness_diagnostics = self.services.task_evidence.freshness_diagnostics();
                    let proof_reuse_count =
                        u32::try_from(freshness_diagnostics.strong_hashes_reused)
                            .unwrap_or(u32::MAX);
                    let conservative_rerun_count =
                        u32::try_from(freshness_diagnostics.conservative_reruns)
                            .unwrap_or(u32::MAX);
                    let mut gate = match result {
                        FinalProofSealResultV1::Sealed {
                            checkpoint,
                            telemetry,
                            gate,
                            ..
                        } => {
                            turn_context.turn_timing_state.record_final_proof_telemetry(
                                checkpoint.estimated_tokens,
                                telemetry.validation_launch_count,
                                telemetry.validation_process_ns,
                                telemetry.validation_aggregate_ns,
                                1,
                                proof_reuse_count,
                                conservative_rerun_count,
                                0,
                            );
                            gate
                        }
                        FinalProofSealResultV1::Memoized {
                            gate,
                            checkpoint_tokens,
                            telemetry,
                        } => {
                            turn_context.turn_timing_state.record_final_proof_telemetry(
                                checkpoint_tokens,
                                telemetry.validation_launch_count,
                                telemetry.validation_process_ns,
                                telemetry.validation_aggregate_ns,
                                1,
                                proof_reuse_count,
                                conservative_rerun_count,
                                1,
                            );
                            gate
                        }
                        FinalProofSealResultV1::PreflightFailed(gate) => gate,
                    };
                    if gate.status == TaskCompletionStatus::Passed {
                        let memoized = finalization
                            .deadline
                            .run(
                                "completion_finalization",
                                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                                self.services
                                    .task_evidence
                                    .memoized_finalization_result(&turn_context.sub_id),
                            )
                            .await;
                        let (memo_persisted, memo_failure_reason) = match memoized {
                            Ok(Some(memoized)) => {
                                finalization
                                    .deadline
                                    .record_elapsed("terminal_memo_hit", Duration::ZERO);
                                if surfaced_result.is_none() {
                                    last_agent_message = Some(memoized);
                                }
                                (true, None)
                            }
                            Ok(None) => {
                                if let Some(message) = last_agent_message.clone() {
                                    match finalization
                                        .deadline
                                        .run(
                                            "completion_finalization",
                                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                                            async {
                                                let Some((_checkpoint_id, payload)) = self
                                                    .services
                                                    .task_evidence
                                                    .completion_checkpoint_payload()
                                                    .await
                                                else {
                                                    return false;
                                                };
                                                let Ok(checkpoint) = serde_json::from_str::<
                                                    CompletionCheckpointV1,
                                                >(&payload)
                                                else {
                                                    return false;
                                                };
                                                let requested_artifacts = checkpoint
                                                    .evidence_artifact_references
                                                    .into_iter()
                                                    .collect::<BTreeSet<_>>();
                                                let history = self.clone_history().await;
                                                let projected = history.prepare_for_finalization(
                                                    &turn_context.model_info.input_modalities,
                                                    crate::context::CompletionCheckpointContext::new(
                                                        payload,
                                                    ),
                                                    &requested_artifacts,
                                                );
                                                if projected.items().is_empty() {
                                                    return false;
                                                }
                                                self.services
                                                    .task_evidence
                                                    .record_finalization_result(
                                                        turn_context.sub_id.clone(),
                                                        message,
                                                        finalization.mutation_quiescent,
                                                        finalization.mutation_quiescent,
                                                    )
                                                    .await
                                            },
                                        )
                                        .await
                                    {
                                        Ok(true) => (true, None),
                                        Ok(false) => (
                                            false,
                                            Some(
                                                "the exact finalization memo was not durably recorded"
                                                    .to_string(),
                                            ),
                                        ),
                                        Err(_) => (
                                            false,
                                            Some(
                                                "finalization memo persistence timed out"
                                                    .to_string(),
                                            ),
                                        ),
                                    }
                                } else {
                                    (
                                        false,
                                        Some(
                                            "passed final proof had no final message to memoize"
                                                .to_string(),
                                        ),
                                    )
                                }
                            }
                            Err(_) => (
                                false,
                                Some("finalization memo lookup timed out".to_string()),
                            ),
                        };
                        if let Some(reason) = memo_failure_reason
                            && downgrade_for_required_finalization_memo(
                                &mut gate,
                                memo_persisted,
                                &reason,
                            )
                        {
                            self.schedule_completion_invalidation_retry(reason);
                        }
                    }
                    completion = Some(gate);
                }
                Ok(None) => {}
                Err(_) => merge_completion_review_partial(
                    &mut completion,
                    vec!["final-proof candidate sealing timed out".to_string()],
                ),
            }
        }
        if abort_reason.is_none() {
            merge_completion_review_partial(&mut completion, completion_review_partial_reasons);
        }
        let worker_join_failure_kind =
            if let TurnTerminalOutcome::WorkerJoinFailed(failure) = &finalization.outcome {
                Some(match failure {
                    WorkerJoinFailure::Cancelled => "cancelled",
                    WorkerJoinFailure::Panicked => "panicked",
                })
            } else {
                None
            };

        let pre_terminal_flush_failure = match finalization
            .deadline
            .run(
                "durable_commit",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.flush_rollout(),
            )
            .await
        {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!(
                "rollout flush failed before terminal dispatch: {error}"
            )),
            Err(_) => Some("rollout flush timed out before terminal dispatch".to_string()),
        };
        if let Some(reason) = pre_terminal_flush_failure {
            warn!(turn_id = %turn_context.sub_id, %reason);
            if abort_reason.is_none() && completion.is_some() {
                merge_completion_review_partial(&mut completion, vec![reason]);
            }
        }

        let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
        if let Some(duration) = timing_snapshot.inclusive_duration() {
            turn_context
                .session_telemetry
                .record_duration(TURN_E2E_DURATION_METRIC, duration, &[]);
        }
        let timing = timing_snapshot.protocol_timing();
        finalization
            .pending_turn_profile
            .get_or_insert_with(|| TurnProfileFact {
                turn_id: turn_context.sub_id.clone(),
                profile: timing_snapshot.legacy_profile.clone(),
                timing: Some(timing.clone()),
            });
        if abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && completion_was_review_finalized
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
        {
            let terminal_invalidation_reason = if let Some(authoritative) =
                terminal_authoritative_inputs.as_ref()
            {
                let validation = finalization
                    .deadline
                    .run("freshness", TERMINAL_MUTATION_FINALIZATION_TIMEOUT, async {
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
                        if let Some(dossier) = current_dossier {
                            self.services
                                .task_evidence
                                .passed_completion_matches_dossier(&dossier)
                                .await
                        } else {
                            false
                        }
                    })
                    .await;
                match validation {
                    Ok(true) => None,
                    Ok(false) => Some("the reviewed candidate drifted before terminal emission"),
                    Err(_) => Some("terminal completion revalidation timed out"),
                }
            } else {
                Some("the final authoritative completion snapshot was unavailable")
            };
            if let Some(reason) = terminal_invalidation_reason {
                if let Some(gate) = completion.as_mut() {
                    gate.status = TaskCompletionStatus::Partial;
                    gate.reasons.push(reason.to_string());
                    gate.reasons.sort();
                    gate.reasons.dedup();
                }
                let invalidation = finalization
                    .deadline
                    .run(
                        "freshness",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.services
                            .task_evidence
                            .invalidate_completion_after_terminal_emission_failure(reason),
                    )
                    .await;
                if !atomic_review_transition_persisted(&invalidation) {
                    warn!(turn_id = %turn_context.sub_id, %reason, "completion invalidation was not durably persisted before terminal emission");
                    self.schedule_completion_invalidation_retry(reason.to_string());
                }
            }
        }
        let completed_at = timing_snapshot.completed_at_unix_secs;
        let duration_ms = timing_snapshot.duration_ms;
        let error = finalization
            .deadline
            .run(
                "preparation",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                async { turn_context.terminal_error.lock().await.clone() },
            )
            .await
            .unwrap_or(None);
        let mut passed_root_completion = abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
        if abort_reason.is_none()
            && surfaced_result.is_none()
            && let Some(gate) = completion
                .as_ref()
                .filter(|gate| gate.status != TaskCompletionStatus::Passed)
        {
            last_agent_message = if turn_context.final_output_json_schema.is_some() {
                None
            } else {
                let status = match gate.status {
                    TaskCompletionStatus::Partial => "partial",
                    TaskCompletionStatus::Blocked => "blocked",
                    TaskCompletionStatus::Passed => {
                        unreachable!("passed gates were filtered above")
                    }
                };
                let explanation = if gate.reasons.is_empty() {
                    "the completion gate did not establish successful completion".to_string()
                } else {
                    gate.reasons.join("; ")
                };
                Some(format!("Task completion is {status}: {explanation}"))
            };
        }
        let mut event = if let Some(reason) = abort_reason.as_ref() {
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
                surfaced_result,
                error,
                completion,
                completed_at,
                duration_ms,
                time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                timing: Some(timing),
            })
        };
        let reasoning_summary = finalization
            .reasoning_policy_recorder
            .take_summary(turn_context.sub_id.clone());
        let durable_outcome = semantic_terminal_outcome(&event);
        let rollout_structure_ready = terminal_rollout_structure_ready(
            missing_call_output_repair_failure.is_none(),
            interrupted_marker_repair.is_empty(),
        );
        let terminal_milestone = self
            .publish_terminal_interaction_milestone(
                finalization,
                turn_context.as_ref(),
                &mut event,
                durable_outcome,
                passed_root_completion,
                rollout_structure_ready,
                interrupted_marker_repair,
            )
            .await;
        if !finalization.coordinator.durable_terminal_committed() {
            passed_root_completion = false;
        }
        if terminal_milestone.live_attempted && !terminal_milestone.live_delivered {
            warn!(turn_id = %turn_context.sub_id, "terminal live delivery failed; durable decision remains authoritative");
        }
        let cleared_active_turn = terminal_milestone.cleared_active_turn;
        if cleared_active_turn {
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.emit_thread_idle_lifecycle_if_idle(),
            )
            .await;
        }
        if cleared_active_turn && abort_reason == Some(TurnAbortReason::Interrupted) {
            self.maybe_start_turn_for_pending_work().await;
        }

        // Everything below this line is cleanup. It is deliberately unable to hold the
        // interactive turn fence or delay abort/replacement/new-turn submission.
        let post_cleanup_started = tokio::time::Instant::now();
        if tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.emit_post_terminal_metrics(
                turn_context.as_ref(),
                turn_had_memory_citation,
                turn_tool_calls,
                &token_usage_at_turn_start,
            ),
        )
        .await
        .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out recording optional post-terminal metrics");
        }
        let parent_notification_completed = if terminal_milestone.live_attempted {
            match tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.finish_terminal_event_dispatch(turn_context.as_ref(), &event),
            )
            .await
            {
                Ok(completed) => completed,
                Err(_) => {
                    warn!(turn_id = %turn_context.sub_id, "timed out finishing terminal event side effects");
                    false
                }
            }
        } else {
            false
        };
        if terminal_milestone.live_attempted && !parent_notification_completed {
            self.schedule_parent_terminal_notification_retry(
                Arc::clone(&turn_context),
                event.clone(),
            );
        }
        if let Some(summary) = reasoning_summary
            && tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::ReasoningPolicySummary(summary),
                ),
            )
            .await
            .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out emitting reasoning policy summary after terminal dispatch");
        }
        if let Some(failure_kind) = worker_join_failure_kind
            && tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::Error(ErrorEvent {
                        message: format!(
                            "The turn worker {failure_kind} before terminal bookkeeping completed."
                        ),
                        codex_error_info: Some(CodexErrorInfo::InternalServerError),
                    }),
                ),
            )
            .await
            .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out emitting worker failure after terminal dispatch");
        }
        if passed_root_completion {
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.set_last_passed_root_completion_turn_id(Some(turn_context.sub_id.clone())),
            )
            .await;
        }
        let circuit_breaker_cleanup = async {
            self.services
                .guardian_rejection_circuit_breaker
                .lock()
                .await
                .clear_turn(&turn_context.sub_id);
        };
        if tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            circuit_breaker_cleanup,
        )
        .await
        .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out clearing the post-terminal circuit breaker");
        }
        // Regular items were flushed before this terminal event was appended; buffering
        // thread writers may not flush it without another explicit barrier.
        match tokio::time::timeout(TERMINAL_MUTATION_FINALIZATION_TIMEOUT, self.flush_rollout())
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!("failed to flush rollout after emitting terminal turn event: {err}")
            }
            Err(_) => {
                warn!(turn_id = %turn_context.sub_id, "timed out flushing rollout after terminal dispatch")
            }
        }
        finalization.deadline.record_elapsed(
            "post_cleanup",
            tokio::time::Instant::now().saturating_duration_since(post_cleanup_started),
        );
        finalization.deadline.finish_unclassified();
        let final_terminalization = self.emit_pending_turn_profile(finalization);
        if finalization.coordinator.durable_terminal_committed() {
            let delivery_state = match finalization.coordinator.delivery_state() {
                CoordinatorDeliveryState::NotAttempted => DurableDeliveryState::NotAttempted,
                CoordinatorDeliveryState::Claimed => DurableDeliveryState::Claimed,
                CoordinatorDeliveryState::Delivered => DurableDeliveryState::Delivered,
                CoordinatorDeliveryState::DeliveryFailed => DurableDeliveryState::DeliveryFailed,
            };
            let phase_timings_ns = finalization.deadline.phase_timings_ns();
            let terminal_identity = format!("{}:{}", self.thread_id, turn_context.sub_id);
            let session = Arc::clone(self);
            let turn_context = Arc::clone(&turn_context);
            self.spawn_terminal_receipt_task(async move {
                let update = TerminalInteractionUpdate {
                    terminal_identity: terminal_identity.clone(),
                    delivery_state,
                    app_server_acknowledged: false,
                    runtime_status_converged: true,
                    rollout_mirrored: false,
                    parent_notification_completed,
                    post_terminal_cleanup_completed: true,
                    active_turn_detached: cleared_active_turn,
                    terminal_interaction_released: cleared_active_turn,
                    recovery_state: pending_terminal_recovery_state(delivery_state),
                    phase_timings_ns,
                    terminalization: final_terminalization.clone(),
                };
                let persistence = tokio::time::timeout(
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    session
                        .services
                        .task_evidence
                        .update_terminal_interaction(update.clone()),
                )
                .await;
                match persistence {
                    Ok(true) => {
                        session
                            .emit_terminalization_receipt(
                                turn_context.as_ref(),
                                &terminal_identity,
                            )
                            .await;
                    }
                    Ok(false) | Err(_) => {
                        warn!(turn_id = %turn_context.sub_id, "late terminalization receipt remains pending; retrying persistence only");
                        session.schedule_terminal_interaction_persistence_retry(
                            Arc::clone(&turn_context),
                            update,
                            None,
                        );
                    }
                }
            });
        }
    }

    pub(crate) fn spawn_terminal_receipt_task<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.terminal_tasks.spawn(task);
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

    pub(crate) async fn terminate_background_terminal(&self, process_id: u32) -> bool {
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
            .finish_turn(&turn_context.sub_id)
            .await;
        finalization.mutation_quiescent = true;
        finalization.task.cancellation_token.cancel();
        finalization.task.worker_abort_handle.abort();
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        turn_context.turn_timing_state.begin_finalization();
        let missing_call_output_repair_succeeded = matches!(
            finalization
                .deadline
                .run(
                    "preparation",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.persist_missing_call_outputs_durable(&turn_context),
                )
                .await,
            Ok(Ok(_))
        );

        let abort_reason = match &finalization.outcome {
            TurnTerminalOutcome::Aborted(reason) => Some(reason.clone()),
            TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted) => {
                Some(TurnAbortReason::Interrupted)
            }
            _ => None,
        };
        let mut interrupted_marker_repair = Vec::new();
        if abort_reason == Some(TurnAbortReason::Interrupted)
            && let Some(marker) = interrupted_turn_history_marker(
                InterruptedTurnHistoryMarker::from_config_and_version(
                    turn_context.config.as_ref(),
                    turn_context.multi_agent_version,
                ),
            )
            && !matches!(
                finalization
                    .deadline
                    .run(
                        "preparation",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.record_conversation_items_durable(
                            &turn_context,
                            std::slice::from_ref(&marker),
                        ),
                    )
                    .await,
                Ok(Ok(()))
            )
        {
            interrupted_marker_repair.push(marker);
        }
        let rollout_structure_ready = terminal_rollout_structure_ready(
            missing_call_output_repair_succeeded,
            interrupted_marker_repair.is_empty(),
        );

        let prior_delivery_state = finalization.coordinator.delivery_state();
        let had_authoritative_claim =
            prior_delivery_state != CoordinatorDeliveryState::NotAttempted;
        let timing_snapshot = turn_context.turn_timing_state.complete_snapshot();
        let timing = timing_snapshot.protocol_timing();
        finalization
            .pending_turn_profile
            .get_or_insert_with(|| TurnProfileFact {
                turn_id: turn_context.sub_id.clone(),
                profile: timing_snapshot.legacy_profile.clone(),
                timing: Some(timing.clone()),
            });
        let mut dispatched_event = None;
        let cleared_active_turn = {
            if let Some(duration) = timing_snapshot.inclusive_duration() {
                turn_context.session_telemetry.record_duration(
                    TURN_E2E_DURATION_METRIC,
                    duration,
                    &[],
                );
            }
            let mut event = if let Some(reason) = abort_reason.clone() {
                EventMsg::TurnAborted(TurnAbortedEvent {
                    turn_id: Some(turn_context.sub_id.clone()),
                    reason,
                    completed_at: timing_snapshot.completed_at_unix_secs,
                    duration_ms: timing_snapshot.duration_ms,
                    timing: Some(timing),
                })
            } else {
                let reason = "turn finalization entered the fail-safe path before an authoritative terminal event was established";
                EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: turn_context.sub_id.clone(),
                    last_agent_message: Some(reason.to_string()),
                    surfaced_result: None,
                    error: Some(ErrorEvent {
                        message: reason.to_string(),
                        codex_error_info: Some(CodexErrorInfo::InternalServerError),
                    }),
                    completion: Some(TaskCompletionGate {
                        status: TaskCompletionStatus::Partial,
                        reasons: vec![reason.to_string()],
                        evidence_path: None,
                    }),
                    completed_at: timing_snapshot.completed_at_unix_secs,
                    duration_ms: timing_snapshot.duration_ms,
                    time_to_first_token_ms: timing_snapshot.time_to_first_token_ms,
                    timing: Some(timing),
                })
            };
            let milestone = self
                .publish_terminal_interaction_milestone(
                    finalization,
                    turn_context.as_ref(),
                    &mut event,
                    "fail_safe".to_string(),
                    false,
                    rollout_structure_ready,
                    interrupted_marker_repair,
                )
                .await;
            if milestone.live_attempted {
                dispatched_event = Some(event);
            }
            milestone.cleared_active_turn
        };

        if cleared_active_turn {
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.emit_thread_idle_lifecycle_if_idle(),
            )
            .await;
        }

        // Fail-safe cleanup is also post-milestone. Bound every persistence/lifecycle operation
        // so the stronger cleanup signal remains useful to shutdown and tests.
        let post_cleanup_started = tokio::time::Instant::now();
        let _ = tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.input_queue
                .clear_pending_for_turn_state(finalization.turn_state.as_ref()),
        )
        .await;
        if !finalization.coordinator.durable_terminal_committed() {
            let reason = "terminal emission failed after completion-review closure";
            let invalidation = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(reason);
            let transition =
                tokio::time::timeout(TERMINAL_MUTATION_FINALIZATION_TIMEOUT, invalidation).await;
            if !matches!(
                transition,
                Ok(crate::task_evidence::AtomicReviewTransition::Persisted(()))
            ) {
                warn!(turn_id = %turn_context.sub_id, "completion invalidation did not persist during fail-safe cleanup");
                self.schedule_completion_invalidation_retry(reason.to_string());
            }
        }
        let mut fail_safe_parent_notification_completed = false;
        if let Some(event) = dispatched_event.as_ref() {
            let parent_notification_completed = match tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.finish_terminal_event_dispatch(turn_context.as_ref(), event),
            )
            .await
            {
                Ok(completed) => completed,
                Err(_) => {
                    warn!(turn_id = %turn_context.sub_id, "timed out finishing fail-safe terminal event side effects");
                    false
                }
            };
            fail_safe_parent_notification_completed = parent_notification_completed;
            if !parent_notification_completed {
                self.schedule_parent_terminal_notification_retry(
                    Arc::clone(&turn_context),
                    event.clone(),
                );
            }
        }
        if let Some(summary) = finalization
            .reasoning_policy_recorder
            .take_summary(turn_context.sub_id.clone())
            && tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.send_event(
                    turn_context.as_ref(),
                    EventMsg::ReasoningPolicySummary(summary),
                ),
            )
            .await
            .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out emitting fail-safe reasoning summary");
        }
        let circuit_breaker_cleanup = async {
            self.services
                .guardian_rejection_circuit_breaker
                .lock()
                .await
                .clear_turn(&turn_context.sub_id);
        };
        let _ = tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            circuit_breaker_cleanup,
        )
        .await;
        match tokio::time::timeout(TERMINAL_MUTATION_FINALIZATION_TIMEOUT, self.flush_rollout())
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!("failed to flush rollout after fail-safe terminal event: {err}"),
            Err(_) => {
                warn!(turn_id = %turn_context.sub_id, "timed out flushing rollout after fail-safe terminal event")
            }
        }
        finalization.deadline.record_elapsed(
            "post_cleanup",
            tokio::time::Instant::now().saturating_duration_since(post_cleanup_started),
        );
        finalization.deadline.finish_unclassified();
        let _ = self.emit_pending_turn_profile(finalization);
        if had_authoritative_claim || finalization.coordinator.durable_terminal_committed() {
            let delivery_state = match finalization.coordinator.delivery_state() {
                CoordinatorDeliveryState::NotAttempted => DurableDeliveryState::NotAttempted,
                CoordinatorDeliveryState::Claimed => DurableDeliveryState::Claimed,
                CoordinatorDeliveryState::Delivered => DurableDeliveryState::Delivered,
                CoordinatorDeliveryState::DeliveryFailed => DurableDeliveryState::DeliveryFailed,
            };
            let update = TerminalInteractionUpdate {
                terminal_identity: format!("{}:{}", self.thread_id, turn_context.sub_id),
                delivery_state,
                app_server_acknowledged: false,
                runtime_status_converged: true,
                rollout_mirrored: false,
                parent_notification_completed: fail_safe_parent_notification_completed,
                post_terminal_cleanup_completed: true,
                active_turn_detached: true,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::Recovered,
                phase_timings_ns: finalization.deadline.phase_timings_ns(),
                terminalization: None,
            };
            let persisted = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services
                    .task_evidence
                    .update_terminal_interaction(update.clone()),
            )
            .await;
            if !matches!(persisted, Ok(true)) {
                warn!(turn_id = %turn_context.sub_id, "fail-safe terminal interaction receipt remains pending; retrying persistence only");
                self.schedule_terminal_interaction_persistence_retry(
                    Arc::clone(&turn_context),
                    update,
                    None,
                );
            }
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

fn semantic_terminal_outcome(event: &EventMsg) -> String {
    match event {
        EventMsg::TurnAborted(_) => "aborted".to_string(),
        EventMsg::TurnComplete(completed) if completed.error.is_some() => "failed".to_string(),
        EventMsg::TurnComplete(completed) => completed
            .completion
            .as_ref()
            .map(|gate| match gate.status {
                TaskCompletionStatus::Passed => "passed",
                TaskCompletionStatus::Partial => "partial",
                TaskCompletionStatus::Blocked => "blocked",
            })
            .unwrap_or("completed")
            .to_string(),
        _ => "terminal".to_string(),
    }
}

fn last_terminal_authority_from_rollout(
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> Option<AuthoritativeTerminalEventV1> {
    items.iter().rev().find_map(|item| {
        let RolloutItem::EventMsg(event) = item else {
            return None;
        };
        let turn_id = match event {
            EventMsg::TurnComplete(event) => event.turn_id.clone(),
            EventMsg::TurnAborted(event) => event.turn_id.clone()?,
            _ => return None,
        };
        let fingerprint = terminal_event_fingerprint(event)?;
        Some(AuthoritativeTerminalEventV1 {
            version: 1,
            terminal_identity: format!("{thread_id}:{turn_id}"),
            turn_id,
            event: event.clone(),
            fingerprint,
            semantic_outcome: semantic_terminal_outcome(event),
            final_proof_identity: None,
            rollout_repair: TerminalRolloutRepairV1::default(),
        })
    })
}

pub(crate) fn open_turn_id_for_terminal_recovery(items: &[RolloutItem]) -> Option<String> {
    let mut open_turn_id = None;
    for item in items {
        let RolloutItem::EventMsg(event) = item else {
            continue;
        };
        match event {
            EventMsg::TurnStarted(started) => open_turn_id = Some(started.turn_id.clone()),
            EventMsg::TurnComplete(completed)
                if open_turn_id.as_deref() == Some(completed.turn_id.as_str()) =>
            {
                open_turn_id = None;
            }
            EventMsg::TurnAborted(aborted)
                if aborted.turn_id.as_deref() == open_turn_id.as_deref() =>
            {
                open_turn_id = None;
            }
            _ => {}
        }
    }
    open_turn_id
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
