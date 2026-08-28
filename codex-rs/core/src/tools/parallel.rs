use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

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
use tracing::instrument;
use tracing::trace_span;
use tracing::warn;

use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
use crate::session::reasoning_governor::CodeModeToolResult;
use crate::session::reasoning_governor::SamplingRequestSignalCollector;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
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
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use crate::tools::tool_dispatch_trace::scope_tool_dispatch_timing;
use crate::turn_timing::ToolCallTimingLineage;
use crate::turn_timing::TurnTimingState;
use codex_protocol::error::CodexErr;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;

pub(crate) const TOOL_RUNTIME_CANCELLATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct ToolCallCompletion {
    pub(crate) response: ResponseInputItem,
    pub(crate) required_terminal: Option<RequiredToolTerminal>,
}

impl ToolCallCompletion {
    pub(crate) fn nonterminal(response: ResponseInputItem) -> Self {
        Self {
            response,
            required_terminal: None,
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
        RequiredToolTerminalCause::TimedOut => "timed out",
        RequiredToolTerminalCause::RecoverableCancellation => "was cancelled",
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
        Some("timeout") => Some(RequiredToolTerminalCause::TimedOut),
        Some("recoverable_cancellation") => {
            Some(RequiredToolTerminalCause::RecoverableCancellation)
        }
        Some("failure") => None,
        _ => match outcome_context.outcome {
            ToolOutputOutcome::Failure => None,
            ToolOutputOutcome::TimedOut => Some(RequiredToolTerminalCause::TimedOut),
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
        FunctionCallError::DeniedToModel(_) => Some(RequiredToolTerminalCause::Blocked),
        FunctionCallError::RespondToModel(_) => None,
        FunctionCallError::Fatal(_) => Some(RequiredToolTerminalCause::Failure),
    }
}

fn reused_failure_diagnosis(
    _tool_name: &codex_tools::ToolName,
    failure_fingerprint: &str,
) -> String {
    serde_json::json!({
        "kind": "reused_failure_diagnosis",
        "failure_fingerprint": failure_fingerprint,
        "retryable": false,
        "required_action": "change_route_or_state",
        "reason": "this exact action already produced the same stable failure against unchanged state; the prior diagnosis remains authoritative",
        "next_action": "Do not repeat this call with unchanged arguments; change the action or relevant state before the next call.",
    })
    .to_string()
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
        Err(_) => "failure",
    }
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    session: Arc<Session>,
    // Tool calls may run later, so retain the step whose tool list advertised them.
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
    workspace_execution:
        Arc<Mutex<std::collections::HashMap<std::path::PathBuf, Weak<RwLock<()>>>>>,
    canonical_workspace_resources:
        Arc<Mutex<std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>>>>,
    sampling_request_signals: Option<SamplingRequestSignalCollector>,
}

struct PendingWorkspaceEvidenceResponse {
    ordinal: u64,
    response: ResponseInputItem,
    workspace_cwd: std::path::PathBuf,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
    timing: Option<Arc<ToolDispatchTiming>>,
}

struct PendingWorkspaceMutation {
    ordinal: u64,
    workspace_cwd: std::path::PathBuf,
    affected_paths: Option<std::collections::BTreeSet<std::path::PathBuf>>,
    observe_command_ledger: bool,
}

#[derive(Default)]
struct WorkspaceEvidenceGenerationBatchState {
    sealed: bool,
    next_ordinal: u64,
    next_effect_ordinal: u64,
    call_ordinals: std::collections::HashMap<String, u64>,
    responses: Vec<PendingWorkspaceEvidenceResponse>,
    mutations: Vec<PendingWorkspaceMutation>,
}

/// Request-scoped workspace evidence collected until all accepted tool calls
/// in one model sampling generation have settled.
pub(crate) struct WorkspaceEvidenceGenerationBatch {
    state: Mutex<WorkspaceEvidenceGenerationBatchState>,
}

pub(crate) struct WorkspaceEvidenceGenerationFlush {
    pub(crate) prefetched_workspace_identity:
        Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>>,
    #[cfg(test)]
    pub(crate) authoritative_capture_count: usize,
    #[cfg(test)]
    pub(crate) registered_call_ids: Vec<String>,
}

impl WorkspaceEvidenceGenerationBatch {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(WorkspaceEvidenceGenerationBatchState::default()),
        }
    }

    pub(crate) fn register_call(&self, call_id: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.sealed {
            return false;
        }
        if state.call_ordinals.contains_key(call_id) {
            return true;
        }
        let ordinal = state.next_ordinal;
        state.next_ordinal = state.next_ordinal.saturating_add(1);
        state.call_ordinals.insert(call_id.to_string(), ordinal);
        true
    }

    pub(crate) fn accepts_call(&self, call_id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.sealed && state.call_ordinals.contains_key(call_id)
    }

    pub(crate) fn record_mutation(
        &self,
        call_id: &str,
        workspace_cwd: std::path::PathBuf,
        affected_paths: Option<std::collections::BTreeSet<std::path::PathBuf>>,
        observe_command_ledger: bool,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.sealed || !state.call_ordinals.contains_key(call_id) {
            return false;
        }
        let ordinal = state.next_effect_ordinal;
        state.next_effect_ordinal = state.next_effect_ordinal.saturating_add(1);
        state.mutations.push(PendingWorkspaceMutation {
            ordinal,
            workspace_cwd,
            affected_paths,
            observe_command_ledger,
        });
        true
    }

    fn queue_response(
        &self,
        response: &ResponseInputItem,
        classification: &crate::tool_history::WorkspaceCallClassification,
        source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
        timing: Option<Arc<ToolDispatchTiming>>,
    ) -> bool {
        let Some(call_id) = response_input_call_id(response) else {
            return false;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(ordinal) = state.call_ordinals.get(call_id).copied() else {
            return false;
        };
        if state.sealed {
            return false;
        }
        state.responses.push(PendingWorkspaceEvidenceResponse {
            ordinal,
            response: response.clone(),
            workspace_cwd: classification.workspace_cwd.clone(),
            source_dependencies,
            source_path_observations,
            timing,
        });
        true
    }

    #[cfg(test)]
    pub(crate) fn queue_mutating_response_for_test(
        &self,
        response: &ResponseInputItem,
        classification: &crate::tool_history::WorkspaceCallClassification,
        source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
    ) -> bool {
        self.queue_response(
            response,
            classification,
            classification.source_dependencies.clone(),
            source_path_observations,
            None,
        )
    }

    pub(crate) async fn flush(
        self: &Arc<Self>,
        session: &Session,
        turn: &TurnContext,
        tracker: &SharedTurnDiffTracker,
    ) -> WorkspaceEvidenceGenerationFlush {
        let (responses, mutations) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.sealed = true;
            (
                std::mem::take(&mut state.responses),
                std::mem::take(&mut state.mutations),
            )
        };
        tracker
            .lock()
            .await
            .clear_workspace_evidence_generation_batch(self);

        struct Group {
            cwd: std::path::PathBuf,
            responses: Vec<PendingWorkspaceEvidenceResponse>,
            mutations: Vec<PendingWorkspaceMutation>,
        }

        struct FinalizedResponse {
            ordinal: u64,
            response: ResponseInputItem,
            revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
            source_dependencies:
                std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
            source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
        }

        let mut groups = std::collections::BTreeMap::<std::path::PathBuf, Group>::new();
        for response in responses {
            let key = canonical_workspace_evidence_key(&response.workspace_cwd);
            groups
                .entry(key)
                .or_insert_with(|| Group {
                    cwd: response.workspace_cwd.clone(),
                    responses: Vec::new(),
                    mutations: Vec::new(),
                })
                .responses
                .push(response);
        }
        for mutation in mutations {
            let key = canonical_workspace_evidence_key(&mutation.workspace_cwd);
            groups
                .entry(key)
                .or_insert_with(|| Group {
                    cwd: mutation.workspace_cwd.clone(),
                    responses: Vec::new(),
                    mutations: Vec::new(),
                })
                .mutations
                .push(mutation);
        }
        // Relay persistence precedes this generation-boundary flush. Exclude
        // only the queued responses that will be bound to the authoritative
        // post-generation identity below; earlier history must still be
        // invalidated by every mutation in the batch.
        let current_generation_response_call_ids = groups
            .values()
            .flat_map(|group| group.responses.iter())
            .filter_map(|response| response_input_call_id(&response.response).map(str::to_string))
            .collect::<std::collections::BTreeSet<_>>();

        let primary_key = canonical_workspace_evidence_key(turn.config.cwd.as_path());
        let mut prefetched_workspace_identity = None;
        let mut finalized_responses = Vec::new();
        #[cfg(test)]
        let mut authoritative_capture_count = 0;
        for (key, mut group) in groups {
            group.responses.sort_by_key(|response| response.ordinal);
            group.mutations.sort_by_key(|mutation| mutation.ordinal);
            let capture_started = Instant::now();
            let capture = session
                .services
                .git_workspace
                .workspace_evidence_identity_with_attribution(&group.cwd)
                .await;
            #[cfg(test)]
            {
                authoritative_capture_count += 1;
            }
            let capture_duration = capture_started.elapsed();
            if let Some(timing) = group
                .responses
                .iter()
                .find_map(|response| response.timing.as_ref())
            {
                timing.record_workspace_evidence_after(capture_duration);
            }

            if !group.mutations.is_empty() {
                let broad_invalidation = group
                    .mutations
                    .iter()
                    .any(|mutation| mutation.affected_paths.is_none());
                let affected_paths = (!broad_invalidation).then(|| {
                    group
                        .mutations
                        .iter()
                        .flat_map(|mutation| {
                            mutation
                                .affected_paths
                                .as_ref()
                                .into_iter()
                                .flatten()
                                .cloned()
                        })
                        .collect::<std::collections::BTreeSet<_>>()
                });
                session
                    .invalidate_tool_history_source_dependencies_excluding_call_ids(
                        turn.config.codex_home.as_path(),
                        affected_paths.as_ref(),
                        capture.identity.as_ref(),
                        &current_generation_response_call_ids,
                    )
                    .await;

                let (final_mutation_revision, observe_command_ledger) = {
                    let tracker = tracker.lock().await;
                    (
                        tracker.current_mutation_revision(),
                        group
                            .mutations
                            .iter()
                            .any(|mutation| mutation.observe_command_ledger),
                    )
                };
                if observe_command_ledger {
                    session
                        .services
                        .command_execution
                        .observe_repository_revision_with_identity(
                            &turn.sub_id,
                            final_mutation_revision,
                            capture.identity.clone(),
                        )
                        .await;
                }
            }

            if key == primary_key {
                // Preserve the distinction between no primary capture and an
                // authoritative capture of a non-Git workspace. The latter
                // must suppress an older continuation prefetch just as a Git
                // identity does.
                prefetched_workspace_identity = Some(capture.identity.clone());
            }
            for response in group.responses {
                finalized_responses.push(FinalizedResponse {
                    ordinal: response.ordinal,
                    response: response.response,
                    revision: capture.identity.clone(),
                    source_dependencies: response.source_dependencies,
                    source_path_observations: response.source_path_observations,
                });
            }
        }

        finalized_responses.sort_by_key(|response| response.ordinal);
        #[cfg(test)]
        let registered_call_ids = finalized_responses
            .iter()
            .filter_map(|response| response_input_call_id(&response.response).map(str::to_string))
            .collect();
        for response in finalized_responses {
            ToolCallRuntime::register_workspace_evidence_observation(
                session,
                turn,
                WorkspaceEvidenceObservation {
                    response: &response.response,
                    revision: response.revision,
                    captured_current: true,
                    source_dependencies: response.source_dependencies,
                    source_path_observations: response.source_path_observations,
                    workspace_gate_guard: None,
                },
            )
            .await;
        }

        WorkspaceEvidenceGenerationFlush {
            prefetched_workspace_identity,
            #[cfg(test)]
            authoritative_capture_count,
            #[cfg(test)]
            registered_call_ids,
        }
    }
}

fn response_input_call_id(response: &ResponseInputItem) -> Option<&str> {
    match response {
        ResponseInputItem::FunctionCallOutput { call_id, .. }
        | ResponseInputItem::McpToolCallOutput { call_id, .. }
        | ResponseInputItem::CustomToolCallOutput { call_id, .. }
        | ResponseInputItem::ToolSearchOutput { call_id, .. } => Some(call_id),
        ResponseInputItem::Message { .. } => None,
    }
}

fn canonical_workspace_evidence_key(cwd: &std::path::Path) -> std::path::PathBuf {
    codex_git_utils::get_git_repo_root(cwd)
        .and_then(|root| dunce::canonicalize(root).ok())
        .or_else(|| dunce::canonicalize(cwd).ok())
        .unwrap_or_else(|| cwd.to_path_buf())
}

fn workspace_tool_may_use_parallel_gate(supports_parallel: bool, workspace_capable: bool) -> bool {
    supports_parallel && !workspace_capable
}

fn bypasses_outer_workspace_gate(tool_name: &codex_tools::ToolName) -> bool {
    // `exec` is only an orchestration carrier. Its nested calls independently
    // acquire the ordinary workspace gate through this runtime.
    crate::tools::code_mode::is_exec_tool_name(tool_name)
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

fn workspace_evidence_classification_for_executed_payload(
    original: &crate::tool_history::WorkspaceCallClassification,
    tool_identity: &str,
    executed_payload: Option<&ToolPayload>,
    default_cwd: &std::path::Path,
) -> crate::tool_history::WorkspaceCallClassification {
    executed_payload.map_or_else(
        || original.clone(),
        |payload| {
            crate::tool_history::classify_workspace_tool_call(tool_identity, payload, default_cwd)
        },
    )
}

fn workspace_evidence_baseline_is_compatible(
    original: &crate::tool_history::WorkspaceCallClassification,
    executed: &crate::tool_history::WorkspaceCallClassification,
) -> bool {
    original.workspace_cwd == executed.workspace_cwd
}

fn canonical_workspace_resource_key(
    classification: Option<&crate::tool_history::WorkspaceCallClassification>,
    cache: &Mutex<std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>>>,
) -> Option<std::path::PathBuf> {
    let classification = classification?;
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(resource) = cache.get(&classification.workspace_cwd) {
        return resource.clone();
    }
    let resource = codex_git_utils::get_git_repo_root(&classification.workspace_cwd)
        .and_then(|repo_root| dunce::canonicalize(repo_root).ok());
    cache.insert(classification.workspace_cwd.clone(), resource.clone());
    resource
}

fn workspace_resource_key_for_admission(
    classification: &crate::tool_history::WorkspaceCallClassification,
    cache: &Mutex<std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>>>,
    supports_parallel: bool,
    workspace_capable: bool,
) -> Option<std::path::PathBuf> {
    (supports_parallel && workspace_capable)
        .then(|| canonical_workspace_resource_key(Some(classification), cache))
        .flatten()
}

#[derive(Debug)]
struct WorkspaceAdmissionPlan {
    bypass_outer_gate: bool,
    resource_key: Option<std::path::PathBuf>,
    supports_parallel: bool,
    workspace_capable: bool,
}

fn workspace_admission_plan(
    tool_name: &codex_tools::ToolName,
    classification: &crate::tool_history::WorkspaceCallClassification,
    cache: &Mutex<std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>>>,
    supports_parallel: bool,
    workspace_capable: bool,
) -> WorkspaceAdmissionPlan {
    WorkspaceAdmissionPlan {
        bypass_outer_gate: bypasses_outer_workspace_gate(tool_name),
        resource_key: workspace_resource_key_for_admission(
            classification,
            cache,
            supports_parallel,
            workspace_capable,
        ),
        supports_parallel,
        workspace_capable,
    }
}

fn workspace_resource_gate(
    workspace_execution: &Mutex<std::collections::HashMap<std::path::PathBuf, Weak<RwLock<()>>>>,
    resource_key: std::path::PathBuf,
) -> Arc<RwLock<()>> {
    let mut gates = workspace_execution
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(&resource_key).and_then(Weak::upgrade) {
        return gate;
    }
    gates.retain(|_, gate| gate.strong_count() > 0);
    let gate = Arc::new(RwLock::new(()));
    gates.insert(resource_key, Arc::downgrade(&gate));
    gate
}

struct WorkspaceEvidenceBaseline {
    revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    cache_hit: bool,
    timed_out_git_dependencies: Vec<crate::git_workspace::WorkspaceEvidenceGitDependency>,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
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
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    source_path_observations: Vec<crate::git_workspace::SourcePathChangeObservation>,
    workspace_gate_guard: Option<WorkspaceGateGuard>,
}

struct WorkspaceResourceGateGuard {
    _guard: OwnedRwLockWriteGuard<()>,
}

enum WorkspaceGateGuard {
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
            let guard = gate.read_owned().await;
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
            let guard = gate.write_owned().await;
            drop(waiter_guard);
            guard
        }
    }
}

async fn acquire_workspace_gate(
    parallel_execution: Arc<RwLock<()>>,
    workspace_execution: Arc<
        Mutex<std::collections::HashMap<std::path::PathBuf, Weak<RwLock<()>>>>,
    >,
    resource_key: Option<std::path::PathBuf>,
    supports_parallel: bool,
    workspace_capable: bool,
    turn_timing_state: &Arc<TurnTimingState>,
) -> WorkspaceGateGuard {
    if workspace_tool_may_use_parallel_gate(supports_parallel, workspace_capable) {
        return WorkspaceGateGuard::Shared {
            _guard: acquire_shared_workspace_gate(parallel_execution, turn_timing_state).await,
        };
    }
    let Some(resource_key) = resource_key.filter(|_| supports_parallel && workspace_capable) else {
        return WorkspaceGateGuard::Exclusive {
            _guard: acquire_exclusive_workspace_gate(parallel_execution, turn_timing_state).await,
        };
    };

    let resource_gate = workspace_resource_gate(workspace_execution.as_ref(), resource_key);
    let resource = WorkspaceResourceGateGuard {
        _guard: acquire_exclusive_workspace_gate(resource_gate, turn_timing_state).await,
    };
    // Acquire the resource first. If an unknown-resource writer is already
    // queued on the global barrier, Tokio's writer preference lets it run
    // before another same-resource waiter extends the read phase.
    let global = acquire_shared_workspace_gate(parallel_execution, turn_timing_state).await;
    WorkspaceGateGuard::ResourceScoped {
        _resource: resource,
        _global: global,
    }
}

async fn capture_workspace_evidence_baseline(
    cache: &crate::git_workspace::GitWorkspaceCache,
    cwd: &std::path::Path,
    source_dependencies: std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
    reuse_latest: bool,
) -> WorkspaceEvidenceBaseline {
    // Register dependency watches before the authoritative snapshot. A change
    // that races the snapshot is then either reflected by the snapshot or
    // invalidates the path-scoped observation.
    let repo_root = codex_git_utils::get_git_repo_root(cwd);
    let source_path_observations = begin_source_path_observations(cache, cwd, &source_dependencies);
    let cached_revision = if reuse_latest {
        repo_root
            .as_deref()
            .and_then(|repo_root| cache.latest_workspace_evidence_identity(repo_root))
    } else {
        None
    };
    let (revision, cache_hit, timed_out_git_dependencies) = match cached_revision {
        Some(revision) => (Some(revision), true, Vec::new()),
        None => {
            let capture = cache
                .workspace_evidence_identity_with_attribution(cwd)
                .await;
            (capture.identity, false, capture.timed_out_git_dependencies)
        }
    };
    WorkspaceEvidenceBaseline {
        revision,
        cache_hit,
        timed_out_git_dependencies,
        source_dependencies,
        source_path_observations,
    }
}

fn begin_source_path_observations(
    cache: &crate::git_workspace::GitWorkspaceCache,
    cwd: &std::path::Path,
    source_dependencies: &std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
) -> Vec<crate::git_workspace::SourcePathChangeObservation> {
    codex_git_utils::get_git_repo_root(cwd)
        .as_ref()
        .map(|repo_root| {
            source_dependencies
                .iter()
                .filter_map(|dependency| {
                    cache.begin_source_path_change_observation(
                        repo_root,
                        std::path::Path::new(&dependency.path),
                        dependency.recursive,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
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
    // `None` is an authoritative identity for a non-Git workspace. The
    // mutation tracker, rather than the presence of a Git revision, determines
    // whether the pre-dispatch observation is still current.
    let captured_current = !mutation_advanced;
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
    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            session,
            step_context,
            tracker,
            parallel_execution: Arc::new(RwLock::new(())),
            workspace_execution: Arc::new(Mutex::new(std::collections::HashMap::new())),
            canonical_workspace_resources: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sampling_request_signals: None,
        }
    }

    pub(crate) fn with_sampling_request_signals(
        mut self,
        collector: SamplingRequestSignalCollector,
    ) -> Self {
        self.sampling_request_signals = Some(collector);
        self
    }

    async fn activate_workspace_evidence_generation(&self, call_id: &str) {
        let batch = &self.step_context.workspace_evidence_generation_batch;
        if !batch.register_call(call_id) {
            return;
        }
        self.tracker
            .lock()
            .await
            .activate_workspace_evidence_generation_batch(batch);
    }

    pub(crate) async fn flush_workspace_evidence_generation(
        &self,
    ) -> WorkspaceEvidenceGenerationFlush {
        self.step_context
            .workspace_evidence_generation_batch
            .flush(
                self.session.as_ref(),
                self.step_context.turn.as_ref(),
                &self.tracker,
            )
            .await
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
            && self
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
        let gate_guard = Some(WorkspaceGateGuard::Shared {
            _guard: Arc::clone(&self.parallel_execution).read_owned().await,
        });
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
        let (revision, captured_current) = match baseline.as_ref() {
            Some(_baseline) if mutation_advanced => {
                let revision = session
                    .services
                    .git_workspace
                    .workspace_evidence_identity(&classification.workspace_cwd)
                    .await;
                // A `None` revision is also authoritative for a non-Git workspace: the
                // observation was captured after the tool completed.
                let captured_current = true;
                (revision, captured_current)
            }
            Some(baseline) => finish_workspace_evidence_capture(baseline, mutation_advanced),
            None => (None, false),
        };
        Self::register_workspace_evidence_observation(
            session,
            turn,
            WorkspaceEvidenceObservation {
                response,
                revision,
                captured_current,
                source_dependencies,
                source_path_observations,
                workspace_gate_guard,
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
        let WorkspaceEvidenceObservation {
            response,
            revision,
            captured_current,
            source_dependencies,
            source_path_observations,
            workspace_gate_guard,
        } = input;
        let Some(observation) =
            crate::tool_history::WorkspaceEvidenceObservation::from_response_item_with_freshness(
                revision,
                &ResponseItem::from(response.clone()),
                source_dependencies,
                captured_current,
            )
            .map(|observation| observation.with_source_path_observations(source_path_observations))
        else {
            return;
        };
        session
            .register_workspace_evidence(
                turn.config.codex_home.as_path(),
                observation,
                workspace_gate_guard,
            )
            .await;
    }

    pub(crate) fn record_code_mode_result(
        &self,
        mut result: CodeModeToolResult<'_>,
        receipts: &[codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt],
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        if result.source_dependencies.is_none() {
            let classification = crate::tool_history::classify_workspace_tool_call(
                result.tool_name.name.as_str(),
                result.payload,
                self.step_context.turn.config.cwd.as_path(),
            );
            result.source_dependencies = classification
                .observes_workspace
                .then_some(classification.source_dependencies);
        }
        collector.record_code_mode_result(result);
        collector.record_accepted_deterministic_continuation_receipts(receipts);
    }

    pub(crate) fn record_code_mode_failure(
        &self,
        cell_id: &str,
        tool_name: &codex_tools::ToolName,
        payload: Option<&ToolPayload>,
        failure_fingerprint: String,
    ) {
        let Some(collector) = &self.sampling_request_signals else {
            return;
        };
        let source_dependencies =
            crate::tool_history::tool_observes_workspace(tool_name.name.as_str()).then(|| {
                payload.map_or_else(std::collections::BTreeSet::new, |payload| {
                    crate::tool_history::source_dependencies_for_tool_call(
                        tool_name.name.as_str(),
                        payload,
                        self.step_context.turn.config.cwd.as_path(),
                    )
                })
            });
        collector.record_code_mode_failure(
            cell_id,
            tool_name,
            payload,
            source_dependencies,
            failure_fingerprint,
        );
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.step_context
            .tool_router()?
            .create_diff_consumer(tool_name)
    }

    /// Centralized eligibility predicate for starting a safe leading call while
    /// the current model response is still streaming. A rejection closes the
    /// eligible prefix.
    pub(crate) fn take_eager_read_eligibility(
        &self,
        call: &ToolCall,
        earlier_calls_eligible: &mut bool,
    ) -> bool {
        // Start the code-mode orchestration carrier as soon as its persisted
        // call item is available. The carrier itself bypasses the workspace
        // gate; each nested call retains its own admission policy. Closing the
        // eager prefix still prevents later model-issued calls from overtaking
        // this response item.
        if *earlier_calls_eligible && crate::tools::code_mode::is_exec_tool_name(&call.tool_name) {
            *earlier_calls_eligible = false;
            return true;
        }
        let typed_read = self.step_context.tool_router().is_some_and(|router| {
            matches!(
                router.classify_tool_name(self.step_context.turn.as_ref(), &call.tool_name),
                TypedToolClass::ReadSearch
            )
        });
        let eligible = *earlier_calls_eligible
            && typed_read
            && self
                .step_context
                .tool_router()
                .is_some_and(|router| router.tool_supports_parallel(call));
        *earlier_calls_eligible = eligible;
        eligible
    }

    #[cfg(test)]
    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        self.handle_tool_call_with_timing(
            call,
            cancellation_token,
            TokioInstant::now(),
            /*eager*/ false,
        )
    }

    #[cfg(test)]
    pub(crate) fn handle_tool_call_with_timing(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
        item_accepted_at: TokioInstant,
        eager: bool,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        let timing = self.create_tool_dispatch_timing(item_accepted_at, eager);
        self.handle_tool_call_with_trace(call, cancellation_token, timing)
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

    #[cfg(test)]
    pub(crate) fn handle_tool_call_with_trace(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
        timing: Arc<ToolDispatchTiming>,
    ) -> impl std::future::Future<Output = Result<ResponseInputItem, CodexErr>> {
        let completion = self.handle_model_tool_call_with_trace(call, cancellation_token, timing);
        async move { completion.await.map(|completion| completion.response) }
    }

    pub(crate) fn handle_model_tool_call_with_trace(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
        timing: Arc<ToolDispatchTiming>,
    ) -> impl std::future::Future<Output = Result<ToolCallCompletion, CodexErr>> {
        self.step_context
            .workspace_evidence_generation_batch
            .register_call(&call.call_id);
        self.step_context
            .turn
            .turn_timing_state
            .record_tool_call(call.tool_name.name.as_str());
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
        let signal_registration = self.sampling_request_signals.as_ref().map(|collector| {
            collector.register_deterministic_tool_call(
                &call.tool_name,
                &call.payload,
                &call.call_id,
            )
        });
        let signal_collector = self.sampling_request_signals.clone();
        async move {
            timing.mark_first_poll();
            let _tool_call_timing_guard = tool_call_timing_guard;
            let workspace_call_classification =
                crate::tool_history::classify_workspace_tool_call(
                    call.tool_name.name.as_str(),
                    &call.payload,
                    self.step_context.turn.config.cwd.as_path(),
                );
            self.activate_workspace_evidence_generation(&call.call_id)
                .await;
            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.suppressed_failure.as_ref()
            {
                let mut output = FunctionCallOutputPayload::from_text(reused_failure_diagnosis(
                    &call.tool_name,
                    &guard.failure_fingerprint,
                ));
                output.success = Some(false);
                let response = ResponseInputItem::FunctionCallOutput {
                    call_id: call.call_id.clone(),
                    output,
                };
                timing.record_outcome("failure");
                timing.mark_output_collected();
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector.record_suppressed_failure(
                        registration.ordinal,
                        &guard.failure_fingerprint,
                    );
                }
                self.register_workspace_evidence_for_response(
                    &response,
                    None,
                    false,
                    None,
                    &workspace_call_classification,
                    None,
                )
                .await;
                return Ok(ToolCallCompletion {
                    required_terminal: Some(RequiredToolTerminal {
                        call_id: call.call_id.clone(),
                        cause: RequiredToolTerminalCause::Failure,
                        message: format!(
                            "required tool `{}` repeated the same failure",
                            call.tool_name
                        ),
                    }),
                    response,
                });
            }
            if let Some(registration) = signal_registration.as_ref()
                && let Some(guard) = registration.suppressed_source_pass.as_ref()
            {
                let response = suppressed_function_response(
                    &call.call_id,
                    serde_json::json!({
                        "kind": "unchanged_source_pass_suppression",
                        "disposition": "suppressed",
                        "reason": "the same broad source action is blocked because the active obligation, evidence identity, and action are unchanged; change one of them before another broad source pass",
                    })
                    .to_string(),
                );
                timing.record_outcome("skipped");
                timing.mark_output_collected();
                if let Some(signal_collector) = signal_collector.as_ref() {
                    signal_collector.record_suppressed_source_pass(
                        registration.ordinal,
                        &guard.evidence_identity,
                    );
                }
                self.register_workspace_evidence_for_response(
                    &response,
                    None,
                    false,
                    None,
                    &workspace_call_classification,
                    None,
                )
                .await;
                return Ok(ToolCallCompletion::nonterminal(response));
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
            // Direct calls own one baseline here. The inner dispatch path skips
            // duplicate evidence work for this source and only nested code-mode
            // calls register inside the execution gate.
            let workspace_revision_before = if observes_workspace {
                let evidence_capture_started = Instant::now();
                let baseline = capture_workspace_evidence_baseline(
                    self.session.services.git_workspace.as_ref(),
                    &workspace_call_classification.workspace_cwd,
                    workspace_call_classification.source_dependencies.clone(),
                    true,
                )
                .await;
                timing.record_workspace_evidence_before(evidence_capture_started.elapsed());
                timing.record_workspace_evidence_before_attribution(
                    baseline.cache_hit,
                    baseline
                        .timed_out_git_dependencies
                        .iter()
                        .map(|dependency| dependency.as_str().to_string())
                        .collect(),
                );
                Some(baseline)
            } else {
                None
            };
            let mutation_revision_before = if observes_workspace {
                Some(self.tracker.lock().await.current_mutation_revision())
            } else {
                None
            };
            let mutation_tracker = Arc::clone(&self.tracker);
            let error_call = call.clone();
            let owner_tool_name = call.tool_name.clone();
            let owner_payload = call.payload.clone();
            let evidence_timing = Arc::clone(&timing);
            let future = self.clone().handle_tool_call_with_source_and_timing(
                call,
                ToolCallSource::Direct,
                cancellation_token,
                timing,
                Some(workspace_call_classification.source_dependencies.clone()),
                Some(workspace_call_classification.clone()),
            );
            let result = future.await;
            evidence_timing.record_outcome(tool_dispatch_outcome_label(&result));
            evidence_timing.mark_output_collected();
            if let Some(collector) = signal_collector.as_ref()
                && !crate::tools::code_mode::is_exec_tool_name(&owner_tool_name)
            {
                let timing_snapshot = evidence_timing.snapshot(TokioInstant::now());
                collector.record_child_runtime(
                    timing_snapshot
                        .first_poll_to_output_collected_ms
                        .or(timing_snapshot.total_duration_ms)
                        .unwrap_or_default(),
                );
            }
            match result {
                Ok(mut response) => {
                    let mutation_advanced = if let Some(before) = mutation_revision_before {
                        mutation_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    let outcome_context = response.outcome_context();
                    let signal = response.sampling_request_signal();
                    let required_terminal =
                        required_tool_terminal(&error_call, outcome_context, signal.as_ref());
                    let owner_key = response.deterministic_continuation_owner_key();
                    if let Some(owner_key) = owner_key.as_deref() {
                        let continuations = self
                            .session
                            .services
                            .code_mode_service
                            .owner_drained_continuation_snapshot(owner_key);
                        let accepted = response.merge_owner_drained_continuations(continuations);
                        self.session
                            .services
                            .code_mode_service
                            .acknowledge_owner_drained_continuations(owner_key, &accepted);
                        if let Some(collector) = &signal_collector {
                            collector
                                .record_accepted_deterministic_continuation_receipts(&accepted);
                        }
                    }
                    let receipts = response.deterministic_continuation_receipts();
                    if let Some(collector) = &signal_collector {
                        collector.record_accepted_deterministic_continuation_receipts(&receipts);
                    }
                    let canonical_artifact_required = response.requires_canonical_artifact();
                    let executed_workspace_call_classification =
                        workspace_evidence_classification_for_executed_payload(
                            &workspace_call_classification,
                            owner_tool_name.name.as_str(),
                            Some(&response.payload),
                            self.step_context.turn.config.cwd.as_path(),
                        );
                    let source_dependencies_override = owner_key.as_deref().and_then(|owner_key| {
                        signal_collector.as_ref().and_then(|collector| {
                            collector.code_mode_source_dependencies(owner_key)
                        })
                    })
                    .or_else(|| response.projected_source_dependencies().cloned());
                    // Ordinary tool results install their canonical projection in the
                    // registry, where this phase is already recorded. Cancellation
                    // owns a synthesized terminal result and intentionally bypasses
                    // the remaining registry pipeline, so materializing its raw
                    // model-visible response is the projection boundary. The timing
                    // slot is write-once, preserving the registry measurement for
                    // ordinary results while closing the abort path's attribution.
                    let projection_started = Instant::now();
                    let response = response.into_response();
                    evidence_timing.record_output_projection(projection_started.elapsed());
                    let code_mode_exec = crate::tools::code_mode::is_exec_tool_name(&owner_tool_name);
                    let (workspace_revision_before, evidence_classification) =
                        if code_mode_exec {
                            match source_dependencies_override.as_ref() {
                                Some(source_dependencies) => {
                                    let classification =
                                        crate::tool_history::WorkspaceCallClassification {
                                            observes_workspace: true,
                                            workspace_cwd: self
                                                .step_context
                                                .turn
                                                .config
                                                .cwd
                                                .clone()
                                                .to_path_buf(),
                                            source_dependencies: source_dependencies.clone(),
                                        };
                                    let baseline = capture_workspace_evidence_baseline(
                                        self.session.services.git_workspace.as_ref(),
                                        &classification.workspace_cwd,
                                        classification.source_dependencies.clone(),
                                        true,
                                    )
                                    .await;
                                    (Some(baseline), classification)
                                }
                                None => {
                                    self.session
                                        .register_non_workspace_code_mode_call(
                                            self.step_context.turn.config.codex_home.as_path(),
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
                    let evidence_capture_started = Instant::now();
                    let workspace_evidence_deferred = self.register_workspace_evidence_for_response(
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        source_dependencies_override,
                        &evidence_classification,
                        Some(Arc::clone(&evidence_timing)),
                    )
                    .await;
                    if evidence_classification.observes_workspace && !workspace_evidence_deferred {
                        evidence_timing
                            .record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_direct_wait_owner_result(
                            authoritative_direct_wait,
                            &owner_tool_name,
                            &owner_payload,
                            signal.as_ref(),
                            &response,
                        );
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
                        required_terminal,
                    })
                }
                Err(FunctionCallError::Fatal(message)) => {
                    if let (Some(collector), Some(ordinal)) = (&signal_collector, signal_ordinal) {
                        collector.record_failure(ordinal, &format!("fatal:{message}"));
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
                        collector.record_failure(ordinal, &format!("model:{other}"));
                    }
                    let response = Self::failure_response(error_call.clone(), other);
                    let evidence_capture_started = Instant::now();
                    let workspace_evidence_deferred = self.register_workspace_evidence_for_response(
                        &response,
                        workspace_revision_before,
                        mutation_advanced,
                        None,
                        &workspace_call_classification,
                        Some(Arc::clone(&evidence_timing)),
                    )
                    .await;
                    if observes_workspace && !workspace_evidence_deferred {
                        evidence_timing
                            .record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                    Ok(ToolCallCompletion {
                        response,
                        required_terminal: terminal_cause.map(|cause| RequiredToolTerminal {
                            call_id: error_call.call_id,
                            cause,
                            message: format!(
                                "required tool `{}` {}",
                                error_call.tool_name,
                                match cause {
                                    RequiredToolTerminalCause::Blocked => "blocked",
                                    RequiredToolTerminalCause::Failure => "failed",
                                    RequiredToolTerminalCause::TimedOut => "timed out",
                                    RequiredToolTerminalCause::RecoverableCancellation => {
                                        "was cancelled"
                                    }
                                }
                            ),
                        }),
                    })
                }
            }
        }
        .in_current_span()
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call_with_source(
        self,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
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
                    )
            })
        } else {
            true
        };
        async move {
            if !accepted {
                return Err(FunctionCallError::Fatal(format!(
                    "refusing nested tool call `{}` after terminal acceptance was sealed or tool-call capacity was exhausted",
                    call.call_id
                )));
            }
            self.step_context
                .turn
                .turn_timing_state
                .record_tool_call(call.tool_name.name.as_str());
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
            self.activate_workspace_evidence_generation(&call.call_id)
                .await;
            let result = self
                .handle_tool_call_with_source_and_timing(
                    call,
                    source,
                    cancellation_token,
                    Arc::clone(&timing),
                    None,
                    None,
                )
                .await;
            timing.record_outcome(tool_dispatch_outcome_label(&result));
            // Own the returned-result boundary here. Inner dispatch marks the
            // ordinary handler path, but cancellation and future early-return
            // paths must record collection before this lifecycle guard drops.
            timing.mark_output_collected();
            if nested_code_mode && let Some(collector) = signal_collector.as_ref() {
                let timing_snapshot = timing.snapshot(TokioInstant::now());
                collector.record_child_runtime(
                    timing_snapshot
                        .first_poll_to_output_collected_ms
                        .or(timing_snapshot.total_duration_ms)
                        .unwrap_or_default(),
                );
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
        projection_source_dependencies: Option<
            std::collections::BTreeSet<crate::tool_history::SourceDependencyV1>,
        >,
        workspace_admission_hint: Option<crate::tool_history::WorkspaceCallClassification>,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        let Some(router) = self.step_context.tool_router() else {
            return Either::Left(std::future::ready(Err(FunctionCallError::Fatal(
                "step tool router was not finalized before tool execution".to_string(),
            ))));
        };
        let supports_parallel = router.tool_supports_parallel(&call);
        let wait_for_runtime_cancellation = router.tool_waits_for_runtime_cancellation(&call);
        let router = Arc::clone(router);
        let session = Arc::clone(&self.session);
        let step_context = Arc::clone(&self.step_context);
        let turn = Arc::clone(&step_context.turn);
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        let workspace_execution = Arc::clone(&self.workspace_execution);
        // The caller-facing token selects terminal ownership here. Handlers
        // receive a separate token so they cannot observe cancellation and
        // publish a competing terminal result before this boundary claims the
        // abort. Runtimes that need cooperative cleanup are notified only
        // after the abort owns the terminal outcome.
        let runtime_cancellation_token = CancellationToken::new();
        let invocation_cancellation_token = runtime_cancellation_token.clone();
        let started = Instant::now();
        let abort_session = Arc::clone(&session);
        let abort_source = source.clone();
        // Direct calls own evidence registration in the outer response path,
        // where code-mode dependency overrides are also available. Nested
        // code-mode calls have no such outer layer and register here.
        let (workspace_admission_classification, workspace_call_classification) =
            workspace_tool_call_classifications_for_dispatch(
                &source,
                call.tool_name.name.as_str(),
                &call.payload,
                turn.config.cwd.as_path(),
                workspace_admission_hint,
            );
        let model_issued = matches!(&source, ToolCallSource::Direct);
        let abort_turn = Arc::clone(&turn);
        let dispatch_state = Arc::new(ToolDispatchState::new());
        let admission_dispatch_state = Arc::clone(&dispatch_state);
        let router_dispatch_state = Arc::clone(&dispatch_state);
        let post_dispatch_state = Arc::clone(&dispatch_state);
        let dispatch_call = call.clone();
        let dispatch_tool_name = call.tool_name.name.clone();
        let evidence_call = call.clone();
        let workspace_capable =
            crate::tool_history::tool_observes_workspace(evidence_call.tool_name.name.as_str());
        let workspace_admission = workspace_admission_plan(
            &evidence_call.tool_name,
            &workspace_admission_classification,
            self.canonical_workspace_resources.as_ref(),
            supports_parallel,
            workspace_capable,
        );
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

        let mut dispatch_handle: AbortOnDropHandle<Result<AnyToolResult, FunctionCallError>> =
            AbortOnDropHandle::new(tokio::spawn(async move {
                let prefetched_workspace_evidence = if let Some(classification) =
                    workspace_call_classification
                        .as_ref()
                        .filter(|classification| classification.observes_workspace)
                {
                    let mutation_revision =
                        evidence_tracker.lock().await.current_mutation_revision();
                    let evidence_capture_started = Instant::now();
                    let baseline = capture_workspace_evidence_baseline(
                        session.services.git_workspace.as_ref(),
                        &classification.workspace_cwd,
                        classification.source_dependencies.clone(),
                        true,
                    )
                    .await;
                    Some((
                        mutation_revision,
                        baseline,
                        evidence_capture_started.elapsed(),
                    ))
                } else {
                    None
                };
                let gate_guard = if workspace_admission.bypass_outer_gate {
                    None
                } else {
                    Some(
                        acquire_workspace_gate(
                            Arc::clone(&lock),
                            Arc::clone(&workspace_execution),
                            workspace_admission.resource_key,
                            workspace_admission.supports_parallel,
                            workspace_admission.workspace_capable,
                            &turn.turn_timing_state,
                        )
                        .await,
                    )
                };
                if !admission_dispatch_state.try_admit() {
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

                let prefetched_workspace_evidence = if let (
                    Some((prefetched_mutation_revision, mut baseline, mut capture_duration)),
                    Some(classification),
                ) = (
                    prefetched_workspace_evidence,
                    workspace_call_classification.as_ref(),
                ) {
                    let admitted_mutation_revision =
                        evidence_tracker.lock().await.current_mutation_revision();
                    if admitted_mutation_revision != prefetched_mutation_revision {
                        let refresh_started = Instant::now();
                        baseline = capture_workspace_evidence_baseline(
                            session.services.git_workspace.as_ref(),
                            &classification.workspace_cwd,
                            classification.source_dependencies.clone(),
                            false,
                        )
                        .await;
                        capture_duration =
                            capture_duration.saturating_add(refresh_started.elapsed());
                    }
                    timing.record_workspace_evidence_before(capture_duration);
                    timing.record_workspace_evidence_before_attribution(
                        baseline.cache_hit,
                        baseline
                            .timed_out_git_dependencies
                            .iter()
                            .map(|dependency| dependency.as_str().to_string())
                            .collect(),
                    );
                    Some((baseline, admitted_mutation_revision))
                } else {
                    None
                };
                let (evidence_revision_before, evidence_mutation_revision_before) =
                    prefetched_workspace_evidence.unzip();

                let projection_source_dependencies = projection_source_dependencies
                    .or_else(|| {
                        workspace_call_classification
                            .as_ref()
                            .filter(|classification| classification.observes_workspace)
                            .map(|classification| classification.source_dependencies.clone())
                    })
                    .or_else(|| {
                        evidence_revision_before
                            .as_ref()
                            .map(|baseline| baseline.source_dependencies.clone())
                    });

                let dispatch = router
                    .dispatch_tool_call_with_terminal_outcome(
                        Arc::clone(&session),
                        Arc::clone(&step_context),
                        invocation_cancellation_token,
                        tracker,
                        dispatch_call,
                        source,
                        router_dispatch_state,
                    )
                    .instrument(dispatch_span.clone());
                let result = scope_tool_dispatch_timing(
                    Arc::clone(&timing),
                    crate::tools::registry::with_precomputed_projection_source_dependencies(
                        projection_source_dependencies,
                        dispatch,
                    ),
                )
                .await;
                timing.mark_output_collected();
                if post_dispatch_state.is_aborted() {
                    return result;
                }
                let successful = result.as_ref().is_ok_and(|result| {
                    result.outcome_for_logging() == codex_tools::ToolOutputOutcome::Success
                });
                turn.turn_timing_state
                    .record_tool_completion(dispatch_tool_name.as_str(), successful);
                let evidence_classification =
                    workspace_call_classification
                        .as_ref()
                        .map(|classification| {
                            workspace_evidence_classification_for_executed_payload(
                                classification,
                                dispatch_tool_name.as_str(),
                                result.as_ref().ok().map(|result| &result.payload),
                                turn.config.cwd.as_path(),
                            )
                        });
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
                let evidence_response = match result.as_ref() {
                    Ok(result) => Some(result.response()),
                    Err(FunctionCallError::Fatal(_)) => None,
                    Err(err) => Some(Self::failure_response_for_message(
                        &evidence_call,
                        err.to_string(),
                    )),
                };
                if let (Some(response), Some(classification)) = (
                    evidence_response.as_ref(),
                    evidence_classification
                        .as_ref()
                        .filter(|classification| classification.observes_workspace),
                ) {
                    let evidence_capture_started = Instant::now();
                    let mutation_advanced = if let Some(before) = evidence_mutation_revision_before
                    {
                        evidence_tracker.lock().await.current_mutation_revision() > before
                    } else {
                        false
                    };
                    let workspace_evidence_deferred = Self::register_workspace_evidence_after_call(
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
                    .await;
                    if !workspace_evidence_deferred {
                        timing.record_workspace_evidence_after(evidence_capture_started.elapsed());
                    }
                } else {
                    drop(gate_guard);
                }
                result
            }));

        Either::Right(
            async move {
                tokio::select! {
                res = &mut dispatch_handle => res.map_err(Self::tool_task_join_error)?,
                _ = cancellation_token.cancelled() => {
                    if dispatch_state.is_terminal() || dispatch_handle.is_finished() {
                        dispatch_handle.await.map_err(Self::tool_task_join_error)?
                    } else {
                        let cancelled_before_admission = match dispatch_state.try_abort() {
                            ToolDispatchAbort::BeforeAdmission => true,
                            ToolDispatchAbort::AfterAdmission => false,
                            ToolDispatchAbort::AlreadyTerminal => {
                                return dispatch_handle.await.map_err(Self::tool_task_join_error)?;
                            }
                        };
                        let secs = started.elapsed().as_secs_f32().max(0.1);
                        abort_dispatch_span.record("aborted", true);
                        if wait_for_runtime_cancellation && !cancelled_before_admission {
                            runtime_cancellation_token.cancel();
                            // The abort owns the terminal outcome; await only so
                            // the runtime can finish process teardown. A
                            // non-cooperative implementation cannot retain the
                            // turn indefinitely after cancellation.
                            match tokio::time::timeout(
                                TOOL_RUNTIME_CANCELLATION_GRACE,
                                &mut dispatch_handle,
                            )
                            .await
                            {
                                Ok(Ok(_)) => {}
                                Ok(Err(err)) if err.is_cancelled() => {}
                                Ok(Err(err)) => return Err(Self::tool_task_join_error(err)),
                                Err(_) => {
                                    warn!(
                                        tool_name = %call.tool_name,
                                        call_id = %call.call_id,
                                        grace_ms = TOOL_RUNTIME_CANCELLATION_GRACE.as_millis(),
                                        "tool runtime cleanup exceeded cancellation grace; aborting dispatch task"
                                    );
                                    dispatch_handle.abort();
                                    match dispatch_handle.await {
                                        Ok(_) => {}
                                        Err(err) if err.is_cancelled() => {}
                                        Err(err) => return Err(Self::tool_task_join_error(err)),
                                    }
                                }
                            }
                        } else {
                            dispatch_handle.abort();
                            match dispatch_handle.await {
                                Ok(result) => return result,
                                Err(err) if err.is_cancelled() => {}
                                Err(err) => return Err(Self::tool_task_join_error(err)),
                            }
                        }
                        if call.tool_name.namespace.is_none()
                            && call.tool_name.name == crate::tools::EXEC_COMMAND_TOOL_NAME
                        {
                            let terminated_processes = abort_session
                                .services
                                .unified_exec_manager
                                .terminate_unpublished_processes_for_call_ids(
                                    std::slice::from_ref(&call.call_id),
                                )
                                .await
                                .map_err(|error| {
                                    FunctionCallError::Fatal(format!(
                                        "failed to terminate unpublished retained process for interrupted exec_command call `{}`: {error}",
                                        call.call_id
                                    ))
                                })?;
                            if terminated_processes > 0 {
                                cancellation_timing.mark_exec_process_exited();
                            }
                            cancellation_timing.record_exec_cleanup_state(
                                /*background_process_expected*/ false,
                                /*running_process_after_cleanup*/ false,
                            );
                        }
                        // Cancelling the dispatch task drops `tool.handle(...)`
                        // before its ordinary return marker can run. Close the
                        // handler boundary only after any retained process has
                        // confirmed termination so lifecycle ordering remains
                        // ProcessExit <= HandlerReturn.
                        cancellation_timing.mark_handler_exit_if_entered();
                        let mut response = Self::aborted_response(&call, secs);
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
        )
    }
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
    fn tool_task_join_error(err: JoinError) -> FunctionCallError {
        FunctionCallError::Fatal(format!("tool task failed to receive: {err:?}"))
    }

    fn failure_response(call: ToolCall, err: FunctionCallError) -> ResponseInputItem {
        Self::failure_response_for_message(&call, err.to_string())
    }

    pub(crate) fn failure_response_for_message(
        call: &ToolCall,
        message: String,
    ) -> ResponseInputItem {
        match &call.payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call.call_id.clone(),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            },
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

    fn aborted_response(call: &ToolCall, secs: f32) -> AnyToolResult {
        AnyToolResult {
            call_id: call.call_id.clone(),
            payload: call.payload.clone(),
            result: Box::new(AbortedToolOutput {
                message: Self::abort_message(call, secs),
            }),
            model_projection: None,
            source_dependencies: None,
            code_mode_feedback: Vec::new(),
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        if crate::tools::is_shell_family_tool_name(&call.tool_name) {
            format!("Wall time: {secs:.1} seconds\naborted by user")
        } else {
            format!("aborted by user after {secs:.1}s")
        }
    }
}

impl ToolCallTimingGuard {
    #[cfg(test)]
    fn capture(
        timing: Arc<ToolDispatchTiming>,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        source: &ToolCallSource,
    ) -> Option<Self> {
        if !tracing::enabled!(tracing::Level::INFO) {
            return None;
        }

        Some(Self::new(
            timing,
            None,
            conversation_id,
            turn_id,
            call,
            source,
            true,
        ))
    }

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
            execution_started = snapshot.parallel_gate_admitted,
            item_to_first_poll_ms = snapshot.item_to_first_poll_ms.unwrap_or(0),
            parallel_gate_wait_ms = snapshot.parallel_gate_wait_ms.unwrap_or(0),
            authorization_state_coordination_ms = snapshot
                .authorization_state_coordination_ms
                .unwrap_or(0),
            first_poll_to_handler_entry_ms = snapshot
                .first_poll_to_handler_entry_ms
                .unwrap_or(0),
            dispatch_duration_ms = snapshot.parallel_gate_wait_ms.unwrap_or(0),
            handler_duration_ms = snapshot.handler_duration_ms.unwrap_or(0),
            workspace_evidence_before_ms = snapshot.workspace_evidence_before_ms.unwrap_or(0),
            workspace_evidence_before_cache_hit = snapshot
                .workspace_evidence_before_cache_hit
                .unwrap_or(false),
            workspace_evidence_before_timed_out_git_dependencies =
                ?snapshot.workspace_evidence_before_timed_out_git_dependencies,
            workspace_evidence_after_ms = snapshot.workspace_evidence_after_ms.unwrap_or(0),
            pre_tool_hook_ms = snapshot.pre_tool_hook_ms.unwrap_or(0),
            post_tool_hook_ms = snapshot.post_tool_hook_ms.unwrap_or(0),
            output_projection_ms = snapshot.output_projection_ms.unwrap_or(0),
            history_persistence_ms = snapshot.history_persistence_ms.unwrap_or(0),
            first_poll_to_output_collected_ms = snapshot
                .first_poll_to_output_collected_ms
                .unwrap_or(0),
            exec_request_to_spawn_ms = snapshot.exec_request_to_spawn_ms.unwrap_or(0),
            exec_spawn_to_exit_ms = snapshot.exec_spawn_to_exit_ms.unwrap_or(0),
            exec_exit_to_delivery_ms = snapshot.exec_exit_to_delivery_ms.unwrap_or(0),
            exec_spawn_to_delivery_ms = snapshot.exec_spawn_to_delivery_ms.unwrap_or(0),
            exec_process_alive_at_delivery = snapshot.exec_process_alive_at_delivery,
            exec_cleanup_state_observed = snapshot.exec_cleanup_state_observed,
            exec_background_process_expected = snapshot.exec_background_process_expected,
            exec_running_process_after_cleanup = snapshot.exec_running_process_after_cleanup,
            exec_running_process_stale = snapshot.exec_running_process_after_cleanup
                && !snapshot.exec_background_process_expected,
            post_handler_ms = snapshot.post_handler_ms.unwrap_or(0),
            total_duration_ms = snapshot.total_duration_ms.unwrap_or(0),
            "tool call completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::session::step_context::StepContext;
    use crate::tools::ToolRouter;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::ToolInvocation;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::ToolExecutionTiming;
    use crate::tools::registry::ToolExecutor;
    use crate::tools::registry::ToolRegistry;
    use crate::turn_diff_tracker::TurnDiffTracker;

    use codex_extension_api::ToolCallOutcome;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    #[test]
    fn required_tool_terminal_classification_preserves_success_yield_and_nonblocking_skips() {
        for context in [
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Deferred)),
            ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Suppressed)),
            ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::NotApplicable)),
        ] {
            assert_eq!(required_tool_terminal_cause(context, None), None);
        }

        assert_eq!(
            required_tool_terminal_cause(
                ToolOutputOutcomeContext::skipped(Some(
                    ToolOutputSkipDisposition::BlockingRequiredOperation,
                )),
                None,
            ),
            Some(RequiredToolTerminalCause::Blocked),
        );
        assert_eq!(
            required_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                None,
            ),
            None,
        );
        assert_eq!(
            required_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(&serde_json::json!({ "outcome": "failure" })),
            ),
            None,
        );
        assert_eq!(
            required_tool_terminal_cause(
                ToolOutputOutcomeContext::new(ToolOutputOutcome::TimedOut),
                None,
            ),
            Some(RequiredToolTerminalCause::TimedOut),
        );
        assert_eq!(
            required_tool_error_terminal_cause(&FunctionCallError::DeniedToModel(
                "rejected by user".to_string(),
            )),
            Some(RequiredToolTerminalCause::Blocked),
        );
        assert_eq!(
            required_tool_error_terminal_cause(&FunctionCallError::RespondToModel(
                "command exited nonzero".to_string(),
            )),
            None,
        );
    }

    #[test]
    fn shell_family_abort_messages_share_the_command_format() {
        for tool_name in [
            crate::tools::SHELL_COMMAND_TOOL_NAME,
            crate::tools::EXEC_COMMAND_TOOL_NAME,
        ] {
            let call = ToolCall {
                tool_name: codex_tools::ToolName::plain(tool_name),
                call_id: format!("{tool_name}-call"),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            };

            assert_eq!(
                ToolCallRuntime::abort_message(&call, 1.25),
                "Wall time: 1.2 seconds\naborted by user"
            );
        }
    }

    #[test]
    fn reused_stable_failures_require_a_changed_action_or_state() {
        let diagnosis: serde_json::Value = serde_json::from_str(&reused_failure_diagnosis(
            &codex_tools::ToolName::plain("read_tool_output"),
            "stable-failure",
        ))
        .expect("valid diagnosis");

        assert_eq!(diagnosis["retryable"], false);
        assert_eq!(diagnosis["required_action"], "change_route_or_state");
        assert!(
            diagnosis["next_action"]
                .as_str()
                .expect("next action")
                .contains("Do not repeat this call with unchanged arguments")
        );
    }
    use tokio::sync::Notify;
    use tokio::sync::oneshot;
    use tracing_test::internal::MockWriter;

    #[test]
    fn workspace_observers_are_serialized_without_an_enforced_read_boundary() {
        assert!(workspace_tool_may_use_parallel_gate(true, false));
        assert!(!workspace_tool_may_use_parallel_gate(true, true));
        assert!(!workspace_tool_may_use_parallel_gate(false, false));
        assert!(!workspace_tool_may_use_parallel_gate(false, true));
    }

    #[tokio::test]
    async fn workspace_gate_waiter_counter_excludes_immediate_shared_admission() {
        let turn_timing_state = Arc::new(TurnTimingState::default());
        let global = Arc::new(RwLock::new(()));
        let resources = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let first = acquire_workspace_gate(
            Arc::clone(&global),
            Arc::clone(&resources),
            None,
            true,
            false,
            &turn_timing_state,
        )
        .await;
        let second = acquire_workspace_gate(
            Arc::clone(&global),
            Arc::clone(&resources),
            None,
            true,
            false,
            &turn_timing_state,
        )
        .await;

        assert_eq!(
            turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count,
            0,
            "immediately granted shared gates are active holders, not waiters"
        );
        drop((first, second));
    }

    #[tokio::test]
    async fn canonical_workspace_resources_overlap_but_same_resource_and_unknown_serialize() {
        let root = tempfile::tempdir().expect("temporary workspace root");
        let first_repo = root.path().join("first");
        let second_repo = root.path().join("second");
        std::fs::create_dir_all(first_repo.join(".git")).expect("first repository marker");
        std::fs::create_dir_all(second_repo.join(".git")).expect("second repository marker");
        let classification = |workspace_cwd: &std::path::Path, observes_workspace| {
            crate::tool_history::WorkspaceCallClassification {
                observes_workspace,
                workspace_cwd: workspace_cwd.to_path_buf(),
                source_dependencies: Default::default(),
            }
        };
        let canonical_resources = Mutex::new(std::collections::HashMap::new());
        let first_key = canonical_workspace_resource_key(
            Some(&classification(&first_repo, false)),
            &canonical_resources,
        )
        .expect("canonical first resource");
        let second_key = canonical_workspace_resource_key(
            Some(&classification(&second_repo, true)),
            &canonical_resources,
        )
        .expect("canonical second resource");
        assert_ne!(first_key, second_key);

        let global = Arc::new(RwLock::new(()));
        let resources = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let turn_timing_state = Arc::new(TurnTimingState::default());
        let first_guard = acquire_workspace_gate(
            Arc::clone(&global),
            Arc::clone(&resources),
            Some(first_key.clone()),
            true,
            true,
            &turn_timing_state,
        )
        .await;
        let second_guard = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_workspace_gate(
                Arc::clone(&global),
                Arc::clone(&resources),
                Some(second_key),
                true,
                true,
                &turn_timing_state,
            ),
        )
        .await
        .expect("distinct canonical repositories should overlap");
        drop(second_guard);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                acquire_workspace_gate(
                    Arc::clone(&global),
                    Arc::clone(&resources),
                    Some(first_key),
                    true,
                    true,
                    &turn_timing_state,
                ),
            )
            .await
            .is_err(),
            "writes against the same canonical repository must serialize"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                acquire_workspace_gate(
                    Arc::clone(&global),
                    Arc::clone(&resources),
                    None,
                    true,
                    true,
                    &turn_timing_state,
                ),
            )
            .await
            .is_err(),
            "an unknown workspace resource must remain globally exclusive"
        );
        drop(first_guard);

        tokio::time::timeout(
            Duration::from_secs(1),
            acquire_workspace_gate(
                Arc::clone(&global),
                Arc::clone(&resources),
                None,
                true,
                true,
                &turn_timing_state,
            ),
        )
        .await
        .expect("unknown resource should enter after known resource exits");

        let replacement_key = std::path::PathBuf::from("replacement-resource");
        let replacement_gate = workspace_resource_gate(resources.as_ref(), replacement_key.clone());
        let retained_keys = resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(retained_keys.len(), 1);
        assert!(retained_keys.contains_key(&replacement_key));
        drop(retained_keys);
        drop(replacement_gate);
    }

    #[test]
    fn kd4_latency_exec_bypasses_outer_gate_while_nested_mutations_remain_exclusive() {
        let exec = codex_tools::ToolName::plain(crate::tools::code_mode::PUBLIC_TOOL_NAME);
        let nested_mutation = codex_tools::ToolName::plain("shell_command");

        assert!(bypasses_outer_workspace_gate(&exec));
        assert!(!bypasses_outer_workspace_gate(&nested_mutation));
        assert!(!workspace_tool_may_use_parallel_gate(false, true));
    }

    #[test]
    fn orchestration_audit_workspace_admission_plan_keeps_outer_and_nested_gate_ownership_explicit()
    {
        let classification = crate::tool_history::WorkspaceCallClassification {
            observes_workspace: true,
            workspace_cwd: std::path::PathBuf::from("missing-workspace"),
            source_dependencies: Default::default(),
        };
        let canonical_resources = Mutex::new(std::collections::HashMap::new());
        let exec = workspace_admission_plan(
            &codex_tools::ToolName::plain(crate::tools::code_mode::PUBLIC_TOOL_NAME),
            &classification,
            &canonical_resources,
            true,
            true,
        );
        let nested = workspace_admission_plan(
            &codex_tools::ToolName::plain("shell_command"),
            &classification,
            &canonical_resources,
            false,
            true,
        );

        assert!(exec.bypass_outer_gate);
        assert!(!nested.bypass_outer_gate);
        assert!(nested.resource_key.is_none());
        assert!(!nested.supports_parallel);
        assert!(nested.workspace_capable);
    }

    #[test]
    fn direct_dispatch_reuses_outer_classification_for_admission_without_duplicate_evidence() {
        let payload = ToolPayload::Function {
            arguments: r#"{"cmd":"cargo test","workdir":"missing-workspace"}"#.to_string(),
        };
        let admission_hint = crate::tool_history::classify_workspace_tool_call(
            "cargo_test",
            &payload,
            std::path::Path::new("missing-workspace"),
        );

        let (admission, inner_evidence) = workspace_tool_call_classifications_for_dispatch(
            &ToolCallSource::Direct,
            "cargo_test",
            &payload,
            std::path::Path::new("missing-workspace"),
            Some(admission_hint.clone()),
        );

        assert_eq!(admission, admission_hint);
        assert_eq!(inner_evidence, None);
    }

    #[test]
    fn post_hook_workspace_evidence_uses_the_executed_payload() {
        let default_cwd = std::path::Path::new("/repo");
        let original_mutation = ToolPayload::Function {
            arguments: serde_json::json!({
                "program": "git",
                "args": ["add", "src/lib.rs"],
                "workdir": "/repo"
            })
            .to_string(),
        };
        let original_mutation = crate::tool_history::classify_workspace_tool_call(
            "exec_command",
            &original_mutation,
            default_cwd,
        );
        assert!(!original_mutation.observes_workspace);

        let rewritten_read = ToolPayload::Function {
            arguments: serde_json::json!({
                "program": "rg",
                "args": ["needle", "src/lib.rs"],
                "workdir": "/other-repo"
            })
            .to_string(),
        };
        let executed_read = workspace_evidence_classification_for_executed_payload(
            &original_mutation,
            "exec_command",
            Some(&rewritten_read),
            default_cwd,
        );
        assert!(executed_read.observes_workspace);
        assert_eq!(
            executed_read.workspace_cwd,
            std::path::PathBuf::from("/other-repo")
        );
        assert_eq!(
            executed_read.source_dependencies,
            std::collections::BTreeSet::from([crate::tool_history::SourceDependencyV1::new(
                std::path::Path::new("/other-repo/src/lib.rs"),
                false,
            )])
        );
        assert!(!workspace_evidence_baseline_is_compatible(
            &original_mutation,
            &executed_read,
        ));

        let original_read = crate::tool_history::classify_workspace_tool_call(
            "exec_command",
            &rewritten_read,
            default_cwd,
        );
        let executed_mutation = workspace_evidence_classification_for_executed_payload(
            &original_read,
            "exec_command",
            Some(&ToolPayload::Function {
                arguments: serde_json::json!({
                    "program": "git",
                    "args": ["add", "src/lib.rs"],
                    "workdir": "/other-repo"
                })
                .to_string(),
            }),
            default_cwd,
        );
        assert!(!executed_mutation.observes_workspace);
        assert_eq!(
            workspace_evidence_classification_for_executed_payload(
                &original_read,
                "exec_command",
                None,
                default_cwd,
            ),
            original_read,
        );
    }

    #[test]
    fn non_workspace_or_serial_admission_skips_workspace_resource_resolution() {
        let missing = std::path::PathBuf::from("definitely-missing-workspace-resource");
        let classification = crate::tool_history::WorkspaceCallClassification {
            observes_workspace: false,
            workspace_cwd: missing,
            source_dependencies: Default::default(),
        };
        let canonical_resources = Mutex::new(std::collections::HashMap::new());

        assert_eq!(
            workspace_resource_key_for_admission(
                &classification,
                &canonical_resources,
                true,
                false,
            ),
            None
        );
        assert_eq!(
            workspace_resource_key_for_admission(
                &classification,
                &canonical_resources,
                false,
                true,
            ),
            None
        );
        assert!(
            canonical_resources
                .lock()
                .expect("canonical resource cache")
                .is_empty()
        );
    }

    #[test]
    fn workspace_resource_resolution_is_cached_for_the_turn() {
        let root = tempfile::tempdir().expect("temporary workspace root");
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("repository marker");
        let classification = crate::tool_history::WorkspaceCallClassification {
            observes_workspace: true,
            workspace_cwd: repo.clone(),
            source_dependencies: Default::default(),
        };
        let canonical_resources = Mutex::new(std::collections::HashMap::new());

        let first =
            workspace_resource_key_for_admission(&classification, &canonical_resources, true, true)
                .expect("first canonical resource");
        std::fs::remove_dir_all(repo.join(".git")).expect("remove repository marker");
        let second =
            workspace_resource_key_for_admission(&classification, &canonical_resources, true, true)
                .expect("cached canonical resource");

        assert_eq!(second, first);
        assert_eq!(
            canonical_resources
                .lock()
                .expect("canonical resource cache")
                .len(),
            1
        );
    }

    #[test]
    fn unchanged_workspace_command_keeps_its_result_fresh_for_model_delivery() {
        let identity = crate::git_workspace::WorkspaceEvidenceIdentity {
            repository_root: Some("repo".to_string()),
            head_identity: Some("head".to_string()),
            index_identity: Some("index".to_string()),
            worktree_identity: Some("worktree".to_string()),
        };
        let baseline = WorkspaceEvidenceBaseline {
            revision: Some(identity.clone()),
            cache_hit: false,
            timed_out_git_dependencies: Vec::new(),
            source_dependencies: Default::default(),
            source_path_observations: Vec::new(),
        };

        let (unchanged_revision, unchanged_current) =
            finish_workspace_evidence_capture(&baseline, false);
        assert_eq!(unchanged_revision, Some(identity.clone()));
        assert!(unchanged_current);

        let (mutated_revision, mutated_current) =
            finish_workspace_evidence_capture(&baseline, true);
        assert_eq!(mutated_revision, Some(identity));
        assert!(!mutated_current);

        let non_git_baseline = WorkspaceEvidenceBaseline {
            revision: None,
            cache_hit: false,
            timed_out_git_dependencies: Vec::new(),
            source_dependencies: Default::default(),
            source_path_observations: Vec::new(),
        };
        let (non_git_revision, non_git_current) =
            finish_workspace_evidence_capture(&non_git_baseline, false);
        assert_eq!(non_git_revision, None);
        assert!(
            non_git_current,
            "an unchanged non-Git workspace has an authoritative empty identity"
        );

        let (mutated_non_git_revision, mutated_non_git_current) =
            finish_workspace_evidence_capture(&non_git_baseline, true);
        assert_eq!(mutated_non_git_revision, None);
        assert!(!mutated_non_git_current);
    }

    #[test]
    fn tool_search_failure_response_is_incomplete() {
        let call = ToolCall {
            tool_name: codex_tools::ToolName::plain("tool_search"),
            call_id: "search-failed".to_string(),
            payload: ToolPayload::ToolSearch {
                arguments: codex_protocol::models::SearchToolCallParams {
                    query: "calendar".to_string(),
                    limit: None,
                },
            },
        };

        assert_eq!(
            ToolCallRuntime::failure_response(
                call,
                FunctionCallError::RespondToModel("failed".to_string()),
            ),
            ResponseInputItem::ToolSearchOutput {
                call_id: "search-failed".to_string(),
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            }
        );
    }

    #[test]
    fn tool_call_timing_guard_correlates_code_mode_source() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let call = ToolCall {
                tool_name: codex_tools::ToolName::plain("test_tool"),
                call_id: "call-1".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            };
            let direct_timing = Arc::new(ToolDispatchTiming::new(
                TokioInstant::now(),
                /*eager*/ false,
            ));
            direct_timing.mark_first_poll();
            let direct_guard = ToolCallTimingGuard::capture(
                direct_timing,
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::Direct,
            );
            assert!(
                direct_guard.is_some(),
                "direct tool calls should create a timing guard"
            );
            drop(direct_guard);

            let code_mode_timing = Arc::new(ToolDispatchTiming::new(
                TokioInstant::now(),
                /*eager*/ false,
            ));
            code_mode_timing.mark_first_poll();
            let code_mode_guard = ToolCallTimingGuard::capture(
                code_mode_timing,
                &"conversation-id",
                "turn-id",
                &call,
                &ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    parent_call_id: Some("outer-call".to_string()),
                    runtime_tool_call_id: "runtime-call-1".to_string(),
                },
            );
            let code_mode_guard = code_mode_guard
                .expect("nested code-mode calls should expose their parent lifecycle");
            assert_eq!(code_mode_guard.tool_source, "code_mode");
            assert_eq!(code_mode_guard.parent_cell_id, "cell-1");
            assert_eq!(code_mode_guard.runtime_tool_call_id, "runtime-call-1");
        });
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_admission_logs_dispatch_only_timing() -> anyhow::Result<()>
    {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let execution_gate = Arc::clone(&runtime.parallel_execution);
        let execution_gate_guard = execution_gate
            .try_write_owned()
            .expect("execution gate should be available before dispatch starts");

        let buffer: &'static std::sync::Mutex<Vec<u8>> =
            Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };
        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for cancelled tool response")
            .expect("cancelled tool response task should join")
            .expect("cancelled tool call should produce a response");

        let logs = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )?;
        let timing_events = logs
            .lines()
            .filter(|line| line.contains("event.name=\"codex.tool_call\""))
            .collect::<Vec<_>>();
        assert_eq!(
            timing_events.len(),
            1,
            "cancelled tool call should emit exactly one timing event; logs:\n{logs}"
        );
        let timing_event = timing_events[0];
        assert!(
            timing_event.contains("execution_started=false"),
            "tool cancelled before admission should not report execution started: {timing_event}"
        );
        assert!(
            timing_event.contains("handler_duration_ms=0"),
            "tool cancelled before admission should report zero handler duration: {timing_event}"
        );
        let duration_field = |name: &str| {
            timing_event.split_whitespace().find_map(|field| {
                field
                    .strip_prefix(&format!("{name}="))
                    .and_then(|value| value.parse::<u64>().ok())
            })
        };
        let parallel_gate_wait_ms = duration_field("parallel_gate_wait_ms")
            .expect("timing event should include parallel_gate_wait_ms");
        let dispatch_duration_ms = duration_field("dispatch_duration_ms")
            .expect("compatibility timing should include dispatch_duration_ms");
        let total_duration_ms = duration_field("total_duration_ms")
            .expect("timing event should include total_duration_ms");
        assert_eq!(
            dispatch_duration_ms, parallel_gate_wait_ms,
            "legacy dispatch timing should alias parallel-gate wait: {timing_event}"
        );
        assert!(
            total_duration_ms >= parallel_gate_wait_ms
                && total_duration_ms - parallel_gate_wait_ms <= 1,
            "tool cancelled before admission should spend its polled lifetime at the gate: {timing_event}"
        );
        drop(execution_gate_guard);

        Ok(())
    }

    #[tokio::test]
    async fn nested_cancellation_before_admission_records_output_collected() -> anyhow::Result<()> {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        turn_context.turn_timing_state.mark_turn_started();
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let execution_gate = Arc::clone(&runtime.parallel_execution);
        let execution_gate_guard = execution_gate
            .try_write_owned()
            .expect("execution gate should be available before dispatch starts");
        let cancellation_token = CancellationToken::new();

        let response_task = tokio::spawn(runtime.handle_tool_call_with_source(
            ToolCall {
                tool_name,
                call_id: "nested-cancelled-call".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                parent_call_id: Some("outer-call".to_string()),
                runtime_tool_call_id: "runtime-call-1".to_string(),
            },
            cancellation_token.clone(),
        ));
        for _ in 0..100 {
            if turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancellation_token.cancel();
        tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for cancelled nested response")
            .expect("cancelled nested response task should join")
            .expect("cancelled nested tool call should return an aborted result");
        drop(execution_gate_guard);

        let timing = turn_context
            .turn_timing_state
            .complete_snapshot()
            .protocol_timing();
        let lifecycle = timing
            .tool_calls
            .iter()
            .find(|call| call.call_id == "nested-cancelled-call")
            .expect("cancelled nested lifecycle should be recorded");
        assert_eq!(
            lifecycle.source,
            codex_protocol::protocol::TurnTimingToolCallSource::CodeMode
        );
        assert_eq!(lifecycle.parent_call_id.as_deref(), Some("outer-call"));
        assert_eq!(lifecycle.parent_cell_id.as_deref(), Some("cell-1"));
        assert_eq!(
            lifecycle.runtime_tool_call_id.as_deref(),
            Some("runtime-call-1")
        );
        assert!(lifecycle.parallel_gate_admitted_at_ms.is_none());
        assert!(lifecycle.output_collected_at_ms.is_some());
        assert!(
            lifecycle.output_projection_ms.is_some(),
            "the synthesized nested abort result must record its model-visible projection"
        );

        Ok(())
    }

    #[tokio::test]
    async fn runtime_respects_non_handler_tool_execution_timing() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let interactive_tool = codex_tools::ToolName::plain("interactive_timing_tool");
        let nested_runtime_tool = codex_tools::ToolName::plain("nested_runtime_timing_tool");
        let handlers = [
            Arc::new(DeclaredTimingHandler {
                tool_name: interactive_tool.clone(),
                timing: ToolExecutionTiming::Interactive,
            }) as Arc<dyn CoreToolRuntime>,
            Arc::new(DeclaredTimingHandler {
                tool_name: nested_runtime_tool.clone(),
                timing: ToolExecutionTiming::NestedRuntime,
            }) as Arc<dyn CoreToolRuntime>,
        ];
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools(handlers),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        turn_context.turn_timing_state.mark_turn_started();

        for (index, tool_name) in [interactive_tool, nested_runtime_tool]
            .into_iter()
            .enumerate()
        {
            runtime
                .clone()
                .handle_tool_call(
                    ToolCall {
                        tool_name,
                        call_id: format!("timing-call-{index}"),
                        payload: ToolPayload::Function {
                            arguments: "{}".to_string(),
                        },
                    },
                    CancellationToken::new(),
                )
                .await
                .expect("non-handler timing tool should complete");
        }

        let profile = turn_context.turn_timing_state.complete_snapshot().profile;
        assert_eq!(
            profile.unions.tool_active_ns, 0,
            "ToolCallRuntime must not override Interactive or NestedRuntime timing ownership"
        );
    }

    struct ImmediateHandler {
        tool_name: codex_tools::ToolName,
    }

    impl ToolExecutor<ToolInvocation> for ImmediateHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Immediate test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for ImmediateHandler {}

    #[tokio::test]
    async fn non_workspace_sampled_call_reuses_precomputed_dependencies_without_mutation_tracker() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("stateless_test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let governor = crate::session::reasoning_governor::SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0);
        let runtime = ToolCallRuntime::new(session, step_context, Arc::clone(&tracker))
            .with_sampling_request_signals(governor.collector(&baselines));
        let held_tracker = Arc::clone(&tracker).lock_owned().await;
        let (release_tracker_tx, release_tracker_rx) = std::sync::mpsc::channel();
        let tracker_holder = std::thread::spawn(move || {
            let _ = release_tracker_rx.recv();
            drop(held_tracker);
        });

        tokio::time::timeout(
            Duration::from_secs(1),
            runtime.handle_tool_call(
                ToolCall {
                    tool_name,
                    call_id: "stateless-sampled-call".to_string(),
                    payload: ToolPayload::Function {
                        arguments: "{}".to_string(),
                    },
                },
                CancellationToken::new(),
            ),
        )
        .await
        .expect("non-workspace sampling must not read the mutation tracker")
        .expect("stateless test tool should succeed");

        let counters = turn_context
            .turn_timing_state
            .complete_snapshot()
            .profile
            .counters;
        assert_eq!(counters.projection_source_dependencies_reuse_count, 1);
        assert_eq!(counters.projection_source_dependencies_fallback_count, 0);

        let _ = release_tracker_tx.send(());
        tracker_holder.join().expect("tracker holder joins");
    }

    #[tokio::test]
    async fn nested_non_workspace_code_mode_result_uses_non_workspace_classification() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let tool_name = codex_tools::ToolName::plain("stateless_test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context =
            StepContext::for_test(Arc::new(turn_context)).with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            Arc::new(session),
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );

        let result = runtime
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name,
                    call_id: "nested-stateless-call".to_string(),
                    payload: ToolPayload::Function {
                        arguments: "{}".to_string(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    parent_call_id: Some("outer-call".to_string()),
                    runtime_tool_call_id: "runtime-call-1".to_string(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("stateless nested tool succeeds");

        assert!(
            result.projected_source_dependencies().is_none(),
            "a non-workspace nested call must not become fail-closed workspace evidence"
        );
    }

    #[tokio::test]
    async fn non_workspace_evidence_skips_workspace_gate() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let turn_context = Arc::new(turn_context);
        let runtime = ToolCallRuntime::new(
            Arc::new(session),
            StepContext::for_test(Arc::clone(&turn_context)),
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let held_gate = Arc::clone(&runtime.parallel_execution)
            .try_write_owned()
            .expect("workspace gate should initially be available");
        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "stateless-evidence".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
        };
        let non_workspace = crate::tool_history::WorkspaceCallClassification {
            observes_workspace: false,
            workspace_cwd: turn_context.config.cwd.clone().to_path_buf(),
            source_dependencies: Default::default(),
        };

        tokio::time::timeout(
            Duration::from_secs(1),
            runtime.register_workspace_evidence_for_response(
                &response,
                None,
                false,
                None,
                &non_workspace,
                None,
            ),
        )
        .await
        .expect("non-workspace evidence must return without waiting for the workspace gate");

        let workspace = crate::tool_history::WorkspaceCallClassification {
            observes_workspace: true,
            workspace_cwd: turn_context.config.cwd.clone().to_path_buf(),
            source_dependencies: Default::default(),
        };
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                runtime.register_workspace_evidence_for_response(
                    &response, None, false, None, &workspace, None,
                ),
            )
            .await
            .is_err(),
            "workspace evidence must retain gate synchronization"
        );

        drop(held_gate);
    }

    #[tokio::test]
    async fn workspace_evidence_releases_gate_before_durable_persistence() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("shell_command");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            Arc::clone(&session),
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let persistence_pause = crate::tool_history::pause_next_tool_history_persistence_for_test(
            &session.thread_id.to_string(),
        );

        let first_call = tokio::spawn(
            runtime.clone().handle_tool_call_with_source(
                ToolCall {
                    tool_name,
                    call_id: "blocked-workspace-evidence-persistence".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "command": "rg needle .",
                            "workdir": turn_context.config.cwd,
                        })
                        .to_string(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "blocked-persistence-cell".to_string(),
                    parent_call_id: Some("outer-blocked-persistence".to_string()),
                    runtime_tool_call_id: "blocked-persistence-runtime-call".to_string(),
                },
                CancellationToken::new(),
            ),
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            persistence_pause.wait_until_reached(),
        )
        .await
        .expect("workspace evidence persistence should reach the durability boundary");

        // A second completed workspace read must not retain its read gate while
        // it queues behind the first call's durable-history I/O permit.
        let follower_observation =
            crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
                None,
                &ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: "queued-workspace-evidence".to_string(),
                    output: FunctionCallOutputPayload::from_text("queued".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                },
                Default::default(),
            )
            .expect("workspace evidence observation");
        let follower_gate = Arc::clone(&runtime.parallel_execution);
        let follower_session = Arc::clone(&session);
        let follower_codex_home = turn_context.config.codex_home.clone();
        let (follower_admitted_tx, follower_admitted_rx) = oneshot::channel();
        let follower = tokio::spawn(async move {
            let read_guard = follower_gate.read_owned().await;
            let _ = follower_admitted_tx.send(());
            follower_session
                .register_workspace_evidence(
                    follower_codex_home.as_path(),
                    follower_observation,
                    read_guard,
                )
                .await;
        });
        follower_admitted_rx
            .await
            .expect("queued workspace evidence should acquire a read gate");

        let later_workspace_gate = tokio::time::timeout(
            Duration::from_secs(1),
            Arc::clone(&runtime.parallel_execution).write_owned(),
        )
        .await
        .expect("blocked durability must not block later workspace gate admission");
        drop(later_workspace_gate);

        persistence_pause.release();
        first_call
            .await
            .expect("first workspace call task should join")
            .expect("first workspace call should succeed");
        follower
            .await
            .expect("queued workspace evidence task should join");
    }

    #[tokio::test]
    async fn nested_workspace_result_relay_does_not_wait_for_durable_persistence() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let persistence_pause = crate::tool_history::pause_next_tool_history_persistence_for_test(
            &session.thread_id.to_string(),
        );
        let observation = crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
            None,
            &ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "nonblocking-nested-relay".to_string(),
                output: FunctionCallOutputPayload::from_text("ready".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            Default::default(),
        )
        .expect("workspace evidence observation");

        tokio::time::timeout(
            Duration::from_secs(1),
            session.register_workspace_evidence(
                turn_context.config.codex_home.as_path(),
                observation,
                (),
            ),
        )
        .await
        .expect("nested result relay must not wait for ledger durability");
        tokio::time::timeout(
            Duration::from_secs(10),
            persistence_pause.wait_until_reached(),
        )
        .await
        .expect("queued durability should start after nested relay returns");

        persistence_pause.release();
        session.flush_tool_history_persistence().await;
    }

    struct BlockingHandler {
        tool_name: codex_tools::ToolName,
        started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        release: Arc<Notify>,
    }

    impl ToolExecutor<ToolInvocation> for BlockingHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Blocking test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async move {
                if let Some(started) = self
                    .started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = started.send(());
                }
                self.release.notified().await;
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for BlockingHandler {}

    #[tokio::test]
    async fn kd4_latency_exec_does_not_hold_workspace_gate_around_nested_runtime() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let exec_name = codex_tools::ToolName::plain(crate::tools::code_mode::PUBLIC_TOOL_NAME);
        let mutation_name = codex_tools::ToolName::plain("shell_command");
        let (exec_started_tx, exec_started_rx) = oneshot::channel();
        let release_exec = Arc::new(Notify::new());
        let handlers = [
            Arc::new(BlockingHandler {
                tool_name: exec_name.clone(),
                started: std::sync::Mutex::new(Some(exec_started_tx)),
                release: Arc::clone(&release_exec),
            }) as Arc<dyn CoreToolRuntime>,
            Arc::new(ImmediateHandler {
                tool_name: mutation_name.clone(),
            }) as Arc<dyn CoreToolRuntime>,
        ];
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools(handlers),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );

        let exec_task = tokio::spawn(runtime.clone().handle_tool_call(
            ToolCall {
                tool_name: exec_name,
                call_id: "exec-wrapper".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            CancellationToken::new(),
        ));
        exec_started_rx
            .await
            .expect("exec orchestration handler should start");

        tokio::time::timeout(
            Duration::from_secs(10),
            runtime.handle_tool_call(
                ToolCall {
                    tool_name: mutation_name,
                    call_id: "nested-mutation".to_string(),
                    payload: ToolPayload::Function {
                        arguments: "{}".to_string(),
                    },
                },
                CancellationToken::new(),
            ),
        )
        .await
        .expect("nested workspace call should not wait for the outer exec wrapper")
        .expect("nested workspace call should succeed");

        release_exec.notify_one();
        exec_task
            .await
            .expect("exec wrapper task should join")
            .expect("exec wrapper should succeed");
    }

    #[tokio::test]
    async fn parallel_gate_wait_is_separate_from_handler_and_released_before_relay()
    -> anyhow::Result<()> {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        turn_context.turn_timing_state.mark_turn_started();
        let tool_name = codex_tools::ToolName::plain("serial_lifecycle_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let timing = runtime.create_tool_dispatch_timing(TokioInstant::now(), false);
        let gate = Arc::clone(&runtime.parallel_execution);
        let held_gate = Arc::clone(&gate)
            .try_write_owned()
            .expect("parallel gate initially available");
        let task = tokio::spawn(runtime.clone().handle_tool_call_with_trace(
            ToolCall {
                tool_name,
                call_id: "gate-lifecycle-call".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            CancellationToken::new(),
            Arc::clone(&timing),
        ));
        for _ in 0..100 {
            if turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count,
            1
        );
        drop(held_gate);
        task.await.expect("tool task joins")?;

        let snapshot = timing.snapshot(TokioInstant::now());
        assert!(snapshot.parallel_gate_wait_ms.is_some());
        assert!(snapshot.handler_duration_ms.is_some());
        assert_eq!(
            turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count,
            0
        );
        assert!(
            Arc::clone(&gate).try_write_owned().is_ok(),
            "handler gate must be released before relay enqueue"
        );
        assert!(timing.mark_relay_enqueue());
        assert!(Arc::clone(&gate).try_write_owned().is_ok());

        Ok(())
    }

    async fn wait_for_workspace_evidence_capture_count(
        cache: &crate::git_workspace::GitWorkspaceCache,
        expected: u64,
    ) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while cache.workspace_evidence_capture_count() < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace evidence capture should start");
    }

    #[tokio::test]
    async fn nested_workspace_evidence_prefetches_while_gate_is_held() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("shell_command");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context =
            StepContext::for_test(Arc::clone(&turn_context)).with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker);
        let repo = tempfile::tempdir().expect("temporary repository cwd");
        let init_status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .status()
            .expect("launch git init");
        assert!(init_status.success(), "initialize temporary repository");
        let captures_before = session
            .services
            .git_workspace
            .workspace_evidence_capture_count();
        let held_gate = Arc::clone(&runtime.parallel_execution)
            .try_write_owned()
            .expect("workspace gate should initially be available");

        let call_task = tokio::spawn(
            runtime.handle_tool_call_with_source(
                ToolCall {
                    tool_name,
                    call_id: "prefetch-before-admission".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "command": "rg needle .",
                            "workdir": repo.path(),
                        })
                        .to_string(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "prefetch-cell".to_string(),
                    parent_call_id: Some("outer-prefetch".to_string()),
                    runtime_tool_call_id: "prefetch-runtime-call".to_string(),
                },
                CancellationToken::new(),
            ),
        );
        wait_for_workspace_evidence_capture_count(
            session.services.git_workspace.as_ref(),
            captures_before.saturating_add(1),
        )
        .await;
        assert!(
            !call_task.is_finished(),
            "prefetch must not bypass workspace gate admission"
        );

        drop(held_gate);
        call_task
            .await
            .expect("prefetched workspace call task should join")
            .expect("prefetched workspace call should succeed");
    }

    #[tokio::test]
    async fn nested_workspace_evidence_refreshes_after_mutation_during_gate_wait() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("shell_command");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context =
            StepContext::for_test(Arc::clone(&turn_context)).with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime =
            ToolCallRuntime::new(Arc::clone(&session), step_context, Arc::clone(&tracker));
        let repo = tempfile::tempdir().expect("temporary repository cwd");
        let init_status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .status()
            .expect("launch git init");
        assert!(init_status.success(), "initialize temporary repository");
        let captures_before = session
            .services
            .git_workspace
            .workspace_evidence_capture_count();
        let held_gate = Arc::clone(&runtime.parallel_execution)
            .try_write_owned()
            .expect("workspace gate should initially be available");

        let call_task = tokio::spawn(
            runtime.handle_tool_call_with_source(
                ToolCall {
                    tool_name,
                    call_id: "refresh-after-gate-mutation".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "command": "rg needle .",
                            "workdir": repo.path(),
                        })
                        .to_string(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "refresh-cell".to_string(),
                    parent_call_id: Some("outer-refresh".to_string()),
                    runtime_tool_call_id: "refresh-runtime-call".to_string(),
                },
                CancellationToken::new(),
            ),
        );
        wait_for_workspace_evidence_capture_count(
            session.services.git_workspace.as_ref(),
            captures_before.saturating_add(1),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while session
                .services
                .git_workspace
                .latest_workspace_evidence_identity(repo.path())
                .is_none()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("prefetched workspace identity should finish before admission");
        tracker.lock().await.record_unknown_mutation();

        drop(held_gate);
        call_task
            .await
            .expect("refreshed workspace call task should join")
            .expect("refreshed workspace call should succeed");
        assert_eq!(
            session
                .services
                .git_workspace
                .workspace_evidence_capture_count(),
            captures_before.saturating_add(2),
            "an intervening mutation must force a fresh post-admission capture"
        );
    }

    #[tokio::test]
    async fn workspace_evidence_baseline_attributes_fresh_then_cached_capture() {
        let repo = tempfile::tempdir().expect("temporary repository cwd");
        let init_status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .status()
            .expect("launch git init");
        assert!(init_status.success(), "initialize temporary repository");
        let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();

        let fresh = capture_workspace_evidence_baseline(
            cache.as_ref(),
            repo.path(),
            Default::default(),
            true,
        )
        .await;
        assert!(!fresh.cache_hit);
        assert!(fresh.timed_out_git_dependencies.is_empty());
        assert!(fresh.revision.is_some());

        let cached = capture_workspace_evidence_baseline(
            cache.as_ref(),
            repo.path(),
            Default::default(),
            true,
        )
        .await;
        assert!(cached.cache_hit);
        assert!(cached.timed_out_git_dependencies.is_empty());
        assert_eq!(cached.revision, fresh.revision);
    }

    #[tokio::test]
    async fn workspace_observers_reuse_sampling_identity_without_git_round_trips() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("shell_command");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker);
        let repo = tempfile::tempdir().expect("temporary repository cwd");
        let init_status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(repo.path())
            .status()
            .expect("launch git init");
        assert!(init_status.success(), "initialize temporary repository");
        session
            .services
            .git_workspace
            .workspace_evidence_identity(repo.path())
            .await
            .expect("seed sampling workspace identity");
        let captures_before = session
            .services
            .git_workspace
            .workspace_evidence_capture_count();

        runtime
            .clone()
            .handle_tool_call(
                ToolCall {
                    tool_name,
                    call_id: "direct-workspace-read".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "command": "rg needle .",
                            "workdir": repo.path(),
                        })
                        .to_string(),
                    },
                },
                CancellationToken::new(),
            )
            .await
            .expect("direct workspace observer should complete");
        runtime
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name: codex_tools::ToolName::plain("shell_command"),
                    call_id: "nested-workspace-read".to_string(),
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "command": "rg needle .",
                            "workdir": repo.path(),
                        })
                        .to_string(),
                    },
                },
                ToolCallSource::CodeMode {
                    cell_id: "cell-1".to_string(),
                    parent_call_id: Some("outer-call".to_string()),
                    runtime_tool_call_id: "runtime-call-1".to_string(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("nested workspace observer should complete");

        assert_eq!(
            session
                .services
                .git_workspace
                .workspace_evidence_capture_count()
                .saturating_sub(captures_before),
            0,
            "direct and nested read-only children must reuse the sampling identity"
        );
    }

    struct ParallelImmediateHandler {
        tool_name: codex_tools::ToolName,
    }

    impl ToolExecutor<ToolInvocation> for ParallelImmediateHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Parallel immediate test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn supports_parallel_tool_calls(&self) -> bool {
            true
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for ParallelImmediateHandler {}

    #[tokio::test]
    async fn eager_read_eligibility_uses_classification_registration_and_prefix_order() {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let parallel_read = Arc::new(ParallelImmediateHandler {
            tool_name: codex_tools::ToolName::plain("read_tool_output"),
        }) as Arc<dyn CoreToolRuntime>;
        let parallel_shell = Arc::new(ParallelImmediateHandler {
            tool_name: codex_tools::ToolName::plain("shell_command"),
        }) as Arc<dyn CoreToolRuntime>;
        let serial_exec = Arc::new(ImmediateHandler {
            tool_name: codex_tools::ToolName::plain(codex_code_mode::PUBLIC_TOOL_NAME),
        }) as Arc<dyn CoreToolRuntime>;
        let serial_read = Arc::new(ImmediateHandler {
            tool_name: codex_tools::ToolName::plain("view_image"),
        }) as Arc<dyn CoreToolRuntime>;
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([parallel_read, parallel_shell, serial_exec, serial_read]),
            Vec::new(),
        ));
        let step_context =
            StepContext::for_test(Arc::new(turn_context)).with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let call_with_arguments = |name: &str, arguments: &str| ToolCall {
            tool_name: codex_tools::ToolName::plain(name),
            call_id: format!("{name}-call"),
            payload: ToolPayload::Function {
                arguments: arguments.to_string(),
            },
        };
        let call = |name: &str| call_with_arguments(name, "{}");

        let mut exec_prefix = true;
        let exec_call = ToolCall {
            tool_name: codex_tools::ToolName::plain(codex_code_mode::PUBLIC_TOOL_NAME),
            call_id: "exec-call".to_string(),
            payload: ToolPayload::Custom {
                input: "text('ok')".to_string(),
            },
        };
        assert!(runtime.take_eager_read_eligibility(&exec_call, &mut exec_prefix));
        assert!(
            !exec_prefix,
            "the serial carrier must close the eager prefix"
        );

        let mut eager_prefix_open = true;
        assert!(
            runtime.take_eager_read_eligibility(&call("read_tool_output"), &mut eager_prefix_open)
        );
        assert!(eager_prefix_open);

        let read_oriented_shell =
            call_with_arguments("shell_command", r#"{"program":"rg","args":["--files"]}"#);
        // Command-name heuristics do not prove that launching a process is
        // side-effect-free without an enforced read-only sandbox.
        assert!(!runtime.take_eager_read_eligibility(&read_oriented_shell, &mut eager_prefix_open));
        assert!(!eager_prefix_open);

        // Once a deferred call appears, a later otherwise-eligible read cannot overtake it.
        assert!(
            !runtime.take_eager_read_eligibility(&call("read_tool_output"), &mut eager_prefix_open)
        );

        let mut shell_prefix = true;
        // Parallel capability alone cannot admit an unclassified shell payload.
        assert!(!runtime.take_eager_read_eligibility(&call("shell_command"), &mut shell_prefix));
        assert!(!shell_prefix);

        let mut serial_prefix = true;
        // ReadSearch classification alone cannot admit a serial registered handler.
        assert!(!runtime.take_eager_read_eligibility(&call("view_image"), &mut serial_prefix));
        assert!(!serial_prefix);

        let mut unknown_prefix = true;
        assert!(
            !runtime.take_eager_read_eligibility(&call("unregistered_tool"), &mut unknown_prefix)
        );
        assert!(!unknown_prefix);
    }

    struct DeclaredTimingHandler {
        tool_name: codex_tools::ToolName,
        timing: ToolExecutionTiming,
    }

    impl ToolExecutor<ToolInvocation> for DeclaredTimingHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Declared timing test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(
                    Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                        as Box<dyn crate::tools::context::ToolOutput>,
                )
            })
        }
    }

    impl CoreToolRuntime for DeclaredTimingHandler {
        fn tool_execution_timing(&self) -> ToolExecutionTiming {
            self.timing
        }
    }

    struct CancellationCleanupHandler {
        tool_name: codex_tools::ToolName,
        started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        invocation_token: std::sync::Mutex<Option<oneshot::Sender<CancellationToken>>>,
        cleanup_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        allow_cleanup: Arc<Notify>,
    }

    impl ToolExecutor<ToolInvocation> for CancellationCleanupHandler {
        fn tool_name(&self) -> codex_tools::ToolName {
            self.tool_name.clone()
        }

        fn spec(&self) -> codex_tools::ToolSpec {
            codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
                name: self.tool_name.name.clone(),
                description: "Cancellation cleanup test tool.".to_string(),
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::default(),
                output_schema: None,
            })
        }

        fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
            Box::pin(self.handle_call(invocation))
        }
    }

    impl CancellationCleanupHandler {
        async fn handle_call(
            &self,
            invocation: ToolInvocation,
        ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
            let started = self
                .started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(started) = started {
                let _ = started.send(());
            }
            let invocation_token = self
                .invocation_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(invocation_token) = invocation_token {
                let _ = invocation_token.send(invocation.cancellation_token.clone());
            }
            invocation.cancellation_token.cancelled().await;
            let cleanup_started = self
                .cleanup_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(cleanup_started) = cleanup_started {
                let _ = cleanup_started.send(());
            }
            self.allow_cleanup.notified().await;
            Ok(Box::new(FunctionToolOutput::from_text(
                "cleanup complete".to_string(),
                Some(false),
            )) as Box<dyn crate::tools::context::ToolOutput>)
        }
    }

    impl CoreToolRuntime for CancellationCleanupHandler {
        fn waits_for_runtime_cancellation(&self) -> bool {
            true
        }
    }

    struct FinishRecorder {
        records: Arc<std::sync::Mutex<Vec<ToolCallOutcome>>>,
    }

    impl codex_extension_api::ToolLifecycleContributor for FinishRecorder {
        fn on_tool_finish<'a>(
            &'a self,
            input: codex_extension_api::ToolFinishInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            let records = Arc::clone(&self.records);
            let outcome = input.outcome;
            Box::pin(async move {
                records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
            })
        }
    }

    #[tokio::test]
    async fn kd4_latency_cancellation_before_gate_admission_skips_runtime_cleanup_grace()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(FinishRecorder {
            records: Arc::clone(&records),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("cleanup_tool_waiting_for_gate");
        let (started_tx, mut started_rx) = oneshot::channel();
        let (cleanup_started_tx, mut cleanup_started_rx) = oneshot::channel();
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            invocation_token: std::sync::Mutex::new(None),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::new(Notify::new()),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let held_gate = Arc::clone(&runtime.parallel_execution)
            .try_write_owned()
            .expect("workspace gate should initially be available");
        let cancellation_token = CancellationToken::new();
        let response_task = tokio::spawn(runtime.handle_tool_call(
            ToolCall {
                tool_name,
                call_id: "cancel-before-admission".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            cancellation_token.clone(),
        ));

        for _ in 0..100 {
            if turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count,
            1,
            "cleanup-owning handler should be waiting for gate admission"
        );

        cancellation_token.cancel();
        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("pre-admission cancellation must not wait for runtime cleanup grace")
            .expect("cancelled tool response task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled tool output should be text");
        };
        assert!(text.contains("aborted by user"));
        assert!(
            started_rx.try_recv().is_err(),
            "handler must not enter after cancellation wins the admission race"
        );
        assert!(
            cleanup_started_rx.try_recv().is_err(),
            "a handler that never started has no runtime cleanup to await"
        );
        assert_eq!(
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[ToolCallOutcome::Aborted],
            "pre-admission cancellation emits exactly one terminal lifecycle result"
        );
        assert_eq!(
            turn_context
                .turn_timing_state
                .lifecycle_context()
                .parallel_gate_waiter_count,
            0
        );
        drop(held_gate);

        Ok(())
    }

    struct BlockingFinishContributor {
        records: Arc<std::sync::Mutex<Vec<ToolCallOutcome>>>,
        finish_started: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        allow_finish: Arc<Notify>,
    }

    impl codex_extension_api::ToolLifecycleContributor for BlockingFinishContributor {
        fn on_tool_finish<'a>(
            &'a self,
            input: codex_extension_api::ToolFinishInput<'a>,
        ) -> codex_extension_api::ToolLifecycleFuture<'a> {
            let records = Arc::clone(&self.records);
            let allow_finish = Arc::clone(&self.allow_finish);
            let finish_started = self
                .finish_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let outcome = input.outcome;
            Box::pin(async move {
                if let Some(finish_started) = finish_started {
                    let _ = finish_started.send(());
                }
                allow_finish.notified().await;
                records
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(outcome);
            })
        }
    }

    #[tokio::test]
    async fn cancellation_after_handler_finishes_preserves_completed_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (finish_started_tx, finish_started_rx) = oneshot::channel();
        let allow_finish = Arc::new(Notify::new());
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(BlockingFinishContributor {
            records: Arc::clone(&records),
            finish_started: std::sync::Mutex::new(Some(finish_started_tx)),
            allow_finish: Arc::clone(&allow_finish),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("test_tool");
        let handler = Arc::new(ImmediateHandler {
            tool_name: tool_name.clone(),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        tokio::time::timeout(Duration::from_secs(1), finish_started_rx)
            .await
            .expect("timed out waiting for lifecycle notification to start")
            .expect("lifecycle notification should start");
        cancellation_token.cancel();
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_finish.notify_waiters();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let expected_response = ResponseInputItem::FunctionCallOutput {
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("ok".to_string()),
                success: Some(true),
            },
        };
        assert_eq!(expected_response, response);

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Completed { success: true }], actual);

        Ok(())
    }

    #[tokio::test]
    async fn cancellation_waiting_for_runtime_cleanup_emits_only_aborted_lifecycle()
    -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(FinishRecorder {
            records: Arc::clone(&records),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("cleanup_tool");
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let allow_cleanup = Arc::new(Notify::new());
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            invocation_token: std::sync::Mutex::new(None),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::clone(&allow_cleanup),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "call-1".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        started_rx.await.expect("handler should start");
        cancellation_token.cancel();
        cleanup_started_rx
            .await
            .expect("handler should start cleanup");
        tokio::time::sleep(Duration::from_millis(10)).await;
        allow_cleanup.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for tool response")
            .expect("tool response task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled tool output should be text");
        };
        assert!(text.contains("aborted by user"));

        let actual = records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        assert_eq!(vec![ToolCallOutcome::Aborted], actual);
        let timing = turn_context
            .turn_timing_state
            .complete_snapshot()
            .protocol_timing();
        let call_timing = timing
            .tool_calls
            .iter()
            .find(|timing| timing.call_id == "call-1")
            .expect("cancelled call timing");
        assert!(call_timing.handler_entry_at_ms.is_some());
        assert!(call_timing.handler_exit_at_ms.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn nested_cancellation_owns_terminal_before_runtime_cleanup() -> anyhow::Result<()> {
        let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder =
            codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
        builder.tool_lifecycle_contributor(Arc::new(FinishRecorder {
            records: Arc::clone(&records),
        }));
        session.services.extensions = Arc::new(builder.build());

        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        turn_context.turn_timing_state.mark_turn_started();
        let tool_name = codex_tools::ToolName::plain("nested_cleanup_tool");
        let (started_tx, _started_rx) = oneshot::channel();
        let (invocation_token_tx, invocation_token_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let allow_cleanup = Arc::new(Notify::new());
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            invocation_token: std::sync::Mutex::new(Some(invocation_token_tx)),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::clone(&allow_cleanup),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let runtime = ToolCallRuntime::new(
            session,
            step_context,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        );
        let cancellation_token = CancellationToken::new();
        let mut response_future = Box::pin(runtime.handle_tool_call_with_source(
            ToolCall {
                tool_name,
                call_id: "nested-cancel-cleanup".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            ToolCallSource::CodeMode {
                cell_id: "cell-1".to_string(),
                parent_call_id: Some("outer-call".to_string()),
                runtime_tool_call_id: "runtime-call-1".to_string(),
            },
            cancellation_token.clone(),
        ));

        assert!(
            futures::poll!(response_future.as_mut()).is_pending(),
            "nested handler should remain in flight"
        );
        let handler_token = tokio::time::timeout(Duration::from_secs(1), invocation_token_rx)
            .await
            .expect("handler should expose its runtime token")
            .expect("runtime token sender should remain alive");
        cancellation_token.cancel();
        assert!(
            !handler_token.is_cancelled(),
            "external cancellation must not reach the handler before the outer boundary owns the terminal result"
        );

        let response_task = tokio::spawn(response_future);
        cleanup_started_rx
            .await
            .expect("owned abort should start cooperative runtime cleanup");
        assert!(handler_token.is_cancelled());
        allow_cleanup.notify_one();

        let response = tokio::time::timeout(Duration::from_secs(1), response_task)
            .await
            .expect("timed out waiting for nested abort response")
            .expect("nested abort task should join")?;
        let response = response.response();
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled nested tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled nested tool output should be text");
        };
        assert!(text.contains("aborted by user"));
        assert_eq!(
            records
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[ToolCallOutcome::Aborted]
        );

        let timing = turn_context
            .turn_timing_state
            .complete_snapshot()
            .protocol_timing();
        let call_timing = timing
            .tool_calls
            .iter()
            .find(|timing| timing.call_id == "nested-cancel-cleanup")
            .expect("nested cancellation timing");
        assert_eq!(
            call_timing.source,
            codex_protocol::protocol::TurnTimingToolCallSource::CodeMode
        );
        assert_eq!(call_timing.parent_call_id.as_deref(), Some("outer-call"));
        assert!(call_timing.handler_entry_at_ms.is_some());
        assert!(call_timing.handler_exit_at_ms.is_some());
        assert!(call_timing.output_projection_ms.is_some());
        let closure = turn_context.turn_timing_state.tool_closure_snapshot();
        assert_eq!(closure.accepted_count, 1);
        assert_eq!(closure.timing_paired_count, 1);
        assert_eq!(closure.terminal_count, 1);
        assert_eq!(
            closure.persisted_count, 0,
            "this runtime unit stops below the owning direct-call persistence boundary"
        );
        assert!(closure.orphan_calls.is_empty());

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_aborts_non_cooperative_runtime_cleanup_after_bounded_grace()
    -> anyhow::Result<()> {
        let (session, turn_context) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let turn_context = Arc::new(turn_context);
        let tool_name = codex_tools::ToolName::plain("non_cooperative_cleanup_tool");
        let (started_tx, started_rx) = oneshot::channel();
        let (cleanup_started_tx, cleanup_started_rx) = oneshot::channel();
        let handler = Arc::new(CancellationCleanupHandler {
            tool_name: tool_name.clone(),
            started: std::sync::Mutex::new(Some(started_tx)),
            invocation_token: std::sync::Mutex::new(None),
            cleanup_started: std::sync::Mutex::new(Some(cleanup_started_tx)),
            allow_cleanup: Arc::new(Notify::new()),
        }) as Arc<dyn CoreToolRuntime>;
        let step_context = StepContext::for_test(Arc::clone(&turn_context));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::from_tools([handler]),
            Vec::new(),
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
        let runtime = ToolCallRuntime::new(session, step_context, tracker);
        let cancellation_token = CancellationToken::new();
        let call = ToolCall {
            tool_name,
            call_id: "non-cooperative-call".to_string(),
            payload: ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        };

        let response_task =
            tokio::spawn(runtime.handle_tool_call(call, cancellation_token.clone()));
        started_rx.await.expect("handler should start");
        cancellation_token.cancel();
        cleanup_started_rx
            .await
            .expect("handler should enter non-cooperative cleanup");
        tokio::time::advance(TOOL_RUNTIME_CANCELLATION_GRACE).await;
        tokio::task::yield_now().await;

        let response = response_task
            .await
            .expect("tool response task should join")?;
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            anyhow::bail!("cancelled tool should return function output");
        };
        let FunctionCallOutputBody::Text(text) = output.body else {
            anyhow::bail!("cancelled tool output should be text");
        };
        assert!(text.contains("aborted by user"));

        Ok(())
    }

    #[test]
    fn suppressed_source_pass_response_is_explicitly_unsuccessful() {
        let response = suppressed_function_response(
            "suppressed-call",
            serde_json::json!({
                "disposition": "suppressed",
                "reason": "policy skipped execution"
            })
            .to_string(),
        );
        let ResponseInputItem::FunctionCallOutput { call_id, output } = response else {
            panic!("suppressed source pass must return a function output");
        };
        assert_eq!(call_id, "suppressed-call");
        assert_eq!(output.success, Some(false));
        let FunctionCallOutputBody::Text(text) = output.body else {
            panic!("suppression receipt must be textual");
        };
        assert!(text.contains("\"disposition\":\"suppressed\""));
    }
}
