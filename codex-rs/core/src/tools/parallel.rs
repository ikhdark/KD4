use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;



use futures::FutureExt;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;
use tokio::task::JoinError;
use tokio::time::Instant as TokioInstant;
use tokio_util::either::Either;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::info;
use tracing::trace_span;
use tracing::warn;

use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
use crate::scoped_workspace_gate::WORKSPACE_GATE_CALL;
use crate::scoped_workspace_gate::WorkspaceGateCall;
use crate::scoped_workspace_gate::WorkspaceGateHolder;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_execution::CodeModeToolResult;
use crate::session::turn_execution::SamplingRequestSignalCollector;
use crate::tools::context::AbortedToolOutput;
use crate::tools::context::RequiredToolTerminal;
use crate::tools::context::RequiredToolTerminalCause;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolDispatchAbort;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::lifecycle::notify_tool_aborted;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::install_synthetic_terminal_projection;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use crate::tools::router::ToolRouter;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;
use crate::turn_timing::ToolCallTimingLineage;
use crate::turn_timing::TurnTimingState;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;

pub(crate) const TOOL_RUNTIME_CLEANUP_DEADLINE: Duration = Duration::from_secs(30);

tokio::task_local! {
    static WORKSPACE_BASELINE_READY: CancellationToken;
}

pub(crate) async fn wait_for_workspace_baseline() {
    if let Ok(ready) = WORKSPACE_BASELINE_READY.try_with(Clone::clone) {
        ready.cancelled().await;
    }
}


#[derive(Debug)]
pub(crate) struct ToolCallCompletion {
    pub(crate) response: ResponseInputItem,
    pub(crate) required_terminal: Option<RequiredToolTerminal>,
    pub(crate) failure_detail: Option<ResponseItem>,
    pub(crate) pending_continuation_ack: Option<crate::tools::code_mode::PendingContinuationAcknowledgment>,
}

impl ToolCallCompletion {
    pub(crate) fn nonterminal(response: ResponseInputItem) -> Self {
        Self {
            response,
            required_terminal: None,
            failure_detail: None,
            pending_continuation_ack: None,
        }
    }
}

fn required_tool_terminal(
    call: &ToolCall,
    outcome_context: ToolOutputOutcomeContext,
    signal: Option<&serde_json::Value>,
) -> Option<RequiredToolTerminal> {
    let cause = required_tool_terminal_cause(outcome_context, signal)?;
    let label = match cause {
        RequiredToolTerminalCause::Blocked => "blocked",
        RequiredToolTerminalCause::Failure => "failed",
    };
    Some(RequiredToolTerminal {
        call_id: call.call_id.clone(),
        cause,
        message: format!("required tool `{}` {label}", call.tool_name),
    })
}

pub(crate) fn required_tool_terminal_cause(
    outcome_context: ToolOutputOutcomeContext,
    signal: Option<&serde_json::Value>,
) -> Option<RequiredToolTerminalCause> {
    if outcome_context.outcome == ToolOutputOutcome::Yielded {
        return None;
    }
    let signalled = signal
        .and_then(|value| value.get("outcome"))
        .and_then(serde_json::Value::as_str);
    match signalled {
        Some("blocked") => Some(RequiredToolTerminalCause::Blocked),
        _ => match outcome_context.outcome {
            ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => None,
            ToolOutputOutcome::Skipped
                if outcome_context.skip_disposition
                    == Some(ToolOutputSkipDisposition::BlockingRequiredOperation) =>
            {
                Some(RequiredToolTerminalCause::Blocked)
            }
            ToolOutputOutcome::Success
            | ToolOutputOutcome::Yielded
            | ToolOutputOutcome::Skipped => None,
        },
    }
}

pub(crate) fn required_tool_error_terminal_cause(
    error: &FunctionCallError,
) -> Option<RequiredToolTerminalCause> {
    match error {
        // Refusal applies to this operation. Required-operation outcomes and
        // turn cancellation have their own explicit terminal paths.
        FunctionCallError::Structured(failure)
            if failure.code == codex_protocol::tool_failure::ToolFailureCode::RequiredOperationBlocked =>
        {
            Some(RequiredToolTerminalCause::Blocked)
        }
        FunctionCallError::Structured(_) | FunctionCallError::DeniedToModel(_)
        | FunctionCallError::RespondToModel(_) => None,
        FunctionCallError::RequiredOperationBlocked(_) => Some(RequiredToolTerminalCause::Blocked),
        FunctionCallError::Fatal(_) => Some(RequiredToolTerminalCause::Failure),
    }
}

fn reused_failure_diagnosis(
    tool_name: &codex_tools::ToolName,
    failure_fingerprint: &str,
) -> String {
    serde_json::json!({
        "kind": "reused_failure_diagnosis",
        "tool_name": tool_name.to_string(),
        "failure_fingerprint": failure_fingerprint,
        "executed": false,
        "outcome": "skipped",
        "retry_condition": "changed_action_or_relevant_state",
        "required_action": "change_route_or_state",
        "reason": "Execution was suppressed because this action previously produced the same failure fingerprint against the observed unchanged state. This is reused failure evidence, not a new execution or a determination that the underlying failure is permanent.",
        "next_action": "Do not repeat this call with unchanged arguments; change the action or relevant state before the next call.",
    })
    .to_string()
}

fn reused_failure_completion(call: &ToolCall, failure_fingerprint: &str) -> ToolCallCompletion {
    let output = FunctionCallOutputPayload::from_text(reused_failure_diagnosis(
        &call.tool_name,
        failure_fingerprint,
    ));
    ToolCallCompletion::nonterminal(ResponseInputItem::FunctionCallOutput {
        call_id: call.call_id.clone(),
        output,
    })
}

struct ToolCallTimingGuard {
    timing: Arc<ToolDispatchTiming>,
    turn_timing_state: Option<Arc<TurnTimingState>>,
    conversation_id: String,
    turn_id: String,
    call_id: String,
    tool_name: codex_tools::ToolName,
    tool_source: &'static str,
    parent_cell_id: String,
    parent_model_call_id: Option<String>,
    runtime_tool_call_id: String,
    emit_log: bool,
}

struct ModelToolGateTimingGuard {
    turn_timing_state: Option<Arc<TurnTimingState>>,
}

enum LifecycleCounter {
    ParallelGateWaiter,
    ActiveTool,
}

struct LifecycleCounterGuard {
    turn_timing_state: Arc<TurnTimingState>,
    counter: LifecycleCounter,
}

impl LifecycleCounterGuard {
    fn increment(turn_timing_state: &Arc<TurnTimingState>, counter: LifecycleCounter) -> Self {
        match counter {
            LifecycleCounter::ParallelGateWaiter => {
                turn_timing_state.adjust_parallel_gate_waiters(1);
            }
            LifecycleCounter::ActiveTool => turn_timing_state.adjust_active_tools(1),
        }
        Self {
            turn_timing_state: Arc::clone(turn_timing_state),
            counter,
        }
    }
}

impl Drop for LifecycleCounterGuard {
    fn drop(&mut self) {
        match self.counter {
            LifecycleCounter::ParallelGateWaiter => {
                self.turn_timing_state.adjust_parallel_gate_waiters(-1);
            }
            LifecycleCounter::ActiveTool => self.turn_timing_state.adjust_active_tools(-1),
        }
    }
}

impl ModelToolGateTimingGuard {
    fn admitted(turn_timing_state: &Arc<TurnTimingState>, model_issued: bool) -> Self {
        let turn_timing_state = model_issued.then(|| {
            turn_timing_state.record_model_tool_gate_admitted();
            Arc::clone(turn_timing_state)
        });
        Self { turn_timing_state }
    }
}

impl Drop for ModelToolGateTimingGuard {
    fn drop(&mut self) {
        if let Some(turn_timing_state) = self.turn_timing_state.as_ref() {
            turn_timing_state.record_model_tool_gate_released();
        }
    }
}

fn tool_dispatch_outcome_label(result: &Result<AnyToolResult, FunctionCallError>) -> &'static str {
    match result {
        Ok(result) => match result.outcome_for_logging() {
            codex_tools::ToolOutputOutcome::Success => "success",
            codex_tools::ToolOutputOutcome::Failure => "failure",
            codex_tools::ToolOutputOutcome::TimedOut => "timed_out",
            codex_tools::ToolOutputOutcome::Yielded => "yielded",
            codex_tools::ToolOutputOutcome::Skipped => "skipped",
        },
        Err(
            FunctionCallError::DeniedToModel(_) | FunctionCallError::RequiredOperationBlocked(_),
        ) => "blocked",
        Err(FunctionCallError::Structured(failure)) => match failure.code {
            codex_protocol::tool_failure::ToolFailureCode::PermissionDenied
            | codex_protocol::tool_failure::ToolFailureCode::RequiredOperationBlocked => "blocked",
            codex_protocol::tool_failure::ToolFailureCode::Cancelled => "cancelled",
            _ => "failure",
        },
        Err(FunctionCallError::RespondToModel(_)) => "failure",
        Err(FunctionCallError::Fatal(_)) => "fatal",
    }
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    session: Arc<Session>,
    // Tool calls may run later, so retain the step whose tool list advertised them.
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
    global_gate_holders: WorkspaceGlobalHolders,
    workspace_execution: Arc<
        Mutex<
            std::collections::HashMap<
                std::path::PathBuf,
                Weak<crate::scoped_workspace_gate::ScopedWorkspaceGate>,
            >,
        >,
    >,
    canonical_workspace_resources: Arc<crate::git_workspace::WorkspaceRootCache>,
    sampling_request_signals: Option<SamplingRequestSignalCollector>,
    terminal_completion_only: bool,
    code_mode_user_intent: Option<(String, u64)>,
}

tokio::task_local! {
    static HOOK_USER_INTENT: Option<(String, u64)>;
}

fn admit_code_mode_user_intent<T>(
    session: &Session,
    intent: Option<&(String, u64)>,
    admit: impl FnOnce() -> T,
) -> Result<T, FunctionCallError> {
    let Some((cell_id, epoch)) = intent else {
        return Ok(admit());
    };
    session
        .input_queue
        .admit_code_mode_tool(*epoch, admit)
        .map_err(|current_epoch| {
            FunctionCallError::RespondToModel(serde_json::json!({
            "type": "input_changed_decision_required", "started": false,
            "cell_id": cell_id, "authoring_epoch": epoch, "current_epoch": current_epoch,
            "message": "Accepted user input changed before effect admission. Preserve completed results; author a new cell after considering the changed input. Do not replay effects with unknown outcomes."
            }).to_string())
        })
}

/// Shared by every direct and nested runtime in a turn.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceExecutionCoordinator {
    parallel_execution: Arc<RwLock<()>>,
    global_gate_holders: WorkspaceGlobalHolders,
    workspace_execution: Arc<
        Mutex<
            std::collections::HashMap<
                std::path::PathBuf,
                Weak<crate::scoped_workspace_gate::ScopedWorkspaceGate>,
            >,
        >,
    >,
}



tokio::task_local! {
    static HOOK_WORKSPACE_LEASE: Option<Arc<OwnedRwLockWriteGuard<()>>>;
}

/// Arbitrary command hooks have unknown effects. Keep their lease and mutation
/// accounting alive even if their caller is cancelled; hook timeouts still apply.
pub(crate) async fn run_workspace_hook<T: Send + 'static>(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: SharedTurnDiffTracker,
    cancellation: CancellationToken,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, FunctionCallError> {
    run_workspace_hook_with_receipt(session, turn, tracker, cancellation, work)
        .await
        .map(|(value, _)| value)
}

/// Identifies only the hook owner's own conservative mutation accounting.
/// It does not certify the command's effects or make earlier validation current.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkspaceHookMutationReceipt {
    before_revision: u64,
    after_revision: u64,
}

impl WorkspaceHookMutationReceipt {
    pub(crate) fn covers_only_hook_effect(&self, candidate: u64, current: u64) -> bool {
        self.before_revision == candidate
            && self.after_revision == current
            && self.before_revision.checked_add(1) == Some(self.after_revision)
    }
}

pub(crate) async fn run_workspace_hook_with_receipt<T: Send + 'static>(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    tracker: SharedTurnDiffTracker,
    cancellation: CancellationToken,
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<(T, Option<WorkspaceHookMutationReceipt>), FunctionCallError> {
    let inherited = HOOK_WORKSPACE_LEASE.try_with(Clone::clone).ok().flatten();
    let user_intent = HOOK_USER_INTENT.try_with(Clone::clone).ok().flatten();
    let commit = session.retain_tool_dispatch_commit();
    let terminal_tasks = session.terminal_tasks.clone();
    terminal_tasks.spawn(async move {
        let _commit = commit;
        close_read_cohort(&turn.workspace_execution_coordinator.global_gate_holders);
        let coordinator = &turn.workspace_execution_coordinator;
        let _lease = match inherited {
            Some(lease) => lease,
            None => tokio::select! {
                lease = Arc::clone(&coordinator.parallel_execution).write_owned() => Arc::new(lease),
                _ = cancellation.cancelled() => return Err(FunctionCallError::RespondToModel("hook cancelled before execution".into())),
            },
        };
        if cancellation.is_cancelled() {
            return Err(FunctionCallError::RespondToModel("hook cancelled before execution".into()));
        }
        // The hook may have waited for a writer since its parent was checked.
        // Linearize its own effects against successful steering, without holding
        // the synchronous input fence across execution or storage work.
        admit_code_mode_user_intent(&session, user_intent.as_ref(), || ())?;
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            codex_hooks::observe_command_effects(Arc::clone(&entered), work),
        )).await;
        let mutation_receipt = if entered.load(std::sync::atomic::Ordering::Acquire) {
            session.services.git_workspace.note_host_workspace_mutation();
            let mut tracker = tracker.lock().await;
            let before_revision = tracker.current_mutation_revision();
            tracker.record_unknown_mutation();
            Some(WorkspaceHookMutationReceipt {
                before_revision,
                after_revision: tracker.current_mutation_revision(),
            })
        } else {
            None
        };
        match result {
            Ok(value) => Ok((value, mutation_receipt)),
            Err(_) => {
                tracing::warn!("workspace hook owner panicked");
                Err(FunctionCallError::Fatal("workspace hook owner panicked".into()))
            }
        }
    }).await.map_err(|error| FunctionCallError::Fatal(format!("hook owner failed: {error}")))?
}

/// Historical call sites keep an inert boundary while old rollouts remain readable.
pub(crate) struct WorkspaceEvidenceGenerationBatch;

pub(crate) struct WorkspaceEvidenceGenerationFlush {
    pub(crate) prefetched_workspace_identity: Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
}

impl WorkspaceEvidenceGenerationBatch {
    pub(crate) fn new() -> Self { Self }
    async fn capture_baseline_for_tool(
        &self, _cache: &crate::git_workspace::GitWorkspaceCache, _turn: Option<&TurnContext>,
        _classification: &crate::tool_history::WorkspaceCallClassification, _tool_name: &str, _mutation_revision: u64,
    ) -> WorkspaceEvidenceBaseline { WorkspaceEvidenceBaseline::default() }
    pub(crate) async fn record_captured_identity(&self, _cwd: &std::path::Path, _mutation_revision: u64,
        _identity: Option<crate::git_workspace::WorkspaceEvidenceIdentity>) {}
    pub(crate) async fn prefetch_identity_after_mutation(&self, _session: &Session, _turn: &TurnContext,
        _cwd: &std::path::Path, _mutation_revision: u64) {}
    fn retain_generation_owner(&self, _session: &Arc<Session>, _turn: &Arc<TurnContext>, _tracker: &SharedTurnDiffTracker) {}
    pub(crate) fn register_call(&self, _call_id: &str) -> bool { false }
    pub(crate) fn accepts_call(&self, _call_id: &str) -> bool { false }
    fn record_code_mode_workspace_observation(&self, _call_id: &str, _mutation_revision: u64) {}
    fn record_workspace_observation(&self, _call_id: &str) {}
    pub(crate) fn record_mutation(&self, _call_id: &str, _cwd: std::path::PathBuf,
        _paths: Option<std::collections::BTreeSet<std::path::PathBuf>>, _observe_command_ledger: bool) -> bool { false }
    fn queue_response(&self, _response: &ResponseInputItem, _classification: &crate::tool_history::WorkspaceCallClassification,
        _dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        _observations: Vec<crate::git_workspace::SourcePathChangeObservation>, _timing: Option<Arc<ToolDispatchTiming>>) -> bool { false }
    pub(crate) async fn flush(self: &Arc<Self>, _session: &Session, _turn: &TurnContext,
        _tracker: &SharedTurnDiffTracker) -> CodexResult<WorkspaceEvidenceGenerationFlush> {
        Ok(WorkspaceEvidenceGenerationFlush {
            prefetched_workspace_identity: None,
        })
    }
}

fn workspace_tool_may_use_parallel_gate(supports_parallel: bool, workspace_capable: bool) -> bool {
    supports_parallel && !workspace_capable
}


/// Resolve native file dependencies using the same ready environment as the
/// handler. Remote/starting/unknown environments have no host-watcher proof.
fn native_file_classification(
    original: crate::tool_history::WorkspaceCallClassification,
    tool_identity: &str,
    payload: &ToolPayload,
    environments: &crate::environment_selection::TurnEnvironmentSnapshot,
) -> crate::tool_history::WorkspaceCallClassification {
    if !matches!(
        tool_identity,
        "read_file" | "list_files" | "exec_command" | "shell" | "shell_command"
    ) {
        return original;
    }
    let mut conservative = original;
    conservative.source_dependencies.clear();
    conservative.source_scope_complete = false;
    let ToolPayload::Function { arguments } = payload else {
        return conservative;
    };
    let Ok(mut arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return conservative;
    };
    let environment = match arguments.get("environment_id") {
        None | Some(serde_json::Value::Null) => environments.primary(),
        Some(serde_json::Value::String(id)) => environments
            .turn_environments
            .iter()
            .find(|environment| environment.environment_id == *id),
        _ => None,
    };
    let Some(environment) = environment.filter(|environment| !environment.environment.is_remote())
    else {
        return conservative;
    };
    let Ok(cwd) = environment.cwd().to_abs_path() else {
        return conservative;
    };
    if let Some(arguments) = arguments.as_object_mut() {
        arguments.remove("environment_id");
    }
    crate::tool_history::classify_workspace_tool_call(
        tool_identity,
        &ToolPayload::Function {
            arguments: arguments.to_string(),
        },
        cwd.as_path(),
    )
}

fn workspace_tool_call_classifications_for_dispatch(
    source: &ToolCallSource,
    tool_identity: &str,
    payload: &ToolPayload,
    default_cwd: &std::path::Path,
    admission_hint: Option<crate::tool_history::WorkspaceCallClassification>,
) -> (
    crate::tool_history::WorkspaceCallClassification,
    Option<crate::tool_history::WorkspaceCallClassification>,
) {
    let admission = admission_hint.unwrap_or_else(|| {
        crate::tool_history::classify_workspace_tool_call(tool_identity, payload, default_cwd)
    });
    let inner_evidence = (!matches!(source, ToolCallSource::Direct)).then(|| admission.clone());
    (admission, inner_evidence)
}

fn workspace_evidence_baseline_is_compatible(
    original: &crate::tool_history::WorkspaceCallClassification,
    executed: &crate::tool_history::WorkspaceCallClassification,
) -> bool {
    original == executed
}

async fn executed_workspace_classification(
    original: &crate::tool_history::WorkspaceCallClassification,
    tool_identity: &str,
    executed_payload: Option<&ToolPayload>,
    default_cwd: &std::path::Path,
    environments: &crate::environment_selection::TurnEnvironmentSnapshot,
) -> Result<crate::tool_history::WorkspaceCallClassification, FunctionCallError> {
    let Some(payload) = executed_payload else {
        return Ok(original.clone());
    };
    crate::tool_history::classify_workspace_tool_call_at_admission(
        tool_identity.to_string(),
        payload.clone(),
        default_cwd.to_path_buf(),
    )
    .await
    .map(|(classification, _)| {
        native_file_classification(classification, tool_identity, payload, environments)
    })
    .map_err(|error| FunctionCallError::Fatal(format!("executed payload analysis failed: {error}")))
}

fn canonical_workspace_resource_key(
    classification: Option<&crate::tool_history::WorkspaceCallClassification>,
    cache: &crate::git_workspace::WorkspaceRootCache,
) -> Option<std::path::PathBuf> {
    let classification = classification?;
    cache.resolve(&classification.workspace_cwd).ok().flatten()
}

async fn workspace_resource_key_for_admission(
    classification: &crate::tool_history::WorkspaceCallClassification,
    cache: &Arc<crate::git_workspace::WorkspaceRootCache>,
    supports_parallel: bool,
    workspace_capable: bool,
) -> Option<std::path::PathBuf> {
    if !supports_parallel || !workspace_capable {
        return None;
    }
    // Root validation touches .git markers, so cache hits also run off-runtime.
    let classification = classification.clone();
    let cache = Arc::clone(cache);
    tokio::task::spawn_blocking(move || {
        canonical_workspace_resource_key(Some(&classification), cache.as_ref())
    })
    .await
    .unwrap_or_else(|error| {
        // Failed discovery retains conservative global workspace serialization.
        warn!(%error, "workspace resource discovery worker failed");
        None
    })
}

#[derive(Debug)]
struct WorkspaceAdmissionPlan {
    native_additions: Option<crate::scoped_workspace_gate::NativeAdditions>,
    native_read_cohort: bool,
    bypass_outer_gate: bool,
    resource_key: Option<std::path::PathBuf>,
    shared_resource: bool,
    supports_parallel: bool,
    workspace_capable: bool,
}

fn workspace_admission_plan(
    delegates_workspace_admission: bool,
    permits_shared_observation: bool,
    classification: &crate::tool_history::WorkspaceCallClassification,
    resource_key: Option<std::path::PathBuf>,
    supports_parallel: bool,
    workspace_capable: bool,
) -> WorkspaceAdmissionPlan {
    WorkspaceAdmissionPlan {
        native_additions: None,
        native_read_cohort: false,
        bypass_outer_gate: delegates_workspace_admission,
        resource_key,
        shared_resource: classification.observes_workspace && permits_shared_observation,
        supports_parallel,
        workspace_capable,
    }
}

fn workspace_resource_gate(
    workspace_execution: &Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            Weak<crate::scoped_workspace_gate::ScopedWorkspaceGate>,
        >,
    >,
    resource_key: std::path::PathBuf,
) -> Arc<crate::scoped_workspace_gate::ScopedWorkspaceGate> {
    let mut gates = workspace_execution
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(&resource_key).and_then(Weak::upgrade) {
        return gate;
    }
    gates.retain(|_, gate| gate.strong_count() > 0);
    let gate = Arc::new(crate::scoped_workspace_gate::ScopedWorkspaceGate::default());
    gates.insert(resource_key, Arc::downgrade(&gate));
    gate
}

/// Pre-admission workspace state captured for one tool call. Fields stay
/// module-private: callers outside this module only pass the opaque slot.
#[derive(Clone, Default)]
pub(crate) struct WorkspaceEvidenceBaseline {
    revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    cache_hit: bool,
    timed_out_git_dependencies: Vec<crate::git_workspace::WorkspaceEvidenceGitDependency>,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
}

fn is_passive_workspace_poll(router: &ToolRouter, call: &ToolCall) -> bool {
    router.semantic_tool_name(&call.tool_name) == codex_tools::ToolName::plain("write_stdin")
        && router.permits_shared_workspace_observation(call)
}

pub(crate) type WorkspaceEvidenceBaselineSlot =
    Arc<Mutex<Option<(WorkspaceEvidenceBaseline, u64)>>>;



#[derive(Default)]
struct ToolCallWorkspaceEvidenceContext {

    projection_source_dependencies:
        Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>>,
    workspace_admission_hint: Option<(crate::tool_history::WorkspaceCallClassification, bool)>,
    baseline_slot: Option<WorkspaceEvidenceBaselineSlot>,
    admitted_revision: Option<Arc<Mutex<Option<u64>>>>,
    dispatch_state: Option<Arc<ToolDispatchState>>,
}

struct WorkspaceEvidenceAfterCall<'a> {
    response: &'a ResponseInputItem,
    baseline: Option<WorkspaceEvidenceBaseline>,
    mutation_advanced: bool,
    source_dependencies_override:
        Option<std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>>,
    classification: &'a crate::tool_history::WorkspaceCallClassification,
    workspace_gate_guard: Option<WorkspaceGateGuard>,
}

struct WorkspaceEvidenceObservation<'a> {
    response: &'a ResponseInputItem,
    revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    captured_current: bool,
    source_scope_complete: bool,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
    workspace_gate_guard: Option<WorkspaceGateGuard>,
}

enum WorkspaceResourceGateGuard {
    Shared {
        _guard: crate::scoped_workspace_gate::WorkspaceLease,
    },
    Exclusive {
        _guard: crate::scoped_workspace_gate::WorkspaceLease,
    },
    Scoped {
        _guard: crate::scoped_workspace_gate::WorkspaceLease,
    },
}

enum WorkspaceGateGuard {
    ReadCohort {
        _guard: Arc<WorkspaceReadCohort>,
    },
    HookExclusive {
        _guard: Arc<OwnedRwLockWriteGuard<()>>,
    },
    Attributed {
        guard: Box<WorkspaceGateGuard>,
        holder: WorkspaceGlobalHolder,
    },
    Shared {
        _guard: OwnedRwLockReadGuard<()>,
    },
    Exclusive {
        _guard: OwnedRwLockWriteGuard<()>,
    },
    ResourceScoped {
        _resource: WorkspaceResourceGateGuard,
        _global: OwnedRwLockReadGuard<()>,
    },
}

impl WorkspaceGateGuard {
    fn attributed(self, holders: &WorkspaceGlobalHolders, shared: bool) -> Self {
        let Ok(holder) = WORKSPACE_GATE_CALL.try_with(|call| Arc::new(call.admitted(shared)))
        else {
            return self;
        };
        holders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holders
            .push(Arc::clone(&holder));
        Self::Attributed {
            guard: Box::new(self),
            holder: WorkspaceGlobalHolder {
                holders: Arc::clone(holders),
                holder,
            },
        }
    }

    fn downgrade_for_observation(self) -> Self {
        match self {
            Self::Attributed { guard, holder } => Self::Attributed {
                guard: Box::new(guard.downgrade_for_observation()),
                holder,
            },
            Self::ResourceScoped {
                _resource: WorkspaceResourceGateGuard::Exclusive { _guard },
                _global,
            } => Self::ResourceScoped {
                _resource: WorkspaceResourceGateGuard::Shared {
                    _guard: _guard.downgrade(),
                },
                _global,
            },
            // Resource-scoped writers hold a shared global guard. Downgrading
            // an unknown-resource exclusive guard would admit those writers,
            // so retain that barrier until the workspace identity is captured.
            other => other,
        }
    }
}

type WorkspaceGlobalHolders = Arc<Mutex<WorkspaceGlobalState>>;

#[derive(Debug, Default)]
struct WorkspaceGlobalState {
    holders: Vec<Arc<WorkspaceGateHolder>>,
    read_cohort: Weak<WorkspaceReadCohort>,
    cohort_closed: bool,
    cohort_arrivals: usize,
}

struct WorkspaceReadCohort {
    admission: futures::future::Shared<
        futures::future::BoxFuture<'static, Arc<OwnedRwLockWriteGuard<()>>>,
    >,
}

fn close_read_cohort(holders: &WorkspaceGlobalHolders) {
    let mut state = holders
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.cohort_closed = true;
}

async fn acquire_read_cohort(
    gate: Arc<RwLock<()>>,
    timing: &Arc<TurnTimingState>,
    holders: &WorkspaceGlobalHolders,
) -> Arc<WorkspaceReadCohort> {
    let cohort = {
        let mut state = holders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A finite cohort cannot be extended indefinitely by a read stream.
        if !state.cohort_closed
            && state.cohort_arrivals < 16
            && let Some(cohort) = state.read_cohort.upgrade()
        {
            state.cohort_arrivals += 1;
            cohort
        } else {
            // Publish the shared acquisition BEFORE waiting so readers already
            // queued behind a writer (or a full cohort) share the next lease.
            // Retain an exclusive barrier: known-root writers use shared global
            // admission and must not overlap these unknown-root reads.
            let cohort = Arc::new(WorkspaceReadCohort {
                admission: async move { Arc::new(gate.write_owned().await) }
                    .boxed()
                    .shared(),
            });
            // Poll once under the admission mutex to establish its FIFO queue
            // position before another cohort/writer can close or replace it.
            // This never waits or spawns a task. All cancelled members dropping
            // the shared future also cancel its queued acquisition/held guard.
            let _ = cohort.admission.clone().now_or_never();
            state.read_cohort = Arc::downgrade(&cohort);
            state.cohort_closed = false;
            state.cohort_arrivals = 1;
            cohort
        }
    };
    if cohort.admission.peek().is_none() {
        let _waiter = LifecycleCounterGuard::increment(timing, LifecycleCounter::ParallelGateWaiter);
        crate::tools::tool_dispatch_trace::observe_resource_wait(
            "resource:workspace_global", cohort.admission.clone(),
        ).await;
    }
    cohort
}

struct WorkspaceGlobalHolder {
    holders: WorkspaceGlobalHolders,
    holder: Arc<WorkspaceGateHolder>,
}

impl Drop for WorkspaceGlobalHolder {
    fn drop(&mut self) {
        self.holders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holders
            .retain(|holder| !Arc::ptr_eq(holder, &self.holder));
    }
}

#[derive(Debug)]
struct WorkspaceGateTimeout(String);

// Admission is not execution. A long observation budget must not hide minutes
// of waiting before a handler runs, and admission must leave the admitted call
// time to run, so a nested call waits at most half of its remaining budget,
// within these bounds. A sibling's exclusive hold usually ends with its yield
// window; a fixed one-second grace refused calls a moment before admission.
// This only bounds nested gate acquisition; the admitted operation and
// explicit process ownership are unchanged.
const NESTED_WORKSPACE_GATE_MIN_GRACE: Duration = Duration::from_secs(1);
const NESTED_WORKSPACE_GATE_MAX_GRACE: Duration = Duration::from_secs(30);

fn nested_workspace_gate_grace(started: TokioInstant, return_bound: TokioInstant) -> Duration {
    (return_bound.saturating_duration_since(started) / 2).clamp(
        NESTED_WORKSPACE_GATE_MIN_GRACE,
        NESTED_WORKSPACE_GATE_MAX_GRACE,
    )
}

async fn acquire_shared_workspace_gate(
    gate: Arc<RwLock<()>>,
    turn_timing_state: &Arc<TurnTimingState>,
) -> OwnedRwLockReadGuard<()> {
    match Arc::clone(&gate).try_read_owned() {
        Ok(guard) => guard,
        Err(_) => {
            let waiter_guard = LifecycleCounterGuard::increment(
                turn_timing_state,
                LifecycleCounter::ParallelGateWaiter,
            );
            let guard = crate::tools::tool_dispatch_trace::observe_resource_wait(
                "resource:workspace_global", gate.read_owned(),
            ).await;
            drop(waiter_guard);
            guard
        }
    }
}

async fn acquire_exclusive_workspace_gate(
    gate: Arc<RwLock<()>>,
    turn_timing_state: &Arc<TurnTimingState>,
) -> OwnedRwLockWriteGuard<()> {
    match Arc::clone(&gate).try_write_owned() {
        Ok(guard) => guard,
        Err(_) => {
            let waiter_guard = LifecycleCounterGuard::increment(
                turn_timing_state,
                LifecycleCounter::ParallelGateWaiter,
            );
            let guard = crate::tools::tool_dispatch_trace::observe_resource_wait(
                "resource:workspace_global", gate.write_owned(),
            ).await;
            drop(waiter_guard);
            guard
        }
    }
}


async fn acquire_workspace_gate_for_call(
    parallel_execution: Arc<RwLock<()>>,
    workspace_execution: Arc<
        Mutex<
            std::collections::HashMap<
                std::path::PathBuf,
                Weak<crate::scoped_workspace_gate::ScopedWorkspaceGate>,
            >,
        >,
    >,
    mut admission: WorkspaceAdmissionPlan,
    turn_timing_state: &Arc<TurnTimingState>,
    global_holders: &WorkspaceGlobalHolders,
    call: &ToolCall,
    source: &ToolCallSource,
) -> Result<WorkspaceGateGuard, WorkspaceGateTimeout> {
    let started = TokioInstant::now();
    let additions = admission.native_additions.take();
    let mut waiting_resource = None;
    let owner = WorkspaceGateCall {
        tool_name: call.tool_name.to_string(),
        call_id: call.call_id.clone(),
    };
    let acquire = WORKSPACE_GATE_CALL.scope(
        owner,
        acquire_workspace_gate_with_additions(
            parallel_execution,
            workspace_execution,
            admission,
            turn_timing_state,
            additions,
            global_holders,
            &mut waiting_resource,
        ),
    );
    let reason = if let Some(return_bound) =
        crate::unified_exec::nested_poll_bound(source.nested_deadline())
    {
        let grace = nested_workspace_gate_grace(started, return_bound);
        let admission_bound = started + grace;
        let bound = return_bound.min(admission_bound);
        // Drop the entire acquisition on expiry, including any partial resource
        // lease and queued native-addition claim, before taking the snapshot.
        if let Ok(guard) = tokio::time::timeout_at(bound, acquire).await {
            return Ok(guard);
        }
        if admission_bound < return_bound {
            format!(
                "the {} ms workspace admission grace was exhausted (execution was not started)",
                grace.as_millis()
            )
        } else {
            "the nested deadline's return margin was reached".to_string()
        }
    } else if matches!(source, ToolCallSource::Direct) {
        // Direct calls retain their existing cancellation-only policy.
        return Ok(acquire.await);
    } else {
        // Without a wrapper budget, a nested call may acquire immediately but
        // must not start an unbounded wait. Do not manufacture a new deadline.
        if let Some(guard) = acquire.now_or_never() {
            return Ok(guard);
        }
        "no nested deadline was supplied; refusing an unbounded wait".to_string()
    };
    const HOLDER_LIMIT: usize = 8;
    let (count, holders) = if let Some(resource) = waiting_resource {
        resource.holders(HOLDER_LIMIT)
    } else {
        let holders = global_holders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            holders.holders.len(),
            holders
                .holders
                .iter()
                .take(HOLDER_LIMIT)
                .map(|holder| (**holder).clone())
                .collect(),
        )
    };
    let mut message = format!(
        "Tool `{}` (call {}) did not run: workspace gate wait ended after {} ms because {reason}. Gate holders: {count}.",
        call.tool_name,
        call.call_id,
        started.elapsed().as_millis(),
    );
    for holder in &holders {
        let mode = if holder.shared { "shared" } else { "exclusive" };
        message.push_str(&format!(
            "\n- `{}` (call {}): {mode}, held for {} ms",
            holder.call.tool_name,
            holder.call.call_id,
            holder.admitted_at.elapsed().as_millis()
        ));
    }
    if count > holders.len() {
        message.push_str(&format!(
            "\n- {} additional holders omitted",
            count - holders.len()
        ));
    } else if count == 0 {
        message.push_str(" No holder remained at the expiry snapshot (admission may also queue behind an earlier waiter).");
    }
    Err(WorkspaceGateTimeout(message))
}

async fn acquire_workspace_gate_with_additions(
    parallel_execution: Arc<RwLock<()>>,
    workspace_execution: Arc<
        Mutex<
            std::collections::HashMap<
                std::path::PathBuf,
                Weak<crate::scoped_workspace_gate::ScopedWorkspaceGate>,
            >,
        >,
    >,
    admission: WorkspaceAdmissionPlan,
    turn_timing_state: &Arc<TurnTimingState>,
    additions: Option<crate::scoped_workspace_gate::NativeAdditions>,
    global_holders: &WorkspaceGlobalHolders,
    waiting_resource: &mut Option<Arc<crate::scoped_workspace_gate::ScopedWorkspaceGate>>,
) -> WorkspaceGateGuard {
    let WorkspaceAdmissionPlan {
        resource_key,
        native_read_cohort,
        shared_resource,
        supports_parallel,
        workspace_capable,
        ..
    } = admission;
    if native_read_cohort
        && resource_key.is_none()
        && shared_resource
        && supports_parallel
        && workspace_capable
    {
        return WorkspaceGateGuard::ReadCohort {
            _guard: acquire_read_cohort(parallel_execution, turn_timing_state, global_holders)
                .await,
        }
        .attributed(global_holders, false);
    }
    // Close before queueing, so later readers cannot bypass a pending writer.
    close_read_cohort(global_holders);
    if workspace_tool_may_use_parallel_gate(supports_parallel, workspace_capable) {
        return WorkspaceGateGuard::Shared {
            _guard: acquire_shared_workspace_gate(parallel_execution, turn_timing_state).await,
        }
        .attributed(global_holders, true);
    }
    let Some(resource_key) = resource_key.filter(|_| supports_parallel && workspace_capable) else {
        return WorkspaceGateGuard::Exclusive {
            _guard: acquire_exclusive_workspace_gate(parallel_execution, turn_timing_state).await,
        }
        .attributed(global_holders, false);
    };

    let resource_gate = workspace_resource_gate(workspace_execution.as_ref(), resource_key);
    *waiting_resource = Some(Arc::clone(&resource_gate));
    let resource = if let Some(additions) = additions {
        let _waiter = LifecycleCounterGuard::increment(
            turn_timing_state,
            LifecycleCounter::ParallelGateWaiter,
        );
        WorkspaceResourceGateGuard::Scoped {
            _guard: crate::tools::tool_dispatch_trace::observe_resource_wait(
                "resource:workspace_scoped", additions.acquire(resource_gate),
            ).await,
        }
    } else if shared_resource {
        WorkspaceResourceGateGuard::Shared {
            _guard: match Arc::clone(&resource_gate).try_read_owned() {
                Ok(guard) => guard,
                Err(()) => {
                    let _waiter = LifecycleCounterGuard::increment(
                        turn_timing_state,
                        LifecycleCounter::ParallelGateWaiter,
                    );
                    crate::tools::tool_dispatch_trace::observe_resource_wait(
                        "resource:workspace_scoped", resource_gate.read_owned(),
                    ).await
                }
            },
        }
    } else {
        WorkspaceResourceGateGuard::Exclusive {
            _guard: match Arc::clone(&resource_gate).try_write_owned() {
                Ok(guard) => guard,
                Err(()) => {
                    let _waiter = LifecycleCounterGuard::increment(
                        turn_timing_state,
                        LifecycleCounter::ParallelGateWaiter,
                    );
                    crate::tools::tool_dispatch_trace::observe_resource_wait(
                        "resource:workspace_scoped", resource_gate.write_owned(),
                    ).await
                }
            },
        }
    };
    // Acquire the resource first. If an unknown-resource writer is already
    // queued on the global barrier, Tokio's writer preference lets it run
    // before another same-resource waiter extends the read phase.
    *waiting_resource = None;
    // Resource acquisition may itself have waited while a new cohort started.
    close_read_cohort(global_holders);
    let global = acquire_shared_workspace_gate(parallel_execution, turn_timing_state).await;
    WorkspaceGateGuard::ResourceScoped {
        _resource: resource,
        _global: global,
    }
    .attributed(global_holders, true)
}

async fn capture_workspace_evidence_baseline(
    _cache: &crate::git_workspace::GitWorkspaceCache,
    _turn: Option<&TurnContext>,
    _cwd: &std::path::Path,
    _source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    _reuse_latest: bool,
) -> WorkspaceEvidenceBaseline {
    WorkspaceEvidenceBaseline::default()
}

fn finish_workspace_evidence_capture(
    baseline: &WorkspaceEvidenceBaseline,
    mutation_advanced: bool,
) -> (
    Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    bool,
) {
    let revision = baseline.revision.clone();
    // The mutation tracker covers the command's own workspace effects. When
    // it stays unchanged, the authoritative pre-dispatch identity remains the
    // post-dispatch identity even for an opaque command. Sampling refreshes
    // the identity again before projection, so external races still fail
    // closed. Gate concurrency does not control whether an otherwise unchanged
    // result is allowed to reach the next model request.
    // A non-Git capture can succeed without an identity. This flag tracks
    // instrumented mutations only; projection must still verify freshness.
    let captured_current = !mutation_advanced
        && revision
            .as_ref()
            .is_none_or(|identity| !identity.unavailable);
    (revision, captured_current)
}

fn workspace_evidence_source_dependencies(
    baseline: Option<&WorkspaceEvidenceBaseline>,
    source_dependencies_override: Option<
        &std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    >,
    classification: &crate::tool_history::WorkspaceCallClassification,
) -> std::collections::BTreeSet<crate::tool_history::SourceDependencyV1> {
    source_dependencies_override.cloned().unwrap_or_else(|| {
        baseline.map_or_else(
            || classification.source_dependencies.clone(),
            |baseline| baseline.source_dependencies.clone(),
        )
    })
}

impl ToolCallRuntime {
    pub(crate) fn mcp_auth_scope_is_current(&self) -> bool {
        self.step_context.mcp.manager().managed_auth_scope_is_current()
    }

    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        let coordinator = &step_context.turn.workspace_execution_coordinator;
        Self {
            parallel_execution: Arc::clone(&coordinator.parallel_execution),
            global_gate_holders: Arc::clone(&coordinator.global_gate_holders),
            workspace_execution: Arc::clone(&coordinator.workspace_execution),
            canonical_workspace_resources: Arc::clone(&session.services.git_workspace.roots),
            session,
            step_context,
            tracker,
            sampling_request_signals: None,
            terminal_completion_only: false,
            code_mode_user_intent: None,
        }
    }

    pub(crate) fn with_code_mode_user_intent(mut self, cell_id: &str, epoch: u64) -> Self {
        self.code_mode_user_intent = Some((cell_id.to_string(), epoch));
        self
    }

    pub(crate) fn with_terminal_completion_only(mut self, terminal_only: bool) -> Self {
        self.terminal_completion_only = terminal_only;
        self
    }

    /// Reuse must not bypass either phase of a configured tool hook.
    fn pre_tool_use_hook_may_run(&self, call: &ToolCall) -> bool {
        let hooks = self.session.hooks();
        match self.step_context.tool_router() {
            Some(router) => {
                router.pre_tool_use_hook_may_run(&hooks, call)
                    || router.post_tool_use_hook_may_run(&ToolInvocation {
                        session: Arc::clone(&self.session),
                        step_context: Arc::clone(&self.step_context),
                        cancellation_token: CancellationToken::new(),
                        tracker: Arc::clone(&self.tracker),
                        call_id: call.call_id.clone(),
                        tool_name: call.tool_name.clone(),
                        source: ToolCallSource::Direct,
                        payload: call.payload.clone(),
                    })
            }
            None => {
                hooks.has_handler_for(codex_protocol::protocol::HookEventName::PreToolUse)
                    || hooks.has_handler_for(codex_protocol::protocol::HookEventName::PostToolUse)
            }
        }
    }

    pub(crate) fn ensure_execution_allowed(&self) -> Result<(), FunctionCallError> {
        if self.terminal_completion_only {
            Err(FunctionCallError::Fatal(
                "tool execution is prohibited during a terminal-only generation".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn with_sampling_request_signals(
        mut self,
        collector: SamplingRequestSignalCollector,
    ) -> Self {
        self.sampling_request_signals = Some(collector);
        self
    }

    /// Classification identity for the turn signal collector, so a namespaced
    /// or wrapped registration is classified like the handler it exposes.
    fn semantic_tool_name(&self, tool_name: &codex_tools::ToolName) -> codex_tools::ToolName {
        self.step_context.tool_router().map_or_else(
            || tool_name.clone(),
            |router| router.semantic_tool_name(tool_name),
        )
    }

    /// These handlers already consume steering without aborting their owner.
    /// Resolve registrations first so configured namespaces keep that behavior.
    pub(crate) fn tool_yields_on_steering(&self, tool_name: &codex_tools::ToolName) -> bool {
        let semantic = self.semantic_tool_name(tool_name);
        crate::tools::code_mode::is_orchestration_tool_name(&semantic)
            || semantic == codex_tools::ToolName::plain("sleep")
            || semantic == codex_tools::ToolName::plain("wait_agent")
    }

    pub(crate) fn command_semantic_name(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<codex_tools::ToolName> {
        let router = self.step_context.tool_router()?;
        let semantic = router.semantic_tool_name(tool_name);
        (router.semantic_capabilities(tool_name).command.is_some()
            || semantic == codex_tools::ToolName::plain("write_stdin"))
        .then_some(semantic)
    }

    pub(crate) fn record_rejected_tool_call(
        &self,
        tool_name: &codex_tools::ToolName,
        payload: &ToolPayload,
        call_id: &str,
        message: &str,
    ) {
        if let Some(collector) = &self.sampling_request_signals {
            let registration = collector.register_deterministic_tool_call_with_reuse(
                &self.semantic_tool_name(tool_name),
                payload,
                call_id,
                crate::tools::registry::TerminalFailureReuse::Never,
            );
            // A parse rejection is diagnostic evidence, not task termination
            // and not permission to suppress future potentially effectful work.
            collector.record_failure(
                registration.ordinal,
                &format!("argument-parse:{message}"),
                false,
            );
        }
    }


    pub(crate) async fn flush_workspace_evidence_generation(
        &self,
    ) -> CodexResult<WorkspaceEvidenceGenerationFlush> {
        self.session
            .flush_workspace_evidence_generation(
                &self.step_context.turn,
                &self.step_context.workspace_evidence_generation_batch,
                &self.tracker,
            )
            .await
    }

    async fn register_passive_poll_evidence(
        session: &Session,
        turn: &TurnContext,
        response: &ResponseInputItem,
        baseline: Option<WorkspaceEvidenceBaseline>,
    ) -> bool {
        // Process bytes have no proven workspace observation interval. A live
        // writer can change files without a terminal mutation revision, and a
        // completed process can return bytes produced before admission. Never
        // refresh or batch-reassociate these bytes with a post-call identity.
        // History authenticates the output independently and appends an unknown
        // freshness notice; it does not suppress the process result or handle.
        Self::register_workspace_evidence_observation(
            session,
            turn,
            WorkspaceEvidenceObservation {
                response,
                revision: baseline.and_then(|baseline| baseline.revision),
                captured_current: false,
                source_scope_complete: false,
                source_dependencies: Default::default(),
                source_path_observations: Vec::new(),
                workspace_gate_guard: None,
            },
        )
        .await;
        false
    }

    async fn register_workspace_evidence_for_response(
        &self,
        response: &ResponseInputItem,
        baseline: Option<WorkspaceEvidenceBaseline>,
        mutation_advanced: bool,
        source_dependencies_override: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
        classification: &crate::tool_history::WorkspaceCallClassification,
        timing: Option<Arc<ToolDispatchTiming>>,
    ) -> bool {
        if !classification.observes_workspace {
            return false;
        }
        if mutation_advanced {
            let source_dependencies = workspace_evidence_source_dependencies(
                baseline.as_ref(),
                source_dependencies_override.as_ref(),
                classification,
            );
            let source_path_observations = baseline
                .as_ref()
                .filter(|baseline| baseline.source_dependencies == source_dependencies)
                .map(|baseline| baseline.source_path_observations.clone())
                .unwrap_or_default();
            if self
                .step_context
                .workspace_evidence_generation_batch
                .queue_response(
                    response,
                    classification,
                    source_dependencies,
                    source_path_observations,
                    timing,
                )
            {
                return true;
            }
        }
        // Only a fresh workspace capture needs synchronization with tool effects.
        let gate_guard = if baseline.is_none() || mutation_advanced {
            Some(
                WorkspaceGateGuard::Shared {
                    _guard: Arc::clone(&self.parallel_execution).read_owned().await,
                }
                .attributed(&self.global_gate_holders, true),
            )
        } else {
            None
        };
        Self::register_workspace_evidence_after_call(
            self.session.as_ref(),
            self.step_context.turn.as_ref(),
            WorkspaceEvidenceAfterCall {
                response,
                baseline,
                mutation_advanced,
                source_dependencies_override,
                classification,
                workspace_gate_guard: gate_guard,
            },
            None,
            None,
        )
        .await
    }

    async fn register_workspace_evidence_after_call(
        session: &Session,
        turn: &TurnContext,
        input: WorkspaceEvidenceAfterCall<'_>,
        generation_batch: Option<&Arc<WorkspaceEvidenceGenerationBatch>>,
        timing: Option<Arc<ToolDispatchTiming>>,
    ) -> bool {
        let WorkspaceEvidenceAfterCall {
            response,
            baseline,
            mutation_advanced,
            source_dependencies_override,
            classification,
            workspace_gate_guard,
        } = input;
        if !classification.observes_workspace {
            return false;
        }
        let source_dependencies = workspace_evidence_source_dependencies(
            baseline.as_ref(),
            source_dependencies_override.as_ref(),
            classification,
        );
        let source_path_observations = baseline
            .as_ref()
            .filter(|baseline| baseline.source_dependencies == source_dependencies)
            .map(|baseline| baseline.source_path_observations.clone())
            .unwrap_or_default();
        if mutation_advanced
            && generation_batch.is_some_and(|batch| {
                batch.queue_response(
                    response,
                    classification,
                    source_dependencies.clone(),
                    source_path_observations.clone(),
                    timing,
                )
            })
        {
            drop(workspace_gate_guard);
            return true;
        }
        let workspace_gate_guard =
            workspace_gate_guard.map(WorkspaceGateGuard::downgrade_for_observation);
        let (revision, captured_current) = match baseline.as_ref() {
            Some(baseline) if !mutation_advanced => {
                finish_workspace_evidence_capture(baseline, mutation_advanced)
            }
            _ => {
                // A mutation or refined payload can invalidate the pre-dispatch
                // baseline. Capture the
                // authoritative post-call identity instead of publishing an
                // observation that is stale from birth. `None` remains an
                // authoritative identity for a non-Git workspace.
                let revision = session
                    .services
                    .git_workspace
                    .workspace_evidence_for_turn(turn, &classification.workspace_cwd)
                    .await
                    .identity;
                (revision, true)
            }
        };
        // Identity capture is complete. History projection and persistence do not
        // need the workspace lease, even when they wait for session state.
        drop(workspace_gate_guard);
        Self::register_workspace_evidence_observation(
            session,
            turn,
            WorkspaceEvidenceObservation {
                response,
                revision,
                captured_current,
                source_scope_complete: classification.source_scope_complete,
                source_dependencies,
                source_path_observations,
                workspace_gate_guard: None,
            },
        )
        .await;
        false
    }

    async fn register_workspace_evidence_observation(
        session: &Session,
        turn: &TurnContext,
        input: WorkspaceEvidenceObservation<'_>,
    ) {
        drop(input.workspace_gate_guard);
        // Record only already-acquired, scoped source output. Do not revive
        // command freshness tracking or perform another source read.
        if input.source_dependencies.is_empty() {
            return;
        }
        let response = session.prepare_workspace_evidence_item(
            &ResponseItem::from(input.response.clone()),
            turn.model_info.truncation_policy.into(),
        ).await;
        if let Some(observation) = crate::tool_history::WorkspaceEvidenceObservation::from_response_item_with_freshness(
            input.revision, &response, input.source_dependencies, input.captured_current,
        ) {
            session.register_workspace_evidence(
                observation.with_source_scope_complete(input.source_scope_complete)
                    .with_source_path_observations(input.source_path_observations),
                (),
            ).await;
        }
    }


    pub(crate) fn record_code_mode_result(
        &self,
        call_id: &str,
        result: CodeModeToolResult<'_>,
        receipts: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        // Dispatch already attached scoped evidence using the executed payload.
        // In particular, None is not an unscoped workspace observation.
        let parent = self
            .session
            .services
            .code_mode_service
            .cell_parent_call_id(&codex_code_mode::CellId::new(result.cell_id.to_string()));
        collector.record_code_mode_parent(result.cell_id, parent.as_deref());
        let semantic_tool_name = self.semantic_tool_name(result.tool_name);
        if !result.canonical_artifact_required
            && let Some(adapter) = self
                .step_context
                .tool_router()
                .and_then(|router| router.terminal_result_adapter(result.tool_name))
        {
            collector.record_registered_owner_result(
                adapter,
                &semantic_tool_name,
                result.payload,
                result.signal,
                Some(result.result),
                true,
            );
        }
        if let Some(router) = self.step_context.tool_router() {
            collector.register_runtime_semantics(
                &semantic_tool_name,
                router.semantic_capabilities(result.tool_name),
            );
        }
        let signal = self
            .step_context
            .environments
            .turn_environments
            .first()
            .filter(|environment| {
                !environment.environment.is_remote()
                    || result.signal.is_some_and(|signal| {
                        signal["command_validation"]["execution_context"].is_object()
                    })
            })
            .and_then(|environment| {
                crate::session::turn_execution::validation_scope_signal(
                    &semantic_tool_name,
                    result.payload,
                    result.signal.cloned(),
                    result.source_dependencies.as_ref(),
                    self.step_context.turn.config.cwd.as_path(),
                    &environment.environment_id,
                )
            })
            .or_else(|| result.signal.cloned());
        collector.record_code_mode_result_for_call(
            Some(call_id),
            CodeModeToolResult {
                tool_name: &semantic_tool_name,
                signal: signal.as_ref(),
                ..result
            },
        );
        collector.record_accepted_deterministic_continuation_receipts(receipts);
    }

    pub(crate) async fn record_code_mode_failure(
        &self,
        cell_id: &str,
        tool_name: &codex_tools::ToolName,
        payload: Option<&ToolPayload>,
        failure_fingerprint: String,
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        let parent = self
            .session
            .services
            .code_mode_service
            .cell_parent_call_id(&codex_code_mode::CellId::new(cell_id.to_string()));
        collector.record_code_mode_parent(cell_id, parent.as_deref());
        // A rejected payload never reached dispatch and cannot add a workspace
        // observation. For dispatched failures, use the same command-aware
        // classification as successful results (including known writers).
        let source_dependencies = match payload {
            None => None,
            Some(payload) => match crate::tool_history::classify_workspace_tool_call_at_admission(
                tool_name.name.to_string(),
                payload.clone(),
                self.step_context.turn.config.cwd.to_path_buf(),
            )
            .await
            {
                Ok((classification, _)) => classification
                    .observes_workspace
                    .then_some(classification.source_dependencies),
                Err(_) => Some(Default::default()),
            },
        };
        let semantic_tool_name = self.semantic_tool_name(tool_name);
        if let Some(router) = self.step_context.tool_router() {
            collector.register_runtime_semantics(
                &semantic_tool_name,
                router.semantic_capabilities(tool_name),
            );
        }
        collector.record_code_mode_failure(
            cell_id,
            &semantic_tool_name,
            payload,
            source_dependencies,
            failure_fingerprint,
        );
    }

    /// Whether the step that advertised this call registered `name`, so a
    /// nested call can point the model at a typed tool that really exists.
    pub(crate) fn has_registered_tool(&self, name: &codex_tools::ToolName) -> bool {
        self.step_context
            .tool_router()
            .is_some_and(|router| router.has_registered_tool(name))
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.step_context
            .tool_router()?
            .create_diff_consumer(tool_name)
    }


    pub(crate) fn take_eager_read_eligibility_with_proof(
        &self,
        call: &ToolCall,
        earlier_calls_eligible: &mut bool,
        read_only: bool,
    ) -> bool {
        let typed_read = self.step_context.tool_router().is_some_and(|router| {
            matches!(
                router.classify_tool_name(self.step_context.turn.as_ref(), &call.tool_name),
                TypedToolClass::ReadSearch
            )
        });
        // Hooks can change a read into a writer. Such commands must wait for
        // ordinary dispatch, where the rewritten input is classified again.
        let safe_read = typed_read
            || (read_only
                && call.tool_name.namespace.is_none()
                && call.tool_name.name.as_str() == crate::tools::EXEC_COMMAND_TOOL_NAME
                && !self.pre_tool_use_hook_may_run(call));
        let parallel_read = safe_read
            && self
                .step_context
                .tool_router()
                .is_some_and(|router| router.tool_supports_parallel(call));
        let eligible = *earlier_calls_eligible && safe_read;
        // A serial read is still safe to overlap with the provider response.
        // Close the prefix after admitting it so later calls cannot overtake
        // the serial handler.
        *earlier_calls_eligible = eligible && parallel_read;
        eligible
    }



    pub(crate) fn create_tool_dispatch_timing(
        &self,
        item_accepted_at: TokioInstant,
        eager: bool,
    ) -> Arc<ToolDispatchTiming> {
        Arc::new(ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&self.step_context.turn.turn_timing_state),
            item_accepted_at,
            eager,
        ))
    }



    #[expect(
        clippy::await_holding_invalid_type,
        reason = "hold the revision stable until its captured identity is published; baseline writers never acquire the tracker lock"
    )]
    pub(crate) fn handle_model_tool_call_with_admission(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
        timing: Arc<ToolDispatchTiming>,
        admission_hint: Option<(crate::tool_history::WorkspaceCallClassification, bool)>,
    ) -> impl std::future::Future<Output = Result<ToolCallCompletion, CodexErr>> {
        self.step_context
            .workspace_evidence_generation_batch
            .register_call(&call.call_id);
        let tool_call_timing_guard = ToolCallTimingGuard::capture_for_turn(
            Arc::clone(&timing),
            Arc::clone(&self.step_context.turn.turn_timing_state),
            &self.session.thread_id,
            &self.step_context.turn.sub_id,
            &call,
            &ToolCallSource::Direct,
        );
        let tool_class = self
            .step_context
            .tool_router()
            .map_or(TypedToolClass::Unknown, |router| {
                router.classify_tool_name(self.step_context.turn.as_ref(), &call.tool_name)
            });
        let authoritative_direct_wait = matches!(&tool_class, TypedToolClass::AgentCommunication);
        let terminal_result_adapter = self
            .step_context
            .tool_router()
            .and_then(|router| router.terminal_result_adapter(&call.tool_name));
        let semantic_tool_name = self.semantic_tool_name(&call.tool_name);
        let passive_poll = self
            .step_context
            .tool_router()
            .is_some_and(|router| is_passive_workspace_poll(router, &call));
        let failure_reuse = self.step_context.tool_router().map_or(
            crate::tools::registry::TerminalFailureReuse::Never,
            |router| router.terminal_failure_reuse(&call.tool_name),
        );
        let signal_registration = self.sampling_request_signals.as_ref().map(|collector| {
            if let Some(router) = self.step_context.tool_router() {
                collector.register_runtime_semantics(
                    &semantic_tool_name,
                    router.semantic_capabilities(&call.tool_name),
                );
            }
            collector.register_deterministic_tool_call_with_reuse(
                &semantic_tool_name,
                &call.payload,
                &call.call_id,
                failure_reuse,
            )
        });
        let signal_collector = self.sampling_request_signals.clone();
        let gate_call = WorkspaceGateCall {
            tool_name: call.tool_name.to_string(),
            call_id: call.call_id.clone(),
        };
        WORKSPACE_GATE_CALL.scope(gate_call, async move {
            self.ensure_execution_allowed().map_err(|error| CodexErr::Fatal(error.to_string()))?;
            timing.mark_first_poll();
            let _tool_call_timing_guard = tool_call_timing_guard;
            let (workspace_call_classification, read_only) = match admission_hint {
                Some(classification) => classification,
                None => crate::tool_history::classify_workspace_tool_call_at_admission(
                    call.tool_name.name.to_string(),
                    call.payload.clone(),
                    self.step_context.turn.config.cwd.to_path_buf(),
                ).await.map_err(|error| CodexErr::Fatal(format!(
                    "workspace admission analysis failed: {error}"
                )))?,
            };
            let workspace_call_classification = native_file_classification(
                workspace_call_classification, call.tool_name.name.as_str(), &call.payload,
                &self.step_context.environments,
            );
            let pre_tool_use_hook_may_run = self.pre_tool_use_hook_may_run(&call);
            if !pre_tool_use_hook_may_run
                && let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.suppressed_failure.as_ref()
            {
                let completion =
                    reused_failure_completion(&call, &guard.failure_fingerprint);
                timing.record_outcome("skipped");
                timing.mark_output_collected();
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector.record_suppressed_failure(
                        registration.ordinal,
                        &guard.failure_fingerprint,
                    );
                }
                return Ok(completion);
            }

            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.blocked_wait_guard.as_ref()
                && let Some(snapshot) = crate::tools::handlers::multi_agents_v2::wait::inspect_authoritative_wait_snapshot(
                        self.session.as_ref(),
                        self.step_context.turn.as_ref(),
                        &guard.assignment_ids,
                    )
                    .await
            {
                if snapshot.owner == guard.owner
                    && snapshot.state_revision == guard.state_revision
                {
                    let response = suppressed_function_response(
                        &call.call_id,
                        serde_json::json!({
                            "kind": "authoritative_wait_suppression",
                            "disposition": "blocked",
                            "owner": guard.owner,
                            "state_revision": guard.state_revision,
                            "reason": "the exact authoritative wait remains blocked at the same owner revision; act on the blocker or report it",
                        })
                        .to_string(),
                    );
                    timing.record_outcome("skipped");
                    timing.mark_output_collected();
                    if let Some(signal_collector) = signal_collector.as_ref() {
                        signal_collector
                            .record_suppressed_result(registration.ordinal, &response);
                    }
                    return Ok(ToolCallCompletion {
                        failure_detail: None,
                        pending_continuation_ack: None,
                        required_terminal: Some(RequiredToolTerminal {
                            call_id: call.call_id.clone(),
                            cause: RequiredToolTerminalCause::Blocked,
                            message: format!("required tool `{}` remained blocked", call.tool_name),
                        }),
                        response,
                    });
                }
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector
                        .clear_blocked_wait_guard(&guard.owner, &guard.state_revision);
                }
            }
            let signal_ordinal = signal_registration
                .as_ref()
                .map(|registration| registration.ordinal);
            let observes_workspace = workspace_call_classification.observes_workspace;
            // The dispatch lane fills this slot only after it owns the
            // repository lease. The outer direct-call path then retains
            // evidence registration without observing a pre-admission state.
            let workspace_evidence_baseline_slot = (observes_workspace || pre_tool_use_hook_may_run).then(|| {
                Arc::new(Mutex::new(None))
            });
            let mutation_tracker = Arc::clone(&self.tracker);
            let error_call = call.clone();
            let owner_tool_name = call.tool_name.clone();
            let owner_payload = call.payload.clone();
            let evidence_timing = Arc::clone(&timing);
            let dispatch_state = Arc::new(ToolDispatchState::new());
            let future = self.clone().handle_tool_call_with_source_and_timing(
                call,
                ToolCallSource::Direct,
                cancellation_token,
                timing,
                ToolCallWorkspaceEvidenceContext {

                    projection_source_dependencies: Some(
                        workspace_call_classification.source_dependencies.clone(),
                    ),
                    workspace_admission_hint: Some((workspace_call_classification.clone(), read_only)),
                    baseline_slot: workspace_evidence_baseline_slot.clone(),
                    admitted_revision: None,
                    dispatch_state: Some(Arc::clone(&dispatch_state)),
                },
            );
            let result = future.await;
            let captured_workspace_baseline = workspace_evidence_baseline_slot.and_then(|slot| {
                let mut slot = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slot.take()
            });
            let (workspace_revision_before, mutation_revision_before) =
                captured_workspace_baseline.unzip();
            evidence_timing.record_outcome(tool_dispatch_outcome_label(&result));
            evidence_timing.mark_output_collected();
            if let Some(collector) = signal_collector.as_ref()
                && !crate::tools::code_mode::is_exec_tool_name(&owner_tool_name)
            {
                let timing_snapshot = evidence_timing.snapshot(TokioInstant::now());
                if let Some(runtime_ms) = timing_snapshot.first_poll_to_output_collected_ms
                    .or(timing_snapshot.total_duration_ms) {
                    collector.record_child_runtime_for_call(&error_call.call_id, runtime_ms);
                }
            }
            match result {
                Ok(mut response) => {
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    let outcome_context = response.outcome_context();
                    let mut original_signal = response.sampling_request_signal();
                    if let Some(revision) = mutation_revision_before {
                        original_signal.get_or_insert_with(|| serde_json::json!({}))
                            ["validation_mutation_revision"] = serde_json::json!(revision);
                    }
                    let signal = self.step_context.environments.turn_environments.first()
                        .filter(|environment| !environment.environment.is_remote()
                            || original_signal.as_ref().is_some_and(|signal| signal["command_validation"]["execution_context"].is_object()))
                        .and_then(|environment| crate::session::turn_execution::validation_scope_signal(
                            &semantic_tool_name, &response.payload, original_signal.clone(),
                            response.source_dependencies.as_ref(),
                            self.step_context.turn.config.cwd.as_path(),
                            &environment.environment_id,
                        )).or(original_signal);
                    let required_terminal =
                        required_tool_terminal(&error_call, outcome_context, signal.as_ref());
                    let owner_key = response.deterministic_continuation_owner_key();
                    let mut pending_receipts = Vec::new();
                    let carried_identities = response.deterministic_continuation_receipts()
                        .iter()
                        .filter_map(codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt::runtime_identity)
                        .collect::<std::collections::HashSet<_>>();
                    if let Some(owner_key) = owner_key.as_deref() {
                        let mut continuations = self
                            .session
                            .services
                            .code_mode_service
                            .owner_drained_continuation_snapshot(owner_key);
                        // A clean cell already consumed these pages into its
                        // computed answer. Retain custody until admission, but
                        // do not reattach their raw pages to that same carrier.
                        continuations.retain(|continuation| continuation.receipt.runtime_identity()
                            .is_none_or(|identity| !carried_identities.contains(&identity)));
                        let accepted = response.merge_owner_drained_continuations(continuations);
                        if let Some(collector) = &signal_collector {
                            collector
                                .record_accepted_deterministic_continuation_receipts(&accepted);
                        }
                        pending_receipts.extend(accepted);
                    }
                    let receipts = response.deterministic_continuation_receipts();
                    if let Some(collector) = &signal_collector {
                        collector.record_accepted_deterministic_continuation_receipts(&receipts);
                    }
                    pending_receipts.extend(receipts);
                    let pending_continuation_ack = owner_key.as_ref()
                        .filter(|_| !pending_receipts.is_empty())
                        .map(|owner_key| crate::tools::code_mode::PendingContinuationAcknowledgment {
                            owner_key: owner_key.clone(),
                            receipts: pending_receipts,
                        });
                    let canonical_artifact_required = response.requires_canonical_artifact();
                    let executed_workspace_call_classification =
                        executed_workspace_classification(
                            &workspace_call_classification,
                            owner_tool_name.name.as_str(),
                            Some(&response.payload).filter(|payload| *payload != &owner_payload),
                            self.step_context.turn.config.cwd.as_path(),
                            &self.step_context.environments,
                        ).await.map_err(|error| CodexErr::Fatal(error.to_string()))?;
                    let code_mode_exec = crate::tools::code_mode::is_exec_tool_name(&owner_tool_name);
                    let source_dependencies_override = owner_key.as_deref().and_then(|owner_key| {
                        signal_collector.as_ref().and_then(|collector| {
                            collector.code_mode_source_dependencies(owner_key)
                        })
                    })
                    .or_else(|| {
                        // A completed cell's nested observations are authoritative.
                        // Generic projection of the carrier cannot distinguish
                        // no repository reads from an unscoped repository read.
                        if code_mode_exec && owner_key.is_some() && signal_collector.is_some() {
                            None
                        } else {
                            response.projected_source_dependencies().cloned()
                        }
                    });
                    // Ordinary tool results install their canonical projection in the
                    // registry, where this phase is already recorded. Cancellation
                    // owns a synthesized terminal result and intentionally bypasses
                    // the remaining registry pipeline, so materializing its raw
                    // model-visible response is the projection boundary. The timing
                    // slot is write-once, preserving the registry measurement for
                    // ordinary results while closing the abort path's attribution.
                    let projection_started = Instant::now();

                    let failure_detail = response.result.model_delivery_unavailable().then(|| {
                        crate::tools::registry::model_delivery_unavailable_receipt(&error_call.call_id)
                    });
                    let response = response.into_response();
                    evidence_timing.record_output_projection(projection_started.elapsed());
                    // Code-mode carriers capture their baseline after the handler.
                    // Include that wait, not just the following registration, in
                    // the post-handler workspace-evidence phase.
                    let evidence_capture_started = Instant::now();
                    let (workspace_revision_before, evidence_classification) =
                        if code_mode_exec {
                            match source_dependencies_override.as_ref() {
                                Some(source_dependencies) => {
                                    let classification =
                                        crate::tool_history::WorkspaceCallClassification {
                                            observes_workspace: true,
                                            // A union of paths carries no completeness proof:
                                            // failed or unretained children can be absent from
                                            // the nested ledger. Exact children stay independently current.
                                            source_scope_complete: false,
                                            workspace_cwd: self
                                                .step_context
                                                .turn
                                                .config
                                                .cwd
                                                .clone()
                                                .to_path_buf(),
                                            source_dependencies: source_dependencies.clone(),
                                        };
                                    let capture_revision = mutation_tracker
                                        .lock().await.current_mutation_revision();
                                    let baseline = capture_workspace_evidence_baseline(
                                        self.session.services.git_workspace.as_ref(),
                                        Some(&self.step_context.turn),
                                        &classification.workspace_cwd,
                                        classification.source_dependencies.clone(),
                                        true,
                                    )
                                    .await;
                                    let tracker = mutation_tracker.lock().await;
                                    if tracker.current_mutation_revision() == capture_revision {
                                        self.step_context.workspace_evidence_generation_batch
                                            .record_captured_identity(
                                                &classification.workspace_cwd,
                                                capture_revision,
                                                baseline.revision.clone(),
                                            ).await;
                                    }
                                    drop(tracker);
                                    (Some(baseline), classification)
                                }
                                None => {
                                    self.session
                                        .register_non_workspace_code_mode_call(
                                            error_call.call_id.clone(),
                                        )
                                        .await;
                                    (None, workspace_call_classification.clone())
                                }
                            }
                        } else {
                            let workspace_revision_before = workspace_revision_before.filter(|_| {
                                workspace_evidence_baseline_is_compatible(
                                    &workspace_call_classification,
                                    &executed_workspace_call_classification,
                                )
                            });
                            (
                                workspace_revision_before,
                                executed_workspace_call_classification,
                            )
                        };

                    let workspace_evidence_deferred = dispatch_state.handler_started() && if passive_poll {
                        Self::register_passive_poll_evidence(
                            &self.session, &self.step_context.turn, &response, workspace_revision_before,
                        ).await
                    } else { self.register_workspace_evidence_for_response(
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        source_dependencies_override,
                        &evidence_classification,
                        Some(Arc::clone(&evidence_timing)),
                    )
                    .await };
                    if evidence_classification.observes_workspace && !workspace_evidence_deferred {
                        evidence_timing
                            .record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_direct_wait_owner_result(
                            authoritative_direct_wait,
                            &semantic_tool_name,
                            &owner_payload,
                            signal.as_ref(),
                            &response,
                        );
                        if let Some(adapter) = terminal_result_adapter.filter(|_| !canonical_artifact_required) {
                            collector.record_registered_owner_result(
                                adapter, &semantic_tool_name, &owner_payload, signal.as_ref(),
                                crate::session::turn_execution::canonical_authoritative_result(&response).as_ref(),
                                false,
                            );
                        }
                        collector.record_response_result(
                            ordinal,
                            outcome_context,
                            signal,
                            &response,
                            canonical_artifact_required,
                        );
                    }
                    Ok(ToolCallCompletion {
                        response,
                        failure_detail,
                        required_terminal,
                        pending_continuation_ack,
                    })
                }
                Err(FunctionCallError::Fatal(message)) => {
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_failure(ordinal, &format!("fatal:{message}"), true);
                    }
                    Err(CodexErr::Fatal(message))
                }
                Err(other) => {
                    let terminal_cause = required_tool_error_terminal_cause(&other);
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_failure(
                            ordinal,
                            &format!("model:{other}"),
                            terminal_cause.is_some(),
                        );
                    }
                    let failure_detail = Self::search_failure_detail(&error_call, &other);
                    let response = Self::failure_response(error_call.clone(), other);
                    let evidence_capture_started = Instant::now();
                    let workspace_evidence_deferred = dispatch_state.handler_started() && if passive_poll {
                        Self::register_passive_poll_evidence(
                            &self.session, &self.step_context.turn, &response, workspace_revision_before,
                        ).await
                    } else { self.register_workspace_evidence_for_response(
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        None,
                        &workspace_call_classification,
                        Some(Arc::clone(&evidence_timing)),
                    )
                    .await };
                    if observes_workspace && !workspace_evidence_deferred {
                        evidence_timing
                            .record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                    Ok(ToolCallCompletion {
                        response,
                        failure_detail,
                        pending_continuation_ack: None,
                        required_terminal: terminal_cause.map(|cause| RequiredToolTerminal {
                            call_id: error_call.call_id,
                            cause,
                            message: format!(
                                "required tool `{}` {}",
                                error_call.tool_name,
                                match cause {
                                    RequiredToolTerminalCause::Blocked => "blocked",
                                    RequiredToolTerminalCause::Failure => "failed",
                                                            }
                            ),
                        }),
                    })
                }
            }
        })
        .in_current_span()
    }


    pub(crate) fn handle_tool_call_with_admitted_revision(
        self,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
        admitted_revision: Option<Arc<Mutex<Option<u64>>>>,
        workspace_evidence_baseline_slot: Option<WorkspaceEvidenceBaselineSlot>,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        self.step_context
            .workspace_evidence_generation_batch
            .register_call(&call.call_id);
        let timing = Arc::new(ToolDispatchTiming::new_with_turn_clock(
            Arc::clone(&self.step_context.turn.turn_timing_state),
            TokioInstant::now(),
            /*eager*/ false,
        ));
        let nested_code_mode = matches!(&source, ToolCallSource::CodeMode { .. });
        let timing_call_id = call.call_id.clone();
        let signal_collector = self.sampling_request_signals.clone();
        let accepted = if let ToolCallSource::CodeMode { parent_call_id, .. } = &source {
            self.step_context.turn.tool_call_acceptance.try_accept(|| {
                self.step_context
                    .turn
                    .turn_timing_state
                    .try_record_accepted_tool_call(
                        &call.call_id,
                        timing.execution_id(),
                        TurnTimingToolCallSource::CodeMode,
                        parent_call_id.as_deref(),
                        call.tool_name.name.as_str(),
                    )
            })
        } else {
            self.step_context.turn.tool_call_acceptance.try_accept(|| {
                self.step_context
                    .turn
                    .turn_timing_state
                    .try_record_accepted_tool_call(
                        &call.call_id,
                        timing.execution_id(),
                        TurnTimingToolCallSource::Direct,
                        None,
                        call.tool_name.name.as_str(),
                    )
            })
        };
        async move {
            if !accepted {
                return Err(FunctionCallError::Fatal(format!(
                    "refusing nested tool call `{}` after terminal acceptance was sealed or tool-call capacity was exhausted",
                    call.call_id
                )));
            }
            let tool_call_timing_guard = ToolCallTimingGuard::capture_for_turn(
                Arc::clone(&timing),
                Arc::clone(&self.step_context.turn.turn_timing_state),
                &self.session.thread_id,
                &self.step_context.turn.sub_id,
                &call,
                &source,
            );
            timing.mark_first_poll();
            let _tool_call_timing_guard = tool_call_timing_guard;
            let result = self
                .handle_tool_call_with_source_and_timing(
                    call,
                    source,
                    cancellation_token,
                    Arc::clone(&timing),
                    ToolCallWorkspaceEvidenceContext {
                        admitted_revision,
                        baseline_slot: workspace_evidence_baseline_slot,
                        ..Default::default()
                    },
                )
                .await;
            timing.record_outcome(tool_dispatch_outcome_label(&result));
            // Own the returned-result boundary here. Inner dispatch marks the
            // ordinary handler path, but cancellation and future early-return
            // paths must record collection before this lifecycle guard drops.
            timing.mark_output_collected();
            if nested_code_mode && let Some(collector) = signal_collector.as_ref() {
                let timing_snapshot = timing.snapshot(TokioInstant::now());
                if let Some(runtime_ms) = timing_snapshot
                    .first_poll_to_output_collected_ms
                    .or(timing_snapshot.total_duration_ms)
                {
                    collector.record_child_runtime_for_call(&timing_call_id, runtime_ms);
                }
            }
            result
        }
    }

    fn handle_tool_call_with_source_and_timing(
        self,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
        timing: Arc<ToolDispatchTiming>,
        workspace_evidence: ToolCallWorkspaceEvidenceContext,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        let ToolCallWorkspaceEvidenceContext {

            projection_source_dependencies,
            workspace_admission_hint,
            baseline_slot: workspace_evidence_baseline_slot,
            admitted_revision,
            dispatch_state,
        } = workspace_evidence;
        if let Err(error) = self.ensure_execution_allowed() {
            return Either::Left(std::future::ready(Err(error)));
        }
        let Some(router) = self.step_context.tool_router() else {
            return Either::Left(std::future::ready(Err(FunctionCallError::Fatal(
                "step tool router was not finalized before tool execution".to_string(),
            ))));
        };
        let supports_parallel = router.tool_supports_parallel(&call);
        let wait_for_runtime_cancellation = router.tool_waits_for_runtime_cancellation(&call);
        let cleanup_deadline = router.tool_cancellation_cleanup_deadline(&call);
        let owns_unified_exec_processes = router.tool_owns_unified_exec_processes(&call);
        let requires_commit_barrier = router.tool_cancellation_requires_commit_barrier(&call);
        let retains_cancelled_cell_output =
            crate::tools::code_mode::is_orchestration_tool_name(&call.tool_name);
        let router = Arc::clone(router);
        let session = Arc::clone(&self.session);
        let step_context = Arc::clone(&self.step_context);
        let turn = Arc::clone(&step_context.turn);
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        let global_gate_holders = Arc::clone(&self.global_gate_holders);
        let workspace_execution = Arc::clone(&self.workspace_execution);
        // The caller-facing token selects terminal ownership here. Handlers
        // receive a separate token so they cannot observe cancellation and
        // publish a competing terminal result before this boundary claims the
        // abort. Runtimes that need cooperative cleanup are notified only
        // after the abort owns the terminal outcome.
        let runtime_cancellation_token = CancellationToken::new();
        let invocation_cancellation_token = runtime_cancellation_token.clone();
        let started = TokioInstant::now();
        let abort_session = Arc::clone(&session);
        let abort_source = source.clone();
        // Direct calls own evidence registration in the outer response path,
        // where code-mode dependency overrides are also available. Nested
        // code-mode calls have no such outer layer and register here.
        let model_issued = matches!(&source, ToolCallSource::Direct);
        let code_mode_parent_call_id = match &source {
            ToolCallSource::CodeMode { parent_call_id, .. } => parent_call_id.clone(),
            _ => None,
        };
        let abort_turn = Arc::clone(&turn);
        let dispatch_state = dispatch_state.unwrap_or_else(|| Arc::new(ToolDispatchState::new()));
        let admission_dispatch_state = Arc::clone(&dispatch_state);
        let router_dispatch_state = Arc::clone(&dispatch_state);
        let post_dispatch_state = Arc::clone(&dispatch_state);
        let dispatch_call = call.clone();
        let dispatch_tool_name = call.tool_name.name.clone();
        let evidence_call = call.clone();
        let native_additions = router.native_addition_paths(&evidence_call);
        let workspace_capable = crate::tool_history::tool_observes_workspace(
            router
                .semantic_tool_name(&evidence_call.tool_name)
                .name
                .as_str(),
        ) || native_additions.is_some();
        let canonical_workspace_resources = Arc::clone(&self.canonical_workspace_resources);
        let code_mode_user_intent = self.code_mode_user_intent.clone();
        let tracks_workspace = workspace_capable
            || !supports_parallel
            || router.delegates_workspace_admission(&evidence_call);
        if tracks_workspace {
            step_context
                .workspace_evidence_generation_batch
                .retain_generation_owner(&session, &turn, &tracker);
        }
        let evidence_tracker = Arc::clone(&tracker);

        let dispatch_span = trace_span!(
            "dispatch_tool_call_with_terminal_outcome",
            otel.name = %call.tool_name,
            tool_name = %call.tool_name,
            call_id = call.call_id.as_str(),
            aborted = false,
        );
        let abort_dispatch_span = dispatch_span.clone();
        let cancellation_timing = Arc::clone(&timing);
        let abort_invocation = ToolInvocation {
            session: Arc::clone(&abort_session),
            step_context: Arc::clone(&step_context),
            cancellation_token: cancellation_token.clone(),
            tracker: Arc::clone(&tracker),
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            source: abort_source.clone(),
            payload: call.payload.clone(),
        };

        // Dropping the response waiter must enter the same supervised cancellation
        // path as explicit cancellation. A child token isolates it from sibling calls.
        let cancellation_token = cancellation_token.child_token();
        let caller_drop = cancellation_token.clone().drop_guard();
        let admission_cancellation_token = cancellation_token.clone();
        let mut commit_guard = (owns_unified_exec_processes
            || requires_commit_barrier
            || retains_cancelled_cell_output)
            .then(|| self.session.retain_tool_dispatch_commit());
        let terminal_tasks = self.session.terminal_tasks.clone();

        let mut dispatch_handle: AbortOnDropHandle<Result<AnyToolResult, FunctionCallError>> =
            AbortOnDropHandle::new(tokio::spawn(crate::tools::tool_dispatch_trace::scope_tool_blocking_timing(Arc::clone(&timing), Box::pin(async move {
                let admission_tool_name = router.semantic_tool_name(&dispatch_call.tool_name);
                let (admission, mut read_only) = match workspace_admission_hint {
                    Some(classification) => classification,
                    None => crate::tool_history::classify_workspace_tool_call_at_admission(
                        admission_tool_name.name.to_string(),
                        dispatch_call.payload.clone(),
                        turn.config.cwd.to_path_buf(),
                    )
                    .await
                    .map_err(|error| {
                        FunctionCallError::Fatal(format!(
                            "workspace admission analysis failed: {error}"
                        ))
                    })?,
                };
                let admission = native_file_classification(
                    admission,
                    admission_tool_name.name.as_str(),
                    &dispatch_call.payload,
                    &step_context.environments,
                );
                let (workspace_admission_classification, workspace_call_classification) =
                    workspace_tool_call_classifications_for_dispatch(
                        &source,
                        admission_tool_name.name.as_str(),
                        &dispatch_call.payload,
                        turn.config.cwd.as_path(),
                        Some(admission),
                    );
                let projection_source_dependencies = projection_source_dependencies.or_else(|| {
                    workspace_call_classification
                        .as_ref()
                        .filter(|classification| classification.observes_workspace)
                        .map(|classification| classification.source_dependencies.clone())
                });
                let (dispatch_call, hook_notice) = HOOK_USER_INTENT
                    .scope(
                        code_mode_user_intent.clone(),
                        scope_tool_dispatch_timing(
                            Arc::clone(&timing),
                            router.prepare_hook_input(ToolInvocation {
                                session: Arc::clone(&session),
                                step_context: Arc::clone(&step_context),
                                // Queued hooks belong to pre-admission caller lifetime.
                                // Once entered, run_workspace_hook retains their effects.
                                cancellation_token: admission_cancellation_token.clone(),
                                tracker: Arc::clone(&tracker),
                                call_id: dispatch_call.call_id,
                                tool_name: dispatch_call.tool_name,
                                source: source.clone(),
                                payload: dispatch_call.payload,
                            }),
                        ),
                    )
                    .await?;
                let hook_rewrote_input = hook_notice.is_some();
                let evidence_call = dispatch_call.clone();
                let admission_tool_name = router.semantic_tool_name(&dispatch_call.tool_name);
                let (workspace_admission_classification, workspace_call_classification) =
                    if hook_rewrote_input {
                        let (classification, rewritten_read_only) =
                            crate::tool_history::classify_workspace_tool_call_at_admission(
                                admission_tool_name.name.to_string(),
                                dispatch_call.payload.clone(),
                                turn.config.cwd.to_path_buf(),
                            )
                            .await
                            .map_err(|error| {
                                FunctionCallError::Fatal(format!(
                                    "rewritten workspace admission analysis failed: {error}"
                                ))
                            })?;
                        // A hook may turn a read into a writer. Never retain
                        // the original payload's admission proof after rewriting.
                        read_only = rewritten_read_only;
                        let classification = native_file_classification(
                            classification,
                            admission_tool_name.name.as_str(),
                            &dispatch_call.payload,
                            &step_context.environments,
                        );
                        workspace_tool_call_classifications_for_dispatch(
                            &source,
                            admission_tool_name.name.as_str(),
                            &dispatch_call.payload,
                            turn.config.cwd.as_path(),
                            Some(classification),
                        )
                    } else {
                        (
                            workspace_admission_classification,
                            workspace_call_classification,
                        )
                    };
                let projection_source_dependencies = if hook_rewrote_input {
                    None
                } else {
                    projection_source_dependencies
                };
                let native_additions = if hook_rewrote_input {
                    router.native_addition_paths(&evidence_call)
                } else {
                    native_additions
                };
                timing.record_boundary(ToolLifecycleBoundary::ResourceResolutionStart);
                let resource_key = workspace_resource_key_for_admission(
                    &workspace_admission_classification,
                    &canonical_workspace_resources,
                    supports_parallel,
                    workspace_capable,
                )
                .await;
                let additions = if let (Some(root), Some(paths), Some(environment)) = (
                    resource_key.as_ref(),
                    native_additions.as_ref(),
                    step_context.environments.primary(),
                ) {
                    if !environment.environment.is_remote()
                        && environment
                            .cwd()
                            .to_abs_path()
                            .ok()
                            .as_ref()
                            .is_some_and(|cwd| cwd.as_path() == turn.config.cwd.as_path())
                    {
                        crate::scoped_workspace_gate::NativeAdditions::resolve(
                            root,
                            turn.config.cwd.as_path(),
                            paths,
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                };
                let supports_parallel =
                    supports_parallel && (native_additions.is_none() || additions.is_some());
                timing.record_boundary(ToolLifecycleBoundary::ResourceResolutionEnd);
                let passive_poll = is_passive_workspace_poll(&router, &evidence_call);
                let post_hook_may_run = router.post_tool_use_hook_may_run(&ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: Arc::clone(&step_context),
                    cancellation_token: invocation_cancellation_token.clone(),
                    tracker: Arc::clone(&tracker),
                    call_id: dispatch_call.call_id.clone(),
                    tool_name: dispatch_call.tool_name.clone(),
                    source: source.clone(),
                    payload: dispatch_call.payload.clone(),
                });
                let hook_lease = if post_hook_may_run
                    && !passive_poll
                    && !router.delegates_workspace_admission(&evidence_call)
                {
                    close_read_cohort(&global_gate_holders);
                    Some(Arc::new(
                        acquire_exclusive_workspace_gate(
                                Arc::clone(&lock),
                                &turn.turn_timing_state,
                            )
                            .await,
                    ))
                } else {
                    None
                };
                let mut workspace_admission = workspace_admission_plan(
                    router.delegates_workspace_admission(&evidence_call)
                        || router.is_workspace_independent(&evidence_call),
                    router.permits_shared_workspace_observation(&evidence_call)
                        // Reuse the shell-aware proof already needed for eager
                        // dispatch. A public name alone cannot opt a custom
                        // handler into shared execution.
                        || (read_only && matches!(
                            router.semantic_capabilities(&evidence_call.tool_name).command,
                            Some(crate::tools::registry::CommandArgumentFormat::Exec)
                        )),
                    &workspace_admission_classification,
                    resource_key,
                    supports_parallel,
                    workspace_capable,
                );
                workspace_admission.native_additions = additions;
                workspace_admission.native_read_cohort = !post_hook_may_run
                    && matches!(
                        evidence_call.tool_name.name.as_str(),
                        "read_file" | "list_files"
                    )
                    && router.permits_shared_workspace_observation(&evidence_call)
                    && workspace_admission_classification.source_scope_complete
                    && !workspace_admission_classification
                        .source_dependencies
                        .is_empty();
                if tracks_workspace {
                    timing.record_boundary(ToolLifecycleBoundary::DiffTrackerWaitStart);
                    // Like the gates: an uncontended acquisition is not
                    // contention, even when Tokio's coop budget defers lock().
                    let mut guard = if let Ok(guard) = tracker.try_lock() {
                        guard
                    } else {
                        crate::tools::tool_dispatch_trace::observe_resource_wait(
                            "resource:diff_tracker", tracker.lock(),
                        ).await
                    };
                    guard.activate_workspace_evidence_generation_batch(
                        &step_context.workspace_evidence_generation_batch,
                    );
                    if read_only
                        && router
                            .semantic_capabilities(&evidence_call.tool_name)
                            .command
                            .is_some()
                    {
                        guard.record_read_only_command_call(&evidence_call.call_id);
                    }
                    drop(guard);
                    timing.record_boundary(ToolLifecycleBoundary::DiffTrackerWaitEnd);
                }
                timing.record_boundary(ToolLifecycleBoundary::WorkspaceGateWaitStart);
                let mut gate_guard = if let Some(lease) = &hook_lease {
                    Some(WorkspaceGateGuard::HookExclusive {
                        _guard: Arc::clone(lease),
                    })
                } else if workspace_admission.bypass_outer_gate {
                    None
                } else {
                    Some(
                        match acquire_workspace_gate_for_call(
                                Arc::clone(&lock),
                                Arc::clone(&workspace_execution),
                                workspace_admission,
                                &turn.turn_timing_state,
                                &global_gate_holders,
                                &dispatch_call,
                                &source,
                            )
                            .await
                        {
                            Ok(guard) => guard,
                            Err(result) => {
                                timing.record_boundary(ToolLifecycleBoundary::WorkspaceGateWaitEnd);
                                timing.mark_output_collected();
                                // Complete without entering the handler or invoking
                                // storage/hooks that could consume the relay margin.
                                if !admission_dispatch_state.try_admit() {
                                    return std::future::pending().await;
                                }
                                admission_dispatch_state.try_complete();
                                return Ok(AnyToolResult {
                                    call_id: dispatch_call.call_id,
                                    payload: dispatch_call.payload,
                                    // Return failure as data, never a rejected JS
                                    // promise that could cancel sibling calls.
                                    result: crate::tools::context::boxed_tool_output(
                                        codex_tools::JsonToolOutput::with_success(
                                            serde_json::json!({
                                                "kind": "workspace_gate_timeout",
                                                "did_run": false,
                                                "execution_state": "not_started",
                                                "process_exited": false,
                                                "exit_code": null,
                                                "output_complete": false,
                                                "output_reduced": false,
                                                "raw_output_artifact_retention_limit_hit": false,
                                                "output": result.0,
                                            }),
                                            Some(false),
                                        )
                                        .with_code_mode_failure_as_data(),
                                    ),
                                    model_projection: None,
                                    source_dependencies: None,
                                    code_mode_feedback: Vec::new(),
                                });
                            }
                        },
                    )
                };
                timing.record_boundary(ToolLifecycleBoundary::WorkspaceGateWaitEnd);
                if admission_cancellation_token.is_cancelled()
                    || !admit_code_mode_user_intent(
                        &session,
                        code_mode_user_intent.as_ref(),
                        || admission_dispatch_state.try_admit(),
                    )?
                {
                    // The cancellation owner won the admission race. Release
                    // any newly acquired guard without entering the handler;
                    // the outer task immediately aborts this pending future and
                    // emits the single terminal lifecycle result.
                    drop(gate_guard);
                    return std::future::pending().await;
                }
                // Gate admission is distinct from authorization and actual
                // handler entry; keep each boundary independently observable.
                timing.mark_parallel_gate_admitted();
                turn.turn_timing_state
                    .record_tool_gate_admitted(dispatch_tool_name.as_str());
                crate::session::turn::reconcile_turn_progress_event(
                    &turn.turn_timing_state,
                    1,
                    "tool request admitted",
                );
                let _model_tool_gate_timing_guard =
                    ModelToolGateTimingGuard::admitted(&turn.turn_timing_state, model_issued);
                let _active_tool_guard = LifecycleCounterGuard::increment(
                    &turn.turn_timing_state,
                    LifecycleCounter::ActiveTool,
                );

                let evidence_capture_classification = workspace_call_classification
                    .as_ref()
                    .or_else(|| {
                        workspace_evidence_baseline_slot
                            .as_ref()
                            .map(|_| &workspace_admission_classification)
                    })
                    .filter(|classification| classification.observes_workspace);
                let baseline_ready = CancellationToken::new();
                let overlap_baseline = router.prepares_during_workspace_baseline(&dispatch_call);
                let baseline_capture = async {
                    let prefetched_workspace_evidence =
                        if let Some(classification) = evidence_capture_classification {
                            // The repository lease must precede baseline work. A queued
                            // call must neither spend Git/disk resources nor carry an
                            // observation made before its execution boundary.
                            timing.record_boundary(ToolLifecycleBoundary::EvidenceTrackerWaitStart);
                            let admitted_mutation_revision =
                                if let Ok(guard) = evidence_tracker.try_lock() {
                                    guard.current_mutation_revision()
                                } else {
                                    crate::tools::tool_dispatch_trace::observe_resource_wait(
                                        "resource:evidence_tracker", evidence_tracker.lock(),
                                    ).await.current_mutation_revision()
                                };
                            if passive_poll
                                && let Some(parent_call_id) = code_mode_parent_call_id.as_deref()
                            {
                                step_context
                                    .workspace_evidence_generation_batch
                                    .record_code_mode_workspace_observation(
                                        parent_call_id,
                                        admitted_mutation_revision,
                                    );
                            }
                            if let Some(slot) = admitted_revision.as_ref() {
                                *slot
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some(admitted_mutation_revision);
                            }
                            timing.record_boundary(ToolLifecycleBoundary::EvidenceTrackerWaitEnd);
                            let evidence_capture_started = Instant::now();
                            let baseline = step_context
                                .workspace_evidence_generation_batch
                                .capture_baseline_for_tool(
                                    session.services.git_workspace.as_ref(),
                                    Some(&turn),
                                    classification,
                                    admission_tool_name.name.as_str(),
                                    admitted_mutation_revision,
                                )
                                .await;
                            let capture_duration = evidence_capture_started.elapsed();
                            timing.record_workspace_evidence_before(capture_duration);
                            timing.record_workspace_evidence_before_attribution(
                                baseline.cache_hit,
                                baseline
                                    .timed_out_git_dependencies
                                    .iter()
                                    .map(|dependency| dependency.as_str().to_string())
                                    .collect(),
                            );
                            if !baseline.revision.as_ref().is_some_and(|identity| {
                                !identity.unavailable
                                    && !baseline.source_path_observations.is_empty()
                                    && identity.repository_root.is_some()
                                    && identity.worktree_identity.is_none()
                            }) && turn
                                .environments
                                .primary()
                                .is_some_and(|environment| !environment.environment.is_remote())
                            {
                                session
                                    .services
                                    .command_execution
                                    .observe_workspace_baseline(
                                        &turn.sub_id,
                                        admitted_mutation_revision,
                                        &classification.workspace_cwd,
                                        baseline.revision.clone(),
                                    )
                                    .await;
                            }
                            if let Some(slot) = workspace_evidence_baseline_slot.as_ref() {
                                // Nested dispatch still owns history evidence. Exporting
                                // replay proof must not consume its pre-read baseline.
                                let nested_baseline = (!model_issued)
                                    .then(|| (baseline.clone(), admitted_mutation_revision));
                                *slot
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    Some((baseline, admitted_mutation_revision));
                                nested_baseline
                            } else {
                                Some((baseline, admitted_mutation_revision))
                            }
                        } else {
                            None
                        };
                    baseline_ready.cancel();
                    prefetched_workspace_evidence
                };

                let projection_source_dependencies = projection_source_dependencies
                    .or_else(|| {
                        workspace_call_classification
                            .as_ref()
                            .filter(|classification| classification.observes_workspace)
                            .map(|classification| classification.source_dependencies.clone())
                    })
                    .or_else(|| {
                        evidence_capture_classification
                            .map(|classification| classification.source_dependencies.clone())
                    });

                // Keep the handler future out of every task-local/join wrapper's
                // inline state; large handler variants otherwise exhaust a worker stack.
                let dispatch = Box::pin(router.dispatch_tool_call_with_terminal_outcome(
                    Arc::clone(&session),
                    Arc::clone(&step_context),
                    invocation_cancellation_token,
                    tracker,
                    dispatch_call,
                    source,
                    router_dispatch_state,
                ))
                .instrument(dispatch_span.clone());
                let dispatch =
                    crate::tools::registry::with_prepared_hook_input(hook_notice, dispatch);

                let dispatch = HOOK_WORKSPACE_LEASE.scope(hook_lease, dispatch);
                let dispatch = WORKSPACE_BASELINE_READY.scope(
                    baseline_ready.clone(),
                    scope_tool_dispatch_timing(
                        Arc::clone(&timing),
                        crate::tools::registry::with_precomputed_projection_source_dependencies(
                            projection_source_dependencies,
                            dispatch,
                        ),
                    ),
                );
                // Both futures stay owned by the supervised dispatch. The
                // repository lease still precedes capture, and opted-in handlers
                // wait at their read/effect boundary, not before pure preparation.
                let (prefetched_workspace_evidence, mut result) =
                    if overlap_baseline && !passive_poll {
                        tokio::join!(baseline_capture, dispatch)
                    } else {
                        let baseline = baseline_capture.await;
                        if passive_poll {
                            // Retain admission ordering, not a claim that later process
                            // output observes this epoch. Its evidence stays unverified.
                            step_context
                                .workspace_evidence_generation_batch
                                .record_workspace_observation(&evidence_call.call_id);
                            drop(gate_guard.take());
                        }
                        (baseline, dispatch.await)
                    };
                let (evidence_revision_before, evidence_mutation_revision_before) =
                    prefetched_workspace_evidence.unzip();
                timing.mark_output_collected();
                // Ordinary calls retain effect ordering under the lease. Passive
                // polls recorded admission above and never claim current evidence.
                if tracks_workspace && !passive_poll {
                    step_context
                        .workspace_evidence_generation_batch
                        .record_workspace_observation(&evidence_call.call_id);
                }
                if post_dispatch_state.is_aborted() {
                    return result;
                }
                let successful = result.as_ref().is_ok_and(|result| {
                    result.outcome_for_logging() == codex_tools::ToolOutputOutcome::Success
                });
                turn.turn_timing_state
                    .record_tool_completion(dispatch_tool_name.as_str(), successful);
                let evidence_classification = match workspace_call_classification.as_ref() {
                    Some(classification) => Some(
                        executed_workspace_classification(
                            classification,
                            admission_tool_name.name.as_str(),
                            result
                                .as_ref()
                                .ok()
                                .map(|result| &result.payload)
                                .filter(|payload| *payload != &evidence_call.payload),
                            turn.config.cwd.as_path(),
                            &step_context.environments,
                        )
                        .await?,
                    ),
                    None => None,
                };
                if let (Some(classification), Ok(result)) =
                    (evidence_classification.as_ref(), result.as_mut())
                {
                    result.source_dependencies = classification.observes_workspace.then(|| {
                        result
                            .projected_source_dependencies()
                            .cloned()
                            .unwrap_or_else(|| classification.source_dependencies.clone())
                    });
                }
                let source_dependencies_override = result
                    .as_ref()
                    .ok()
                    .and_then(|result| result.projected_source_dependencies().cloned());
                let evidence_revision_before = evidence_revision_before.filter(|_| {
                    workspace_call_classification
                        .as_ref()
                        .zip(evidence_classification.as_ref())
                        .is_some_and(|(original, executed)| {
                            workspace_evidence_baseline_is_compatible(original, executed)
                        })
                });
                if post_dispatch_state.handler_started()
                    && let Some(classification) = evidence_classification
                        .as_ref()
                        .filter(|classification| classification.observes_workspace)
                {
                    // Direct calls register in their outer response owner, and
                    // non-workspace calls have no evidence consumer at all.
                    let evidence_response = match result.as_ref() {
                        Ok(result) => Some(result.response()),
                        Err(FunctionCallError::Fatal(_)) => None,
                        Err(err) => Some(Self::failure_response_for_message(
                            &evidence_call,
                            err.to_string(),
                        )),
                    };
                    let Some(response) = evidence_response.as_ref() else {
                        drop(gate_guard);
                        return result;
                    };
                    let evidence_capture_started = Instant::now();
                    let mutation_revision =
                        evidence_tracker.lock().await.current_mutation_revision();
                    if !passive_poll
                        && let Some(parent_call_id) = code_mode_parent_call_id.as_deref()
                    {
                        // Record while the workspace lease is held. Registration
                        // can release it before persisting the observation.
                        step_context
                            .workspace_evidence_generation_batch
                            .record_code_mode_workspace_observation(
                                parent_call_id,
                                mutation_revision,
                            );
                    }
                    let mutation_advanced = evidence_mutation_revision_before
                        .is_some_and(|before| mutation_revision > before);
                    let workspace_evidence_deferred = if passive_poll {
                        drop(gate_guard);
                        Self::register_passive_poll_evidence(
                            &session,
                            &turn,
                            response,
                            evidence_revision_before,
                        )
                        .await
                    } else {
                        Self::register_workspace_evidence_after_call(
                            session.as_ref(),
                            turn.as_ref(),
                            WorkspaceEvidenceAfterCall {
                                response,
                                baseline: evidence_revision_before,
                                mutation_advanced,
                                source_dependencies_override,
                                classification,
                                workspace_gate_guard: gate_guard,
                            },
                            Some(&step_context.workspace_evidence_generation_batch),
                            Some(Arc::clone(&timing)),
                        )
                        .await
                    };
                    if !workspace_evidence_deferred {
                        timing.record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                } else {
                    drop(gate_guard);
                }
                result
            }))));

        let cancellation_owner = terminal_tasks.spawn(
            async move {
                tokio::select! {
                res = &mut dispatch_handle => Self::joined_tool_result(res, &dispatch_state).await,
                _ = cancellation_token.cancelled() => {
                    if dispatch_state.is_terminal() || dispatch_handle.is_finished() {
                        Self::joined_tool_result(dispatch_handle.await, &dispatch_state).await
                    } else {
                        let cancelled_before_admission = match dispatch_state.try_abort() {
                            ToolDispatchAbort::BeforeAdmission => true,
                            ToolDispatchAbort::AfterAdmission => false,
                            ToolDispatchAbort::AlreadyTerminal => {
                                return Self::joined_tool_result(dispatch_handle.await, &dispatch_state).await;
                            }
                        };
                        let mut cancellation_recovery: Option<serde_json::Value>;
                        let secs = started.elapsed().as_secs_f32();
                        abort_dispatch_span.record("aborted", true);
                        if cancelled_before_admission {
                            cancellation_recovery = Some(serde_json::json!({
                                "kind":"cancellation_recovery", "effects_state":"none",
                                "retry_safe":false,
                                "required_action":"This call did not reach admission; preserve effects of earlier calls and retry only if still authorized.",
                            }));
                            dispatch_handle.abort();
                            match dispatch_handle.await {
                                Ok(result) => return result,
                                Err(err) if err.is_cancelled() => {}
                                Err(err) => return Err(Self::tool_task_join_error(err)),
                            }
                            cancellation_timing.mark_handler_exit_if_entered();
                        } else {
                            if wait_for_runtime_cancellation {
                                runtime_cancellation_token.cancel();
                            }
                            let cleanup = supervise_cancelled_dispatch_cleanup(
                                dispatch_handle,
                                CancelledDispatchCleanupPolicy {
                                    wait_for_runtime_cancellation,
                                    owns_unified_exec_processes,
                                    requires_commit_barrier,
                                    cleanup_deadline,
                                },
                                abort_invocation.clone(),
                                Arc::clone(&cancellation_timing),
                            );
                            if owns_unified_exec_processes || requires_commit_barrier || retains_cancelled_cell_output {
                                // A retained exec process is part of the turn's terminal
                                // ownership barrier. Do not publish TurnAborted until the
                                // originating handler and its process registry entry close.
                                // State commits likewise publish their update before the response.
                                cancellation_recovery = cleanup.await;
                            } else {
                                // Other runtimes retain their owned dispatch future in a
                                // supervised task so user-visible cancellation does not wait
                                // for teardown beyond the terminal result they already own.
                                cancellation_recovery = Some(serde_json::json!({
                                    "kind": "cancellation_recovery",
                                    "cleanup_state": "pending",
                                    "effects": crate::tools::context::ToolEffectRecovery::unknown(
                                        None,
                                        Some("Runtime cleanup is still supervised in the background; cancellation does not establish that effects stopped."),
                                    ),
                                }));
                                abort_session.terminal_tasks.spawn(cleanup);
                            }
                        }
                        // Required state/process cleanup has joined. Lifecycle observers
                        // do not extend the turn's durable-commit barrier.
                        drop(commit_guard.take());
                        // Hook rewrites can happen before admission. The patch
                        // owner retains the amended code until its single-use
                        // receipt is consumed under the mutation gate.
                        let patch_retry = abort_session.services.retained_patches.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recovery_for_call(&call.call_id);
                        if let Some(retry) = patch_retry {
                            let receipt = serde_json::json!({"kind": "retained_patch_recovery", "retry": retry});
                            let recovery = cancellation_recovery.get_or_insert_with(|| serde_json::json!({}));
                            recovery["retained_patch_recovery"] = receipt;
                        }
                        let mut response = Self::aborted_response(&call, secs, &abort_invocation);
                        if let Some(recovery) = cancellation_recovery {
                            response.result = Box::new(AbortedToolOutput {
                                message: Self::abort_message_for(&abort_invocation, &call, secs),
                                recovery: Some(recovery),
                            });
                        }
                        scope_tool_dispatch_timing(
                            Arc::clone(&cancellation_timing),
                            install_synthetic_terminal_projection(
                                &abort_invocation,
                                &mut response,
                            ),
                        )
                        .await;
                        notify_tool_aborted(
                            abort_session.as_ref(),
                            abort_turn.as_ref(),
                            call.call_id.as_str(),
                            &call.tool_name,
                            abort_source,
                        )
                        .await;
                        Ok(response)
                    }
                },
            }
        }
            .in_current_span(),
        );
        Either::Right(async move {
            let result = cancellation_owner
                .await
                .map_err(Self::tool_task_join_error)?;
            caller_drop.disarm();
            result
        })
    }
}

struct CancelledDispatchCleanupPolicy {
    wait_for_runtime_cancellation: bool,
    owns_unified_exec_processes: bool,
    requires_commit_barrier: bool,
    cleanup_deadline: Duration,
}

// Handler settlement and external-effect certainty are independent facts.
// Keep values/recovery references native even when cancellation owns the call.
#[derive(serde::Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
enum CancelledHandlerSettlement {
    SettledSuccess { call_id: String, result: serde_json::Value },
    SettledFailure { call_id: String, result: Option<serde_json::Value>, error: Option<String> },
    Unresolved { call_id: String },
}

async fn supervise_cancelled_dispatch_cleanup(
    mut dispatch_handle: AbortOnDropHandle<Result<AnyToolResult, FunctionCallError>>,
    policy: CancelledDispatchCleanupPolicy,
    invocation: ToolInvocation,
    timing: Arc<ToolDispatchTiming>,
) -> Option<serde_json::Value> {
    let CancelledDispatchCleanupPolicy {
        wait_for_runtime_cancellation,
        owns_unified_exec_processes,
        requires_commit_barrier,
        cleanup_deadline,
    } = policy;
    let ToolInvocation {
        session,
        call_id,
        tool_name,
        step_context,
        ..
    } = invocation;
    let mut recovery = Vec::new();
    let mut live_operations = Vec::new();
    let mut settlement = CancelledHandlerSettlement::Unresolved { call_id: call_id.clone() };
    let mut effects = crate::tools::context::ToolEffectRecovery::unknown(None, None);
    let recovery_for = |result: Option<&serde_json::Value>, error: Option<&str>| {
        step_context.tool_router().map_or_else(
            || crate::tools::context::ToolEffectRecovery::unknown(result, error),
            |router| router.cancellation_recovery(&tool_name, result, error),
        )
    };
    let cleanup_until = tokio::time::Instant::now() + cleanup_deadline;
    if wait_for_runtime_cancellation {
        match tokio::time::timeout_at(cleanup_until, &mut dispatch_handle).await {
            Ok(Ok(Err(error))) => {
                settlement = CancelledHandlerSettlement::SettledFailure {
                    call_id: call_id.clone(), result: None, error: Some(error.to_string()),
                };
                effects = recovery_for(None, Some(&error.to_string()));
                recovery.push(
                    serde_json::json!({"kind":"handler_recovery","detail":error.to_string()}),
                );
            }
            Ok(Ok(Ok(result))) => {
                let value = result.cancellation_result();
                settlement = if result.success_for_logging() {
                    CancelledHandlerSettlement::SettledSuccess { call_id: result.call_id, result: value.clone() }
                } else {
                    CancelledHandlerSettlement::SettledFailure { call_id: result.call_id, result: Some(value.clone()), error: None }
                };
                effects = recovery_for(Some(&value), None);
                recovery.push(serde_json::json!({
                    "kind": "handler_result",
                    "result": value,
                }));
            }
            Ok(Err(err)) if err.is_cancelled() => {}
            Ok(Err(err)) => {
                recovery.push(
                    serde_json::json!({"kind":"cleanup_join_failed","detail":err.to_string()}),
                );
                warn!(
                    tool_name = %tool_name,
                    call_id,
                    error = ?err,
                    "cancelled tool cleanup task failed"
                );
            }
            Err(_) => {
                recovery.push(serde_json::json!({"kind":"cleanup_deadline_exceeded","deadline_ms":cleanup_deadline.as_millis()}));
                warn!(
                    tool_name = %tool_name,
                    call_id,
                    deadline_ms = cleanup_deadline.as_millis(),
                    "tool runtime cleanup exceeded its supervised deadline; aborting dispatch task"
                );
                dispatch_handle.abort();
                if let Err(err) = dispatch_handle.await
                    && !err.is_cancelled()
                {
                    warn!(
                        tool_name = %tool_name,
                        call_id,
                        error = ?err,
                        "aborted tool cleanup task failed to join"
                    );
                }
            }
        }
    } else {
        dispatch_handle.abort();
        if let Err(err) = dispatch_handle.await
            && !err.is_cancelled()
        {
            recovery
                .push(serde_json::json!({"kind":"cleanup_join_failed","detail":err.to_string()}));
            warn!(
                tool_name = %tool_name,
                call_id,
                error = ?err,
                "aborted tool dispatch failed to join during cleanup"
            );
        }
    }

    if owns_unified_exec_processes {
        let cleanup = session
            .services
            .unified_exec_manager
            .terminate_unpublished_processes_for_call_ids(std::slice::from_ref(&call_id));
        let result = tokio::time::timeout_at(cleanup_until, cleanup).await
            .unwrap_or_else(|_| Err("process termination exceeded the runtime cleanup deadline; processes may survive".into()));
        match result {
            Ok(terminated_processes) => {
                if terminated_processes > 0 {
                    timing.mark_exec_process_exited();
                }
                timing.record_exec_cleanup_state(
                    /*background_process_expected*/ false,
                    /*running_process_after_cleanup*/ false,
                );
            }
            Err(error) => {
                timing.record_exec_cleanup_state(false, true);
                effects.uncertain_effects.push(error.clone());
                let surviving = session
                    .services
                    .unified_exec_manager
                    .list_processes()
                    .await
                    .into_iter()
                    .filter(|process| process.item_id == call_id)
                    .map(|process| process.process_id)
                    .collect::<Vec<_>>();
                for process_id in &surviving {
                    let Ok(process_id) = process_id.parse::<u32>() else { continue };
                    if let Some(incarnation) = session.services.unified_exec_manager
                        .process_incarnation(process_id).await
                    {
                        live_operations.push(serde_json::json!({"tool":"write_stdin", "arguments":{
                            "session_id":process_id,"incarnation":incarnation,"chars":""
                        }}));
                    }
                }
                recovery.push(serde_json::json!({
                    "kind": "process_cleanup_failed",
                    "detail": error,
                    "processes_may_be_running": true,
                    "potentially_surviving_process_ids": surviving,
                    "call_id": call_id,
                }));
                warn!(
                    tool_name = %tool_name,
                    call_id,
                    %error,
                    "failed to terminate unpublished retained process for cancelled tool"
                );
            }
        }
    }
    // The ordinary handler marker is dropped with an aborted dispatch. Close
    // it only after the supervised process cleanup has reached a terminal
    // state so ProcessExit remains ordered before HandlerReturn.
    timing.mark_handler_exit_if_entered();
    // The handler has joined and any required process cleanup has been awaited.
    // Commit-barrier handlers finish their state commit before answering, so a
    // joined result or error states the committed effects. Processes and
    // external calls can still have unreported effects.
    let effects_reported = effects.uncertain_effects.is_empty()
        && requires_commit_barrier
        && !owns_unified_exec_processes
        && recovery.iter().all(|entry| {
            matches!(
                entry["kind"].as_str(),
                Some("handler_result" | "handler_recovery")
            )
        });
    Some(serde_json::json!({
        "kind": "cancellation_recovery",
        "call_id": call_id,
        "effects_state": if effects_reported { "reported" } else { "unknown" },
        "retry_safe": false,
        "effects": effects,
        "live_operations":live_operations,
        "handler_settlement": settlement,
        "required_action": if effects_reported {
            "The handler finished; its details state the committed effects. Retry only work those details show was not committed."
        } else {
            "Inspect committed effects and retained process state before retrying."
        },
        "details": recovery,
    }))
}

fn suppressed_function_response(call_id: &str, message: String) -> ResponseInputItem {
    let mut output = FunctionCallOutputPayload::from_text(message);
    output.success = Some(false);
    ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output,
    }
}

impl ToolCallRuntime {
    async fn joined_tool_result(
        result: Result<Result<AnyToolResult, FunctionCallError>, JoinError>,
        dispatch_state: &ToolDispatchState,
    ) -> Result<AnyToolResult, FunctionCallError> {
        match result {
            Ok(result) => result,
            Err(error) => {
                let error = Self::tool_task_join_error(error);
                // A joined panic cannot execute the registry's terminal branch;
                // it must still obey the same completion/cancellation election.
                dispatch_state.try_complete();
                Err(error)
            }
        }
    }

    fn tool_task_join_error(err: JoinError) -> FunctionCallError {
        FunctionCallError::Fatal(format!("tool task failed to receive: {err:?}"))
    }

    pub(crate) fn search_failure_detail(
        call: &ToolCall,
        error: &FunctionCallError,
    ) -> Option<ResponseItem> {
        if !matches!(call.payload, ToolPayload::ToolSearch { .. }) {
            return None;
        }
        Some(Self::search_failure_detail_for_call_id(
            &call.call_id,
            error,
        ))
    }

    pub(crate) fn search_failure_detail_for_call_id(
        call_id: &str,
        error: &FunctionCallError,
    ) -> ResponseItem {
        let (kind, message) = match error {
            FunctionCallError::Structured(failure) => ("failed", failure.message.as_str()),
            FunctionCallError::RespondToModel(message) => ("failed", message.as_str()),
            FunctionCallError::DeniedToModel(message)
            | FunctionCallError::RequiredOperationBlocked(message) => ("denied", message.as_str()),
            FunctionCallError::Fatal(_) => (
                "internal",
                "Tool search could not complete because of an internal failure.",
            ),
        };
        let end = message.floor_char_boundary(1_024.min(message.len()));
        let mut failure = error.failure();
        // Preserve discovery's bounded diagnostic and internal-error redaction
        // in the structured channel as well as in the legacy message field.
        failure.message = message[..end].to_string();
        // Reuse the untrusted contextual envelope so discovery diagnostics do
        // not become a new user turn in history projection or compaction.
        crate::context::ContextualUserFragment::into(
            crate::context::AdditionalContextUserFragment::new(
                "tool_search_failure".to_string(),
                serde_json::json!({
                "tool_search_failure": {"call_id":call_id,"kind":kind,"message":&message[..end],"truncated":end < message.len(),"tool_failure":failure}
                }).to_string(),
            ),
        )
    }

    fn failure_response(call: ToolCall, err: FunctionCallError) -> ResponseInputItem {
        Self::failure_response_for_message(&call, err.model_message())
    }

    pub(crate) fn failure_response_for_message(
        call: &ToolCall,
        message: String,
    ) -> ResponseInputItem {
        match &call.payload {
            ToolPayload::ToolSearch { .. } => {
                warn!(call_id = call.call_id, %message, "tool search failed");
                ResponseInputItem::ToolSearchOutput {
                    call_id: call.call_id.clone(),
                    status: "incomplete".to_string(),
                    execution: "client".to_string(),
                    tools: Vec::new(),
                    omitted_result_count: None,
                }
            }
            ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
                call_id: call.call_id.clone(),
                name: None,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
            _ => ResponseInputItem::FunctionCallOutput {
                call_id: call.call_id.clone(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
        }
    }

    fn aborted_response(call: &ToolCall, secs: f32, invocation: &ToolInvocation) -> AnyToolResult {
        AnyToolResult {
            call_id: call.call_id.clone(),
            payload: call.payload.clone(),
            result: Box::new(AbortedToolOutput {
                message: Self::abort_message_for(invocation, call, secs),
                recovery: None,
            }),
            model_projection: None,
            source_dependencies: None,
            code_mode_feedback: Vec::new(),
        }
    }

    /// Renders a cancelled call from whatever origin recorded itself.
    ///
    /// The call's own cause wins over the turn's: a nested runtime deadline is
    /// more specific than the turn tearing down around it. Only a cancellation
    /// no origin recorded falls back to the historical user wording.
    fn abort_message_for(invocation: &ToolInvocation, call: &ToolCall, secs: f32) -> String {
        let cause = invocation.source.cancellation_cause().or_else(|| {
            invocation
                .step_context
                .turn
                .cancellation_cause
                .get()
                .cloned()
        });
        match cause {
            Some(cause) => Self::abort_message_with_reason(call, secs, &cause.describe()),
            None => Self::abort_message(call, secs),
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        Self::abort_message_with_reason(call, secs, "aborted by user")
    }

    fn abort_message_with_reason(call: &ToolCall, secs: f32, reason: &str) -> String {
        if crate::tools::is_shell_family_tool_name(&call.tool_name) {
            format!("Wall time: {secs:.1} seconds\n{reason}")
        } else {
            format!("{reason} after {secs:.1}s")
        }
    }
}

impl ToolCallTimingGuard {
    fn capture_for_turn(
        timing: Arc<ToolDispatchTiming>,
        turn_timing_state: Arc<TurnTimingState>,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        source: &ToolCallSource,
    ) -> Self {
        Self::new(
            timing,
            Some(turn_timing_state),
            conversation_id,
            turn_id,
            call,
            source,
            tracing::enabled!(tracing::Level::INFO),
        )
    }

    fn new(
        timing: Arc<ToolDispatchTiming>,
        turn_timing_state: Option<Arc<TurnTimingState>>,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        source: &ToolCallSource,
        emit_log: bool,
    ) -> Self {
        let (tool_source, parent_cell_id, parent_model_call_id, runtime_tool_call_id) = match source
        {
            ToolCallSource::Direct => ("direct", String::new(), None, String::new()),
            ToolCallSource::CodeMode {
                cell_id,
                parent_call_id,
                runtime_tool_call_id,
                ..
            } => (
                "code_mode",
                cell_id.clone(),
                parent_call_id.clone(),
                runtime_tool_call_id.clone(),
            ),
        };

        Self {
            timing,
            turn_timing_state,
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            tool_source,
            parent_cell_id,
            parent_model_call_id,
            runtime_tool_call_id,
            emit_log,
        }
    }
}

impl Drop for ToolCallTimingGuard {
    fn drop(&mut self) {
        let completed_at = TokioInstant::now();
        // Snapshot once so concurrent boundary updates cannot make one event
        // internally inconsistent. Keep the legacy dispatch field as a
        // compatibility alias for parallel-gate wait.
        let snapshot = self.timing.snapshot(completed_at);
        if let Some(turn_timing_state) = self.turn_timing_state.as_ref() {
            turn_timing_state.record_tool_dispatch_timing(
                &self.call_id,
                &self.tool_name.to_string(),
                match self.tool_source {
                    "direct" => TurnTimingToolCallSource::Direct,
                    _ => TurnTimingToolCallSource::CodeMode,
                },
                ToolCallTimingLineage {
                    parent_call_id: self.parent_model_call_id.as_deref(),
                    parent_cell_id: (!self.parent_cell_id.is_empty())
                        .then_some(self.parent_cell_id.as_str()),
                    runtime_tool_call_id: (!self.runtime_tool_call_id.is_empty())
                        .then_some(self.runtime_tool_call_id.as_str()),
                },
                snapshot.clone(),
            );
            crate::session::turn::reconcile_turn_progress_event(
                turn_timing_state,
                1,
                "tool lifecycle completion",
            );
        }
        if !self.emit_log {
            return;
        }
        info!(
            event.name = "codex.tool_call",
            trace_id = %codex_otel::current_span_trace_id().unwrap_or_default(),
            conversation.id = %self.conversation_id,
            turn_id = %self.turn_id,
            tool_name = %self.tool_name,
            call_id = %self.call_id,
            tool_source = self.tool_source,
            parent_cell_id = %self.parent_cell_id,
            parent_model_call_id = self.parent_model_call_id.as_deref().unwrap_or_default(),
            runtime_tool_call_id = %self.runtime_tool_call_id,
            eager = snapshot.eager,
            outcome = snapshot.outcome.unwrap_or("unknown"),
            timing_log_version = 2,
            parallel_gate_admitted = snapshot.parallel_gate_admitted,
            execution_started = snapshot.handler_duration_ms.is_some(),
            item_to_first_poll_ms = snapshot.item_to_first_poll_ms,
            parallel_gate_wait_ms = snapshot.parallel_gate_wait_ms,
            authorization_state_coordination_ms = snapshot
                .authorization_state_coordination_ms
                ,
            first_poll_to_handler_entry_ms = snapshot
                .first_poll_to_handler_entry_ms
                ,
            dispatch_duration_ms = snapshot.parallel_gate_wait_ms,
            handler_duration_ms = snapshot.handler_duration_ms,
            workspace_evidence_before_ms = snapshot.workspace_evidence_before_ms,
            workspace_evidence_before_cache_hit = snapshot
                .workspace_evidence_before_cache_hit
                ,
            workspace_evidence_before_timed_out_git_dependencies =
                ?snapshot.workspace_evidence_before_timed_out_git_dependencies,
            workspace_evidence_after_ms = snapshot.workspace_evidence_after_ms,
            pre_tool_hook_ms = snapshot.pre_tool_hook_ms,
            post_tool_hook_ms = snapshot.post_tool_hook_ms,
            output_projection_ms = snapshot.output_projection_ms,
            history_persistence_ms = snapshot.history_persistence_ms,
            first_poll_to_output_collected_ms = snapshot
                .first_poll_to_output_collected_ms
                ,
            exec_request_to_spawn_ms = snapshot.exec_request_to_spawn_ms,
            exec_spawn_to_exit_ms = snapshot.exec_spawn_to_exit_ms,
            exec_exit_to_delivery_ms = snapshot.exec_exit_to_delivery_ms,
            exec_spawn_to_delivery_ms = snapshot.exec_spawn_to_delivery_ms,
            exec_process_alive_at_delivery = snapshot.exec_process_alive_at_delivery,
            exec_cleanup_state_observed = snapshot.exec_cleanup_state_observed,
            exec_background_process_expected = snapshot.exec_background_process_expected,
            exec_running_process_after_cleanup = snapshot.exec_running_process_after_cleanup,
            exec_running_process_stale = snapshot.exec_running_process_after_cleanup
                && !snapshot.exec_background_process_expected,
            post_handler_ms = snapshot.post_handler_ms,
            total_duration_ms = snapshot.total_duration_ms,
            "tool call completed"
        );
    }
}

