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
use crate::task_evidence::CandidateDiffSnapshotV1;
use crate::task_evidence::CompletionCheckpointV1;
use crate::task_evidence::FinalProofSealInputV1;
use crate::task_evidence::FinalProofSealResultV1;
use crate::task_evidence::TerminalClaimResult;
use crate::task_evidence::TerminalDecisionClaim;
use crate::task_evidence::TerminalDeliveryState as DurableDeliveryState;
use crate::task_evidence::TerminalInteractionUpdate;
use crate::task_evidence::TerminalRecoveryState;
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
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
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

const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TERMINAL_MUTATION_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(1);
const TERMINALIZATION_DEADLINE: Duration = Duration::from_secs(5);
const TASK_COMPACT_METRIC: &str = "codex.task.compact";
const WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON: &str =
    "the workspace finalization fence could not be sealed for terminal dispatch";

pub(crate) type SessionTaskResult = CodexResult<Option<String>>;

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
    timing.terminalization = TurnTimingTerminalization {
        final_mutation_to_seal_ns: phases
            .get("hooks_quiescence")
            .copied()
            .unwrap_or_default()
            .saturating_add(phases.get("fence").copied().unwrap_or_default())
            .saturating_add(phases.get("final_proof_gate").copied().unwrap_or_default()),
        completion_gate_ns: phases.get("final_proof_gate").copied().unwrap_or_default(),
        review_preflight_ns: phases.get("review_preflight").copied().unwrap_or_default(),
        review_ns: phases.get("review").copied().unwrap_or_default(),
        terminal_memo_hit_count: u32::from(phases.contains_key("terminal_memo_hit")),
        diff_refresh_count: u32::from(phases.contains_key("diff_refresh")),
        preparation_ns: phases.get("preparation").copied().unwrap_or_default(),
        hooks_quiescence_ns: phases.get("hooks_quiescence").copied().unwrap_or_default(),
        fence_ns: phases.get("fence").copied().unwrap_or_default(),
        freshness_ns: phases.get("freshness").copied().unwrap_or_default(),
        gate_ns: phases.get("gate").copied().unwrap_or_default(),
        durable_commit_ns: phases.get("durable_commit").copied().unwrap_or_default(),
        delivery_attempt_ns: phases.get("delivery_attempt").copied().unwrap_or_default(),
        interaction_release_ns: phases
            .get("interaction_release")
            .copied()
            .unwrap_or_default(),
        post_cleanup_ns: phases.get("post_cleanup").copied().unwrap_or_default(),
        unclassified_ns: phases.get("unclassified").copied().unwrap_or_default(),
        ..TurnTimingTerminalization::default()
    };
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
    deadline: TerminalDeadline,
    mutation_quiescent: bool,
}

struct TerminalInteractionMilestone {
    live_attempted: bool,
    live_delivered: bool,
    cleared_active_turn: bool,
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
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                result = store.heartbeat_workspace_finalization(
                                    &repo_root,
                                    fence_id.clone(),
                                    root_session_id.clone(),
                                ) => {
                                    match result {
                                        Ok(true) => {}
                                        Ok(false) | Err(_) => {
                                            healthy.store(false, Ordering::Release);
                                            break;
                                        }
                                    }
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

    async fn seal_for_terminal_dispatch(
        &mut self,
        deadline: &TerminalDeadline,
    ) -> Result<(), String> {
        if !self.is_healthy() {
            return Err("workspace finalization fence is unhealthy".to_string());
        }
        let Some(fence) = self.fence.clone() else {
            self.healthy.store(false, Ordering::Release);
            return Err("workspace finalization fence is missing".to_string());
        };
        let seal_result = deadline
            .run(
                "fence",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.store
                    .seal_workspace_finalization_dispatch(&self.repo_root, fence),
            )
            .await;
        match seal_result {
            Err(_) => {
                self.healthy.store(false, Ordering::Release);
                Err("timed out sealing the workspace finalization fence".to_string())
            }
            Ok(Ok(sealed_fence)) => {
                self.fence = Some(sealed_fence);
                if self.is_healthy() {
                    Ok(())
                } else {
                    Err("workspace finalization fence became unhealthy while sealing".to_string())
                }
            }
            Ok(Err(error)) => {
                self.healthy.store(false, Ordering::Release);
                Err(error.to_string())
            }
        }
    }

    async fn release(&mut self) -> Result<(), String> {
        self.heartbeat_cancel.cancel();
        if let Some(task) = self.heartbeat_task.take() {
            // Do not wait for task-store I/O already in progress inside the heartbeat.
            task.abort();
            let _ = task.await;
        }
        let Some(fence) = self.fence.clone() else {
            return Ok(());
        };
        let release_result = tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.store
                .release_workspace_finalization(&self.repo_root, fence),
        )
        .await;
        match release_result {
            Ok(Ok(())) => {
                self.fence = None;
                Ok(())
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err("timed out releasing the workspace finalization fence".to_string()),
        }
    }
}

async fn seal_passed_completion_for_terminal_dispatch(
    completion: &mut Option<TaskCompletionGate>,
    guard: Option<&mut WorkspaceFinalizationGuard>,
    deadline: &TerminalDeadline,
) -> Option<&'static str> {
    if !completion
        .as_ref()
        .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed)
    {
        return None;
    }
    let guard = guard?;
    if let Err(error) = guard.seal_for_terminal_dispatch(deadline).await {
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
            "workspace_epoch": workspace_epoch,
            "workspace_path_snapshots": &workspace_path_snapshot_identity,
        }),
    );
    let workspace_manifest_identity = final_proof_hash(
        "KD4_FINAL_PROOF_WORKSPACE_MANIFEST_V1",
        &serde_json::json!({
            "workspace_epoch": workspace_epoch,
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

impl Drop for WorkspaceFinalizationGuard {
    fn drop(&mut self) {
        self.heartbeat_cancel.cancel();
        if let Some(task) = self.heartbeat_task.take() {
            task.abort();
        }
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
            match tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                store.release_workspace_finalization(&repo_root, fence.clone()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(
                    fence_id = %fence.fence_id,
                    %error,
                    "failed to release workspace finalization fence during cleanup"
                ),
                Err(_) => warn!(
                    fence_id = %fence.fence_id,
                    "timed out releasing workspace finalization fence during cleanup; relying on lease expiry"
                ),
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
            || self.terminal_interaction_pending.load(Ordering::Acquire)
        {
            return;
        }
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
            return;
        }

        {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.is_some()
                || self.terminal_interaction_pending.load(Ordering::Acquire)
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
            // The supervisor has accepted the final task result at this point. Start the
            // single terminalization clock before scheduling the session-owned finalizer so
            // executor delay cannot extend the five-second correctness window.
            let terminal_deadline = TerminalDeadline::start();
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
                    deadline: terminal_deadline,
                    mutation_quiescent: false,
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
        // Completion finalization admission and execution/activity guards are part of the
        // interactive turn fence. Release them before notifying abort/replacement waiters.
        drop(finalization.completion_finalization_permit.take());
        drop(finalization.task._agent_execution_guard.take());
        drop(finalization.task._completion_activity_guard.take());
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
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_interaction_released();
            }
            self.terminal_interaction_pending
                .store(false, Ordering::Release);
            detached
        };
        finalization.deadline.record_elapsed(
            "interaction_release",
            tokio::time::Instant::now().saturating_duration_since(started),
        );
        active_turn_detached
    }

    async fn publish_terminal_interaction_milestone(
        &self,
        finalization: &mut TerminalFinalization,
        turn_context: &TurnContext,
        event: &mut EventMsg,
        durable_outcome: String,
        durable_success_established: bool,
    ) -> TerminalInteractionMilestone {
        let terminal_identity = format!("{}:{}", self.thread_id, turn_context.sub_id);
        apply_terminal_phase_timings(event, &finalization.deadline.phase_timings_ns());
        let durable_rollout = finalization
            .deadline
            .run(
                "durable_commit",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.persist_terminal_event_for_dispatch(
                    event,
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                ),
            )
            .await;
        let durable_rollout_committed = matches!(durable_rollout, Ok(Ok(())));
        if !durable_rollout_committed {
            let reason = match durable_rollout {
                Ok(Ok(())) => unreachable!("successful rollout handled above"),
                Ok(Err(error)) => error,
                Err(TerminalWaitError::OperationTimedOut) => {
                    "terminal event persistence timed out".to_string()
                }
                Err(TerminalWaitError::DeadlineExhausted) => {
                    "terminalization deadline expired before terminal persistence".to_string()
                }
            };
            warn!(turn_id = %turn_context.sub_id, %reason, "durable terminal decision was not established");
            if let Some(permit) = finalization.permit.as_ref() {
                permit.mark_durable_commit(false);
            }
            self.try_send_live_event(Event {
                id: turn_context.sub_id.clone(),
                msg: EventMsg::Error(ErrorEvent {
                    message: format!(
                        "Turn terminal storage failed; successful completion was not established: {reason}"
                    ),
                    codex_error_info: Some(CodexErrorInfo::InternalServerError),
                }),
            });
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        }

        let claim = TerminalDecisionClaim {
            terminal_identity: terminal_identity.clone(),
            durable_outcome,
            deadline_exhausted_phase: finalization.deadline.exhausted_phase(),
            mutation_quiescent: finalization.mutation_quiescent,
            durable_success_established,
            retained_ownership: (!finalization.mutation_quiescent)
                .then(|| "turn-owned mutation lease or fence".to_string())
                .into_iter()
                .collect(),
            phase_timings_ns: finalization.deadline.phase_timings_ns(),
        };
        let claim_result = finalization
            .deadline
            .run(
                "durable_commit",
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services
                    .task_evidence
                    .commit_terminal_decision_and_claim(claim),
            )
            .await;
        let claim_committed = matches!(claim_result, Ok(TerminalClaimResult::Claimed));
        if let Some(permit) = finalization.permit.as_ref() {
            permit.mark_durable_commit(claim_committed);
        }
        if !claim_committed {
            let reason = match claim_result {
                Ok(TerminalClaimResult::AlreadyClaimed) => {
                    "terminal live-delivery identity was already durably claimed"
                }
                Ok(TerminalClaimResult::Failed) => "terminal decision claim persistence failed",
                Ok(TerminalClaimResult::Claimed) => unreachable!("handled above"),
                Err(TerminalWaitError::OperationTimedOut) => {
                    "terminal decision claim persistence timed out"
                }
                Err(TerminalWaitError::DeadlineExhausted) => {
                    "terminalization deadline expired before the delivery claim"
                }
            };
            warn!(turn_id = %turn_context.sub_id, %reason);
            // Recovery or a duplicate in-process caller that sees an authoritative claim must
            // converge interaction ownership without retrying the live terminal event.
            if !matches!(claim_result, Ok(TerminalClaimResult::AlreadyClaimed)) {
                self.try_send_live_event(Event {
                    id: turn_context.sub_id.clone(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: format!(
                            "Turn terminal storage failed; successful completion was not established: {reason}"
                        ),
                        codex_error_info: Some(CodexErrorInfo::InternalServerError),
                    }),
                });
            }
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        }

        let coordinator_claimed = finalization
            .permit
            .as_ref()
            .is_some_and(TurnTerminalPermit::mark_delivery_claimed);
        if !coordinator_claimed {
            warn!(turn_id = %turn_context.sub_id, "terminal delivery was already claimed in memory; refusing duplicate send");
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            return TerminalInteractionMilestone {
                live_attempted: false,
                live_delivered: false,
                cleared_active_turn,
            };
        }

        apply_terminal_phase_timings(event, &finalization.deadline.phase_timings_ns());
        let delivery_started = tokio::time::Instant::now();
        let live_delivered = self.dispatch_terminal_event_live(turn_context, event.clone());
        finalization.deadline.record_elapsed(
            "delivery_attempt",
            tokio::time::Instant::now().saturating_duration_since(delivery_started),
        );
        if let Some(permit) = finalization.permit.as_ref() {
            permit.mark_delivery_attempted(live_delivered);
        }
        let cleared_active_turn = self.detach_terminal_turn(finalization).await;

        let delivery_state = if live_delivered {
            DurableDeliveryState::Delivered
        } else {
            DurableDeliveryState::DeliveryFailed
        };
        let update = TerminalInteractionUpdate {
            terminal_identity,
            delivery_state,
            active_turn_detached: cleared_active_turn,
            terminal_interaction_released: true,
            recovery_state: TerminalRecoveryState::None,
            phase_timings_ns: finalization.deadline.phase_timings_ns(),
        };
        match tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.services
                .task_evidence
                .update_terminal_interaction(update),
        )
        .await
        {
            Ok(true) => {}
            Ok(false) | Err(_) => warn!(
                turn_id = %turn_context.sub_id,
                "terminal interaction receipt remains claimed; recovery will converge without resending"
            ),
        }

        TerminalInteractionMilestone {
            live_attempted: true,
            live_delivered,
            cleared_active_turn,
        }
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

        // A mutating command can only finish its ledger cleanup after its task observes
        // cancellation. Signal the task before waiting for that cleanup, otherwise an
        // interrupt can deadlock here and the app server never receives TurnAborted.
        let mutation_cleanup_completed = matches!(
            finalization
                .deadline
                .run(
                    "hooks_quiescence",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .command_execution
                        .cancel_mutations_for_terminal_turn_with_timeout(
                            &turn_context.sub_id,
                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        ),
                )
                .await,
            Ok(true)
        );
        if !mutation_cleanup_completed {
            warn!(
                turn_id = %turn_context.sub_id,
                "timed out waiting for turn-owned workspace mutations to finalize"
            );
        }
        if requires_abort_cleanup {
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
            let marker_persistence = async {
                self.record_conversation_items(
                    turn_context.as_ref(),
                    std::slice::from_ref(&marker),
                )
                .await;
                self.flush_rollout().await
            };
            match finalization
                .deadline
                .run(
                    "preparation",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    marker_persistence,
                )
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!("failed to flush interrupted-turn marker before terminal event: {err}")
                }
                Err(_) => {
                    warn!(turn_id = %turn_context.sub_id, "timed out persisting interrupted-turn marker before terminal event")
                }
            }
        }

        let (mut last_agent_message, abort_reason) = match &finalization.outcome {
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
        let post_hook_mutation_cleanup_completed = matches!(
            finalization
                .deadline
                .run(
                    "hooks_quiescence",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .command_execution
                        .cancel_mutations_for_terminal_turn_with_timeout(
                            &turn_context.sub_id,
                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        ),
                )
                .await,
            Ok(true)
        );
        finalization.mutation_quiescent = mutation_cleanup_completed
            && pending_input_result.is_ok()
            && terminal_lifecycle_result.is_ok()
            && post_hook_mutation_cleanup_completed;

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
        if !finalization.mutation_quiescent {
            completion_review_partial_reasons.push(
                "terminal mutation quiescence could not be established before the absolute deadline"
                    .to_string(),
            );
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
                            match tokio::time::timeout(
                                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                                store.begin_workspace_finalization(&repo_root, root_session_id),
                            )
                            .await
                            {
                                Ok(Ok(fence)) => {
                                    finalization.workspace_finalization_guard = Some(
                                        WorkspaceFinalizationGuard::new(store, repo_root, fence),
                                    );
                                }
                                Ok(Err(error)) => completion_review_partial_reasons.push(format!(
                                    "workspace finalization fence could not be acquired: {error}"
                                )),
                                Err(_) => completion_review_partial_reasons.push(
                                    "workspace finalization fence acquisition timed out".to_string(),
                                ),
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
                                        let retry_dossier = self
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
                                                    let _ = self
                                                        .services
                                                        .task_evidence
                                                        .supersede_provisional_completion_review(
                                                            retry_dossier,
                                                        )
                                                        .await;
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
            let _ = finalization
                .deadline
                .run(
                    "freshness",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .task_evidence
                        .invalidate_completion_after_terminal_emission_failure(reason),
                )
                .await;
            if let Some(gate) = atomically_persisted_completion.as_mut() {
                gate.status = TaskCompletionStatus::Partial;
                gate.reasons.push(reason.to_string());
                gate.reasons.sort();
                gate.reasons.dedup();
            }
            completion_review_partial_reasons.push(reason.to_string());
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
            if !finalization.mutation_quiescent {
                final_proof_child_gate_state.push(
                    "terminal mutation quiescence was not established before candidate sealing"
                        .to_string(),
                );
            }
            if let Some(gate) = completion
                .as_ref()
                .filter(|gate| gate.status != TaskCompletionStatus::Passed)
            {
                final_proof_child_gate_state.extend(gate.reasons.iter().cloned());
            }
            final_proof_child_gate_state.sort();
            final_proof_child_gate_state.dedup();

            if finalization.completion_finalization_permit.is_none() {
                match finalization
                    .deadline
                    .run(
                        "gate",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.services.agent_control.begin_completion_finalization(),
                    )
                    .await
                {
                    Ok(Ok(permit)) => finalization.completion_finalization_permit = Some(permit),
                    Ok(Err(error)) => final_proof_child_gate_state.push(format!(
                        "completion finalization admission could not be acquired: {error}"
                    )),
                    Err(_) => final_proof_child_gate_state
                        .push("completion finalization admission timed out".to_string()),
                }
            }
            if finalization.completion_finalization_permit.is_some()
                && finalization.workspace_finalization_guard.is_none()
            {
                let coordinator = self.services.agent_control.task_coordinator();
                match (
                    coordinator.store(),
                    coordinator.root_session_id(),
                    self.services.task_evidence.repository_root(),
                ) {
                    (Some(store), Some(root_session_id), Some(repo_root)) => {
                        let repo_root = repo_root.to_path_buf();
                        match finalization
                            .deadline
                            .run(
                                "fence",
                                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                                store.begin_workspace_finalization(&repo_root, root_session_id),
                            )
                            .await
                        {
                            Ok(Ok(fence)) => {
                                finalization.workspace_finalization_guard =
                                    Some(WorkspaceFinalizationGuard::new(store, repo_root, fence));
                            }
                            Ok(Err(error)) => final_proof_child_gate_state.push(format!(
                                "workspace finalization fence could not be acquired: {error}"
                            )),
                            Err(_) => final_proof_child_gate_state.push(
                                "workspace finalization fence acquisition timed out".to_string(),
                            ),
                        }
                    }
                    (None, None, _) => {}
                    _ => final_proof_child_gate_state.push(
                        "typed-work finalization state was only partially initialized".to_string(),
                    ),
                }
            }
            let fence_ready = finalization
                .workspace_finalization_guard
                .as_ref()
                .is_none_or(WorkspaceFinalizationGuard::is_healthy);
            if finalization.completion_finalization_permit.is_some() && fence_ready {
                finalization
                    .deadline
                    .record_elapsed("diff_refresh", Duration::ZERO);
                let sealed = finalization
                    .deadline
                    .run(
                        "final_proof_gate",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        seal_terminal_final_proof(
                            self,
                            &turn_context,
                            last_agent_message.as_deref(),
                            final_proof_child_gate_state,
                        ),
                    )
                    .await;
                match sealed {
                    Ok(Some(FinalProofSealResultV1::Sealed { gate, .. }))
                    | Ok(Some(FinalProofSealResultV1::Memoized(gate)))
                    | Ok(Some(FinalProofSealResultV1::PreflightFailed(gate))) => {
                        if gate.status == TaskCompletionStatus::Passed {
                            let memoized = finalization
                                .deadline
                                .run(
                                    "completion_finalization",
                                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                                    self.services.task_evidence.memoized_finalization_result(),
                                )
                                .await;
                            if let Ok(Some(memoized)) = memoized {
                                finalization
                                    .deadline
                                    .record_elapsed("terminal_memo_hit", Duration::ZERO);
                                last_agent_message = Some(memoized);
                            } else if let Some(message) = last_agent_message.clone() {
                                let _ = finalization
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
                                            let Ok(checkpoint) =
                                                serde_json::from_str::<CompletionCheckpointV1>(
                                                    &payload,
                                                )
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
                                                .record_finalization_result(message)
                                                .await
                                        },
                                    )
                                    .await;
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
            } else {
                merge_completion_review_partial(&mut completion, final_proof_child_gate_state);
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
        self.services
            .analytics_events_client
            .track_turn_profile(TurnProfileFact {
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
            let terminal_invalidation_reason = if finalization
                .workspace_finalization_guard
                .as_ref()
                .is_some_and(|guard| !guard.is_healthy())
            {
                Some("the workspace finalization fence was lost before terminal emission")
            } else if let Some(authoritative) = terminal_authoritative_inputs.as_ref() {
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
                let _ = finalization
                    .deadline
                    .run(
                        "freshness",
                        TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        self.services
                            .task_evidence
                            .invalidate_completion_after_terminal_emission_failure(reason),
                    )
                    .await;
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
            let _ = finalization
                .deadline
                .run(
                    "freshness",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .task_evidence
                        .invalidate_completion_after_terminal_emission_failure(reason),
                )
                .await;
        }
        if abort_reason.is_none()
            && let Some(reason) = seal_passed_completion_for_terminal_dispatch(
                &mut completion,
                finalization.workspace_finalization_guard.as_mut(),
                &finalization.deadline,
            )
            .await
        {
            let _ = finalization
                .deadline
                .run(
                    "freshness",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .task_evidence
                        .invalidate_completion_after_terminal_emission_failure(reason),
                )
                .await;
        }
        let mut passed_root_completion = abort_reason.is_none()
            && !turn_context.session_source.is_non_root_agent()
            && completion
                .as_ref()
                .is_some_and(|gate| gate.status == TaskCompletionStatus::Passed);
        if abort_reason.is_none()
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
        let durable_outcome = match &event {
            EventMsg::TurnAborted(_) => "aborted".to_string(),
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
        };
        let terminal_milestone = self
            .publish_terminal_interaction_milestone(
                finalization,
                turn_context.as_ref(),
                &mut event,
                durable_outcome,
                passed_root_completion,
            )
            .await;
        if !finalization.coordinator.durable_terminal_committed() {
            passed_root_completion = false;
        }
        if terminal_milestone.live_attempted && !terminal_milestone.live_delivered {
            warn!(turn_id = %turn_context.sub_id, "terminal live delivery failed; durable decision remains authoritative");
        }
        let cleared_active_turn = terminal_milestone.cleared_active_turn;
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
        if terminal_milestone.live_attempted
            && tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.finish_terminal_event_dispatch(turn_context.as_ref(), &event),
            )
            .await
            .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out finishing terminal event side effects");
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
        if let Some(mut guard) = finalization.workspace_finalization_guard.take()
            && let Err(error) = guard.release().await
        {
            warn!(%error, "failed to release workspace finalization fence after terminal emission");
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
        if cleared_active_turn {
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.emit_thread_idle_lifecycle_if_idle(),
            )
            .await;
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
        if finalization.coordinator.durable_terminal_committed() {
            let delivery_state = match finalization.coordinator.delivery_state() {
                CoordinatorDeliveryState::NotAttempted => DurableDeliveryState::NotAttempted,
                CoordinatorDeliveryState::Claimed => DurableDeliveryState::Claimed,
                CoordinatorDeliveryState::Delivered => DurableDeliveryState::Delivered,
                CoordinatorDeliveryState::DeliveryFailed => DurableDeliveryState::DeliveryFailed,
            };
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services.task_evidence.update_terminal_interaction(
                    TerminalInteractionUpdate {
                        terminal_identity: format!("{}:{}", self.thread_id, turn_context.sub_id),
                        delivery_state,
                        active_turn_detached: true,
                        terminal_interaction_released: true,
                        recovery_state: TerminalRecoveryState::None,
                        phase_timings_ns: finalization.deadline.phase_timings_ns(),
                    },
                ),
            )
            .await;
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
        let mutation_cleanup_completed = matches!(
            finalization
                .deadline
                .run(
                    "hooks_quiescence",
                    TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                    self.services
                        .command_execution
                        .cancel_mutations_for_terminal_turn_with_timeout(
                            &turn_context.sub_id,
                            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                        ),
                )
                .await,
            Ok(true)
        );
        finalization.mutation_quiescent &= mutation_cleanup_completed;
        if !mutation_cleanup_completed {
            warn!(
                turn_id = %turn_context.sub_id,
                "timed out waiting for turn-owned workspace mutations during fail-safe finalization"
            );
        }
        finalization.task.cancellation_token.cancel();
        finalization.task.worker_abort_handle.abort();
        turn_context
            .turn_metadata_state
            .cancel_git_enrichment_task();
        turn_context.turn_timing_state.begin_finalization();

        let prior_delivery_state = finalization.coordinator.delivery_state();
        let had_authoritative_claim =
            prior_delivery_state != CoordinatorDeliveryState::NotAttempted;
        let mut dispatched_event = None;
        let cleared_active_turn = if !had_authoritative_claim {
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
            let mut event = if let Some(reason) = abort_reason {
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
            let milestone = self
                .publish_terminal_interaction_milestone(
                    finalization,
                    turn_context.as_ref(),
                    &mut event,
                    "fail_safe".to_string(),
                    false,
                )
                .await;
            if milestone.live_attempted {
                dispatched_event = Some(event);
            }
            milestone.cleared_active_turn
        } else {
            // A crash/panic after the durable claim has an intentional at-most-once tradeoff:
            // never resend, but always converge the interactive milestone.
            let cleared_active_turn = self.detach_terminal_turn(finalization).await;
            let durable_delivery_state = match prior_delivery_state {
                CoordinatorDeliveryState::Claimed => DurableDeliveryState::Claimed,
                CoordinatorDeliveryState::Delivered => DurableDeliveryState::Delivered,
                CoordinatorDeliveryState::DeliveryFailed => DurableDeliveryState::DeliveryFailed,
                CoordinatorDeliveryState::NotAttempted => unreachable!("handled above"),
            };
            let update = TerminalInteractionUpdate {
                terminal_identity: format!("{}:{}", self.thread_id, turn_context.sub_id),
                delivery_state: durable_delivery_state,
                active_turn_detached: cleared_active_turn,
                terminal_interaction_released: true,
                recovery_state: TerminalRecoveryState::Recovered,
                phase_timings_ns: finalization.deadline.phase_timings_ns(),
            };
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services
                    .task_evidence
                    .update_terminal_interaction(update),
            )
            .await;
            cleared_active_turn
        };

        // Fail-safe cleanup is also post-milestone. Bound every persistence/lifecycle operation
        // so the stronger cleanup signal remains useful to shutdown and tests.
        let post_cleanup_started = tokio::time::Instant::now();
        let _ = tokio::time::timeout(
            TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
            self.input_queue
                .clear_pending_for_turn_state(finalization.turn_state.as_ref()),
        )
        .await;
        if !had_authoritative_claim && !finalization.coordinator.durable_terminal_committed() {
            let invalidation = self
                .services
                .task_evidence
                .invalidate_completion_after_terminal_emission_failure(
                    "terminal emission failed after completion-review closure",
                );
            if tokio::time::timeout(TERMINAL_MUTATION_FINALIZATION_TIMEOUT, invalidation)
                .await
                .is_err()
            {
                warn!(turn_id = %turn_context.sub_id, "timed out invalidating completion during fail-safe cleanup");
            }
        }
        if let Some(event) = dispatched_event.as_ref()
            && tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.finish_terminal_event_dispatch(turn_context.as_ref(), event),
            )
            .await
            .is_err()
        {
            warn!(turn_id = %turn_context.sub_id, "timed out finishing fail-safe terminal event side effects");
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
        if let Some(mut guard) = finalization.workspace_finalization_guard.take()
            && let Err(error) = guard.release().await
        {
            warn!(%error, "failed to release workspace finalization fence during fail-safe cleanup");
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
        if cleared_active_turn {
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.emit_thread_idle_lifecycle_if_idle(),
            )
            .await;
        }
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
        if had_authoritative_claim || finalization.coordinator.durable_terminal_committed() {
            let delivery_state = match finalization.coordinator.delivery_state() {
                CoordinatorDeliveryState::NotAttempted => DurableDeliveryState::NotAttempted,
                CoordinatorDeliveryState::Claimed => DurableDeliveryState::Claimed,
                CoordinatorDeliveryState::Delivered => DurableDeliveryState::Delivered,
                CoordinatorDeliveryState::DeliveryFailed => DurableDeliveryState::DeliveryFailed,
            };
            let _ = tokio::time::timeout(
                TERMINAL_MUTATION_FINALIZATION_TIMEOUT,
                self.services.task_evidence.update_terminal_interaction(
                    TerminalInteractionUpdate {
                        terminal_identity: format!("{}:{}", self.thread_id, turn_context.sub_id),
                        delivery_state,
                        active_turn_detached: true,
                        terminal_interaction_released: true,
                        recovery_state: TerminalRecoveryState::Recovered,
                        phase_timings_ns: finalization.deadline.phase_timings_ns(),
                    },
                ),
            )
            .await;
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
