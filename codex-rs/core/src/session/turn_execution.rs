use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_config::schema::canonicalize as canonicalize_json;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_protocol::protocol::TurnTimingGenerationDisposition;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingProgressKind;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::ToolPayload;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::tool_history::SourceDependencyV1;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::turn_timing::TurnTimingState;
use crate::validation_admission::ValidationClassification;
use crate::validation_admission::ValidationOperation;
use crate::validation_admission::classify_validation;

const TURN_EFFICIENCY_TOOL_CALL_THRESHOLD: usize = 8;
const TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL: u64 = 500;
const DISTINCT_FAILURE_RECOVERY_ADVISORY_THRESHOLD: u32 = 2;
const SUCCESSFUL_REPLAY_GATE_LIMIT: usize = 32;
const SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT: usize = 64 * 1024;
const SOFT_CONVERGENCE_AFTER: std::time::Duration = std::time::Duration::from_secs(120);
// Elapsed time alone does not distinguish a long investigation from a stall:
// the intervention fired two minutes into recorded implementation turns that
// were still reading new files. Require that this many completed generations
// in a row added no new evidence, mutation, or plan change first.
pub(crate) const SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS: u32 = 3;
const SOFT_CONVERGENCE_DIRECTIVE: &str = "Soft convergence intervention: Use existing evidence and stop optional exploration. Continue only to satisfy an unresolved requirement, complete required implementation or validation, or resolve a correctness-relevant uncertainty. Preserve the requested scope. This is not a completion test or a hard deadline: do not cancel running work, truncate the answer, claim unfinished work is complete, or abandon obtainable required evidence. Report limitations only when evidence is genuinely unavailable or work is blocked. This instruction takes effect on this already-needed continuation; it does not interrupt an in-flight request.";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ContinuationDisposition {
    #[default]
    ModelRequired,
    TerminalCompletionRequired,
    SurfaceExistingResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenerationRequestDisposition {
    pub(crate) purpose: Option<TurnTimingGenerationPurpose>,
    pub(crate) sampling: SamplingGenerationDisposition,
    pub(crate) relevant_state_fingerprint: String,
    pub(crate) failure_fingerprint: Option<String>,
    pub(crate) terminal_completion_only: bool,
}

impl GenerationRequestDisposition {
    pub(crate) fn require_terminal_completion(mut self) -> Self {
        self.purpose = Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning);
        self.sampling = SamplingGenerationDisposition::DecisionBearing;
        self.terminal_completion_only = true;
        self
    }

    pub(crate) fn timing_disposition(&self) -> TurnTimingGenerationDisposition {
        match &self.sampling {
            SamplingGenerationDisposition::DecisionBearing => {
                TurnTimingGenerationDisposition::DecisionBearing
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SamplingGenerationDisposition {
    DecisionBearing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SamplingRequestBaselines {
    mutation_revision: u64,
    plan_revision: u64,
    input_revision: u64,
    tool_exposure_revision: u64,
}

impl SamplingRequestBaselines {
    fn revision_key(&self) -> String {
        format!(
            "mutation={};plan={};input={};tool_exposure={}",
            self.mutation_revision,
            self.plan_revision,
            self.input_revision,
            self.tool_exposure_revision,
        )
    }

    pub(crate) fn relevant_state_fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(self.revision_key().as_bytes()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SamplingRequestSettledState {
    pub(crate) mutation_revision: u64,
    pub(crate) tool_exposure_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SamplingToolOutcomeKind {
    Success,
    Yielded,
    Unknown,
    Failure,
    Blocked,
    Timeout,
    RecoverableCancellation,
    Skipped,
}

#[derive(Clone, Debug)]
struct SamplingToolOutcome {
    ordinal: u64,
    kind: SamplingToolOutcomeKind,
    skip_disposition: Option<ToolOutputSkipDisposition>,
    plan: Option<UpdatePlanArgs>,
    source_evidence: Option<Value>,
    unfinished_mutation_obligation: bool,
    failure_fingerprint: Option<String>,
    failure_is_terminal: bool,
    failure_diagnosis_reused: bool,
    canonical_artifact_required: bool,
    nested_in_code_mode: bool,
    wraps_nested_terminal: bool,
    executed_cargo_test_targets: BTreeSet<String>,
    tests_executed: bool,
}

impl SamplingToolOutcome {
    fn from_signal(
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        plan: Option<UpdatePlanArgs>,
        signal: Option<&Value>,
    ) -> Self {
        let kind = sampling_tool_outcome_kind(outcome_context.outcome, signal);
        Self {
            ordinal,
            kind,
            skip_disposition: outcome_context.skip_disposition,
            plan,
            source_evidence: sampling_source_evidence(signal),
            unfinished_mutation_obligation: sampling_unfinished_mutation_obligation(signal),
            failure_fingerprint: sampling_failure_fingerprint(signal),
            failure_is_terminal: sampling_failure_is_terminal(signal),
            failure_diagnosis_reused: false,
            canonical_artifact_required: false,
            nested_in_code_mode: false,
            wraps_nested_terminal: signal
                .and_then(|signal| signal.get("nested_ordinal"))
                .and_then(Value::as_u64)
                .is_some(),
            executed_cargo_test_targets: BTreeSet::new(),
            tests_executed: false,
        }
    }

    fn plain(ordinal: u64, kind: SamplingToolOutcomeKind, plan: Option<UpdatePlanArgs>) -> Self {
        let outcome = match kind {
            SamplingToolOutcomeKind::Success => ToolOutputOutcome::Success,
            SamplingToolOutcomeKind::Yielded => ToolOutputOutcome::Yielded,
            SamplingToolOutcomeKind::Timeout => ToolOutputOutcome::TimedOut,
            SamplingToolOutcomeKind::Skipped => ToolOutputOutcome::Skipped,
            SamplingToolOutcomeKind::Failure
            | SamplingToolOutcomeKind::Blocked
            | SamplingToolOutcomeKind::Unknown
            | SamplingToolOutcomeKind::RecoverableCancellation => ToolOutputOutcome::Failure,
        };
        let mut sampling_outcome =
            Self::from_signal(ordinal, ToolOutputOutcomeContext::new(outcome), plan, None);
        // A plain outcome has already been classified by its caller. Preserve
        // distinctions that cannot be reconstructed from the coarse protocol
        // outcome alone (blocked and recoverable cancellation both serialize
        // as generic failures).
        sampling_outcome.kind = kind;
        sampling_outcome
    }

    fn is_failure_evidence(&self) -> bool {
        outcome_reopens_failure_evidence(self.kind, self.skip_disposition)
    }
}

fn outcome_reopens_failure_evidence(
    kind: SamplingToolOutcomeKind,
    skip_disposition: Option<ToolOutputSkipDisposition>,
) -> bool {
    match kind {
        SamplingToolOutcomeKind::Success
        | SamplingToolOutcomeKind::Yielded
        | SamplingToolOutcomeKind::Unknown => false,
        SamplingToolOutcomeKind::Skipped => {
            skip_disposition == Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
        }
        SamplingToolOutcomeKind::Failure
        | SamplingToolOutcomeKind::Blocked
        | SamplingToolOutcomeKind::Timeout
        | SamplingToolOutcomeKind::RecoverableCancellation => true,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
enum AuthoritativeWaitDisposition {
    Blocked,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeWaitObservation {
    disposition: AuthoritativeWaitDisposition,
    identity: String,
    owner: String,
    state_revision: String,
    action_identity: String,
    result: AuthoritativeWaitOwnerResult,
    assignment_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthoritativeWaitOwnerResult {
    pub(crate) adapter: String,
    pub(crate) value: Value,
    pub(crate) surfaceable_message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AuthoritativeWaitResolution {
    Blocked(AuthoritativeWaitOwnerResult),
    Terminal(AuthoritativeWaitOwnerResult),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockedWaitGuard {
    pub(crate) owner: String,
    pub(crate) state_revision: String,
    pub(crate) assignment_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct BlockedWaitGate {
    action_identity: String,
    guard: BlockedWaitGuard,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SuppressedFailureGuard {
    pub(crate) failure_fingerprint: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SuccessfulReplayGuard {
    response: ResponseInputItem,
    evidence: SuccessfulReplayEvidence,
}

#[derive(Clone, Debug)]
struct SuccessfulReplayEvidence {
    mutation_revision: u64,
    workspace_revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    source_paths: Vec<crate::git_workspace::SourcePathChangeObservation>,
}

impl SuccessfulReplayGuard {
    pub(crate) fn source_path_observations(
        &self,
    ) -> Vec<crate::git_workspace::SourcePathChangeObservation> {
        self.evidence.source_paths.clone()
    }
    pub(crate) fn is_fresh(
        &self,
        mutation_revision: u64,
        cache: &crate::git_workspace::GitWorkspaceCache,
        workspace_revision: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> bool {
        self.evidence.workspace_revision.as_ref() == workspace_revision
            && workspace_revision.is_none_or(|revision| !revision.unavailable)
            && self.evidence.mutation_revision == mutation_revision
            && !self.evidence.source_paths.is_empty()
            && self
                .evidence
                .source_paths
                .iter()
                .all(|path| cache.source_path_change_observation_is_current(path))
    }

    pub(crate) fn response_for_call(&self, call_id: &str) -> Option<ResponseInputItem> {
        let mut response = self.response.clone();
        match &mut response {
            ResponseInputItem::FunctionCallOutput {
                call_id: response_call_id,
                ..
            }
            | ResponseInputItem::McpToolCallOutput {
                call_id: response_call_id,
                ..
            }
            | ResponseInputItem::CustomToolCallOutput {
                call_id: response_call_id,
                ..
            }
            | ResponseInputItem::ToolSearchOutput {
                call_id: response_call_id,
                ..
            } => *response_call_id = call_id.to_string(),
            ResponseInputItem::Message { .. } => return None,
        }
        Some(response)
    }
}

#[derive(Clone, Debug)]
struct RepeatedFailureGate {
    state_revision: String,
    action_identity: String,
    failure_fingerprint: String,
}

#[derive(Clone, Debug)]
struct SuccessfulReplayGate {
    state_revision: String,
    action_identity: String,
    response: ResponseInputItem,
    evidence: SuccessfulReplayEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuredActionClass {
    BroadSource,
    PreciseSource,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StructuredActionIdentity {
    identity: String,
    class: StructuredActionClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeterministicCycleKind {
    Empty,
    ToolFailure,
    NestedToolFailure,
    ResidualToolContinuation,
    BroadSourcePass,
    StructuredToolPass,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeterministicCycle {
    key: String,
    kind: DeterministicCycleKind,
    failure_only: bool,
    repeated_failure: Option<(String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TurnEfficiencyGuardHandle {
    settled_revision: String,
    deterministic_cycle: Option<String>,
}

struct DeterministicDispatchLedger {
    blocked_wait_gate: Option<BlockedWaitGate>,
    repeated_failure_gate: Option<RepeatedFailureGate>,
    successful_replay_gates: VecDeque<SuccessfulReplayGate>,
    timing: Arc<TurnTimingState>,
}

impl DeterministicDispatchLedger {
    fn new(timing: Arc<TurnTimingState>) -> Self {
        Self {
            blocked_wait_gate: None,
            repeated_failure_gate: None,
            successful_replay_gates: VecDeque::new(),
            timing,
        }
    }
}

#[derive(Default)]
struct SamplingRequestSignalState {
    outcomes: Vec<SamplingToolOutcome>,
    structured_actions: BTreeMap<u64, StructuredActionIdentity>,
    recovery_action_identities: BTreeMap<u64, RecoveryActionIdentity>,
    evidence_items: BTreeMap<u64, String>,
    successful_replay_responses: BTreeMap<u64, ResponseInputItem>,
    successful_replay_evidence: BTreeMap<u64, SuccessfulReplayEvidence>,
    replayed_ordinals: BTreeSet<u64>,
    validation_ordinals: BTreeSet<u64>,
    validation_proof_ordinals: BTreeSet<u64>,
    test_validation_ordinals: BTreeSet<u64>,
    validation_mutation_revision: Option<u64>,
    final_verification_ordinals: BTreeSet<u64>,
    mutation_ordinals: BTreeSet<u64>,
    suppressed_blocked_wait: bool,
    deterministic_continuation_receipts: BTreeSet<String>,
    registered_count: usize,
    wait_call_count: usize,
    process_monitor_ordinals: BTreeSet<u64>,
    saw_artifact_read: bool,
    saw_canonical_artifact_requirement: bool,
    saw_validation: bool,
    saw_mutation: bool,
    saw_coordination: bool,
    direct_wait_agent_count: usize,
    direct_code_mode_exec_count: usize,
    code_mode_nested_tool_count: usize,
    code_mode_source_dependencies: BTreeMap<String, BTreeSet<SourceDependencyV1>>,
    authoritative_wait_observations: Vec<AuthoritativeWaitObservation>,
    child_runtime_ms: u64,
    child_runtime_sample_count: usize,
}

impl SamplingRequestSignalState {
    fn accumulate_code_mode_source_dependencies(
        &mut self,
        cell_id: &str,
        source_dependencies: Option<BTreeSet<SourceDependencyV1>>,
    ) {
        let Some(source_dependencies) = source_dependencies else {
            return;
        };
        match self
            .code_mode_source_dependencies
            .entry(cell_id.to_string())
        {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(source_dependencies);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let accumulated = entry.get_mut();
                if accumulated.is_empty() || source_dependencies.is_empty() {
                    // Empty means an observation could not be scoped. Preserve
                    // that fail-closed state for the whole cell.
                    accumulated.clear();
                } else {
                    accumulated.extend(source_dependencies);
                }
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExecutedValidationSummary {
    pub(crate) count: u32,
    pub(crate) duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FreshSuccessfulValidation {
    mutation_revision: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingOwnerDrainedContinuation {
    pub(crate) preserved_content: Vec<Value>,
    pub(crate) receipt: TurnTimingDeterministicContinuationReceipt,
}

pub(crate) struct CodeModeToolResult<'a> {
    pub(crate) cell_id: &'a str,
    pub(crate) tool_name: &'a ToolName,
    pub(crate) payload: &'a ToolPayload,
    pub(crate) source_dependencies: Option<BTreeSet<SourceDependencyV1>>,
    pub(crate) outcome_context: ToolOutputOutcomeContext,
    pub(crate) signal: Option<&'a Value>,
    pub(crate) result: &'a Value,
    pub(crate) canonical_artifact_required: bool,
}

#[derive(Clone, Default)]
pub(crate) struct SamplingRequestSignalCollector {
    next_ordinal: Arc<AtomicU64>,
    state: Arc<Mutex<SamplingRequestSignalState>>,
    dispatch_ledger: Option<Arc<Mutex<DeterministicDispatchLedger>>>,
    request_state_revision: String,
    request_mutation_revision: u64,
}

pub(crate) struct SamplingToolCallRegistration {
    pub(crate) ordinal: u64,
    pub(crate) blocked_wait_guard: Option<BlockedWaitGuard>,
    pub(crate) suppressed_failure: Option<SuppressedFailureGuard>,
    pub(crate) replayed_success: Option<SuccessfulReplayGuard>,
}

impl SamplingRequestSignalCollector {
    pub(crate) fn completion_evidence_key(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let evidence = state
            .outcomes
            .iter()
            .filter(|outcome| !state.replayed_ordinals.contains(&outcome.ordinal))
            .map(|outcome| {
                format!(
                    "{:?}:{:?}:{:?}:{}:{:?}",
                    outcome.kind,
                    outcome.source_evidence,
                    outcome.failure_fingerprint,
                    outcome.tests_executed,
                    state.evidence_items.get(&outcome.ordinal)
                )
            })
            .collect::<BTreeSet<_>>();
        (!evidence.is_empty())
            .then(|| format!("{:x}", Sha256::digest(format!("{evidence:?}").as_bytes())))
    }

    #[cfg(test)]
    pub(crate) fn register_tool_call(&self) -> u64 {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count = state.registered_count.saturating_add(1);
        ordinal
    }

    pub(crate) fn register_deterministic_tool_call(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        _current_call_id: &str,
    ) -> SamplingToolCallRegistration {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let wait = is_wait_tool(tool_name);
        let live_process_poll = tool_name_matches(tool_name, "write_stdin");
        let direct_code_mode_exec = crate::tools::code_mode::is_exec_tool_name(tool_name);
        let canonical = canonical_tool_action(payload);
        let action_identity = deterministic_action_identity(tool_name, &canonical);
        let structured_action =
            structured_action_identity_from_canonical(tool_name, payload, &canonical);
        let (validation, validation_proof, test_execution) =
            validation_status_from_arguments(tool_name, &canonical.value);
        let final_verification = final_diff_status_from_arguments(tool_name, &canonical.value);
        let mutation = is_mutation_tool(tool_name);
        let replayable_action = structured_action.as_ref().is_some_and(|action| {
            matches!(
                action.class,
                StructuredActionClass::BroadSource | StructuredActionClass::PreciseSource
            )
        }) || validation_proof
            || final_verification;
        let (blocked_wait_guard, suppressed_failure, replayed_success) = self
            .dispatch_ledger
            .as_ref()
            .map(|ledger| {
                let ledger = ledger
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let blocked_wait_guard = action_identity.as_ref().and_then(|action_identity| {
                    ledger
                        .blocked_wait_gate
                        .as_ref()
                        .filter(|gate| gate.action_identity == *action_identity)
                        .map(|gate| gate.guard.clone())
                });
                let suppressed_failure = structured_action
                    .as_ref()
                    .filter(|_| terminal_failure_can_be_reused_without_dispatch(tool_name))
                    .and_then(|action| {
                        ledger
                            .repeated_failure_gate
                            .as_ref()
                            .filter(|gate| gate.state_revision == self.request_state_revision)
                            .filter(|gate| gate.action_identity == action.identity)
                            .map(|gate| SuppressedFailureGuard {
                                failure_fingerprint: gate.failure_fingerprint.clone(),
                            })
                    });
                let replayed_success = replayable_action
                    .then_some(structured_action.as_ref())
                    .flatten()
                    .and_then(|action| {
                        ledger
                            .successful_replay_gates
                            .iter()
                            .rev()
                            .find(|gate| {
                                gate.state_revision == self.request_state_revision
                                    && gate.action_identity == action.identity
                            })
                            .map(|gate| SuccessfulReplayGuard {
                                response: gate.response.clone(),
                                evidence: gate.evidence.clone(),
                            })
                    });
                (blocked_wait_guard, suppressed_failure, replayed_success)
            })
            .unwrap_or_default();

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count = state.registered_count.saturating_add(1);
        if wait {
            state.wait_call_count = state.wait_call_count.saturating_add(1);
        }
        if live_process_poll {
            state.process_monitor_ordinals.insert(ordinal);
        }
        if direct_code_mode_exec {
            state.direct_code_mode_exec_count = state.direct_code_mode_exec_count.saturating_add(1);
        }
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= is_coordination_tool(tool_name);
        if validation {
            state.validation_ordinals.insert(ordinal);
        }
        if validation_proof {
            state.validation_proof_ordinals.insert(ordinal);
        }
        if test_execution {
            state.test_validation_ordinals.insert(ordinal);
        }
        if final_verification {
            state.final_verification_ordinals.insert(ordinal);
        }
        if mutation {
            state.mutation_ordinals.insert(ordinal);
        }
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
        }
        if let Some(identity) = recovery_action_identity(tool_name, &canonical) {
            state.recovery_action_identities.insert(ordinal, identity);
        }

        SamplingToolCallRegistration {
            ordinal,
            blocked_wait_guard,
            suppressed_failure,
            replayed_success,
        }
    }

    pub(crate) fn clear_blocked_wait_guard(&self, owner: &str, state_revision: &str) {
        let Some(ledger) = self.dispatch_ledger.as_ref() else {
            return;
        };
        let mut ledger = ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.blocked_wait_gate.as_ref().is_some_and(|gate| {
            gate.guard.owner == owner && gate.guard.state_revision == state_revision
        }) {
            ledger.blocked_wait_gate = None;
        }
    }

    pub(crate) fn record_child_runtime(&self, runtime_ms: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.child_runtime_ms = state.child_runtime_ms.saturating_add(runtime_ms);
        state.child_runtime_sample_count = state.child_runtime_sample_count.saturating_add(1);
    }

    fn turn_efficiency_sample(&self) -> (usize, u64, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state
                .registered_count
                .saturating_sub(state.direct_code_mode_exec_count)
                .saturating_add(state.code_mode_nested_tool_count),
            state.child_runtime_ms,
            state.child_runtime_sample_count,
        )
    }

    pub(crate) fn record_suppressed_result(&self, ordinal: u64, response: &ResponseInputItem) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Blocked,
            None,
        ));
        if let Some(evidence_identity) = response_evidence_identity(response) {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
        state.suppressed_blocked_wait = true;
    }

    pub(crate) fn record_suppressed_failure(&self, ordinal: u64, failure_fingerprint: &str) {
        let mut outcome =
            SamplingToolOutcome::plain(ordinal, SamplingToolOutcomeKind::Failure, None);
        outcome.failure_fingerprint = Some(failure_fingerprint.to_string());
        outcome.failure_is_terminal = true;
        outcome.failure_diagnosis_reused = true;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(outcome);
    }

    pub(crate) fn record_accepted_deterministic_continuation_receipts(
        &self,
        receipts: &[TurnTimingDeterministicContinuationReceipt],
    ) {
        if receipts.is_empty() {
            return;
        }
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .deterministic_continuation_receipts
                .extend(receipts.iter().filter_map(|receipt| {
                    (receipt.suppressed_continuation_count > 0)
                        .then(|| receipt.runtime_identity())
                        .flatten()
                }));
        }
        if let Some(ledger) = &self.dispatch_ledger {
            let ledger = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ledger
                .timing
                .record_accepted_deterministic_continuation_receipts(receipts);
        }
    }

    pub(crate) fn record_direct_wait_owner_result(
        &self,
        validated_owner_path: bool,
        tool_name: &ToolName,
        payload: &ToolPayload,
        signal: Option<&Value>,
        response: &ResponseInputItem,
    ) {
        if !validated_owner_path || !tool_name_matches(tool_name, "wait_agent") {
            return;
        }
        let Some(observation) = authoritative_wait_observation(
            "multi_agent_v2",
            tool_name,
            payload,
            signal,
            canonical_authoritative_result(response).as_ref(),
        ) else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.direct_wait_agent_count = state.direct_wait_agent_count.saturating_add(1);
        state.authoritative_wait_observations.push(observation);
    }

    pub(crate) fn record_code_mode_result(&self, result: CodeModeToolResult<'_>) {
        let CodeModeToolResult {
            cell_id,
            tool_name,
            payload,
            source_dependencies,
            outcome_context,
            signal,
            result,
            canonical_artifact_required,
        } = result;
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let plan = sampling_plan(signal);
        let mut outcome = SamplingToolOutcome::from_signal(ordinal, outcome_context, plan, signal);
        if outcome.is_failure_evidence() && outcome.failure_fingerprint.is_none() {
            outcome.failure_fingerprint = Some(code_mode_result_failure_fingerprint(
                tool_name, payload, result,
            ));
        }
        outcome.canonical_artifact_required = canonical_artifact_required;
        outcome.nested_in_code_mode = true;
        let output = result
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default();
        outcome.executed_cargo_test_targets = executed_cargo_test_targets(output);
        outcome.tests_executed = output_proves_test_execution(output);
        let canonical = canonical_tool_action(payload);
        let structured_action =
            structured_action_identity_from_canonical(tool_name, payload, &canonical);
        let (validation, validation_proof, test_execution) =
            validation_status_from_arguments(tool_name, &canonical.value);
        let final_verification = final_diff_status_from_arguments(tool_name, &canonical.value);
        let mutation = is_mutation_tool(tool_name);
        let evidence_identity = outcome
            .source_evidence
            .as_ref()
            .and_then(value_evidence_identity)
            .or_else(|| value_evidence_identity(result));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        if tool_name_matches(tool_name, "write_stdin") {
            state.process_monitor_ordinals.insert(ordinal);
        }
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= is_coordination_tool(tool_name);
        if validation {
            state.validation_ordinals.insert(ordinal);
        }
        if validation_proof {
            state.validation_proof_ordinals.insert(ordinal);
        }
        if test_execution {
            state.test_validation_ordinals.insert(ordinal);
        }
        if final_verification {
            state.final_verification_ordinals.insert(ordinal);
        }
        if mutation {
            state.mutation_ordinals.insert(ordinal);
        }
        state.accumulate_code_mode_source_dependencies(cell_id, source_dependencies);
        state.outcomes.push(outcome);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
        }
        if let Some(identity) = recovery_action_identity(tool_name, &canonical) {
            state.recovery_action_identities.insert(ordinal, identity);
        }
        if let Some(evidence_identity) = evidence_identity {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
        if !is_wait_tool(tool_name) {
            return;
        }
        if let Some(observation) = authoritative_wait_observation(
            "code_mode_cell",
            tool_name,
            payload,
            signal,
            Some(result),
        ) {
            state.authoritative_wait_observations.push(observation);
        }
    }

    pub(crate) fn code_mode_source_dependencies(
        &self,
        cell_id: &str,
    ) -> Option<BTreeSet<SourceDependencyV1>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .code_mode_source_dependencies
            .get(cell_id)
            .cloned()
    }

    pub(crate) fn record_code_mode_failure(
        &self,
        cell_id: &str,
        tool_name: &ToolName,
        payload: Option<&ToolPayload>,
        source_dependencies: Option<BTreeSet<SourceDependencyV1>>,
        failure_fingerprint: String,
    ) {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let mut outcome =
            SamplingToolOutcome::plain(ordinal, SamplingToolOutcomeKind::Failure, None);
        outcome.failure_fingerprint = Some(failure_fingerprint);
        outcome.nested_in_code_mode = true;
        let canonical = payload.map(canonical_tool_action);
        let structured_action = payload
            .zip(canonical.as_ref())
            .and_then(|(payload, canonical)| {
                structured_action_identity_from_canonical(tool_name, payload, canonical)
            });
        let (validation, validation_proof, test_execution) = canonical
            .as_ref()
            .map(|canonical| validation_status_from_arguments(tool_name, &canonical.value))
            .unwrap_or_default();
        let final_verification = canonical
            .as_ref()
            .is_some_and(|canonical| final_diff_status_from_arguments(tool_name, &canonical.value));
        let mutation = payload.is_some_and(|_| is_mutation_tool(tool_name));
        let coordination = payload.is_some_and(|_| is_coordination_tool(tool_name));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= coordination;
        if validation {
            state.validation_ordinals.insert(ordinal);
        }
        if validation_proof {
            state.validation_proof_ordinals.insert(ordinal);
        }
        if test_execution {
            state.test_validation_ordinals.insert(ordinal);
        }
        if final_verification {
            state.final_verification_ordinals.insert(ordinal);
        }
        if mutation {
            state.mutation_ordinals.insert(ordinal);
        }
        state.accumulate_code_mode_source_dependencies(cell_id, source_dependencies);
        state.outcomes.push(outcome);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
        }
        if let Some(identity) = canonical
            .as_ref()
            .and_then(|canonical| recovery_action_identity(tool_name, canonical))
        {
            state.recovery_action_identities.insert(ordinal, identity);
        }
    }

    fn authoritative_wait_observation(&self) -> Option<AuthoritativeWaitObservation> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let direct_owner = state.registered_count == 1
            && state.direct_wait_agent_count == 1
            && state.direct_code_mode_exec_count == 0
            && state.code_mode_nested_tool_count == 0;
        let code_mode_owner = state.registered_count == 1
            && state.direct_wait_agent_count == 0
            && state.direct_code_mode_exec_count == 1
            && state.code_mode_nested_tool_count == 1;
        if !(direct_owner || code_mode_owner) || state.authoritative_wait_observations.len() != 1 {
            return None;
        }
        state.authoritative_wait_observations.first().cloned()
    }

    fn suppressed_blocked_wait(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .suppressed_blocked_wait
    }

    pub(crate) fn record_failure(&self, ordinal: u64, failure: &str, failure_is_terminal: bool) {
        let mut outcome =
            SamplingToolOutcome::plain(ordinal, SamplingToolOutcomeKind::Failure, None);
        outcome.failure_fingerprint = Some(format!(
            "direct_tool.{:x}",
            Sha256::digest(failure.as_bytes())
        ));
        outcome.failure_is_terminal = failure_is_terminal;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(outcome);
    }

    pub(crate) fn record_replay_dependencies(
        &self,
        ordinal: u64,
        mutation_revision: u64,
        source_paths: Vec<crate::git_workspace::SourcePathChangeObservation>,
        workspace_revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Validation and final verification may depend on services, environment,
        // or Git metadata beyond these paths. A workspace watcher alone is not proof.
        if mutation_revision != self.request_mutation_revision
            || source_paths.is_empty()
            || state.validation_ordinals.contains(&ordinal)
            || state.final_verification_ordinals.contains(&ordinal)
        {
            return;
        }
        if state.successful_replay_evidence.len() >= SUCCESSFUL_REPLAY_GATE_LIMIT {
            state.successful_replay_evidence.pop_first();
        }
        state.successful_replay_evidence.insert(
            ordinal,
            SuccessfulReplayEvidence {
                mutation_revision,
                workspace_revision,
                source_paths,
            },
        );
    }

    pub(crate) fn record_response_result(
        &self,
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        signal: Option<Value>,
        response: &ResponseInputItem,
        canonical_artifact_required: bool,
    ) {
        let plan = sampling_plan(signal.as_ref());
        let mut outcome =
            SamplingToolOutcome::from_signal(ordinal, outcome_context, plan, signal.as_ref());
        let output = response_output_text(response).unwrap_or_default();
        outcome.executed_cargo_test_targets = executed_cargo_test_targets(&output);
        outcome.tests_executed = output_proves_test_execution(&output);
        if outcome.is_failure_evidence() && outcome.failure_fingerprint.is_none() {
            outcome.failure_fingerprint = response_failure_fingerprint(response);
        }
        let evidence_identity = outcome
            .source_evidence
            .as_ref()
            .and_then(value_evidence_identity)
            .or_else(|| response_evidence_identity(response));
        outcome.canonical_artifact_required = canonical_artifact_required;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replayable = state
            .structured_actions
            .get(&ordinal)
            .is_some_and(|action| {
                matches!(
                    action.class,
                    StructuredActionClass::BroadSource | StructuredActionClass::PreciseSource
                )
            })
            || state.validation_proof_ordinals.contains(&ordinal)
            || state.final_verification_ordinals.contains(&ordinal);
        if outcome.kind == SamplingToolOutcomeKind::Success
            && (!state.test_validation_ordinals.contains(&ordinal) || outcome.tests_executed)
            && replayable
            && state.successful_replay_evidence.contains_key(&ordinal)
            && response_has_replayable_call_id(response)
            && response_replay_text_size(response)
                .is_some_and(|size| size <= SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT)
        {
            state
                .successful_replay_responses
                .insert(ordinal, response.clone());
            while state.successful_replay_responses.len() > SUCCESSFUL_REPLAY_GATE_LIMIT {
                state.successful_replay_responses.pop_first();
            }
        }
        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.outcomes.push(outcome);
        if let Some(evidence_identity) = evidence_identity {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
    }

    pub(crate) fn record_replayed_response_result(
        &self,
        ordinal: u64,
        response: &ResponseInputItem,
    ) {
        self.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            response,
            false,
        );
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replayed_ordinals
            .insert(ordinal);
    }

    fn successful_replay_candidates(
        &self,
    ) -> Vec<(String, ResponseInputItem, SuccessfulReplayEvidence)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An ordinal is an identity, not proof of execution order. Resolve
        // duplicate/conflicting outcomes once, before cloning bounded responses.
        let mut successful = BTreeMap::new();
        for outcome in &state.outcomes {
            successful
                .entry(outcome.ordinal)
                .and_modify(|unique| *unique = false)
                .or_insert(outcome.kind == SamplingToolOutcomeKind::Success);
        }
        state
            .successful_replay_responses
            .iter()
            .filter(|(ordinal, _)| successful.get(ordinal) == Some(&true))
            .filter_map(|(ordinal, response)| {
                let evidence = state.successful_replay_evidence.get(ordinal)?;
                state
                    .structured_actions
                    .get(ordinal)
                    .map(|action| (action.identity.clone(), response.clone(), evidence.clone()))
            })
            .collect()
    }

    #[cfg(test)]
    fn deterministic_cycle_key(&self) -> Option<String> {
        self.deterministic_cycle().map(|cycle| cycle.key)
    }

    fn failure_fingerprint(&self) -> Option<String> {
        self.deterministic_cycle()
            .and_then(|cycle| cycle.repeated_failure)
            .map(|(_, fingerprint)| fingerprint)
    }

    fn deterministic_cycle(&self) -> Option<DeterministicCycle> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.registered_count == 0 {
            return Some(DeterministicCycle {
                key: "empty".to_string(),
                kind: DeterministicCycleKind::Empty,
                failure_only: false,
                repeated_failure: None,
            });
        }
        let mut outer_code_mode_outcomes = state
            .outcomes
            .iter()
            .filter(|outcome| !outcome.nested_in_code_mode);
        let outer_code_mode_wrapper = outer_code_mode_outcomes.next().is_some_and(|outcome| {
            outcome.kind == SamplingToolOutcomeKind::Success || outcome.wraps_nested_terminal
        }) && outer_code_mode_outcomes.next().is_none();
        let code_mode_owned = state.code_mode_nested_tool_count > 0
            && state.registered_count == 1
            && state.direct_code_mode_exec_count == 1
            && outer_code_mode_wrapper;
        let outcomes = state
            .outcomes
            .iter()
            .filter(|outcome| !code_mode_owned || outcome.nested_in_code_mode)
            .collect::<Vec<_>>();
        let failures = outcomes
            .iter()
            .copied()
            .filter(|outcome| outcome.is_failure_evidence())
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            let suppressible_failure = failures.iter().all(|outcome| outcome.failure_is_terminal);
            let failure_only = if code_mode_owned {
                outcomes.len() == state.code_mode_nested_tool_count
                    && outcomes.iter().all(|outcome| outcome.is_failure_evidence())
            } else {
                state.outcomes.len() == state.registered_count
                    && outcomes.iter().all(|outcome| outcome.is_failure_evidence())
            };
            let nested_only = failures.iter().all(|outcome| outcome.nested_in_code_mode);
            let mut fingerprints = failures
                .iter()
                .map(|outcome| outcome.failure_fingerprint.as_deref())
                .collect::<Option<Vec<_>>>()?;
            fingerprints.sort_unstable();
            fingerprints.dedup();
            let failure_fingerprint = fingerprints.into_iter().collect::<Vec<_>>().join("|");
            let repeated_action_identity = if code_mode_owned {
                state
                    .outcomes
                    .iter()
                    .filter(|outcome| !outcome.nested_in_code_mode)
                    .find_map(|outcome| state.structured_actions.get(&outcome.ordinal))
                    .map(|action| action.identity.clone())
            } else if failures.len() == 1 {
                state
                    .structured_actions
                    .get(&failures[0].ordinal)
                    .map(|action| action.identity.clone())
            } else {
                None
            };
            let mut failure_action_bindings = if code_mode_owned {
                let action_identity = repeated_action_identity.clone()?;
                failures
                    .iter()
                    .map(|outcome| {
                        Some((
                            action_identity.clone(),
                            outcome.failure_fingerprint.as_deref()?.to_string(),
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?
            } else {
                failures
                    .iter()
                    .map(|outcome| {
                        let action_identity = state
                            .structured_actions
                            .get(&outcome.ordinal)
                            .map(|action| action.identity.clone())?;
                        let fingerprint = outcome.failure_fingerprint.as_deref()?.to_string();
                        Some((action_identity, fingerprint))
                    })
                    .collect::<Option<Vec<_>>>()?
            };
            failure_action_bindings.sort_unstable();
            let failure_action_identity = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&failure_action_bindings).ok()?)
            );
            let (kind, key_prefix) = if nested_only {
                (
                    DeterministicCycleKind::NestedToolFailure,
                    "NestedToolFailure",
                )
            } else {
                (DeterministicCycleKind::ToolFailure, "ToolFailure")
            };
            return Some(DeterministicCycle {
                key: format!("{key_prefix}:{failure_fingerprint}:{failure_action_identity}"),
                kind,
                failure_only,
                repeated_failure: if suppressible_failure {
                    repeated_action_identity
                        .map(|action_identity| (action_identity, failure_fingerprint))
                } else {
                    None
                },
            });
        }
        // A successful `write_stdin` result is monitoring state for a process
        // that is still owned by the executor. It may remain byte-for-byte
        // unchanged until output or termination, so it is not a failed or
        // no-progress reasoning attempt. Actual write/poll errors were handled
        // by the failure branch above and remain eligible for deduplication.
        if state.process_monitor_ordinals.len() == state.registered_count {
            return None;
        }
        let incomplete_code_mode_outcomes = code_mode_owned
            && (outcomes.len() != state.code_mode_nested_tool_count
                || !outcomes
                    .iter()
                    .all(|outcome| outcome.kind == SamplingToolOutcomeKind::Success));
        let incomplete_direct_outcomes = !code_mode_owned
            && (state.outcomes.len() != state.registered_count
                || state.structured_actions.len() != state.registered_count
                || state.evidence_items.len() != state.registered_count
                || !state
                    .outcomes
                    .iter()
                    .all(|outcome| outcome.kind == SamplingToolOutcomeKind::Success));
        if incomplete_code_mode_outcomes || incomplete_direct_outcomes {
            return None;
        }

        let semantic_evidence_only = outcomes.iter().all(|outcome| {
            outcome.source_evidence.as_ref().is_some_and(|evidence| {
                evidence
                    .get("source")
                    .and_then(Value::as_str)
                    .is_some_and(|source| !source.is_empty())
                    && evidence.get("scope").is_some_and(|scope| !scope.is_null())
                    && evidence
                        .get("identity")
                        .is_some_and(|identity| !identity.is_null())
            })
        }) && !state.saw_validation
            && !state.saw_mutation
            && !state.saw_coordination;
        let ordered = outcomes
            .into_iter()
            .map(|outcome| {
                let action = state.structured_actions.get(&outcome.ordinal)?;
                let evidence = state.evidence_items.get(&outcome.ordinal)?;
                Some((outcome.ordinal, action, evidence))
            })
            .collect::<Option<Vec<_>>>()?;
        let all_broad_source = ordered
            .iter()
            .all(|(_, action, _)| action.class == StructuredActionClass::BroadSource)
            && state.wait_call_count == 0
            && !state.saw_validation
            && !state.saw_mutation
            && !state.saw_coordination;
        let residual_tool_continuation = !state.deterministic_continuation_receipts.is_empty();
        let mut ordered = ordered;
        ordered.sort_by_key(|(ordinal, _, _)| *ordinal);
        let mut action_evidence = ordered
            .iter()
            .map(|(_, action, evidence)| format!("{}:{evidence}", action.identity))
            .collect::<Vec<_>>();
        // Reordering the same broad reads does not establish progress. Keep
        // each result bound to its action and preserve duplicate observations.
        // Other tool passes can contain operations whose order is meaningful.
        if all_broad_source {
            action_evidence.sort_unstable();
        }
        let action_evidence = action_evidence.join("|");
        let semantic_evidence = semantic_evidence_only.then(|| {
            let mut identities = ordered
                .iter()
                .map(|(_, _, evidence)| (*evidence).clone())
                .collect::<Vec<_>>();
            identities.sort_unstable();
            identities.join("|")
        });
        let receipts = state
            .deterministic_continuation_receipts
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("|");
        let kind = if all_broad_source || semantic_evidence_only {
            DeterministicCycleKind::BroadSourcePass
        } else if residual_tool_continuation {
            DeterministicCycleKind::ResidualToolContinuation
        } else {
            DeterministicCycleKind::StructuredToolPass
        };
        Some(DeterministicCycle {
            key: format!(
                "{kind:?}:{}:receipts:{receipts}",
                semantic_evidence.as_deref().unwrap_or(&action_evidence)
            ),
            kind,
            failure_only: false,
            repeated_failure: None,
        })
    }

    pub(crate) fn is_wait_only(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count > 0 && state.wait_call_count == state.registered_count
    }

    #[cfg(test)]
    pub(crate) fn has_process_monitor(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.process_monitor_ordinals.is_empty()
    }

    pub(crate) fn observed_successful_process_monitor(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.iter().any(|outcome| {
            matches!(
                outcome.kind,
                SamplingToolOutcomeKind::Success | SamplingToolOutcomeKind::Yielded
            ) && state.process_monitor_ordinals.contains(&outcome.ordinal)
        })
    }

    pub(crate) fn observed_yielded_execution(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcomes
            .iter()
            .any(|outcome| outcome.kind == SamplingToolOutcomeKind::Yielded)
    }

    #[cfg(test)]
    fn saw_validation(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .saw_validation
    }

    #[cfg(test)]
    pub(crate) fn executed_validation_summary(&self) -> ExecutedValidationSummary {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let executed_validation_count = state
            .validation_ordinals
            .iter()
            .filter(|ordinal| {
                state.outcomes.iter().any(|outcome| {
                    outcome.ordinal == **ordinal
                        && outcome.kind != SamplingToolOutcomeKind::Skipped
                        && !outcome.failure_diagnosis_reused
                        && !state.replayed_ordinals.contains(ordinal)
                })
            })
            .count();
        let count = u32::try_from(executed_validation_count).unwrap_or(u32::MAX);
        let completed_outcome_count = state
            .outcomes
            .iter()
            .filter(|outcome| {
                !outcome.failure_diagnosis_reused
                    && !state.replayed_ordinals.contains(&outcome.ordinal)
            })
            .count();
        let duration_is_validation_only = state.child_runtime_sample_count
            == executed_validation_count
            && completed_outcome_count
                == executed_validation_count.saturating_add(state.direct_code_mode_exec_count);

        ExecutedValidationSummary {
            count,
            // Runtime samples do not carry ordinals today. Attribute their
            // aggregate only when every timed child was an executed
            // validation; mixed requests retain a truthful zero duration.
            duration_ms: if duration_is_validation_only {
                state.child_runtime_ms
            } else {
                0
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn validation_workspace_revision_for_test(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .validation_mutation_revision
    }

    pub(crate) fn record_validation_workspace_revision(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        mutation_revision: u64,
    ) {
        if validation_invocation_status(tool_name, payload).1 {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .validation_mutation_revision = Some(mutation_revision);
        }
    }

    fn update_unresolved_failures(
        &self,
        unresolved: &mut BTreeSet<Option<RecoveryActionIdentity>>,
        fresh_validation: bool,
    ) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for outcome in &state.outcomes {
            let identity = state
                .recovery_action_identities
                .get(&outcome.ordinal)
                .cloned();
            if outcome.is_failure_evidence() {
                unresolved.insert(identity);
            } else if outcome.kind == SamplingToolOutcomeKind::Success
                && identity.is_some()
                && (!state.test_validation_ordinals.contains(&outcome.ordinal)
                    || outcome.tests_executed)
                && (!state.validation_ordinals.contains(&outcome.ordinal) || fresh_validation)
            {
                // A broader Cargo run can recover a focused failure only when
                // it actually ran that target under the same invocation context.
                // Unknown failures and unrelated validations remain open.
                unresolved.retain(|failed| {
                    failed != &identity
                        && !failed.as_ref().zip(identity.as_ref()).is_some_and(
                            |(failed, recovered)| {
                                fresh_validation
                                    && recovered.covers_cargo_failure(
                                        failed,
                                        &outcome.executed_cargo_test_targets,
                                    )
                            },
                        )
                });
            }
        }
    }

    fn fresh_successful_validation(&self) -> Option<FreshSuccessfulValidation> {
        let allocated_ordinal_count = self.next_ordinal.load(Ordering::Acquire);
        let allocated_count = usize::try_from(allocated_ordinal_count).ok()?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let latest_validation_ordinal = state
            .validation_proof_ordinals
            .iter()
            .next_back()
            .copied()?;
        let outcome_ordinals = state
            .outcomes
            .iter()
            .map(|outcome| outcome.ordinal)
            .collect::<BTreeSet<_>>();
        let terminal_observation_is_valid = state
            .outcomes
            .iter()
            .filter(|outcome| outcome.ordinal > latest_validation_ordinal)
            .all(|outcome| {
                if state.final_verification_ordinals.contains(&outcome.ordinal) {
                    return true;
                }
                if state
                    .structured_actions
                    .get(&outcome.ordinal)
                    .is_some_and(|action| {
                        matches!(
                            action.class,
                            StructuredActionClass::BroadSource
                                | StructuredActionClass::PreciseSource
                        )
                    })
                {
                    return true;
                }
                // Completing an existing plan is bookkeeping, not another
                // workspace observation that requires repeating the tests.
                outcome
                    .plan
                    .as_ref()
                    .is_some_and(|plan| !plan.plan.is_empty() && !plan_is_unfinished(plan))
            });
        if state.outcomes.len() != allocated_count
            || outcome_ordinals.len() != allocated_count
            || !(0..allocated_ordinal_count).all(|ordinal| outcome_ordinals.contains(&ordinal))
            || !terminal_observation_is_valid
            || state
                .outcomes
                .iter()
                .any(|outcome| outcome.kind != SamplingToolOutcomeKind::Success)
            || state
                .outcomes
                .iter()
                .any(|outcome| outcome.unfinished_mutation_obligation)
            || state.outcomes.iter().any(|outcome| {
                state.test_validation_ordinals.contains(&outcome.ordinal) && !outcome.tests_executed
            })
            || state.saw_canonical_artifact_requirement
            || state.saw_coordination
            || state.suppressed_blocked_wait
            || !state.authoritative_wait_observations.is_empty()
            || state
                .validation_proof_ordinals
                .iter()
                .any(|ordinal| !outcome_ordinals.contains(ordinal))
            || state
                .mutation_ordinals
                .range((
                    std::ops::Bound::Excluded(latest_validation_ordinal),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .is_some()
        {
            return None;
        }

        Some(FreshSuccessfulValidation {
            mutation_revision: state.validation_mutation_revision,
        })
    }

    fn generation_purpose(
        &self,
        baselines: &SamplingRequestBaselines,
        settled: &SamplingRequestSettledState,
        has_pending_input: bool,
        deterministic_protocol_fallback: bool,
    ) -> Option<TurnTimingGenerationPurpose> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observed_failure = state
            .outcomes
            .iter()
            .any(SamplingToolOutcome::is_failure_evidence);
        let observed_new_failure = state
            .outcomes
            .iter()
            .any(|outcome| outcome.is_failure_evidence() && !outcome.failure_diagnosis_reused);
        // Mixed generations use the protocol's conservative precedence. The
        // initial/compaction cases are selected by the caller before this
        // post-tool classifier runs.
        if has_pending_input {
            Some(TurnTimingGenerationPurpose::InitialReasoning)
        } else if state.saw_mutation || settled.mutation_revision > baselines.mutation_revision {
            Some(if observed_failure {
                TurnTimingGenerationPurpose::Repair
            } else {
                TurnTimingGenerationPurpose::ImplementationDecision
            })
        } else if state.saw_validation {
            Some(if observed_new_failure {
                TurnTimingGenerationPurpose::FailureDiagnosis
            } else {
                TurnTimingGenerationPurpose::ValidationInterpretation
            })
        } else if state.saw_coordination {
            Some(TurnTimingGenerationPurpose::Coordination)
        } else if state.registered_count > 0 && state.wait_call_count == state.registered_count {
            Some(TurnTimingGenerationPurpose::Wait)
        } else if observed_new_failure {
            Some(TurnTimingGenerationPurpose::FailureDiagnosis)
        } else if state.saw_artifact_read
            || state.saw_canonical_artifact_requirement
            || state
                .structured_actions
                .values()
                .any(|action| action.class == StructuredActionClass::BroadSource)
        {
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        } else if state.registered_count > 0 {
            // Every successful tool result is new model-visible evidence even
            // when it does not fall into a more specific workflow class.
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        } else if deterministic_protocol_fallback {
            Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
        } else {
            None
        }
    }

    pub(crate) fn progress_kinds(
        &self,
        baselines: &SamplingRequestBaselines,
        settled: &SamplingRequestSettledState,
    ) -> Vec<TurnTimingProgressKind> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut progress = Vec::new();
        if settled.mutation_revision != baselines.mutation_revision {
            progress.push(TurnTimingProgressKind::WorkspaceMutation);
        }
        if state
            .validation_ordinals
            .iter()
            .any(|ordinal| !state.replayed_ordinals.contains(ordinal))
        {
            progress.push(TurnTimingProgressKind::ValidationResult);
        }
        if state
            .outcomes
            .iter()
            .any(SamplingToolOutcome::is_failure_evidence)
        {
            progress.push(TurnTimingProgressKind::FailureObservation);
        }
        progress.sort_by_key(|kind| *kind as u8);
        progress.dedup();
        progress
    }

    #[cfg(test)]
    fn push(&self, outcome: SamplingToolOutcome) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcomes
            .push(outcome);
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<SamplingToolOutcome> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcomes
            .clone()
    }
}

fn sampling_tool_outcome_kind(
    outcome: ToolOutputOutcome,
    signal: Option<&Value>,
) -> SamplingToolOutcomeKind {
    let signalled = signal
        .and_then(|value| value.get("outcome"))
        .and_then(Value::as_str)
        .map(|outcome| match outcome {
            "blocked" => SamplingToolOutcomeKind::Blocked,
            "timeout" => SamplingToolOutcomeKind::Timeout,
            "recoverable_cancellation" => SamplingToolOutcomeKind::RecoverableCancellation,
            "failure" => SamplingToolOutcomeKind::Failure,
            "skipped" => SamplingToolOutcomeKind::Skipped,
            "success" => SamplingToolOutcomeKind::Success,
            _ => SamplingToolOutcomeKind::Unknown,
        });
    match outcome {
        ToolOutputOutcome::Success => signalled.unwrap_or(SamplingToolOutcomeKind::Success),
        // A signal can refine a transport-level failure, but cannot turn it into
        // success, an advisory skip, or unclassified non-failure evidence.
        ToolOutputOutcome::Failure => match signalled {
            Some(
                kind @ (SamplingToolOutcomeKind::Blocked
                | SamplingToolOutcomeKind::Timeout
                | SamplingToolOutcomeKind::RecoverableCancellation),
            ) => kind,
            _ => SamplingToolOutcomeKind::Failure,
        },
        ToolOutputOutcome::TimedOut => SamplingToolOutcomeKind::Timeout,
        ToolOutputOutcome::Yielded => SamplingToolOutcomeKind::Yielded,
        ToolOutputOutcome::Skipped => SamplingToolOutcomeKind::Skipped,
    }
}

fn sampling_plan(signal: Option<&Value>) -> Option<UpdatePlanArgs> {
    signal
        .filter(|value| value.get("kind").and_then(Value::as_str) == Some("plan_update"))
        .and_then(|value| value.get("plan"))
        .and_then(|value| serde_json::from_value::<UpdatePlanArgs>(value.clone()).ok())
}

fn sampling_source_evidence(signal: Option<&Value>) -> Option<Value> {
    signal
        .and_then(|value| {
            value
                .get("semantic_evidence")
                .or_else(|| value.get("source_evidence"))
                .or_else(|| value.get("source_closure"))
        })
        .filter(|value| !value.is_null())
        .cloned()
}

fn sampling_unfinished_mutation_obligation(signal: Option<&Value>) -> bool {
    signal
        .and_then(|value| value.get("unfinished_mutation_obligation"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn sampling_failure_fingerprint(signal: Option<&Value>) -> Option<String> {
    signal.and_then(value_failure_signature)
}

fn sampling_failure_is_terminal(signal: Option<&Value>) -> bool {
    signal.is_some_and(value_failure_is_terminal)
}

struct CanonicalToolAction {
    kind: &'static str,
    value: Value,
    identity_payload: Option<String>,
}

fn canonical_tool_action(payload: &ToolPayload) -> CanonicalToolAction {
    match payload {
        ToolPayload::Function { arguments } => match serde_json::from_str::<Value>(arguments) {
            Ok(arguments) => {
                let value = canonicalize_json(&arguments);
                let identity_payload = serde_json::to_string(&value).ok();
                CanonicalToolAction {
                    kind: "function",
                    value,
                    identity_payload,
                }
            }
            Err(_) => CanonicalToolAction {
                kind: "function",
                value: Value::String(arguments.clone()),
                identity_payload: None,
            },
        },
        ToolPayload::ToolSearch { arguments } => CanonicalToolAction {
            kind: "tool_search",
            value: Value::String(arguments.query.clone()),
            identity_payload: Some(arguments.query.clone()),
        },
        ToolPayload::Custom { input } => CanonicalToolAction {
            kind: "custom",
            value: Value::String(input.clone()),
            identity_payload: Some(input.clone()),
        },
    }
}

#[cfg(test)]
fn action_identities(
    tool_name: &ToolName,
    payload: &ToolPayload,
) -> (Option<String>, Option<StructuredActionIdentity>) {
    let action = canonical_tool_action(payload);
    (
        deterministic_action_identity(tool_name, &action),
        structured_action_identity_from_canonical(tool_name, payload, &action),
    )
}

fn deterministic_action_identity(
    tool_name: &ToolName,
    action: &CanonicalToolAction,
) -> Option<String> {
    if !tool_name_matches(tool_name, "wait") && !tool_name_matches(tool_name, "wait_agent") {
        return None;
    }
    if action.kind != "function" {
        return None;
    }
    if action.value.get("force_fresh").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let arguments = action.identity_payload.as_deref()?;
    let action_class = serde_json::to_string(tool_name).ok()?;
    Some(format!("{action_class}\n{arguments}"))
}

#[cfg(test)]
fn structured_action_identity(
    tool_name: &ToolName,
    payload: &ToolPayload,
) -> Option<StructuredActionIdentity> {
    let action = canonical_tool_action(payload);
    structured_action_identity_from_canonical(tool_name, payload, &action)
}

fn structured_action_identity_from_canonical(
    tool_name: &ToolName,
    payload: &ToolPayload,
    canonical: &CanonicalToolAction,
) -> Option<StructuredActionIdentity> {
    // Fresh execution must bypass both replay lookup and storage, including
    // consecutive calls that all explicitly request force_fresh.
    if canonical.value.get("force_fresh").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let class = source_invocation_class_from_canonical(tool_name, payload, canonical);
    let action =
        serde_json::to_string(&(tool_name, canonical.identity_payload.as_deref()?)).ok()?;
    let identity = format!("{:x}", Sha256::digest(action.as_bytes()));
    Some(StructuredActionIdentity { identity, class })
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RecoveryActionIdentity {
    exact: String,
    cargo_test: Option<CargoTestRecoveryScope>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct CargoTestRecoveryScope {
    context: String,
    target: Option<String>,
}

impl RecoveryActionIdentity {
    fn covers_cargo_failure(&self, failed: &Self, executed_targets: &BTreeSet<String>) -> bool {
        let Some((recovered, failed)) = self.cargo_test.as_ref().zip(failed.cargo_test.as_ref())
        else {
            return false;
        };
        recovered.context == failed.context
            && recovered.target.is_none()
            && failed
                .target
                .as_ref()
                .is_some_and(|target| executed_targets.contains(&target.replace('-', "_")))
    }
}

fn cargo_test_recovery_scope(
    tool_name: &ToolName,
    arguments: &Value,
) -> Option<CargoTestRecoveryScope> {
    let (program, args) = match command_invocation(tool_name, arguments)? {
        CommandInvocation::Argv { program, args } => (program, args),
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            // Only a literal single command can establish equivalent execution.
            if !script
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " _-.".contains(c))
            {
                return None;
            }
            let mut words = script.split_whitespace();
            (
                words.next()?.to_string(),
                words.map(str::to_string).collect(),
            )
        }
    };
    if !matches!(program.as_str(), "cargo" | "cargo.exe") || args.first()?.as_str() != "test" {
        return None;
    }
    let mut target = None;
    let mut base_args = Vec::new();
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--test" if target.is_none() => {
                let name = args.next()?;
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
                {
                    return None;
                }
                target = Some(name.clone());
            }
            "--offline" | "--locked" | "--frozen" => base_args.push(arg.clone()),
            "--jobs" | "-j" => {
                base_args.push(arg.clone());
                let jobs = args.next()?;
                jobs.parse::<u32>().ok()?;
                base_args.push(jobs.clone());
            }
            // Filters, features, package/target selection, harness options and
            // custom Cargo configurations need stronger coverage evidence.
            _ => return None,
        }
    }
    let mut context = arguments.clone();
    let fields = context.as_object_mut()?;
    for field in [
        "cmd",
        "command",
        "kind",
        "program",
        "args",
        "script_body",
        "force_fresh",
        "yield_time_ms",
        "max_output_tokens",
    ] {
        fields.remove(field);
    }
    Some(CargoTestRecoveryScope {
        context: serialized_evidence_identity(&(tool_name, program, base_args, context))?,
        target,
    })
}

fn output_proves_test_execution(text: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(text);
    text.lines().any(|line| {
        let line = line.trim();
        // Shell summaries retain source line numbers. Interpret the original
        // runner's status, not arbitrary numbers elsewhere in the output.
        let line = line
            .split_once(':')
            .filter(|(prefix, _)| {
                !prefix.is_empty() && prefix.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or(line, |(_, line)| line.trim());
        let positive = |value: &str| value.parse::<usize>().is_ok_and(|count| count > 0);
        if let Some(rest) = line.strip_prefix("Ran ") {
            let mut words = rest.split_whitespace();
            return words.next().is_some_and(positive)
                && matches!(words.next(), Some("test" | "tests"));
        }
        if let Some(rest) = line.strip_prefix("# pass ") {
            return positive(rest.trim());
        }
        let summary = line
            .strip_prefix("test result: ok. ")
            .or_else(|| line.strip_prefix("Tests:").map(str::trim))
            .or_else(|| line.contains(" tests run:").then_some(line))
            .or_else(|| {
                let pytest = line.trim_matches('=').trim();
                (pytest.ends_with('s') && pytest.contains(" passed") && pytest.contains(" in "))
                    .then_some(pytest)
            });
        summary.is_some_and(|summary| {
            let words = summary.split_whitespace().collect::<Vec<_>>();
            words
                .windows(2)
                .any(|pair| positive(pair[0]) && pair[1].trim_matches([';', ',']) == "passed")
        })
    })
}

fn executed_cargo_test_targets(text: &str) -> BTreeSet<String> {
    let parsed = serde_json::from_str::<Value>(text).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| value.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(text);
    text.lines()
        .filter_map(|line| {
            let running = line.trim().strip_prefix("Running ")?;
            let (_, executable) = running.rsplit_once('(')?;
            let executable = executable.strip_suffix(')')?.rsplit(['/', '\\']).next()?;
            let executable = executable.strip_suffix(".exe").unwrap_or(executable);
            let (target, hash) = executable.rsplit_once('-')?;
            (hash.len() >= 8 && hash.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| target.to_string())
        })
        .collect()
}

fn recovery_action_identity(
    tool_name: &ToolName,
    canonical: &CanonicalToolAction,
) -> Option<RecoveryActionIdentity> {
    // A forced execution is still a retry of the same action. Replay eligibility
    // is separate from whether its successful result resolves a prior failure.
    let exact = if canonical.kind == "function" && canonical.value.get("force_fresh").is_some() {
        let mut arguments = canonical.value.clone();
        arguments.as_object_mut()?.remove("force_fresh");
        let identity_payload = serde_json::to_string(&arguments).ok()?;
        serialized_evidence_identity(&(tool_name, identity_payload))
    } else {
        serialized_evidence_identity(&(tool_name, canonical.identity_payload.as_deref()?))
    }?;
    Some(RecoveryActionIdentity {
        exact,
        cargo_test: cargo_test_recovery_scope(tool_name, &canonical.value),
    })
}

struct Sha256Writer(Sha256);

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        Digest::update(&mut self.0, buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_evidence_identity(value: &impl Serialize) -> Option<String> {
    let mut writer = Sha256Writer(Sha256::new());
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(format!("{:x}", writer.0.finalize()))
}

fn response_evidence_identity(response: &ResponseInputItem) -> Option<String> {
    match response {
        ResponseInputItem::Message {
            role,
            content,
            phase,
        } => serialized_evidence_identity(&(role, content, phase)),
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            serialized_evidence_identity(output)
        }
        ResponseInputItem::McpToolCallOutput { output, .. } => serialized_evidence_identity(output),
        ResponseInputItem::ToolSearchOutput {
            status,
            execution,
            tools,
            omitted_result_count,
            ..
        } => serialized_evidence_identity(&(status, execution, tools, omitted_result_count)),
    }
}

fn response_has_replayable_call_id(response: &ResponseInputItem) -> bool {
    matches!(
        response,
        ResponseInputItem::FunctionCallOutput { .. }
            | ResponseInputItem::McpToolCallOutput { .. }
            | ResponseInputItem::CustomToolCallOutput { .. }
            | ResponseInputItem::ToolSearchOutput { .. }
    )
}

fn value_evidence_identity(value: &Value) -> Option<String> {
    serialized_evidence_identity(&canonicalize_json(value))
}

fn terminal_failure_can_be_reused_without_dispatch(tool_name: &ToolName) -> bool {
    // Keep reuse limited to deterministic local state transitions and artifact reads whose
    // producer explicitly classified the failure as terminal. Process, filesystem search, and
    // MCP failures can recover while their arguments and request revision remain unchanged.
    tool_name_matches(tool_name, "update_plan") || tool_name_matches(tool_name, "read_tool_output")
}

fn response_failure_fingerprint(response: &ResponseInputItem) -> Option<String> {
    let value = response_output_text(response)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    value_failure_signature(&value)
}

fn value_failure_is_terminal(value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };

    fields.get("retryable").and_then(Value::as_bool) == Some(false)
        || fields
            .get("failure")
            .and_then(Value::as_object)
            .and_then(|failure| failure.get("retryable"))
            .and_then(Value::as_bool)
            == Some(false)
}

fn value_failure_signature(value: &Value) -> Option<String> {
    let fields = value.as_object()?;
    fields
        .get("failure_signature")
        .and_then(Value::as_str)
        .or_else(|| {
            fields
                .get("failure")
                .and_then(Value::as_object)
                .and_then(|failure| failure.get("fingerprint"))
                .and_then(Value::as_str)
        })
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_owned)
}

fn code_mode_result_failure_fingerprint(
    tool_name: &ToolName,
    payload: &ToolPayload,
    result: &Value,
) -> String {
    if let Some(fingerprint) = value_failure_signature(result) {
        return fingerprint;
    }
    let action = canonical_tool_action(payload);
    let canonical = serde_json::to_vec(&serde_json::json!({
        "tool_name": tool_name,
        "payload": canonical_tool_payload(&action),
        "result": canonicalize_json(result),
    }))
    .unwrap_or_default();
    format!("code_mode.nested_tool.{:x}", Sha256::digest(canonical))
}

fn canonical_response_body(response: &ResponseInputItem) -> Option<Value> {
    let mut value = serde_json::to_value(response).ok()?;
    if let Value::Object(object) = &mut value {
        object.remove("call_id");
    }
    Some(canonicalize_json(&value))
}

fn canonical_authoritative_result(response: &ResponseInputItem) -> Option<Value> {
    response_output_text(response)
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|value| canonicalize_json(&value))
        .or_else(|| canonical_response_body(response))
}

fn response_output_text(response: &ResponseInputItem) -> Option<Cow<'_, str>> {
    let output = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output,
        _ => return None,
    };
    match &output.body {
        codex_protocol::models::FunctionCallOutputBody::Text(text) => Some(Cow::Borrowed(text)),
        codex_protocol::models::FunctionCallOutputBody::ContentItems(_) => {
            output.body.to_text().map(Cow::Owned)
        }
    }
}

fn response_replay_text_size(response: &ResponseInputItem) -> Option<usize> {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;

    let output = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output,
        _ => return None,
    };
    match &output.body {
        FunctionCallOutputBody::Text(text) => Some(text.len()),
        // Keep the original content array on replay. Non-text content still
        // needs normal dispatch, and cannot bypass the byte limit via its text.
        FunctionCallOutputBody::ContentItems(items) => {
            items.iter().try_fold(0usize, |size, item| match item {
                FunctionCallOutputContentItem::InputText { text } => size.checked_add(text.len()),
                _ => None,
            })
        }
    }
}

fn authoritative_wait_observation(
    expected_adapter: &str,
    tool_name: &ToolName,
    payload: &ToolPayload,
    signal: Option<&Value>,
    result: Option<&Value>,
) -> Option<AuthoritativeWaitObservation> {
    let proof = signal?.get("authoritative_wait_owner_v1")?;
    if proof.get("adapter").and_then(Value::as_str) != Some(expected_adapter) {
        return None;
    }
    let disposition = match proof.get("disposition").and_then(Value::as_str)? {
        "blocked" => AuthoritativeWaitDisposition::Blocked,
        "terminal" => AuthoritativeWaitDisposition::Terminal,
        _ => return None,
    };
    let owner = proof.get("owner").and_then(Value::as_str)?.trim();
    let state_revision = proof.get("state_revision").and_then(Value::as_str)?.trim();
    let receipt_identity = proof
        .get("receipt_identity")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let surfaceable_message = (disposition == AuthoritativeWaitDisposition::Terminal)
        .then(|| {
            proof
                .get("surfaceable_message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .map(ToOwned::to_owned)
        })
        .flatten();
    if owner.is_empty() || state_revision.is_empty() {
        return None;
    }
    let action = canonical_tool_action(payload);
    let result = canonicalize_json(result?);
    let action_identity = deterministic_action_identity(tool_name, &action)?;
    let identity = serde_json::to_vec(&serde_json::json!({
        "adapter": expected_adapter,
        "disposition": disposition,
        "owner": owner,
        "state_revision": state_revision,
        "action": canonical_tool_payload(&action),
        "receipt_identity": (disposition == AuthoritativeWaitDisposition::Terminal)
            .then_some(receipt_identity),
        "surfaceable_message": surfaceable_message,
    }))
    .ok()?;
    let assignment_ids = result
        .get("typed_deltas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|delta| delta.get("assignment_id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    Some(AuthoritativeWaitObservation {
        disposition,
        identity: format!("{:x}", Sha256::digest(identity)),
        owner: owner.to_string(),
        state_revision: state_revision.to_string(),
        action_identity,
        result: AuthoritativeWaitOwnerResult {
            adapter: expected_adapter.to_string(),
            value: result,
            surfaceable_message,
        },
        assignment_ids,
    })
}

fn canonical_tool_payload(action: &CanonicalToolAction) -> Value {
    serde_json::json!({
        "kind": action.kind,
        "value": action.value,
    })
}

fn command_invocation(tool_name: &ToolName, arguments: &Value) -> Option<CommandInvocation> {
    if !is_validation_tool(tool_name) {
        return None;
    }
    let script_field = if tool_name_matches(tool_name, "shell_command") {
        "command"
    } else {
        "cmd"
    };
    let args = match arguments.get("args") {
        Some(value) => Some(
            value
                .as_array()?
                .iter()
                .map(|arg| arg.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?,
        ),
        None => None,
    };
    CommandInvocation::from_parts(
        &tool_name.name,
        script_field,
        arguments.get(script_field).and_then(Value::as_str),
        arguments.get("kind").and_then(Value::as_str),
        arguments.get("program").and_then(Value::as_str),
        args.as_deref(),
        arguments.get("script_body").and_then(Value::as_str),
    )
    .ok()
}

fn validation_invocation_status(tool_name: &ToolName, payload: &ToolPayload) -> (bool, bool, bool) {
    validation_status_from_arguments(tool_name, &canonical_tool_action(payload).value)
}

fn validation_status_from_arguments(tool_name: &ToolName, arguments: &Value) -> (bool, bool, bool) {
    let Some(invocation) = command_invocation(tool_name, arguments) else {
        return (false, false, false);
    };
    match classify_validation(&invocation) {
        ValidationClassification::Validation {
            leaves,
            exit_code_is_authoritative,
            ..
        } if !leaves.is_empty() => (
            true,
            exit_code_is_authoritative,
            leaves
                .iter()
                .any(|leaf| leaf.operation == ValidationOperation::Test),
        ),
        _ => (false, false, false),
    }
}

fn final_diff_status_from_arguments(tool_name: &ToolName, arguments: &Value) -> bool {
    match command_invocation(tool_name, arguments) {
        Some(CommandInvocation::Argv { program, args }) => final_diff_status_program_is_read_only(
            &program,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        Some(CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script)) => {
            final_diff_status_script_is_read_only(&script)
        }
        _ => false,
    }
}

fn final_diff_status_script_is_read_only(script: &str) -> bool {
    let script = script.trim();
    if script.is_empty()
        || script
            .chars()
            .any(|character| matches!(character, '|' | '>' | '<' | '`' | '$' | '(' | ')'))
    {
        return false;
    }

    let without_conjunctions = script.replace("&&", ";");
    if without_conjunctions.contains('&') {
        return false;
    }
    let normalized = without_conjunctions.replace(['\r', '\n'], ";");
    let clauses = normalized
        .split(';')
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect::<Vec<_>>();
    if clauses.is_empty() {
        return false;
    }

    clauses.iter().all(|clause| {
        let words = clause.split_whitespace().collect::<Vec<_>>();
        match words.split_first() {
            Some((program, args)) => final_diff_status_program_is_read_only(program, args),
            None => false,
        }
    })
}

fn final_diff_status_program_is_read_only(program: &str, args: &[&str]) -> bool {
    let basename = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let basename = basename.strip_suffix(".exe").unwrap_or(&basename);
    basename == "git"
        && matches!(args.first().copied(), Some("diff" | "status"))
        && classify_read_only_evidence_program(program, args) != StructuredActionClass::Other
}

fn is_wait_tool(tool_name: &ToolName) -> bool {
    tool_name_matches(tool_name, "wait")
}

fn is_validation_tool(tool_name: &ToolName) -> bool {
    ["exec_command", "shell_command", "unified_exec"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
}

fn is_mutation_tool(tool_name: &ToolName) -> bool {
    ["apply_patch", "apply_patch_tool"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
}

fn is_coordination_tool(tool_name: &ToolName) -> bool {
    ["spawn_agent", "send_message", "followup_task", "wait_agent"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
}

#[cfg(test)]
fn source_invocation_class(tool_name: &ToolName, payload: &ToolPayload) -> StructuredActionClass {
    let canonical = canonical_tool_action(payload);
    source_invocation_class_from_canonical(tool_name, payload, &canonical)
}

fn source_invocation_class_from_canonical(
    tool_name: &ToolName,
    payload: &ToolPayload,
    canonical: &CanonicalToolAction,
) -> StructuredActionClass {
    if ["read_tool_output"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
    {
        return StructuredActionClass::BroadSource;
    }
    if !["exec_command", "shell_command", "unified_exec"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
    {
        return StructuredActionClass::Other;
    }
    if !matches!(payload, ToolPayload::Function { .. }) || canonical.identity_payload.is_none() {
        return StructuredActionClass::Other;
    }
    let Some(invocation) = command_invocation(tool_name, &canonical.value) else {
        return StructuredActionClass::Other;
    };
    match invocation {
        CommandInvocation::Argv { program, args } => classify_read_only_evidence_program(
            &program,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            // This intentionally admits only literal, simple command forms. Shell
            // expansion, quoting, redirection and composition must execute normally.
            if script.chars().any(|c| {
                matches!(
                    c,
                    ';' | '&'
                        | '|'
                        | '>'
                        | '<'
                        | '`'
                        | '$'
                        | '('
                        | ')'
                        | '\n'
                        | '\r'
                        | '\''
                        | '"'
                        | '#'
                )
            }) {
                return StructuredActionClass::Other;
            }
            let words = script.split_whitespace().collect::<Vec<_>>();
            match words.split_first() {
                Some((program, args)) => classify_read_only_evidence_program(program, args),
                None => StructuredActionClass::Other,
            }
        }
    }
}

fn classify_read_only_evidence_program(program: &str, args: &[&str]) -> StructuredActionClass {
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program);
    // Optional replay is limited to known options; unknown options may execute
    // preprocessors, external diff helpers, pagers, or write output files.
    let known_options: &[&str] = match program {
        "rg" => &[
            "--files",
            "-n",
            "--line-number",
            "-l",
            "--files-with-matches",
            "-i",
            "-F",
            "--fixed-strings",
            "--hidden",
            "--no-ignore",
            "-g",
            "--glob",
            "--",
            "-q",
            "--count",
            "-c",
        ],
        "grep" => &["-n", "-i", "-F", "-r", "-R", "-l", "-q", "-c", "--"],
        "cat" | "head" | "tail" => &["-n", "-c", "--"],
        "git" => &[
            "--short",
            "--porcelain",
            "--stat",
            "--check",
            "--name-only",
            "--name-status",
            "--oneline",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--cached",
            "--staged",
            "--",
            "-n",
            "-p",
            "-s",
            "-z",
        ],
        _ => return StructuredActionClass::Other,
    };
    if args
        .iter()
        .any(|arg| arg.starts_with('-') && !known_options.contains(arg))
    {
        return StructuredActionClass::Other;
    }
    match program {
        "rg" if args.contains(&"--files") => StructuredActionClass::BroadSource,
        "rg" | "grep" | "cat" | "head" | "tail" if !args.is_empty() => {
            StructuredActionClass::PreciseSource
        }
        "git" => match args.first().copied() {
            Some("status" | "log" | "ls-files") => StructuredActionClass::BroadSource,
            Some("diff" | "show" | "grep" | "blame") => StructuredActionClass::PreciseSource,
            _ => StructuredActionClass::Other,
        },
        _ => StructuredActionClass::Other,
    }
}

fn tool_name_matches(tool_name: &ToolName, candidate: &str) -> bool {
    tool_name.namespace.is_none() && tool_name.name == candidate
}

pub(crate) struct TurnExecutionControl {
    soft_convergence_started_at: tokio::time::Instant,
    soft_convergence_issued: bool,
    /// Completed generations since the last one that produced new evidence,
    /// a workspace mutation, or a plan/input change.
    continuations_without_progress: u32,
    plan: Option<UpdatePlanArgs>,
    plan_revision: u64,
    input_revision: u64,
    dispatch_ledger: Arc<Mutex<DeterministicDispatchLedger>>,
    consecutive_no_progress: u32,
    consecutive_obligation_no_progress: u32,
    last_cycle: Option<String>,
    last_state_revision: Option<String>,
    directive_issued: bool,
    proven_loop_active: bool,
    distinct_failure_recovery_state_revision: Option<String>,
    distinct_failure_recovery_attempts: u32,
    turn_efficiency_guard: Option<TurnEfficiencyGuardHandle>,
    turn_efficiency_tool_calls: usize,
    turn_efficiency_child_runtime_ms: u64,
    unresolved_failures: BTreeSet<Option<RecoveryActionIdentity>>,
    budget_progress_evidence: BTreeSet<String>,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SamplingConvergenceDecision {
    pub(crate) continuation: ContinuationDisposition,
    pub(crate) directive: Option<String>,
    pub(crate) proven_loop_activated: bool,
    pub(crate) authoritative_wait: Option<AuthoritativeWaitResolution>,
}

impl TurnExecutionControl {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_with_timing(Arc::new(TurnTimingState::default()))
    }

    pub(crate) fn new_with_timing(timing: Arc<TurnTimingState>) -> Self {
        Self {
            soft_convergence_started_at: tokio::time::Instant::now(),
            soft_convergence_issued: false,
            continuations_without_progress: 0,
            plan: None,
            plan_revision: 0,
            input_revision: 0,
            dispatch_ledger: Arc::new(Mutex::new(DeterministicDispatchLedger::new(timing))),
            consecutive_no_progress: 0,
            consecutive_obligation_no_progress: 0,
            last_cycle: None,
            last_state_revision: None,
            directive_issued: false,
            proven_loop_active: false,
            distinct_failure_recovery_state_revision: None,
            distinct_failure_recovery_attempts: 0,
            turn_efficiency_guard: None,
            turn_efficiency_tool_calls: 0,
            turn_efficiency_child_runtime_ms: 0,
            unresolved_failures: BTreeSet::new(),
            budget_progress_evidence: BTreeSet::new(),
        }
    }

    pub(crate) fn take_soft_convergence_directive(
        &mut self,
        is_continuation: bool,
    ) -> Option<String> {
        if !is_continuation
            || self.soft_convergence_issued
            || self.soft_convergence_started_at.elapsed() < SOFT_CONVERGENCE_AFTER
            || self.continuations_without_progress < SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS
        {
            return None;
        }
        self.soft_convergence_issued = true;
        Some(SOFT_CONVERGENCE_DIRECTIVE.to_string())
    }

    /// Renew the emergency allowance only for new evidence, not new call IDs,
    /// wrapper formatting, replayed results, or repeated validation failures.
    pub(crate) fn observe_budget_progress(
        &mut self,
        baselines: &SamplingRequestBaselines,
        signals: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> bool {
        let state = signals
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut progressed = settled.mutation_revision != baselines.mutation_revision;
        for outcome in &state.outcomes {
            if state.replayed_ordinals.contains(&outcome.ordinal)
                || outcome.failure_diagnosis_reused
                || (state.code_mode_nested_tool_count > 0 && !outcome.nested_in_code_mode)
                || matches!(
                    outcome.kind,
                    SamplingToolOutcomeKind::Skipped | SamplingToolOutcomeKind::Yielded
                )
            {
                continue;
            }
            let evidence = if outcome.is_failure_evidence() {
                outcome.failure_fingerprint.clone()
            } else {
                outcome
                    .source_evidence
                    .as_ref()
                    .and_then(value_evidence_identity)
                    .or_else(|| {
                        state
                            .validation_ordinals
                            .contains(&outcome.ordinal)
                            .then(|| state.evidence_items.get(&outcome.ordinal).cloned())
                            .flatten()
                    })
            };
            if let Some(evidence) = evidence {
                // Identical bytes from a different source/selector establish
                // new coverage. Repeating the same action and bytes does not.
                let evidence = if outcome.is_failure_evidence() {
                    format!("failure:{evidence}")
                } else {
                    format!(
                        "source:{}:{evidence}",
                        state
                            .structured_actions
                            .get(&outcome.ordinal)
                            .map(|action| action.identity.as_str())
                            .unwrap_or_default()
                    )
                };
                progressed |= self.budget_progress_evidence.insert(evidence);
            }
        }
        if progressed {
            self.continuations_without_progress = 0;
        } else {
            self.continuations_without_progress =
                self.continuations_without_progress.saturating_add(1);
        }
        progressed
    }

    #[cfg(test)]
    pub(crate) fn baselines(&self, mutation_revision: u64) -> SamplingRequestBaselines {
        self.baselines_with_tool_exposure_revision(mutation_revision, 0)
    }

    pub(crate) fn baselines_with_tool_exposure_revision(
        &self,
        mutation_revision: u64,
        tool_exposure_revision: u64,
    ) -> SamplingRequestBaselines {
        SamplingRequestBaselines {
            mutation_revision,
            plan_revision: self.plan_revision,
            input_revision: self.input_revision,
            tool_exposure_revision,
        }
    }

    pub(crate) fn collector(
        &self,
        baselines: &SamplingRequestBaselines,
    ) -> SamplingRequestSignalCollector {
        SamplingRequestSignalCollector {
            next_ordinal: Arc::new(AtomicU64::new(0)),
            state: Arc::new(Mutex::new(SamplingRequestSignalState::default())),
            dispatch_ledger: Some(Arc::clone(&self.dispatch_ledger)),
            request_state_revision: baselines.revision_key(),
            request_mutation_revision: baselines.mutation_revision,
        }
    }

    pub(crate) fn initial_generation_request(
        &self,
        baselines: &SamplingRequestBaselines,
    ) -> GenerationRequestDisposition {
        GenerationRequestDisposition {
            purpose: Some(TurnTimingGenerationPurpose::InitialReasoning),
            sampling: SamplingGenerationDisposition::DecisionBearing,
            relevant_state_fingerprint: baselines.relevant_state_fingerprint(),
            failure_fingerprint: None,
            terminal_completion_only: false,
        }
    }

    pub(crate) fn continuation_generation_request(
        &self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
        has_pending_input: bool,
    ) -> GenerationRequestDisposition {
        let relevant_state_fingerprint = format!(
            "{:x}",
            Sha256::digest(self.settled_revision_key(settled).as_bytes())
        );
        GenerationRequestDisposition {
            // A server-requested continuation can finish reasoning or issue a
            // tool even when the preceding response changed no tracked state.
            // Only an explicit owner result may complete work without sampling.
            purpose: collector.generation_purpose(baselines, settled, has_pending_input, false),
            sampling: SamplingGenerationDisposition::DecisionBearing,
            relevant_state_fingerprint,
            failure_fingerprint: collector.failure_fingerprint(),
            terminal_completion_only: false,
        }
    }

    pub(crate) fn accepted_user_input(&mut self) {
        self.input_revision = self.input_revision.saturating_add(1);
        self.unresolved_failures.clear();
        self.reset_convergence();
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timing = Arc::clone(&ledger.timing);
        *ledger = DeterministicDispatchLedger::new(timing);
        drop(ledger);
    }

    pub(crate) fn evaluate_convergence(
        &mut self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> SamplingConvergenceDecision {
        let settled_revision = self.settled_revision_key(settled);
        // Validation proves only its observed execution. It cannot prove that
        // all user-requested changes, checks, or child lifecycle actions are done.
        if settled.mutation_revision != baselines.mutation_revision
            || self.plan_revision != baselines.plan_revision
            || self.input_revision != baselines.input_revision
            || settled.tool_exposure_revision != baselines.tool_exposure_revision
        {
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        }

        if self
            .last_state_revision
            .as_deref()
            .is_some_and(|previous| previous != settled_revision)
        {
            self.reset_turn_efficiency_guard();
            self.reset_distinct_failure_recovery();
        }
        let efficiency_sample_eligible = collector.authoritative_wait_observation().is_none();
        let (request_tool_calls, request_child_runtime_ms, runtime_samples) =
            if efficiency_sample_eligible {
                collector.turn_efficiency_sample()
            } else {
                (0, 0, 0)
            };
        let request_runtime_limit_ms = u64::try_from(request_tool_calls)
            .unwrap_or(u64::MAX)
            .saturating_mul(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        let request_has_negligible_runtime = request_tool_calls > 0
            && runtime_samples == request_tool_calls
            && request_child_runtime_ms <= request_runtime_limit_ms;
        let request_cycle = efficiency_sample_eligible
            .then(|| collector.deterministic_cycle())
            .flatten();
        let request_deterministic_cycle = request_cycle.as_ref().map(|cycle| cycle.key.clone());
        let repeated_cycle = request_cycle.as_ref().is_some_and(|cycle| {
            self.last_cycle.as_deref() == Some(cycle.key.as_str())
                && self.last_state_revision.as_deref() == Some(settled_revision.as_str())
        });
        let failure_only_cycle = request_cycle.as_ref().is_some_and(|cycle| {
            cycle.failure_only
                && matches!(
                    cycle.kind,
                    DeterministicCycleKind::ToolFailure | DeterministicCycleKind::NestedToolFailure
                )
        });
        let distinct_failure_recovery_attempt = if failure_only_cycle {
            if self.distinct_failure_recovery_state_revision.as_deref()
                == Some(settled_revision.as_str())
            {
                self.distinct_failure_recovery_attempts =
                    self.distinct_failure_recovery_attempts.saturating_add(1);
                (!repeated_cycle).then_some(self.distinct_failure_recovery_attempts)
            } else {
                self.distinct_failure_recovery_state_revision = Some(settled_revision.clone());
                self.distinct_failure_recovery_attempts = 1;
                None
            }
        } else {
            // Successful evidence, mixed success/failure results, and
            // ambiguous observations all break a failure-only recovery run.
            self.reset_distinct_failure_recovery();
            None
        };
        if distinct_failure_recovery_attempt
            .is_some_and(|attempt| attempt >= DISTINCT_FAILURE_RECOVERY_ADVISORY_THRESHOLD)
        {
            // Different actions or failure fingerprints are not a proven
            // loop. Keep the latest identity so an exact retry still
            // converges, but leave tools available for a narrower recovery.
            self.last_cycle = request_deterministic_cycle;
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = true;
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "Failure-recovery advisory: multiple distinct strategies failed while relevant state remained unchanged. Use a narrower or materially different recovery strategy; if none remains, truthfully report the failures and any blocker."
                        .to_string(),
                ),
                proven_loop_activated: false,
                authoritative_wait: None,
            };
        }
        if !request_has_negligible_runtime
            || self
                .turn_efficiency_guard
                .as_ref()
                .is_some_and(|guard| guard.settled_revision != settled_revision)
        {
            // State progress and substantive child runtime start a fresh
            // efficiency window. Distinct negligible-runtime cycles remain in
            // the current window, but cannot force completion without a
            // repeated semantic identity.
            self.reset_turn_efficiency_guard();
        }

        if self.turn_efficiency_guard.as_ref().is_some_and(|guard| {
            guard.settled_revision == settled_revision
                && guard.deterministic_cycle.is_some()
                && guard.deterministic_cycle == request_deterministic_cycle
        }) {
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = true;
            self.proven_loop_active = true;
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "Turn-efficiency guard: the same deterministic tool cycle repeated. Reuse its retained result rather than re-executing the unchanged operation. Reuse adds no new external evidence, but may support useful analysis or context recovery. It does not establish task completion; other actions remain available for required work."
                        .to_string(),
                ),
                proven_loop_activated: true,
                authoritative_wait: None,
            };
        }

        if request_has_negligible_runtime {
            self.turn_efficiency_tool_calls = self
                .turn_efficiency_tool_calls
                .saturating_add(request_tool_calls);
            self.turn_efficiency_child_runtime_ms = self
                .turn_efficiency_child_runtime_ms
                .saturating_add(request_child_runtime_ms);
        }
        let negligible_runtime_limit_ms = u64::try_from(self.turn_efficiency_tool_calls)
            .unwrap_or(u64::MAX)
            .saturating_mul(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        let exceeds_turn_efficiency_guard = request_has_negligible_runtime
            && repeated_cycle
            && self.turn_efficiency_tool_calls >= TURN_EFFICIENCY_TOOL_CALL_THRESHOLD
            && self.turn_efficiency_child_runtime_ms <= negligible_runtime_limit_ms;
        if exceeds_turn_efficiency_guard && self.turn_efficiency_guard.is_none() {
            // The first high-volume negligible-runtime observation is only a
            // consolidation signal, not proof of semantic completeness.
            self.turn_efficiency_guard = Some(TurnEfficiencyGuardHandle {
                settled_revision: settled_revision.clone(),
                deterministic_cycle: request_deterministic_cycle,
            });
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = true;
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "Turn-efficiency guard: repeated equivalent calls are a loop signal, not a completion test. Use existing evidence and stop optional exploration. Continue only to satisfy an unresolved requirement, complete required implementation or validation, or resolve a correctness-relevant uncertainty. Preserve the requested scope."
                        .to_string(),
                ),
                proven_loop_activated: false,
                authoritative_wait: None,
            };
        }

        if let Some(observation) = collector.authoritative_wait_observation() {
            // Monitoring polls report owner/process state; they are not failed
            // attempts and must not advance the failure/no-progress breaker.
            self.consecutive_no_progress = 0;
            self.consecutive_obligation_no_progress = 0;
            self.last_cycle = None;
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = false;
            self.proven_loop_active = false;
            self.reset_distinct_failure_recovery();
            {
                let mut ledger = self
                    .dispatch_ledger
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if ledger.blocked_wait_gate.as_ref().is_some_and(|gate| {
                    gate.guard.owner == observation.owner
                        && gate.guard.state_revision != observation.state_revision
                }) {
                    ledger.blocked_wait_gate = None;
                }
            }
            // A child assignment or code-mode cell becoming terminal proves
            // that wait is done, not that the consuming turn has no work left.
            if observation.disposition == AuthoritativeWaitDisposition::Terminal
                && matches!(
                    observation.result.adapter.as_str(),
                    "multi_agent_v2" | "code_mode_cell"
                )
            {
                return SamplingConvergenceDecision::default();
            }
            if observation.disposition == AuthoritativeWaitDisposition::Terminal
                && observation.result.surfaceable_message.is_some()
            {
                // The owner has already supplied the exact assistant text for
                // this terminal state. Surface it directly instead of making
                // the model restate an authoritative completion.
                return SamplingConvergenceDecision {
                    continuation: ContinuationDisposition::SurfaceExistingResult,
                    proven_loop_activated: false,
                    authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
                        observation.result,
                    )),
                    ..Default::default()
                };
            }
            return match observation.disposition {
                AuthoritativeWaitDisposition::Terminal => {
                    // A terminal owner result without designated assistant
                    // text still needs one semantic final response. The exact
                    // owner receipt is already authoritative, so make that
                    // generation tool-free on its first observation.
                    SamplingConvergenceDecision {
                        continuation: ContinuationDisposition::TerminalCompletionRequired,
                        directive: Some(
                            "The authoritative owner is terminal and its state is unchanged. Complete now from the existing owner result; do not call another tool."
                                .to_string(),
                        ),
                        proven_loop_activated: true,
                        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
                            observation.result,
                        )),
                    }
                }
                AuthoritativeWaitDisposition::Blocked => {
                    self.dispatch_ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .blocked_wait_gate = Some(BlockedWaitGate {
                        action_identity: observation.action_identity,
                        guard: BlockedWaitGuard {
                            owner: observation.owner,
                            state_revision: observation.state_revision,
                            assignment_ids: observation.assignment_ids,
                        },
                    });
                    SamplingConvergenceDecision {
                        continuation: ContinuationDisposition::ModelRequired,
                        directive: Some(
                            "The authoritative owner is blocked and requires main-agent action. Do not repeat the unchanged wait. Act on the blocker now, or truthfully report it if no in-scope action can resolve it."
                                .to_string(),
                        ),
                        proven_loop_activated: true,
                        authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
                            observation.result,
                        )),
                    }
                }
            };
        }

        if collector.suppressed_blocked_wait() {
            self.reset_distinct_failure_recovery();
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = true;
            self.proven_loop_active = true;
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "The unchanged authoritative wait remains suppressed because its blocker was already surfaced. Use another action to resolve the blocker or finish the remaining work; report it if no in-scope recovery exists."
                        .to_string(),
                ),
                proven_loop_activated: true,
                authoritative_wait: None,
            };
        }

        let Some(cycle) = request_cycle else {
            // Missing or ambiguous structured identity is possible progress.
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        };
        if cycle.kind == DeterministicCycleKind::Empty {
            // A tool-free continuation is a protocol/model signal, not an
            // action/result cycle. It provides no semantic identity that the
            // host can prove repeated, so it must never spend the convergence
            // budget or escalate tool restrictions.
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        }

        if repeated_cycle {
            self.consecutive_no_progress = self.consecutive_no_progress.saturating_add(1);
            self.consecutive_obligation_no_progress =
                self.consecutive_obligation_no_progress.saturating_add(1);
        } else {
            // A successful structured action can be new evidence. Read-only
            // observations and failures cannot advance state, so their first
            // observation starts the no-progress sequence immediately.
            let starts_no_progress_sequence = matches!(
                cycle.kind,
                DeterministicCycleKind::BroadSourcePass
                    | DeterministicCycleKind::ToolFailure
                    | DeterministicCycleKind::NestedToolFailure
            );
            self.consecutive_no_progress = u32::from(starts_no_progress_sequence);
            self.consecutive_obligation_no_progress = u32::from(starts_no_progress_sequence);
            self.directive_issued = false;
            self.proven_loop_active = false;
        }
        if let Some((action_identity, failure_fingerprint)) = cycle.repeated_failure.as_ref() {
            self.dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .repeated_failure_gate = Some(RepeatedFailureGate {
                state_revision: settled_revision.clone(),
                action_identity: action_identity.clone(),
                failure_fingerprint: failure_fingerprint.clone(),
            });
        }
        self.last_cycle = Some(cycle.key.clone());
        self.last_state_revision = Some(settled_revision);

        // The first successful structured observation initializes the counter
        // at zero because it may be new evidence. An exact repetition against
        // the same settled state increments it to one, which is already the
        // requested two-observation fixed point. A different cycle may supply
        // new evidence and does not prove repetition.
        let threshold = 1;
        if !repeated_cycle
            || self.consecutive_no_progress < threshold
                && self.consecutive_obligation_no_progress < threshold
        {
            return SamplingConvergenceDecision::default();
        }

        // An exact repeated failure can suppress that action, but says nothing
        // about the availability of a different recovery or remaining task work.
        let proven_loop_activated =
            self.directive_issued && repeated_cycle && !self.proven_loop_active;
        if proven_loop_activated {
            self.proven_loop_active = true;
        }
        self.directive_issued = true;
        let directive = if proven_loop_activated {
            "Convergence advisory: an ordered deterministic action/result cycle repeated against identical state. Reuse the existing result rather than re-executing the unchanged operation. Reuse adds no new external evidence, but may support useful analysis or restore evidence after context compaction. It does not establish completion or lack of useful reasoning progress. Other tools remain available for unfinished work and recovery."
        } else if cycle.kind == DeterministicCycleKind::BroadSourcePass {
            "Convergence advisory: the broad source pass returned the same evidence for the same action. Reuse retained evidence rather than re-executing the unchanged operation. Continue required coverage, implementation, validation, and correctness-relevant investigation; runtime bookkeeping cannot establish semantic completeness."
        } else if self.consecutive_no_progress == threshold
            || self.consecutive_obligation_no_progress == threshold
        {
            "Convergence advisory: repeated deterministic evidence adds no new external evidence; it does not establish that analysis made no useful progress. Reuse retained results and stop optional exploration. Continue to satisfy unresolved requirements, complete required implementation or validation, or resolve correctness-relevant uncertainty. Preserve the requested scope."
        } else if self.proven_loop_active {
            "Convergence escalation: an ordered deterministic action/result cycle has repeated after the convergence directive against identical state. Do not repeat it. Change the hypothesis or state, narrow the observation, or truthfully complete; existing task lifecycle rules still govern termination."
        } else {
            "Convergence escalation: structured state still has not changed. Equivalent completed actions remain blocked. Choose a new hypothesis, change state, narrow the observation, or truthfully complete; a no-progress count alone never ends the task."
        };
        SamplingConvergenceDecision {
            continuation: ContinuationDisposition::ModelRequired,
            directive: Some(directive.to_string()),
            proven_loop_activated,
            authoritative_wait: None,
        }
    }

    fn reset_convergence(&mut self) {
        self.consecutive_no_progress = 0;
        self.consecutive_obligation_no_progress = 0;
        self.continuations_without_progress = 0;
        self.last_cycle = None;
        self.last_state_revision = None;
        self.directive_issued = false;
        self.proven_loop_active = false;
        self.reset_distinct_failure_recovery();
        self.reset_turn_efficiency_guard();
    }

    fn reset_distinct_failure_recovery(&mut self) {
        self.distinct_failure_recovery_state_revision = None;
        self.distinct_failure_recovery_attempts = 0;
    }

    fn reset_turn_efficiency_guard(&mut self) {
        self.turn_efficiency_guard = None;
        self.turn_efficiency_tool_calls = 0;
        self.turn_efficiency_child_runtime_ms = 0;
    }

    pub(crate) fn settled_revision_key(&self, settled: &SamplingRequestSettledState) -> String {
        format!(
            "mutation={};plan={};input={};tool_exposure={}",
            settled.mutation_revision,
            self.plan_revision,
            self.input_revision,
            settled.tool_exposure_revision,
        )
    }

    pub(crate) fn settle(
        &mut self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) {
        let latest_plan = {
            let state = collector
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .outcomes
                .iter()
                .filter(|outcome| outcome.kind == SamplingToolOutcomeKind::Success)
                .filter_map(|outcome| outcome.plan.as_ref().map(|plan| (outcome.ordinal, plan)))
                .max_by_key(|(ordinal, _)| *ordinal)
                .map(|(_, plan)| plan.clone())
        };
        let changed_plan = latest_plan.filter(|plan| {
            self.plan
                .as_ref()
                .is_none_or(|current| current.plan != plan.plan)
        });
        if let Some(plan) = changed_plan.as_ref() {
            self.plan = Some(plan.clone());
            self.plan_revision = self.plan_revision.saturating_add(1);
        }
        // Never stamp observations with a later workspace or request revision.
        // The dispatcher checks the captured revision again under its workspace lease.
        let replay_candidates = if baselines.revision_key() == self.settled_revision_key(settled) {
            collector.successful_replay_candidates()
        } else {
            Vec::new()
        };
        if !replay_candidates.is_empty() {
            let state_revision = self.settled_revision_key(settled);
            let mut ledger = self
                .dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (action_identity, response, evidence) in replay_candidates {
                ledger.successful_replay_gates.retain(|gate| {
                    gate.state_revision != state_revision || gate.action_identity != action_identity
                });
                ledger
                    .successful_replay_gates
                    .push_back(SuccessfulReplayGate {
                        state_revision: state_revision.clone(),
                        action_identity,
                        response,
                        evidence,
                    });
                while ledger.successful_replay_gates.len() > SUCCESSFUL_REPLAY_GATE_LIMIT {
                    ledger.successful_replay_gates.pop_front();
                }
            }
        }
        let fresh_validation = collector
            .fresh_successful_validation()
            .is_some_and(|validation| {
                validation.mutation_revision == Some(settled.mutation_revision)
            });
        collector.update_unresolved_failures(&mut self.unresolved_failures, fresh_validation);
    }
}

fn plan_is_unfinished(plan: &UpdatePlanArgs) -> bool {
    !plan.plan.is_empty()
        && plan
            .plan
            .iter()
            .any(|item| item.status != StepStatus::Completed)
}

#[cfg(test)]
mod tests {
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::protocol::DeterministicContinuationClass;
    use codex_protocol::protocol::DeterministicContinuationHostAction;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn plan(statuses: &[StepStatus]) -> UpdatePlanArgs {
        UpdatePlanArgs {
            explanation: None,
            plan: statuses
                .iter()
                .cloned()
                .enumerate()
                .map(|(index, status)| PlanItemArg {
                    step: format!("step {index}"),
                    status,
                })
                .collect(),
        }
    }

    fn collector_with(outcome: SamplingToolOutcomeKind) -> SamplingRequestSignalCollector {
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(0, outcome, None));
        collector
    }

    #[test]
    fn yielded_tool_output_is_resumable_and_not_failure_evidence() {
        let kind = sampling_tool_outcome_kind(ToolOutputOutcome::Yielded, None);

        assert_eq!(kind, SamplingToolOutcomeKind::Yielded);
        assert!(!outcome_reopens_failure_evidence(kind, None));
    }

    fn settled(mutation_revision: u64) -> SamplingRequestSettledState {
        SamplingRequestSettledState {
            mutation_revision,
            tool_exposure_revision: 0,
        }
    }

    // Retention fixtures cannot authorize reuse: their watcher identity is invalid.
    // The freshness regression uses a real cache registration instead.
    fn record_test_replay_dependencies(collector: &SamplingRequestSignalCollector, ordinal: u64) {
        let evidence = serde_json::from_value(json!({
            "watcher_epoch": 0, "watcher_generation": 0, "registration_generation": 0,
            "repo_root": std::env::temp_dir(), "path": std::env::temp_dir().join("replay-source"), "recursive": false,
        })).unwrap();
        collector.record_replay_dependencies(
            ordinal,
            collector.request_mutation_revision,
            vec![evidence],
            None,
        );
    }

    fn record_invocation_result(
        collector: &SamplingRequestSignalCollector,
        tool_name: ToolName,
        payload: ToolPayload,
        call_id: &str,
        outcome: ToolOutputOutcome,
    ) {
        let registration =
            collector.register_deterministic_tool_call(&tool_name, &payload, call_id);
        collector.record_validation_workspace_revision(&tool_name, &payload, 0);
        record_test_replay_dependencies(collector, registration.ordinal);
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(outcome),
            None,
            &if validation_invocation_status(&tool_name, &payload).2 {
                runner_tool_response(call_id, "Ran 1 test in 0.001s\nOK")
            } else {
                successful_tool_response(call_id, r#"{"status":"complete"}"#)
            },
            false,
        );
    }

    fn validation_proof_payload() -> ToolPayload {
        ToolPayload::Function {
            arguments: serde_json::json!({
                "cmd": "python -m unittest -q",
            })
            .to_string(),
        }
    }

    fn final_diff_status_payload() -> ToolPayload {
        ToolPayload::Function {
            arguments: serde_json::json!({
                "cmd": "git diff --check && git status --short",
            })
            .to_string(),
        }
    }

    fn recorded_validation_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        outcome: ToolOutputOutcome,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            validation_proof_payload(),
            "validation-call",
            outcome,
        );
        collector
    }

    fn settle_plan(control: &mut TurnExecutionControl, plan: UpdatePlanArgs) {
        let baselines = control.baselines(0);
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(
            0,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        control.settle(&baselines, &collector, &settled(0));
    }

    fn observe_no_progress(control: &mut TurnExecutionControl) {
        let baselines = control.baselines(0);
        assert!(!control.observe_budget_progress(
            &baselines,
            &SamplingRequestSignalCollector::default(),
            &settled(0),
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn soft_convergence_is_once_per_turn_and_only_on_a_needed_continuation() {
        let mut control = TurnExecutionControl::new();
        assert!(control.take_soft_convergence_directive(true).is_none());
        tokio::time::advance(SOFT_CONVERGENCE_AFTER - std::time::Duration::from_millis(1)).await;
        for _ in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS {
            observe_no_progress(&mut control);
        }
        assert!(control.take_soft_convergence_directive(true).is_none());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(control.take_soft_convergence_directive(false).is_none());
        let directive = control
            .take_soft_convergence_directive(true)
            .expect("soft intervention");
        assert_eq!(directive, SOFT_CONVERGENCE_DIRECTIVE);
        assert!(directive.contains("complete required implementation or validation"));
        assert!(directive.contains("Preserve the requested scope"));
        assert!(directive.contains("do not cancel running work"));
        assert!(directive.contains("abandon obtainable required evidence"));
        assert!(directive.contains("does not interrupt an in-flight request"));
        control.reset_convergence();
        tokio::time::advance(SOFT_CONVERGENCE_AFTER).await;
        assert!(control.take_soft_convergence_directive(true).is_none());
        assert!(
            !control
                .initial_generation_request(&control.baselines(0))
                .terminal_completion_only
        );
        let mut next_turn = TurnExecutionControl::new();
        assert!(next_turn.take_soft_convergence_directive(true).is_none());
        tokio::time::advance(SOFT_CONVERGENCE_AFTER).await;
        for _ in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS {
            observe_no_progress(&mut next_turn);
        }
        assert_eq!(
            next_turn.take_soft_convergence_directive(true).as_deref(),
            Some(SOFT_CONVERGENCE_DIRECTIVE)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn soft_convergence_waits_for_generations_without_progress() {
        let mut control = TurnExecutionControl::new();
        tokio::time::advance(SOFT_CONVERGENCE_AFTER).await;
        // Time alone is not a stall: an investigation that keeps producing new
        // evidence or mutations must not be told to stop exploring.
        for _ in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS - 1 {
            observe_no_progress(&mut control);
        }
        assert!(control.take_soft_convergence_directive(true).is_none());
        let baselines = control.baselines(0);
        assert!(control.observe_budget_progress(
            &baselines,
            &SamplingRequestSignalCollector::default(),
            &settled(1),
        ));
        for _ in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS - 1 {
            observe_no_progress(&mut control);
        }
        assert!(
            control.take_soft_convergence_directive(true).is_none(),
            "a mutation resets the no-progress streak"
        );
        observe_no_progress(&mut control);
        assert_eq!(
            control.take_soft_convergence_directive(true).as_deref(),
            Some(SOFT_CONVERGENCE_DIRECTIVE)
        );
    }

    #[test]
    fn successful_read_without_original_dependencies_is_not_retained() {
        let collector = SamplingRequestSignalCollector::default();
        let call = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &ToolPayload::Function {
                arguments: r#"{"cmd":"cat src/lib.rs"}"#.into(),
            },
            "read",
        );
        collector.record_response_result(
            call.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("read", "source"),
            false,
        );
        assert!(collector.successful_replay_candidates().is_empty());
        assert!(
            collector
                .state
                .lock()
                .unwrap()
                .successful_replay_responses
                .is_empty()
        );
    }

    #[test]
    fn failed_artifact_read_requires_failure_diagnosis() {
        let control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        record_invocation_result(
            &collector,
            ToolName::plain("read_tool_output"),
            ToolPayload::Function {
                arguments: r#"{"artifact_id":"missing"}"#.into(),
            },
            "read",
            ToolOutputOutcome::Failure,
        );
        assert_eq!(
            collector.generation_purpose(&baseline, &settled(0), false, false),
            Some(TurnTimingGenerationPurpose::FailureDiagnosis)
        );
    }

    #[test]
    fn replayed_validation_is_not_new_execution_or_external_progress() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &validation_proof_payload(),
            "reused-validation",
        );
        let response = runner_tool_response("reused-validation", "Ran 1 test in 0.01s\n\nOK\n");
        collector.record_replayed_response_result(registration.ordinal, &response);
        assert_eq!(collector.executed_validation_summary().count, 0);
        assert!(
            !collector
                .progress_kinds(&baselines, &settled(0))
                .contains(&TurnTimingProgressKind::ValidationResult)
        );
        assert_eq!(
            collector.successful_replay_candidates().len(),
            0,
            "validation output without complete dependency evidence cannot authorize replay"
        );

        let executed = control.collector(&baselines);
        let registration = executed.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &validation_proof_payload(),
            "executed-validation",
        );
        executed.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &response,
            false,
        );
        assert_eq!(executed.executed_validation_summary().count, 1);
        assert!(
            executed
                .progress_kinds(&baselines, &settled(0))
                .contains(&TurnTimingProgressKind::ValidationResult)
        );
    }

    #[test]
    fn completion_evidence_tracks_result_changes_without_call_id_noise() {
        let control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let evidence = |call_id: &str, output: &str, replayed: bool| {
            let collector = control.collector(&baseline);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                call_id,
            );
            let response = runner_tool_response(call_id, output);
            if replayed {
                collector.record_replayed_response_result(registration.ordinal, &response);
            } else {
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                    None,
                    &response,
                    false,
                );
            }
            collector.completion_evidence_key()
        };
        let first = evidence("first", "observed state A", false);
        assert!(first.is_some());
        assert_eq!(
            first,
            evidence("different-call-id", "observed state A", false)
        );
        assert_ne!(first, evidence("first", "observed state B", false));
        assert_eq!(evidence("replayed", "observed state A", true), None);
    }

    #[test]
    fn deferred_tool_activation_changes_relevant_state_identity() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines_with_tool_exposure_revision(0, 4);
        let settled = SamplingRequestSettledState {
            mutation_revision: 0,
            tool_exposure_revision: 5,
        };

        assert_ne!(
            baselines.revision_key(),
            control.settled_revision_key(&settled)
        );
    }

    #[test]
    fn retryable_exec_and_mcp_failures_do_not_arm_pre_dispatch_suppression() {
        let cases = [
            (
                ToolName::plain("exec_command"),
                ToolPayload::Function {
                    arguments: r#"{"cmd":"cargo test -p codex-core focused"}"#.to_string(),
                },
            ),
            (
                ToolName::namespaced("mcp__example__", "read"),
                ToolPayload::Function {
                    arguments: r#"{"uri":"memo://codex/example-note"}"#.to_string(),
                },
            ),
        ];

        for (tool_name, payload) in cases {
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            let settled_state = settled(0);
            let first = control.collector(&baseline);
            let registration =
                first.register_deterministic_tool_call(&tool_name, &payload, "transient-failure");
            first.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(json!({
                    "failure_signature": "io.locked",
                    "retryable": true,
                })),
                &successful_tool_response(
                    "transient-failure",
                    r#"{"failure_signature":"io.locked","retryable":true}"#,
                ),
                false,
            );

            assert_eq!(
                control.evaluate_convergence(&baseline, &first, &settled_state),
                SamplingConvergenceDecision::default()
            );
            let retry = control.collector(&baseline);
            assert!(
                retry
                    .register_deterministic_tool_call(
                        &tool_name,
                        &payload,
                        "transient-failure-retry",
                    )
                    .suppressed_failure
                    .is_none(),
                "retryable {tool_name} failure must dispatch again"
            );
        }
    }

    #[test]
    fn nested_application_retryable_field_does_not_arm_suppression() {
        let tool_name = ToolName::plain("exec_command");
        let payload = ToolPayload::Function {
            arguments: r#"{"cmd":"cargo test -p codex-core focused"}"#.to_string(),
        };
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let settled_state = settled(0);
        let first = control.collector(&baseline);
        let registration =
            first.register_deterministic_tool_call(&tool_name, &payload, "application-failure");
        first.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({
                "failure_signature": "io.locked",
                "payload": {"retryable": false},
            })),
            &successful_tool_response(
                "application-failure",
                r#"{"failure_signature":"io.locked","payload":{"retryable":false}}"#,
            ),
            false,
        );

        assert_eq!(
            control.evaluate_convergence(&baseline, &first, &settled_state),
            SamplingConvergenceDecision::default()
        );
        let retry = control.collector(&baseline);
        assert!(
            retry
                .register_deterministic_tool_call(
                    &tool_name,
                    &payload,
                    "application-failure-retry",
                )
                .suppressed_failure
                .is_none(),
            "nested application data must not classify a failure as terminal"
        );
    }

    #[test]
    fn mcp_application_retryable_field_is_not_control_metadata() {
        let collector = SamplingRequestSignalCollector::default();
        let tool_name = ToolName::namespaced("mcp__example__", "read");
        let payload = ToolPayload::Function {
            arguments: r#"{"uri":"memo://codex/example-note"}"#.to_string(),
        };
        let result = json!({
            "content": [{"type": "text", "text": "application result"}],
            "isError": true,
            "retryable": false,
        });
        let signal = crate::tools::context::semantic_failure_sampling_signal(result.clone());

        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "mcp-cell",
            tool_name: &tool_name,
            payload: &payload,
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            signal: Some(&signal),
            result: &result,
            canonical_artifact_required: false,
        });

        assert!(!collector.snapshot()[0].failure_is_terminal);
    }

    #[test]
    fn direct_mcp_application_retryable_field_is_not_control_metadata() {
        let collector = SamplingRequestSignalCollector::default();
        let result = json!({
            "content": [{"type": "text", "text": "application result"}],
            "isError": true,
            "retryable": false,
        });
        let signal = crate::tools::context::semantic_failure_sampling_signal(result.clone());
        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "mcp-call".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                result.to_string(),
            ),
        };

        collector.record_response_result(
            collector.register_tool_call(),
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(signal),
            &response,
            false,
        );

        assert!(!collector.snapshot()[0].failure_is_terminal);
    }

    #[test]
    fn powershell_pipeline_uses_shared_validation_classification() {
        let collector = SamplingRequestSignalCollector::default();
        collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &ToolPayload::Function {
                arguments: serde_json::json!({
                    "kind": "powershell_script",
                    "script_body": "cargo test -p codex-core | Select-Object -First 1",
                })
                .to_string(),
            },
            "powershell-validation",
        );

        assert!(collector.saw_validation());
    }

    #[test]
    fn executed_validation_summary_requires_a_completed_non_skipped_result() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(0);

        let registered_only = control.collector(&baselines);
        registered_only.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &ToolPayload::Function {
                arguments: r#"{"cmd":"cargo test -p codex-core focused"}"#.to_string(),
            },
            "registered-only",
        );
        registered_only.record_child_runtime(25);
        assert_eq!(
            registered_only.executed_validation_summary(),
            ExecutedValidationSummary::default()
        );

        let skipped =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Skipped);
        skipped.record_child_runtime(50);
        assert_eq!(
            skipped.executed_validation_summary(),
            ExecutedValidationSummary::default()
        );

        let completed =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        completed.record_child_runtime(125);
        assert_eq!(
            completed.executed_validation_summary(),
            ExecutedValidationSummary {
                count: 1,
                duration_ms: 125,
            }
        );

        let mixed = control.collector(&baselines);
        record_invocation_result(
            &mixed,
            ToolName::plain("read_tool_output"),
            ToolPayload::Function {
                arguments: r#"{"artifact_id":"artifact-1"}"#.to_string(),
            },
            "read-call",
            ToolOutputOutcome::Success,
        );
        mixed.record_child_runtime(10);
        record_invocation_result(
            &mixed,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: r#"{"cmd":"cargo test -p codex-core focused"}"#.to_string(),
            },
            "mixed-validation",
            ToolOutputOutcome::Success,
        );
        mixed.record_child_runtime(100);
        assert_eq!(
            mixed.executed_validation_summary(),
            ExecutedValidationSummary {
                count: 1,
                duration_ms: 0,
            },
            "unkeyed child runtimes must not be attributed across a mixed request"
        );
    }

    #[test]
    fn fresh_successful_validation_preserves_model_decisions() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let baselines = control.baselines(0);
        let settled_state = settled(0);
        let collector =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &collector, &settled_state);

        assert!(collector.fresh_successful_validation().is_some());
        assert!(control.unresolved_failures.is_empty());
        let decision = control.evaluate_convergence(&baselines, &collector, &settled_state);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(decision.directive.is_none());
        assert!(!decision.proven_loop_activated);

        let continuation =
            control.continuation_generation_request(&baselines, &collector, &settled_state, false);
        assert!(!continuation.terminal_completion_only);
        assert_eq!(
            continuation.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
    }

    #[test]
    fn recognized_validation_records_proof_across_shapes_without_required_metadata() {
        for (tool, arguments) in [
            ("exec_command", json!({"cmd": "python -m unittest -q"})),
            (
                "exec_command",
                json!({"cmd": "just core-test-fast core_lib -E test(parser)"}),
            ),
            (
                "exec_command",
                json!({"cmd": "just core-gate tool-output-recovery"}),
            ),
            ("exec_command", json!({"cmd": "uv run pytest -q"})),
            (
                "exec_command",
                json!({"kind": "argv", "program": "python", "args": ["scripts/rust_test_runner.py", "run-target", "core_lib", "-E", "test(parser)"]}),
            ),
            (
                "exec_command",
                json!({"kind": "script", "cmd": "python -m unittest -q"}),
            ),
            (
                "exec_command",
                json!({"kind": "argv", "program": "python", "args": ["-m", "unittest", "-q"]}),
            ),
            (
                "exec_command",
                json!({"program": "python", "args": ["-m", "unittest", "-q"]}),
            ),
            (
                "exec_command",
                json!({"kind": "powershell_script", "script_body": "python -m unittest -q"}),
            ),
            ("shell_command", json!({"command": "python -m unittest -q"})),
            (
                "exec_command",
                json!({"cmd": "python -m unittest -q", "validation": {"covered_paths": ["src"]}}),
            ),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let settled_state = settled(0);
            let collector = control.collector(&baselines);
            record_invocation_result(
                &collector,
                ToolName::plain(tool),
                ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                "validation",
                ToolOutputOutcome::Success,
            );
            collector.record_child_runtime(25);
            control.settle(&baselines, &collector, &settled_state);

            assert_eq!(
                collector.executed_validation_summary(),
                ExecutedValidationSummary {
                    count: 1,
                    duration_ms: 25
                },
                "{tool}: {arguments}"
            );
            assert!(collector.fresh_successful_validation().is_some());
            let decision = control.evaluate_convergence(&baselines, &collector, &settled_state);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired,
                "{tool}: {arguments}"
            );
            assert!(decision.directive.is_none());
            assert!(!decision.proven_loop_activated);
        }
    }

    #[test]
    fn masked_or_skipped_validation_cannot_create_proof_or_replay_success() {
        let temp = tempfile::tempdir().expect("validation fixture");
        std::fs::write(
            temp.path().join("test_failing.py"),
            "import unittest\nclass Failing(unittest.TestCase):\n    def test_failure(self):\n        self.fail('validation review sentinel')\n",
        )
        .expect("write failing unittest");
        let python = if cfg!(windows) { "python" } else { "python3" };
        let shell = if cfg!(windows) { "pwsh" } else { "sh" };
        let shell_flags: &[&str] = if cfg!(windows) {
            &["-NoProfile", "-Command"]
        } else {
            &["-c"]
        };
        for (script, failure_visible) in [
            (format!("{python} -m unittest -q; exit 0"), true),
            (
                format!("{python} -c \"pass\" || {python} -m unittest -q"),
                false,
            ),
        ] {
            let output = std::process::Command::new(shell)
                .args(shell_flags)
                .arg(&script)
                .current_dir(temp.path())
                .output()
                .expect("execute validation fixture");
            assert!(
                output.status.success(),
                "compound command masks test outcome"
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stderr).contains("validation review sentinel"),
                failure_visible,
                "{script}"
            );
            let mut wrapper_args = shell_flags
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            wrapper_args.push(script.clone());
            for arguments in [
                json!({"cmd": script}),
                json!({"kind": "argv", "program": shell, "args": wrapper_args}),
                json!({"kind": "powershell_script", "script_body": script}),
            ] {
                for nested in [false, true] {
                    let mut control = TurnExecutionControl::new();
                    settle_plan(&mut control, plan(&[StepStatus::Completed]));
                    let baselines = control.baselines(0);
                    let collector = control.collector(&baselines);
                    let payload = ToolPayload::Function {
                        arguments: arguments.to_string(),
                    };
                    if nested {
                        collector.record_code_mode_result(CodeModeToolResult {
                            cell_id: "compound-validation",
                            tool_name: &ToolName::plain("exec_command"),
                            payload: &payload,
                            source_dependencies: None,
                            outcome_context: ToolOutputOutcomeContext::new(
                                ToolOutputOutcome::Success,
                            ),
                            signal: None,
                            result: &json!({"exit_code": output.status.code()}),
                            canonical_artifact_required: false,
                        });
                    } else {
                        record_invocation_result(
                            &collector,
                            ToolName::plain("exec_command"),
                            payload.clone(),
                            "compound-validation",
                            ToolOutputOutcome::Success,
                        );
                    }
                    control.settle(&baselines, &collector, &settled(0));
                    assert!(
                        collector.fresh_successful_validation().is_none(),
                        "{arguments}; nested={nested}"
                    );
                    let next = control.collector(&control.baselines(0));
                    let replay = next.register_deterministic_tool_call(
                        &ToolName::plain("exec_command"),
                        &payload,
                        "repeat-compound",
                    );
                    assert!(
                        replay
                            .replayed_success
                            .and_then(|guard| guard.response_for_call("repeat-compound"))
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn metadata_cannot_turn_non_validation_commands_into_proof() {
        for command in [
            "git status --short",
            "echo 'python -m unittest -q'",
            "unknown-test-runner",
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let settled_state = settled(0);
            let collector = control.collector(&baselines);
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                ToolPayload::Function {
                    arguments: json!({
                        "cmd": command,
                        "validation": {"covered_paths": ["src"]},
                    })
                    .to_string(),
                },
                "not-validation",
                ToolOutputOutcome::Success,
            );
            control.settle(&baselines, &collector, &settled_state);
            assert_eq!(collector.executed_validation_summary().count, 0);
            assert!(
                collector.fresh_successful_validation().is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn nested_code_mode_validation_without_metadata_requires_successful_completion() {
        for outcome in [
            ToolOutputOutcome::Success,
            ToolOutputOutcome::Failure,
            ToolOutputOutcome::Skipped,
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let settled_state = settled(0);
            let collector = control.collector(&baselines);
            collector.record_validation_workspace_revision(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                0,
            );
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "validation-cell",
                tool_name: &ToolName::plain("exec_command"),
                payload: &validation_proof_payload(),
                source_dependencies: None,
                outcome_context: ToolOutputOutcomeContext::new(outcome),
                signal: None,
                // The runner output proves the tests ran; this case isolates the
                // outcome, not the execution evidence.
                result: &json!({
                    "exit_code": if outcome == ToolOutputOutcome::Success { 0 } else { 1 },
                    "output": "Ran 1 test in 0.001s\nOK",
                }),
                canonical_artifact_required: false,
            });
            control.settle(&baselines, &collector, &settled_state);
            assert_eq!(
                collector.fresh_successful_validation().is_some(),
                outcome == ToolOutputOutcome::Success,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn failed_skipped_or_incomplete_validation_cannot_create_proof() {
        for outcome in [ToolOutputOutcome::Failure, ToolOutputOutcome::Skipped] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let settled_state = settled(0);
            let collector = recorded_validation_collector(&control, &baselines, outcome);
            control.settle(&baselines, &collector, &settled_state);

            assert!(collector.fresh_successful_validation().is_none());
        }

        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let baselines = control.baselines(0);
        let settled_state = settled(0);
        let collector = control.collector(&baselines);
        collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &validation_proof_payload(),
            "incomplete-validation",
        );
        control.settle(&baselines, &collector, &settled_state);
        assert!(collector.fresh_successful_validation().is_none());
    }

    #[test]
    fn final_diff_status_observation_requires_exact_git_executable() {
        for (program, preserves_validation) in [
            ("git", true),
            ("git.exe", true),
            ("GIT.EXE", true),
            ("git.exe.exe", false),
            ("git.EXE.EXE", false),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let collector =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                ToolPayload::Function {
                    arguments: json!({
                        "cmd": format!("{program} diff --check && {program} status --short")
                    })
                    .to_string(),
                },
                "final-diff-status",
                ToolOutputOutcome::Success,
            );
            let settled_state = settled(0);
            control.settle(&baselines, &collector, &settled_state);
            assert_eq!(
                collector.fresh_successful_validation().is_some(),
                preserves_validation,
                "executable: {program}"
            );
        }
    }

    #[test]
    fn source_convergence_requires_exact_executable_identity() {
        for program in ["rg.exe", "RG.EXE", "rg.exe.exe", "rg.EXE.EXE"] {
            for direct_argv in [true, false] {
                let mut control = TurnExecutionControl::new();
                settle_plan(&mut control, plan(&[StepStatus::Completed]));
                let (baselines, settled) = unchanged_state(&control);
                let arguments = if direct_argv {
                    json!({"kind": "argv", "program": program, "args": ["--files"]})
                } else {
                    json!({"cmd": format!("{program} --files")})
                };
                for generation in 0..2 {
                    let collector = control.collector(&baselines);
                    let registration = collector.register_deterministic_tool_call(
                        &ToolName::plain("exec_command"),
                        &ToolPayload::Function {
                            arguments: arguments.to_string(),
                        },
                        "source-call",
                    );
                    collector.record_response_result(
                        registration.ordinal,
                        ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                        None,
                        &successful_tool_response("source-call", "src/lib.rs"),
                        false,
                    );
                    let decision = control.evaluate_convergence(&baselines, &collector, &settled);
                    assert_eq!(
                        decision
                            .directive
                            .as_deref()
                            .is_some_and(|directive| directive.starts_with(
                                "Convergence advisory: the broad source pass returned the same evidence"
                            )),
                        generation == 1 && matches!(program, "rg.exe" | "RG.EXE"),
                        "program={program}, direct_argv={direct_argv}, generation={generation}"
                    );
                }
            }
        }
    }

    #[test]
    fn unsafe_final_observations_do_not_preserve_validation_proof() {
        {
            let extra_payload = ToolPayload::Function {
                arguments:
                    r#"{"cmd":"git diff --check | Out-File result.txt; git status --short"}"#
                        .to_string(),
            };
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let collector =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                final_diff_status_payload(),
                "first-final-observation",
                ToolOutputOutcome::Success,
            );
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                extra_payload,
                "extra-final-observation",
                ToolOutputOutcome::Success,
            );
            let settled_state = settled(0);
            control.settle(&baselines, &collector, &settled_state);

            assert!(collector.fresh_successful_validation().is_none());
        }
        assert!(!final_diff_status_script_is_read_only(
            "git diff --check | Out-File result.txt; git status --short"
        ));
        for script in [
            "git diff --output=result.txt; git status --short",
            "git diff --output result.txt; git status --short",
            "git diff --ext-diff; git status --short",
            "git diff --textconv; git status --short",
        ] {
            assert!(!final_diff_status_script_is_read_only(script), "{script}");
        }
    }

    #[test]
    fn a_fresh_validation_resolves_prior_failure_without_forcing_completion() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let failed_baselines = control.baselines(0);
        let failed =
            recorded_validation_collector(&control, &failed_baselines, ToolOutputOutcome::Failure);
        control.settle(&failed_baselines, &failed, &settled(0));
        assert!(!control.unresolved_failures.is_empty());

        let recovery_baselines = control.baselines(0);
        let recovered = recorded_validation_collector(
            &control,
            &recovery_baselines,
            ToolOutputOutcome::Success,
        );
        control.settle(&recovery_baselines, &recovered, &settled(0));
        assert!(control.unresolved_failures.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&recovery_baselines, &recovered, &settled(0))
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn repeated_force_fresh_calls_never_return_a_replayed_response() {
        for arguments in [
            json!({"kind": "argv", "program": "rg", "args": ["--files", "src"], "force_fresh": true}),
            json!({"cmd": "python -m unittest -q", "force_fresh": true}),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let payload = ToolPayload::Function {
                arguments: arguments.to_string(),
            };
            for call_id in ["fresh-1", "fresh-2", "fresh-3"] {
                let baselines = control.baselines(0);
                let collector = control.collector(&baselines);
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &payload,
                    call_id,
                );
                assert!(
                    registration
                        .replayed_success
                        .and_then(|guard| guard.response_for_call(call_id))
                        .is_none(),
                    "{call_id} must execute instead of returning cached output: {arguments}"
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                    None,
                    &successful_tool_response(call_id, "fresh result"),
                    false,
                );
                control.settle(&baselines, &collector, &settled(0));
            }
        }
    }

    #[test]
    fn structured_text_failure_retains_its_fingerprint() {
        let collector = SamplingRequestSignalCollector::default();
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments: r#"{"artifact_id":"missing"}"#.to_string(),
            },
            "failure",
        );
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            None,
            &structured_text_response("failure", &[r#"{"failure_signature":"missing-artifact"}"#]),
            false,
        );
        assert_eq!(
            collector.snapshot()[0].failure_fingerprint.as_deref(),
            Some("missing-artifact")
        );
        assert_eq!(
            collector.deterministic_cycle().expect("failure cycle").kind,
            DeterministicCycleKind::ToolFailure
        );
    }

    #[test]
    fn structured_text_read_replays_exact_content_and_respects_limits() {
        use codex_protocol::models::FunctionCallOutputContentItem;

        let payload = ToolPayload::Function {
            arguments: r#"{"cmd":"cat src/lib.rs"}"#.to_string(),
        };
        for case in ["text", "oversized", "image"] {
            let mut control = TurnExecutionControl::new();
            let baselines = control.baselines(0);
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &payload,
                "read",
            );
            let mut response = structured_text_response("read", &["first line", "second line"]);
            if let ResponseInputItem::FunctionCallOutput { output, .. } = &mut response {
                let items = output.content_items_mut().expect("structured content");
                match case {
                    "oversized" => items.push(FunctionCallOutputContentItem::InputText {
                        text: " ".repeat(SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT),
                    }),
                    "image" => items.push(FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AA==".to_string(),
                        detail: None,
                    }),
                    _ => {}
                }
            }
            record_test_replay_dependencies(&collector, registration.ordinal);
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None,
                &response,
                false,
            );
            control.settle(&baselines, &collector, &settled(0));
            let retry = control.collector(&control.baselines(0));
            let replay = retry
                .register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &payload,
                    "retry",
                )
                .replayed_success;
            if case == "text" {
                assert_eq!(
                    replay
                        .expect("text array should replay")
                        .response_for_call("retry"),
                    Some(structured_text_response(
                        "retry",
                        &["first line", "second line"]
                    ))
                );
            } else {
                assert!(replay.is_none(), "{case} must execute through dispatch");
            }
        }
    }

    #[test]
    fn structured_text_wait_retains_authoritative_result_fields() {
        let collector = SamplingRequestSignalCollector::default();
        let tool = ToolName::plain("wait_agent");
        let payload = ToolPayload::Function {
            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
        };
        collector.register_deterministic_tool_call(&tool, &payload, "wait");
        let value =
            json!({"typed_deltas":[{"assignment_id":"assignment-1"}], "status":"completed"});
        collector.record_direct_wait_owner_result(
            true,
            &tool,
            &payload,
            Some(&json!({
                "authoritative_wait_owner_v1": {
                    "adapter": "multi_agent_v2",
                    "disposition": "terminal",
                    "owner": "child-1",
                    "state_revision": "completed"
                }
            })),
            &structured_text_response("wait", &[&value.to_string()]),
        );
        let observation = collector
            .authoritative_wait_observation()
            .expect("authoritative wait");
        assert_eq!(observation.result.value, value);
        assert_eq!(observation.assignment_ids, vec!["assignment-1".to_string()]);
    }

    fn structured_text_response(call_id: &str, segments: &[&str]) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_content_items(
                segments
                    .iter()
                    .map(
                        |text| codex_protocol::models::FunctionCallOutputContentItem::InputText {
                            text: (*text).to_string(),
                        },
                    )
                    .collect(),
            ),
        }
    }

    #[tokio::test]
    async fn successful_read_replays_only_for_the_exact_unchanged_state() {
        let root = tempfile::TempDir::new().unwrap();
        let source = root.path().join("source.rs");
        std::fs::write(&source, "original").unwrap();
        let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
        let observations = cache
            .begin_source_path_change_observations(root.path(), &[(source, false)])
            .await
            .unwrap();
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({
                "kind": "argv",
                "program": "rg",
                "args": ["--files", "codex-rs/core/src"],
            })
            .to_string(),
        };
        let baselines = control.baselines(0);
        let first = control.collector(&baselines);
        let registration = first.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &payload,
            "first-read",
        );
        first.record_replay_dependencies(registration.ordinal, 0, observations, None);
        first.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("first-read", r#"{"status":"complete"}"#),
            false,
        );
        control.settle(&baselines, &first, &settled(0));

        let unchanged_baselines = control.baselines(0);
        let unchanged = control.collector(&unchanged_baselines);
        let replay = unchanged.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &payload,
            "replayed-read",
        );
        let guard = replay
            .replayed_success
            .expect("unchanged read should replay");
        assert!(guard.is_fresh(0, &cache, None));
        assert!(!guard.is_fresh(1, &cache, None));
        cache.note_host_workspace_mutation();
        assert!(
            !guard.is_fresh(0, &cache, None),
            "external changes invalidate the original proof"
        );
        let replayed_response = guard
            .response_for_call("replayed-read")
            .expect("replayed response should retain a call id");
        assert!(matches!(
            replayed_response,
            ResponseInputItem::FunctionCallOutput { call_id, .. } if call_id == "replayed-read"
        ));

        let changed_baselines = control.baselines(1);
        let changed = control.collector(&changed_baselines);
        assert!(
            changed
                .register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &payload,
                    "changed-state-read",
                )
                .replayed_success
                .is_none(),
            "a mutation revision must invalidate read replay"
        );
    }

    #[test]
    fn replay_retention_is_bounded_and_ambiguous_outcomes_are_excluded() {
        let control = TurnExecutionControl::new();
        let collector = control.collector(&control.baselines(0));
        for index in 0..SUCCESSFUL_REPLAY_GATE_LIMIT + 5 {
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                ToolPayload::Function {
                    arguments: json!({"cmd": format!("cat file-{index}")}).to_string(),
                },
                &format!("read-{index}"),
                ToolOutputOutcome::Success,
            );
        }
        assert_eq!(
            collector.successful_replay_candidates().len(),
            SUCCESSFUL_REPLAY_GATE_LIMIT
        );
        let oversized = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &ToolPayload::Function {
                arguments: json!({"cmd": "cat oversized"}).to_string(),
            },
            "oversized",
        );
        record_test_replay_dependencies(&collector, oversized.ordinal);
        collector.record_response_result(
            oversized.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response(
                "oversized",
                &"x".repeat(SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT + 1),
            ),
            false,
        );
        assert!(
            !collector
                .state
                .lock()
                .unwrap()
                .successful_replay_responses
                .contains_key(&oversized.ordinal)
        );
        let duplicate = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &ToolPayload::Function {
                arguments: json!({"cmd": "cat duplicate"}).to_string(),
            },
            "duplicate",
        );
        record_test_replay_dependencies(&collector, duplicate.ordinal);
        collector.record_response_result(
            duplicate.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("duplicate", "small"),
            false,
        );
        collector.record_failure(duplicate.ordinal, "conflicting result", true);
        assert_eq!(
            collector.successful_replay_candidates().len(),
            SUCCESSFUL_REPLAY_GATE_LIMIT - 2
        );
        assert!(
            !collector
                .successful_replay_candidates()
                .iter()
                .any(|(_, response, _)| matches!(response,
            ResponseInputItem::FunctionCallOutput { call_id, .. } if call_id == "duplicate"))
        );
    }

    #[test]
    fn missing_timing_never_opens_the_negligible_runtime_guard() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        for generation in 0..3 {
            let collector = high_volume_tool_pass_collector(
                &control,
                &baselines,
                &format!("changed-{generation}"),
            );
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert!(decision.directive.is_none());
            assert!(control.turn_efficiency_guard.is_none());
            assert_eq!(control.turn_efficiency_tool_calls, 0);
        }
    }

    #[test]
    fn uncertain_native_read_commands_and_namespaced_lookalikes_execute_normally() {
        for (tool, arguments) in [
            (
                ToolName::plain("exec_command"),
                r#"{"cmd":"git diff --output=report.txt"}"#,
            ),
            (
                ToolName::plain("exec_command"),
                r#"{"cmd":"rg --pre=generator needle src"}"#,
            ),
            (
                ToolName::plain("exec_command"),
                r#"{"cmd":"cat $(touch changed) file"}"#,
            ),
            (
                ToolName::plain("exec_command"),
                r#"{"kind":"argv","program":"git","args":["diff","--ext-diff"]}"#,
            ),
            (
                ToolName {
                    namespace: Some("external".to_string()),
                    name: "read_tool_output".to_string(),
                },
                "{}",
            ),
            (ToolName::plain("external_read_tool_output"), "{}"),
        ] {
            let mut control = TurnExecutionControl::new();
            let baselines = control.baselines(0);
            let payload = ToolPayload::Function {
                arguments: arguments.to_string(),
            };
            let collector = control.collector(&baselines);
            record_invocation_result(
                &collector,
                tool.clone(),
                payload.clone(),
                "first",
                ToolOutputOutcome::Success,
            );
            control.settle(&baselines, &collector, &settled(0));
            let next = control.collector(&control.baselines(0));
            assert!(
                next.register_deterministic_tool_call(&tool, &payload, "next")
                    .replayed_success
                    .is_none(),
                "{tool:?}: {arguments}"
            );
        }
    }

    #[test]
    fn read_before_shell_mutation_is_not_relabelled_with_the_settled_revision() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(10);
        let collector = control.collector(&baseline);
        let payload = ToolPayload::Function {
            arguments: r#"{"cmd":"cat src/lib.rs"}"#.to_string(),
        };
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            payload.clone(),
            "read",
            ToolOutputOutcome::Success,
        );
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: r#"{"cmd":"echo changed > src/lib.rs"}"#.to_string(),
            },
            "write",
            ToolOutputOutcome::Success,
        );
        control.settle(&baseline, &collector, &settled(11));
        let next = control.collector(&control.baselines(11));
        assert!(
            next.register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &payload,
                "reread"
            )
            .replayed_success
            .is_none()
        );
    }

    #[test]
    fn equal_unscoped_output_does_not_merge_different_observations() {
        for signal in [
            None,
            Some(json!({"kind":"semantic_evidence", "semantic_evidence":["same-body-hash"]})),
        ] {
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            let mut keys = std::collections::HashSet::new();
            for path in ["src/a", "src/b", "src/c"] {
                let collector = control.collector(&baseline);
                let payload = ToolPayload::Function {
                    arguments: json!({"cmd": format!("git status --short -- {path}")}).to_string(),
                };
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &payload,
                    "read",
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                    signal.clone(),
                    &successful_tool_response("read", ""),
                    false,
                );
                keys.insert(
                    collector
                        .deterministic_cycle_key()
                        .expect("source observation"),
                );
                assert_eq!(
                    control.evaluate_convergence(&baseline, &collector, &settled(0)),
                    SamplingConvergenceDecision::default()
                );
            }
            assert_eq!(keys.len(), 3);
        }
    }

    #[test]
    fn successful_non_read_command_is_not_cached_for_replay() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let payload = ToolPayload::Function {
            arguments: r#"{"cmd":"echo complete"}"#.to_string(),
        };
        let baselines = control.baselines(0);
        let first = control.collector(&baselines);
        record_invocation_result(
            &first,
            ToolName::plain("exec_command"),
            payload.clone(),
            "first-command",
            ToolOutputOutcome::Success,
        );
        control.settle(&baselines, &first, &settled(0));

        let repeated_baselines = control.baselines(0);
        let repeated = control.collector(&repeated_baselines);
        assert!(
            repeated
                .register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &payload,
                    "repeated-command",
                )
                .replayed_success
                .is_none()
        );
    }

    #[test]
    fn unfinished_plan_does_not_invalidate_fresh_validation_evidence() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        settle_plan(&mut control, plan(&[StepStatus::InProgress]));
        let baselines = control.baselines(0);
        let settled_state = settled(0);
        let collector =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &collector, &settled_state);

        assert!(collector.fresh_successful_validation().is_some());
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &collector, &settled_state)
                .continuation,
            ContinuationDisposition::ModelRequired,
        );
    }

    #[test]
    fn validation_never_completes_the_task_with_or_without_a_plan() {
        for scenario in [
            "no plan",
            "no plan after edit",
            "empty plan",
            "new input",
            "stale validation",
            "missing revision",
            "complete",
        ] {
            let mut control = TurnExecutionControl::new();
            if !scenario.starts_with("no plan") {
                settle_plan(
                    &mut control,
                    if scenario == "empty plan" {
                        plan(&[])
                    } else {
                        plan(&[StepStatus::Completed])
                    },
                );
            }
            let revision = u64::from(scenario == "no plan after edit");
            let baselines = control.baselines(revision);
            let collector =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            let settled_state = settled(revision);
            collector.record_validation_workspace_revision(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                revision,
            );
            if scenario == "stale validation" {
                collector.record_validation_workspace_revision(
                    &ToolName::plain("exec_command"),
                    &validation_proof_payload(),
                    1,
                );
            }
            if scenario == "missing revision" {
                collector.state.lock().unwrap().validation_mutation_revision = None;
            }
            assert_eq!(
                collector
                    .fresh_successful_validation()
                    .is_some_and(|validation| {
                        validation.mutation_revision == Some(settled_state.mutation_revision)
                    }),
                !matches!(scenario, "stale validation" | "missing revision"),
                "{scenario}",
            );
            control.settle(&baselines, &collector, &settled_state);
            if scenario == "new input" {
                control.input_revision += 1;
            }
            assert_eq!(
                control
                    .evaluate_convergence(&baselines, &collector, &settled_state)
                    .continuation,
                ContinuationDisposition::ModelRequired,
                "{scenario}"
            );
        }
    }

    #[test]
    fn nested_code_mode_dependencies_are_unioned_per_cell_and_fail_closed() {
        let collector = SamplingRequestSignalCollector::default();
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        for path in ["/repo/src/foo.rs", "/repo/src/bar.rs"] {
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "cell-scoped",
                tool_name: &ToolName::plain("exec_command"),
                payload: &payload,
                source_dependencies: Some(BTreeSet::from([SourceDependencyV1::new(
                    std::path::Path::new(path),
                    false,
                )])),
                outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                signal: None,
                result: &json!({"path": path}),
                canonical_artifact_required: false,
            });
        }
        assert_eq!(
            collector
                .code_mode_source_dependencies("cell-scoped")
                .expect("scoped cell dependencies"),
            BTreeSet::from([
                SourceDependencyV1::new(std::path::Path::new("/repo/src/foo.rs"), false),
                SourceDependencyV1::new(std::path::Path::new("/repo/src/bar.rs"), false),
            ])
        );

        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell-global",
            tool_name: &ToolName::plain("cargo_test"),
            payload: &payload,
            source_dependencies: Some(BTreeSet::new()),
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            signal: None,
            result: &json!({"status": "passed"}),
            canonical_artifact_required: false,
        });
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell-global",
            tool_name: &ToolName::plain("exec_command"),
            payload: &payload,
            source_dependencies: Some(BTreeSet::from([SourceDependencyV1::new(
                std::path::Path::new("/repo/src/foo.rs"),
                false,
            )])),
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            signal: None,
            result: &json!({"path": "/repo/src/foo.rs"}),
            canonical_artifact_required: false,
        });
        assert!(
            collector
                .code_mode_source_dependencies("cell-global")
                .expect("global cell dependencies")
                .is_empty()
        );
    }

    #[test]
    fn nested_code_mode_observation_records_the_result_evidence() {
        let collector = SamplingRequestSignalCollector::default();
        let tool_name = ToolName::plain("read_tool_output");
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        let nested_result = json!({
            "artifact_id": "artifact-1",
            "output": "retained nested evidence",
        });
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell-borrowed-result",
            tool_name: &tool_name,
            payload: &payload,
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            signal: None,
            result: &nested_result,
            canonical_artifact_required: false,
        });

        assert_eq!(collector.snapshot().len(), 1);
        let outcome = &collector.snapshot()[0];
        assert_eq!(outcome.kind, SamplingToolOutcomeKind::Success);
        assert!(outcome.nested_in_code_mode);
        assert_eq!(
            collector
                .state
                .lock()
                .unwrap()
                .evidence_items
                .get(&0)
                .map(String::as_str),
            Some("4c533d060cfe7157a1373cf2e83d837ecfd20198e08e2eb8e5ae2d6048e15483")
        );
    }

    fn unchanged_state(
        control: &TurnExecutionControl,
    ) -> (SamplingRequestBaselines, SamplingRequestSettledState) {
        let baselines = control.baselines(7);
        let settled = SamplingRequestSettledState {
            mutation_revision: 7,
            tool_exposure_revision: 0,
        };
        (baselines, settled)
    }

    fn authoritative_wait_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        identity: &str,
        mixed: bool,
        surfaceable_message: Option<&str>,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        {
            let mut state = collector
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.registered_count = if mixed { 2 } else { 1 };
            state.direct_wait_agent_count = 1;
            state.authoritative_wait_observations = vec![AuthoritativeWaitObservation {
                disposition: AuthoritativeWaitDisposition::Terminal,
                identity: identity.to_string(),
                owner: "owner-1".to_string(),
                state_revision: "revision-1".to_string(),
                action_identity: "wait-action".to_string(),
                result: AuthoritativeWaitOwnerResult {
                    adapter: "multi_agent_v2".to_string(),
                    value: json!({"message": "terminal owner result"}),
                    surfaceable_message: surfaceable_message.map(ToOwned::to_owned),
                },
                assignment_ids: Vec::new(),
            }];
        }
        collector
    }

    #[test]
    fn completed_child_with_designated_surface_keeps_parent_work_available() {
        for adapter in ["multi_agent_v2", "code_mode_cell"] {
            for surface in [None, Some("child result")] {
                let mut control = TurnExecutionControl::new();
                let (baselines, settled) = unchanged_state(&control);
                for _ in 0..3 {
                    let collector = control.collector(&baselines);
                    let signal = json!({"authoritative_wait_owner_v1": {
                        "adapter": adapter, "disposition": "terminal", "owner": "child-1",
                        "state_revision": "completed", "surfaceable_message": surface,
                    }});
                    if adapter == "multi_agent_v2" {
                        let tool_name = ToolName::plain("wait_agent");
                        let payload = ToolPayload::Function {
                            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
                        };
                        let registration = collector.register_deterministic_tool_call(
                            &tool_name,
                            &payload,
                            "wait-call",
                        );
                        let response = ResponseInputItem::FunctionCallOutput {
                            call_id: "wait-call".to_string(),
                            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                                "child finished".to_string(),
                            ),
                        };
                        collector.record_response_result(
                            registration.ordinal,
                            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                            None,
                            &response,
                            false,
                        );
                        collector.record_direct_wait_owner_result(
                            true,
                            &tool_name,
                            &payload,
                            Some(&signal),
                            &response,
                        );
                    } else {
                        let _registration = collector.register_deterministic_tool_call(
                            &ToolName::plain("exec"),
                            &ToolPayload::Function {
                                arguments: r#"{"code":"await tools.wait({cell_id: 'child-1'})"}"#
                                    .to_string(),
                            },
                            "exec-call",
                        );
                        collector.record_code_mode_result(CodeModeToolResult {
                            cell_id: "parent-cell",
                            tool_name: &ToolName::plain("wait"),
                            payload: &ToolPayload::Function {
                                arguments: r#"{"cell_id":"child-1"}"#.to_string(),
                            },
                            source_dependencies: None,
                            outcome_context: ToolOutputOutcomeContext::new(
                                ToolOutputOutcome::Success,
                            ),
                            signal: Some(&signal),
                            result: &json!("child finished"),
                            canonical_artifact_required: false,
                        });
                    }
                    assert_eq!(
                        collector
                            .authoritative_wait_observation()
                            .expect("real owner registration")
                            .result
                            .adapter,
                        adapter
                    );
                    assert_eq!(
                        control.evaluate_convergence(&baselines, &collector, &settled),
                        SamplingConvergenceDecision::default()
                    );
                    let request = control
                        .continuation_generation_request(&baselines, &collector, &settled, false);
                    assert!(!request.terminal_completion_only);
                    assert_eq!(
                        request.sampling,
                        SamplingGenerationDisposition::DecisionBearing
                    );
                }
            }
        }
    }

    #[test]
    fn code_mode_wait_surface_requires_explicit_owner_designation() {
        let tool_name = ToolName::plain("wait");
        let payload = ToolPayload::Function {
            arguments: r#"{"cell_id":"cell-1"}"#.to_string(),
        };
        let raw_result = json!("arbitrary execution output");
        let base_proof = json!({
            "authoritative_wait_owner_v1": {
                "adapter": "code_mode_cell",
                "disposition": "terminal",
                "owner": "cell-1",
                "state_revision": "completed",
            }
        });
        let without_projection = authoritative_wait_observation(
            "code_mode_cell",
            &tool_name,
            &payload,
            Some(&base_proof),
            Some(&raw_result),
        )
        .expect("terminal observation");
        assert_eq!(without_projection.result.surfaceable_message, None);

        let designated_proof = json!({
            "authoritative_wait_owner_v1": {
                "adapter": "code_mode_cell",
                "disposition": "terminal",
                "owner": "cell-1",
                "state_revision": "completed",
                "surfaceable_message": "canonical cell completion",
            }
        });
        let with_projection = authoritative_wait_observation(
            "code_mode_cell",
            &tool_name,
            &payload,
            Some(&designated_proof),
            Some(&raw_result),
        )
        .expect("terminal observation with projection");
        assert_eq!(
            with_projection.result.surfaceable_message.as_deref(),
            Some("canonical cell completion")
        );
        assert_ne!(
            without_projection.identity, with_projection.identity,
            "the surface projection participates in convergence identity"
        );
    }

    #[test]
    fn blocked_or_empty_wait_surface_is_never_carried() {
        let tool_name = ToolName::plain("wait_agent");
        let payload = ToolPayload::Function {
            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
        };
        for (disposition, surfaceable_message) in
            [("blocked", "must not surface"), ("terminal", "   ")]
        {
            let signal = json!({
                "authoritative_wait_owner_v1": {
                    "adapter": "multi_agent_v2",
                    "disposition": disposition,
                    "owner": "owner-1",
                    "state_revision": "revision-1",
                    "surfaceable_message": surfaceable_message,
                }
            });
            let result = json!({"message": "raw tool message"});
            let observation = authoritative_wait_observation(
                "multi_agent_v2",
                &tool_name,
                &payload,
                Some(&signal),
                Some(&result),
            )
            .expect("authoritative observation");
            assert_eq!(observation.result.surfaceable_message, None);
        }
    }

    #[test]
    fn terminal_child_waits_and_mixed_calls_preserve_parent_decisions() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let first = authoritative_wait_collector(&control, &baselines, "first", false, None);
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &first, &settled)
                .continuation,
            ContinuationDisposition::ModelRequired
        );

        let changed_identity =
            authoritative_wait_collector(&control, &baselines, "changed", false, None);
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &changed_identity, &settled)
                .continuation,
            ContinuationDisposition::ModelRequired
        );

        let mixed = authoritative_wait_collector(&control, &baselines, "changed", true, None);
        assert_eq!(
            control.evaluate_convergence(&baselines, &mixed, &settled),
            SamplingConvergenceDecision::default()
        );

        let changed_baselines = control.baselines(8);
        let changed_settled = SamplingRequestSettledState {
            mutation_revision: 8,
            ..settled
        };
        let changed_state =
            authoritative_wait_collector(&control, &changed_baselines, "changed", false, None);
        assert_eq!(
            control
                .evaluate_convergence(&changed_baselines, &changed_state, &changed_settled)
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn mixed_generation_purpose_uses_conservative_precedence() {
        let control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let classify =
            |configure: fn(&mut SamplingRequestSignalState), has_pending_input, terminal| {
                let collector = control.collector(&baselines);
                {
                    let mut state = collector
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    configure(&mut state);
                }
                collector.generation_purpose(&baselines, &settled, has_pending_input, terminal)
            };

        fn all_signals(state: &mut SamplingRequestSignalState) {
            state.saw_mutation = true;
            state.saw_validation = true;
            state.saw_coordination = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
            state.outcomes.push(SamplingToolOutcome::plain(
                0,
                SamplingToolOutcomeKind::Failure,
                None,
            ));
        }
        assert_eq!(
            classify(all_signals, true, true),
            Some(TurnTimingGenerationPurpose::InitialReasoning)
        );
        assert_eq!(
            classify(all_signals, false, true),
            Some(TurnTimingGenerationPurpose::Repair)
        );

        fn validation_and_later(state: &mut SamplingRequestSignalState) {
            state.saw_validation = true;
            state.saw_coordination = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(validation_and_later, false, true),
            Some(TurnTimingGenerationPurpose::ValidationInterpretation)
        );

        fn coordination_and_later(state: &mut SamplingRequestSignalState) {
            state.saw_coordination = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(coordination_and_later, false, true),
            Some(TurnTimingGenerationPurpose::Coordination)
        );

        fn wait_only(state: &mut SamplingRequestSignalState) {
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(wait_only, false, true),
            Some(TurnTimingGenerationPurpose::Wait)
        );

        fn artifact(state: &mut SamplingRequestSignalState) {
            state.saw_artifact_read = true;
        }
        assert_eq!(
            classify(artifact, false, true),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
        assert_eq!(
            classify(|_| {}, false, true),
            Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
        );
        assert_eq!(classify(|_| {}, false, false), None);

        fn generic_tool_result(state: &mut SamplingRequestSignalState) {
            state.registered_count = 1;
        }
        assert_eq!(
            classify(generic_tool_result, false, false),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
    }

    fn successful_tool_response(call_id: &str, evidence: &str) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                json!({"evidence": evidence}).to_string(),
            ),
        }
    }

    /// A test runner's output reaches the model as text, not wrapped in a JSON
    /// envelope. Test-execution proof is read from that text, so validation
    /// responses must carry it the way the real tool does.
    fn runner_tool_response(call_id: &str, output: &str) -> ResponseInputItem {
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                output.to_string(),
            ),
        }
    }

    fn read_only_pass_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        arguments: &str,
        evidence: &str,
    ) -> (SamplingRequestSignalCollector, SamplingToolCallRegistration) {
        let collector = control.collector(baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments: arguments.to_string(),
            },
            "read-call",
        );
        // Broad source passes are always dispatched, so the collector always
        // records a real tool result for this ordinal.
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("read-call", evidence),
            false,
        );
        (collector, registration)
    }

    fn structured_tool_pass_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        arguments: &str,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Function {
                arguments: arguments.to_string(),
            },
            "exec-call",
        );
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("exec-call", evidence),
            false,
        );
        collector
    }

    fn high_volume_tool_pass_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            "batch",
        );
        for ordinal in 0..TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            let arguments = format!(r#"{{"command":"inspect-{ordinal}"}}"#);
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "batch",
                tool_name: &ToolName::plain("exec_command"),
                payload: &ToolPayload::Function { arguments },
                source_dependencies: None,
                outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                signal: None,
                result: &json!({"output": evidence}),
                canonical_artifact_required: false,
            });
        }
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("batch", evidence),
            false,
        );
        collector
    }

    fn residual_tool_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        receipt_state: &str,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments:
                    r#"{"artifact_id":"artifact-1","selectors":[{"kind":"bytes","start":0,"end":1}]}"#
                        .to_string(),
            },
            "artifact-call",
        );
        collector.record_accepted_deterministic_continuation_receipts(&[
            TurnTimingDeterministicContinuationReceipt::new(
                DeterministicContinuationClass::ArtifactRange,
                "resource-1".to_string(),
                receipt_state.to_string(),
                DeterministicContinuationHostAction::DrainArtifactRanges,
                "bounds-1".to_string(),
                1,
            ),
        ]);
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("artifact-call", evidence),
            false,
        );
        collector
    }

    #[test]
    fn direct_canonical_artifact_requirement_reaches_execution_control() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let settled = settled(0);
        let collector = control.collector(&baselines);
        let ordinal = collector.register_tool_call();

        collector.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("canonical-call", "artifact"),
            true,
        );

        let outcomes = collector.snapshot();
        assert!(outcomes[0].canonical_artifact_required);
        assert!(
            collector
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .saw_canonical_artifact_requirement
        );
        assert_eq!(
            collector.generation_purpose(&baselines, &settled, false, false),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
    }

    #[test]
    fn residual_tool_continuation_becomes_deterministic_only_when_cycle_is_unchanged() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let first = residual_tool_collector(&control, &baselines, "revision-1", "same");
        assert_eq!(
            control.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );
        assert_eq!(
            control
                .continuation_generation_request(&baselines, &first, &settled, false)
                .sampling,
            SamplingGenerationDisposition::DecisionBearing
        );

        let repeated = residual_tool_collector(&control, &baselines, "revision-1", "same");
        let repeated_decision = control.evaluate_convergence(&baselines, &repeated, &settled);
        assert_eq!(
            repeated_decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(repeated_decision.directive.is_some());
        let repeated_request =
            control.continuation_generation_request(&baselines, &repeated, &settled, false);
        assert_eq!(
            repeated_request.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
        let changed_evidence =
            residual_tool_collector(&control, &baselines, "revision-2", "changed");
        assert!(
            control
                .evaluate_convergence(&baselines, &changed_evidence, &settled)
                .directive
                .is_none()
        );
        assert_eq!(
            control
                .continuation_generation_request(&baselines, &changed_evidence, &settled, false)
                .sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
    }

    #[test]
    fn repeated_read_only_pass_activates_loop_guard_but_requires_a_model_decision() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let arguments =
            r#"{"artifact_id":"artifact-1","selectors":[{"kind":"lines","start":1,"end":1}]}"#;

        for generation in 1..=2 {
            let (collector, _) =
                read_only_pass_collector(&control, &baselines, arguments, "same-evidence");
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation > 1);
            if generation > 1 {
                // The directive asks the model to choose a productive next step;
                // it does not establish a single host-owned protocol outcome.
                assert_eq!(
                    control
                        .continuation_generation_request(&baselines, &collector, &settled, false)
                        .timing_disposition(),
                    TurnTimingGenerationDisposition::DecisionBearing
                );
            }
        }
    }

    #[test]
    fn repeated_broad_source_directive_does_not_claim_the_pass_was_suppressed() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let arguments = r#"{"artifact_id":"artifact-1"}"#;

        let mut directive = None;
        for _ in 1..=2 {
            let (collector, _) =
                read_only_pass_collector(&control, &baselines, arguments, "same-evidence");
            directive = control
                .evaluate_convergence(&baselines, &collector, &settled)
                .directive;
        }

        let directive =
            directive.expect("a repeated broad source pass issues a convergence directive");
        assert!(
            directive.starts_with(
                "Convergence advisory: the broad source pass returned the same evidence"
            ),
            "unexpected directive: {directive}"
        );
        assert!(
            !directive.contains("suppress"),
            "the directive must not promise suppression the host does not perform: {directive}"
        );
        assert!(directive.contains("Continue required coverage, implementation, validation"));
        assert!(directive.contains("runtime bookkeeping cannot establish semantic completeness"));
    }

    #[test]
    fn semantic_evidence_converges_across_different_read_paths_and_presentations() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let calls = [
            (
                "exec_command",
                r#"{"kind":"argv","program":"rg","args":["-n","stable","src/lib.rs"]}"#,
                "src/lib.rs:10:let stable = compute();",
            ),
            (
                "shell_command",
                r#"{"command":"git diff -- src/lib.rs"}"#,
                "diff --git a/src/lib.rs b/src/lib.rs\n@@ -9,0 +10 @@\n+let stable = compute();",
            ),
            (
                "read_tool_output",
                r#"{"artifact_id":"artifact-1","selectors":[{"kind":"lines","start":10,"end":10}]}"#,
                "  --> src/lib.rs:10:1\n10 | let stable = compute();\n   | ^^^",
            ),
        ];

        let mut cycle_key = None;
        for (generation, (tool, arguments, presentation)) in calls.into_iter().enumerate() {
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain(tool),
                &ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                "semantic-call",
            );
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                Some(json!({
                    "kind": "semantic_evidence",
                    "semantic_evidence": {
                        "source": "src/lib.rs",
                        "scope": {"start": 10, "end": 10},
                        "identity": crate::tools::context::semantic_evidence_for_command_output(presentation.as_bytes()),
                    },
                })),
                &successful_tool_response("semantic-call", presentation),
                false,
            );
            let current_key = collector
                .deterministic_cycle_key()
                .expect("semantic evidence cycle");
            if let Some(expected_key) = &cycle_key {
                assert_eq!(expected_key, &current_key);
            } else {
                cycle_key = Some(current_key.clone());
            }
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation > 0);
        }
    }

    #[test]
    fn reordered_broad_reads_converge_without_merging_changed_results() {
        fn collect(
            control: &TurnExecutionControl,
            baselines: &SamplingRequestBaselines,
            tool: &str,
            calls: &[(&str, &str)],
        ) -> SamplingRequestSignalCollector {
            let collector = control.collector(baselines);
            for (index, (artifact, evidence)) in calls.iter().enumerate() {
                let call_id = format!("call-{index}");
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain(tool),
                    &ToolPayload::Function {
                        arguments: json!({"artifact_id": artifact}).to_string(),
                    },
                    &call_id,
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                    None,
                    &successful_tool_response(&call_id, evidence),
                    false,
                );
            }
            collector
        }

        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let calls = [("artifact-a", "evidence-a"), ("artifact-b", "evidence-b")];
        let first = collect(&control, &baselines, "read_tool_output", &calls);
        let first_key = first.deterministic_cycle_key().expect("complete read pass");
        assert!(
            control
                .evaluate_convergence(&baselines, &first, &settled)
                .directive
                .is_none()
        );

        let reversed = collect(
            &control,
            &baselines,
            "read_tool_output",
            &[calls[1], calls[0]],
        );
        assert_eq!(
            reversed.deterministic_cycle_key().as_ref(),
            Some(&first_key)
        );
        assert!(
            control
                .evaluate_convergence(&baselines, &reversed, &settled)
                .directive
                .is_some()
        );

        for changed in [
            vec![("artifact-a", "changed"), calls[1]],
            vec![("artifact-a", "evidence-b"), ("artifact-b", "evidence-a")],
            vec![calls[0], calls[1], calls[0]],
            vec![("artifact-c", "evidence-a"), calls[1]],
        ] {
            let collector = collect(&control, &baselines, "read_tool_output", &changed);
            assert_ne!(
                collector
                    .deterministic_cycle_key()
                    .expect("complete changed pass"),
                first_key
            );
        }

        // Other actions may have effects that depend on their order.
        let ordered = collect(&control, &baselines, "other_tool", &calls);
        let reversed = collect(&control, &baselines, "other_tool", &[calls[1], calls[0]]);
        assert_ne!(
            ordered.deterministic_cycle_key().expect("complete actions"),
            reversed
                .deterministic_cycle_key()
                .expect("complete actions")
        );
    }

    #[test]
    fn broad_source_cycle_preserves_evidence_multiplicity() {
        fn collector_with_reads(
            control: &TurnExecutionControl,
            baselines: &SamplingRequestBaselines,
            count: usize,
        ) -> SamplingRequestSignalCollector {
            let collector = control.collector(baselines);
            for index in 0..count {
                let call_id = format!("read-call-{index}");
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain("read_tool_output"),
                    &ToolPayload::Function {
                        arguments: r#"{"artifact_id":"artifact-1"}"#.to_string(),
                    },
                    &call_id,
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                    None,
                    &successful_tool_response(&call_id, "same-evidence"),
                    false,
                );
            }
            collector
        }

        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let one_read = collector_with_reads(&control, &baselines, 1);
        let two_reads = collector_with_reads(&control, &baselines, 2);

        assert_ne!(
            one_read.deterministic_cycle_key(),
            two_reads.deterministic_cycle_key()
        );
    }

    #[test]
    fn action_identities_share_one_canonical_function_payload() {
        let tool_name = ToolName::plain("wait_agent");
        let left = ToolPayload::Function {
            arguments: r#"{"target":"agent-1","timeout_ms":10}"#.to_string(),
        };
        let right = ToolPayload::Function {
            arguments: r#"{"timeout_ms":10,"target":"agent-1"}"#.to_string(),
        };

        let (left_deterministic, left_structured) = action_identities(&tool_name, &left);
        let (right_deterministic, right_structured) = action_identities(&tool_name, &right);

        assert_eq!(left_deterministic, right_deterministic);
        assert_eq!(left_structured, right_structured);
    }

    #[test]
    fn source_evidence_classification_distinguishes_precise_queries_from_broad_inventory() {
        let payload = |program: &str, args: &[&str]| ToolPayload::Function {
            arguments: json!({"kind":"argv", "program":program, "args":args}).to_string(),
        };
        let tool_name = ToolName::plain("exec_command");

        assert_eq!(
            source_invocation_class(&tool_name, &payload("rg", &["--files", "src"])),
            StructuredActionClass::BroadSource
        );
        assert_eq!(
            source_invocation_class(&tool_name, &payload("rg", &["semantic_evidence", "src"])),
            StructuredActionClass::PreciseSource
        );
        assert_eq!(
            source_invocation_class(&tool_name, &payload("git", &["diff", "--", "src/lib.rs"])),
            StructuredActionClass::PreciseSource
        );
        assert_eq!(
            source_invocation_class(&tool_name, &payload("git", &["status", "--short"])),
            StructuredActionClass::BroadSource
        );
        assert_eq!(
            source_invocation_class(&tool_name, &payload("echo", &["not source evidence"])),
            StructuredActionClass::Other
        );
    }

    fn direct_failure_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        fingerprint: &str,
    ) -> SamplingRequestSignalCollector {
        direct_failure_collector_for_artifact(control, baselines, "artifact-1", fingerprint)
    }

    fn direct_failure_collector_for_artifact(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        artifact_id: &str,
        fingerprint: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        let tool_name = ToolName::plain("read_tool_output");
        let payload = ToolPayload::Function {
            arguments: format!(
                r#"{{"artifact_id":"{artifact_id}","selectors":[{{"kind":"lines","start":1,"end":1}}]}}"#
            ),
        };
        let registration =
            collector.register_deterministic_tool_call(&tool_name, &payload, "failure-call");
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({
                "outcome": "failure",
                "failure": { "fingerprint": fingerprint },
            })),
            &successful_tool_response("failure-call", "diagnostic"),
            false,
        );
        collector
    }

    #[test]
    fn stable_continuation_failure_keeps_other_recovery_actions_available() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=3 {
            let collector = direct_failure_collector(&control, &baselines, "io.locked");
            assert!(
                collector
                    .deterministic_cycle_key()
                    .is_some_and(|key| key.starts_with("ToolFailure:io.locked:"))
            );
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation >= 2);
            assert_eq!(decision.proven_loop_activated, generation == 3);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            if generation == 3 {
                let completion = control
                    .continuation_generation_request(&baselines, &collector, &settled, false);
                assert!(!completion.terminal_completion_only);
            }
        }
    }

    #[test]
    fn budget_progress_rejects_repeated_and_alternating_failure_evidence() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        for (fingerprint, expected) in [
            ("first", true),
            ("second", true),
            ("first", false),
            ("second", false),
        ] {
            let collector = direct_failure_collector(&control, &baselines, fingerprint);
            assert_eq!(
                control.observe_budget_progress(&baselines, &collector, &settled),
                expected
            );
        }
        let collector = control.collector(&baselines);
        let changed = SamplingRequestSettledState {
            mutation_revision: 1,
            ..settled
        };
        assert!(control.observe_budget_progress(&baselines, &collector, &changed));
    }

    #[test]
    fn budget_progress_distinguishes_new_source_coverage_from_repeated_reads() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        for (file, expected) in [
            ("a.txt", true),
            ("a.txt", false),
            ("b.txt", true),
            ("a.txt", false),
        ] {
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("shell_command"),
                &ToolPayload::Function {
                    arguments: json!({"command": format!("cat {file}")}).to_string(),
                },
                "read-source",
            );
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                Some(json!({"semantic_evidence": ["identical file content"]})),
                &successful_tool_response("read-source", "identical file content"),
                false,
            );
            assert_eq!(
                control.observe_budget_progress(&baselines, &collector, &settled),
                expected
            );
        }
    }

    #[test]
    fn write_stdin_failures_are_retried_for_live_process_state() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        let tool_name = ToolName::plain("write_stdin");
        let payload = ToolPayload::Function {
            arguments: r#"{"session_id":7,"chars":"","yield_time_ms":30000}"#.to_string(),
        };
        let action_identity = structured_action_identity(&tool_name, &payload)
            .expect("write_stdin is deterministic")
            .identity;
        collector
            .dispatch_ledger
            .as_ref()
            .expect("control collector has a dispatch ledger")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .repeated_failure_gate = Some(RepeatedFailureGate {
            state_revision: collector.request_state_revision.clone(),
            action_identity,
            failure_fingerprint: "prior.poll.failure".to_string(),
        });

        assert!(
            collector
                .register_deterministic_tool_call(&tool_name, &payload, "poll-again")
                .suppressed_failure
                .is_none(),
            "a live process can recover without changing the poll arguments"
        );
        assert!(collector.has_process_monitor());
        assert!(!collector.observed_successful_process_monitor());
    }

    #[test]
    fn successful_write_stdin_observation_is_relevant_progress() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("write_stdin"),
            &ToolPayload::Function {
                arguments: r#"{"session_id":7,"chars":"","yield_time_ms":30000}"#.to_string(),
            },
            "poll-success",
        );
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("poll-success", r#"{"session_id":7,"running":true}"#),
            false,
        );

        assert!(collector.has_process_monitor());
        assert!(collector.observed_successful_process_monitor());
    }

    #[test]
    fn only_the_current_failure_envelope_supplies_a_failure_signature() {
        assert_eq!(
            value_failure_signature(&json!({"failure_signature": "current"})).as_deref(),
            Some("current")
        );
        assert_eq!(
            value_failure_signature(&json!({"failure": {"fingerprint": "current"}})).as_deref(),
            Some("current")
        );
        assert_eq!(
            value_failure_signature(&json!({
                "metadata": {"failure_signature": "historical"},
                "result": {"failure_signature": "application-data"},
            })),
            None
        );
    }

    #[test]
    fn stringified_json_is_not_failure_control_metadata() {
        assert_eq!(
            value_failure_signature(&json!(r#"{"failure_signature":"application-data"}"#)),
            None
        );
    }

    #[test]
    fn yielded_write_stdin_observation_is_relevant_progress() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("write_stdin"),
            &ToolPayload::Function {
                arguments: r#"{"session_id":7,"chars":"","yield_time_ms":30000}"#.to_string(),
            },
            "poll-yielded",
        );
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            None,
            &successful_tool_response("poll-yielded", r#"{"session_id":7,"running":true}"#),
            false,
        );

        assert!(collector.has_process_monitor());
        assert!(collector.observed_successful_process_monitor());
    }

    #[test]
    fn direct_failure_uses_the_producers_normalized_failure_signature() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let collector = control.collector(&baselines);
        let payload = ToolPayload::Function {
            arguments: r#"{"package":"codex-core","test_filter":"semantic"}"#.to_string(),
        };
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &payload,
            "cargo-failure",
        );
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            None,
            &ResponseInputItem::FunctionCallOutput {
                call_id: "cargo-failure".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    r#"{"failure_signature":"validation-failure-v1:stable-diagnostic"}"#
                        .to_string(),
                ),
            },
            false,
        );

        assert!(collector.deterministic_cycle_key().is_some_and(|key| {
            key.starts_with("ToolFailure:validation-failure-v1:stable-diagnostic:")
        }));
    }

    #[test]
    fn direct_tool_errors_receive_a_stable_failure_fingerprint() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments:
                    r#"{"artifact_id":"missing","selectors":[{"kind":"lines","start":1,"end":1}]}"#
                        .to_string(),
            },
            "missing-call",
        );
        collector.record_failure(
            registration.ordinal,
            "model:artifact `missing` was not found",
            false,
        );

        assert!(
            collector
                .deterministic_cycle_key()
                .is_some_and(|key| key.starts_with("ToolFailure:direct_tool."))
        );
    }

    #[test]
    fn stable_model_visible_failure_arms_exact_repeat_suppression() {
        let mut control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let settled_state = settled(0);
        let payload = ToolPayload::Function {
            arguments:
                r#"{"artifact_id":"expired","selectors":[{"kind":"lines","start":1,"end":1}]}"#
                    .to_string(),
        };
        let first = control.collector(&baselines);
        let registration = first.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &payload,
            "expired-artifact",
        );
        first.record_failure(
            registration.ordinal,
            "model:artifact `expired` has expired",
            true,
        );

        assert_eq!(
            control.evaluate_convergence(&baselines, &first, &settled_state),
            SamplingConvergenceDecision::default()
        );
        let first_cycle = first
            .deterministic_cycle_key()
            .expect("direct failure cycle");
        for generation in 2..=3 {
            let repeated = control.collector(&baselines);
            let registration = repeated.register_deterministic_tool_call(
                &ToolName::plain("read_tool_output"),
                &payload,
                "expired-artifact-retry",
            );
            let guard = registration
                .suppressed_failure
                .expect("exact retry is suppressed");
            repeated.record_suppressed_failure(registration.ordinal, &guard.failure_fingerprint);

            assert_eq!(repeated.turn_efficiency_sample(), (1, 0, 0));
            let outcomes = repeated.snapshot();
            assert_eq!(outcomes.len(), 1);
            assert!(!outcomes[0].nested_in_code_mode);
            assert!(outcomes[0].failure_diagnosis_reused);
            assert_eq!(
                repeated.deterministic_cycle_key().as_ref(),
                Some(&first_cycle)
            );
            let decision = control.evaluate_convergence(&baselines, &repeated, &settled_state);
            assert_eq!(decision.proven_loop_activated, generation == 3);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
        }

        let changed_payload = ToolPayload::Function {
            arguments:
                r#"{"artifact_id":"replacement","selectors":[{"kind":"lines","start":1,"end":1}]}"#
                    .to_string(),
        };
        assert!(
            control
                .collector(&baselines)
                .register_deterministic_tool_call(
                    &ToolName::plain("read_tool_output"),
                    &changed_payload,
                    "replacement-artifact",
                )
                .suppressed_failure
                .is_none(),
            "changed arguments must remain dispatchable"
        );
    }

    #[test]
    fn failure_signature_convergence_does_not_equate_changed_signatures() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let first = direct_failure_collector(&control, &baselines, "io.locked");
        let changed = direct_failure_collector(&control, &baselines, "schema.invalid");

        assert_ne!(
            first.deterministic_cycle_key(),
            changed.deterministic_cycle_key()
        );
    }

    #[test]
    fn failure_signature_convergence_does_not_equate_changed_actions() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let first =
            direct_failure_collector_for_artifact(&control, &baselines, "artifact-1", "io.locked");
        let changed =
            direct_failure_collector_for_artifact(&control, &baselines, "artifact-2", "io.locked");

        assert_ne!(
            first.deterministic_cycle_key(),
            changed.deterministic_cycle_key()
        );
    }

    #[test]
    fn distinct_failure_strategies_allow_narrow_successful_recovery() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for (generation, (artifact_id, fingerprint)) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ]
        .into_iter()
        .enumerate()
        {
            let collector = direct_failure_collector_for_artifact(
                &control,
                &baselines,
                artifact_id,
                fingerprint,
            );
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert_eq!(decision.directive.is_some(), generation == 1);
            assert!(!decision.proven_loop_activated);
        }

        let narrower_success = structured_tool_pass_collector(
            &control,
            &baselines,
            r#"{"command":"inspect-narrower-input"}"#,
            "narrower-evidence",
        );
        assert_eq!(
            control.evaluate_convergence(&baselines, &narrower_success, &settled),
            SamplingConvergenceDecision::default()
        );
    }

    #[test]
    fn slow_terminal_failures_still_spend_distinct_failure_recovery_budget() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for (index, (artifact_id, fingerprint)) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ]
        .into_iter()
        .enumerate()
        {
            let collector = direct_failure_collector_for_artifact(
                &control,
                &baselines,
                artifact_id,
                fingerprint,
            );
            collector
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL + 1);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert_eq!(decision.directive.is_some(), index == 1);
        }
    }

    #[test]
    fn successful_result_resets_distinct_failure_recovery_budget() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let first =
            direct_failure_collector_for_artifact(&control, &baselines, "artifact-1", "io.locked");
        assert_eq!(
            control.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );

        let success = structured_tool_pass_collector(
            &control,
            &baselines,
            r#"{"command":"inspect-new-evidence"}"#,
            "new-evidence",
        );
        assert_eq!(
            control.evaluate_convergence(&baselines, &success, &settled),
            SamplingConvergenceDecision::default()
        );

        let after_success = direct_failure_collector_for_artifact(
            &control,
            &baselines,
            "artifact-2",
            "schema.invalid",
        );
        assert_eq!(
            control.evaluate_convergence(&baselines, &after_success, &settled),
            SamplingConvergenceDecision::default()
        );

        let recovery = direct_failure_collector_for_artifact(
            &control,
            &baselines,
            "artifact-3",
            "permission.denied",
        );
        assert_eq!(
            control.evaluate_convergence(&baselines, &recovery, &settled),
            SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "Failure-recovery advisory: multiple distinct strategies failed while relevant state remained unchanged. Use a narrower or materially different recovery strategy; if none remains, truthfully report the failures and any blocker."
                        .to_string(),
                ),
                proven_loop_activated: false,
                authoritative_wait: None,
            }
        );

        let mut mixed_control = TurnExecutionControl::new();
        let (mixed_baselines, mixed_settled) = unchanged_state(&mixed_control);
        let first = direct_failure_collector_for_artifact(
            &mixed_control,
            &mixed_baselines,
            "mixed-artifact-1",
            "io.locked",
        );
        let _ = mixed_control.evaluate_convergence(&mixed_baselines, &first, &mixed_settled);

        let mixed = mixed_control.collector(&mixed_baselines);
        let success_registration = mixed.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments: r#"{"artifact_id":"successful-artifact"}"#.to_string(),
            },
            "mixed-success",
        );
        mixed.record_response_result(
            success_registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("mixed-success", "retained evidence"),
            false,
        );
        let failure_registration = mixed.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments: r#"{"artifact_id":"mixed-artifact-2"}"#.to_string(),
            },
            "mixed-failure",
        );
        mixed.record_response_result(
            failure_registration.ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({
                "outcome": "failure",
                "failure": { "fingerprint": "schema.invalid" },
            })),
            &successful_tool_response("mixed-failure", "diagnostic"),
            false,
        );
        assert!(
            !mixed
                .deterministic_cycle()
                .expect("mixed deterministic cycle")
                .failure_only
        );
        assert_eq!(
            mixed_control.evaluate_convergence(&mixed_baselines, &mixed, &mixed_settled,),
            SamplingConvergenceDecision::default()
        );

        let after_mixed = direct_failure_collector_for_artifact(
            &mixed_control,
            &mixed_baselines,
            "mixed-artifact-3",
            "permission.denied",
        );
        assert_eq!(
            mixed_control.evaluate_convergence(&mixed_baselines, &after_mixed, &mixed_settled,),
            SamplingConvergenceDecision::default()
        );
    }

    #[test]
    fn multi_failure_cycle_retains_action_to_failure_pairing() {
        fn collector_for(
            control: &TurnExecutionControl,
            baselines: &SamplingRequestBaselines,
            failures: &[(&str, &str)],
        ) -> SamplingRequestSignalCollector {
            let collector = control.collector(baselines);
            for (index, (artifact_id, fingerprint)) in failures.iter().enumerate() {
                let payload = ToolPayload::Function {
                    arguments: format!(r#"{{"artifact_id":"{artifact_id}"}}"#),
                };
                let call_id = format!("failure-call-{index}");
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain("read_tool_output"),
                    &payload,
                    &call_id,
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                    Some(json!({
                        "outcome": "failure",
                        "failure": { "fingerprint": fingerprint },
                    })),
                    &successful_tool_response(&call_id, "diagnostic"),
                    false,
                );
            }
            collector
        }

        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let first = collector_for(
            &control,
            &baselines,
            &[("artifact-a", "failure-a"), ("artifact-b", "failure-b")],
        );
        let swapped = collector_for(
            &control,
            &baselines,
            &[("artifact-a", "failure-b"), ("artifact-b", "failure-a")],
        );

        assert_ne!(
            first.deterministic_cycle_key(),
            swapped.deterministic_cycle_key()
        );

        let reversed = collector_for(
            &control,
            &baselines,
            &[("artifact-b", "failure-b"), ("artifact-a", "failure-a")],
        );
        assert_eq!(
            first.deterministic_cycle_key(),
            reversed.deterministic_cycle_key()
        );

        let duplicated = collector_for(
            &control,
            &baselines,
            &[
                ("artifact-a", "failure-a"),
                ("artifact-b", "failure-b"),
                ("artifact-b", "failure-b"),
            ],
        );
        assert_ne!(
            first.deterministic_cycle_key(),
            duplicated.deterministic_cycle_key()
        );
    }

    #[test]
    fn changed_artifact_reads_reset_the_obligation_progress_budget() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=3 {
            let arguments = format!(
                r#"{{"artifact_id":"artifact-{generation}","selectors":[{{"kind":"lines","start":1,"end":1}}]}}"#
            );
            let evidence = format!("evidence-{generation}");
            let (collector, _) =
                read_only_pass_collector(&control, &baselines, &arguments, &evidence);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
        }
    }

    #[test]
    fn repeated_structured_tool_pass_converges_without_resetting_history() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=3 {
            let collector = structured_tool_pass_collector(
                &control,
                &baselines,
                r#"{"command":"inspect"}"#,
                "same-result",
            );
            assert!(collector.deterministic_cycle_key().is_some());
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation >= 2);
        }
    }

    #[test]
    fn turn_efficiency_repeated_high_tool_count_cycle_preserves_other_actions() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let first = high_volume_tool_pass_collector(&control, &baselines, "same-result");
        for _ in 0..TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            first.record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        }
        let initial = control.evaluate_convergence(&baselines, &first, &settled);
        assert_eq!(
            first.turn_efficiency_sample(),
            (
                TURN_EFFICIENCY_TOOL_CALL_THRESHOLD,
                TURN_EFFICIENCY_TOOL_CALL_THRESHOLD as u64
                    * TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL,
                TURN_EFFICIENCY_TOOL_CALL_THRESHOLD,
            )
        );
        assert_eq!(initial.continuation, ContinuationDisposition::ModelRequired);
        assert!(initial.directive.is_none());
        assert!(!initial.proven_loop_activated);

        let repeated = high_volume_tool_pass_collector(&control, &baselines, "same-result");
        for _ in 0..TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            repeated.record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        }
        let advisory = control.evaluate_convergence(&baselines, &repeated, &settled);
        assert_eq!(
            advisory.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(advisory.directive.is_some());
        assert!(!advisory.proven_loop_activated);

        let repeated_after_advisory =
            high_volume_tool_pass_collector(&control, &baselines, "same-result");
        for _ in 0..TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            repeated_after_advisory
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        }
        let terminal = control.evaluate_convergence(&baselines, &repeated_after_advisory, &settled);
        assert_eq!(
            terminal.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(terminal.directive.is_some());
        assert!(terminal.proven_loop_activated);
    }

    #[test]
    fn changing_sequential_tiny_calls_do_not_request_consolidation() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=9 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&control, &baselines, &arguments, &evidence);
            collector.record_child_runtime(100);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert!(decision.directive.is_none());
            assert!(!decision.proven_loop_activated);
        }
    }

    #[test]
    fn changed_tiny_call_cycles_remain_recoverable_without_efficiency_advisory() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=TURN_EFFICIENCY_TOOL_CALL_THRESHOLD * 2 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&control, &baselines, &arguments, &evidence);
            collector.record_child_runtime(100);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert!(decision.directive.is_none());
            assert!(!decision.proven_loop_activated);
        }
    }

    #[test]
    fn turn_efficiency_substantive_average_child_runtime_does_not_trigger_guard() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=TURN_EFFICIENCY_TOOL_CALL_THRESHOLD + 1 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&control, &baselines, &arguments, &evidence);
            collector
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL + 1);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
        }
    }

    #[test]
    fn proven_loop_preserves_a_different_required_action() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for generation in 1..=3 {
            let collector = structured_tool_pass_collector(
                &control,
                &baselines,
                r#"{"command":"inspect"}"#,
                "same-result",
            );
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert_eq!(decision.proven_loop_activated, generation == 3);
            let request =
                control.continuation_generation_request(&baselines, &collector, &settled, false);
            assert!(!request.terminal_completion_only);
        }

        let different = structured_tool_pass_collector(
            &control,
            &baselines,
            r#"{"command":"inspect-required-dependency"}"#,
            "new-result",
        );
        let decision = control.evaluate_convergence(&baselines, &different, &settled);
        assert_eq!(decision, SamplingConvergenceDecision::default());
    }

    #[test]
    fn uncertain_dispatch_identity_fails_open() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        for _ in 0..2 {
            let collector = control.collector(&baselines);
            let _registration = collector.register_deterministic_tool_call(
                &ToolName::plain("shell_command"),
                &ToolPayload::Function {
                    arguments: r#"{"command":"git status --short"}"#.to_string(),
                },
                "shell-current",
            );
            assert!(collector.deterministic_cycle_key().is_none());
        }
    }

    #[test]
    fn empty_tool_free_cycles_never_spend_the_no_progress_budget() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for _ in 1..=4 {
            let collector = control.collector(&baselines);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
            assert!(!decision.proven_loop_activated);
        }
    }

    fn blocked_wait(collector: &SamplingRequestSignalCollector, receipt_identity: &str) {
        let tool_name = ToolName::plain("wait_agent");
        let payload = ToolPayload::Function {
            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
        };
        let _registration =
            collector.register_deterministic_tool_call(&tool_name, &payload, "wait-call");
        let signal = json!({
            "authoritative_wait_owner_v1": {
                "adapter": "multi_agent_v2",
                "disposition": "blocked",
                "owner": "owner-1",
                "state_revision": "revision-1",
                "receipt_identity": receipt_identity,
            }
        });
        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "wait-call".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                json!({
                    "message": "owner needs main action",
                    "typed_deltas": [{
                        "assignment_id": "01900000-0000-7000-8000-000000000001",
                    }],
                    "receipt": "receipt-1",
                })
                .to_string(),
            ),
        };
        collector.record_direct_wait_owner_result(
            true,
            &tool_name,
            &payload,
            Some(&signal),
            &response,
        );
    }

    #[test]
    fn blocked_wait_directs_main_action_on_the_first_exact_observation() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let collector = control.collector(&baselines);
        blocked_wait(&collector, "receipt-1");
        let decision = control.evaluate_convergence(&baselines, &collector, &settled);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(decision.directive.is_some());
        assert!(matches!(
            decision.authoritative_wait,
            Some(AuthoritativeWaitResolution::Blocked(_))
        ));
        assert!(decision.proven_loop_activated);

        let repeated = control.collector(&baselines);
        let registration = repeated.register_deterministic_tool_call(
            &ToolName::plain("wait_agent"),
            &ToolPayload::Function {
                arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
            },
            "repeated-wait-call",
        );
        assert!(registration.blocked_wait_guard.is_some());
    }

    #[test]
    fn repeated_suppressed_blocked_wait_keeps_recovery_tools_available() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let first = control.collector(&baselines);
        blocked_wait(&first, "receipt-1");
        let _ = control.evaluate_convergence(&baselines, &first, &settled);

        let tool_name = ToolName::plain("wait_agent");
        let payload = ToolPayload::Function {
            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
        };
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &tool_name,
            &payload,
            "suppressed-wait-call",
        );
        assert!(registration.blocked_wait_guard.is_some());
        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "suppressed-wait-call".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                json!({
                    "kind": "authoritative_wait_suppression",
                    "disposition": "blocked",
                    "owner": "owner-1",
                    "state_revision": "revision-1",
                })
                .to_string(),
            ),
        };
        collector.record_suppressed_result(registration.ordinal, &response);

        let outcomes = collector.snapshot();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].kind, SamplingToolOutcomeKind::Blocked);
        assert!(outcomes[0].is_failure_evidence());
        assert_eq!(collector.turn_efficiency_sample(), (1, 0, 0));
        let decision = control.evaluate_convergence(&baselines, &collector, &settled);
        assert!(decision.proven_loop_activated);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn blocked_wait_receipt_change_does_not_reset_owner_revision_convergence() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        let first = control.collector(&baselines);
        blocked_wait(&first, "receipt-1");
        let _ = control.evaluate_convergence(&baselines, &first, &settled);
        let changed = control.collector(&baselines);
        blocked_wait(&changed, "receipt-2");
        let decision = control.evaluate_convergence(&baselines, &changed, &settled);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(decision.directive.is_some());
        let guard = control
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked_wait_gate
            .clone()
            .expect("blocked wait gate");
        assert_eq!(guard.guard.owner, "owner-1");
        assert_eq!(guard.guard.state_revision, "revision-1");
    }

    #[test]
    fn failed_tool_signals_cannot_hide_failure_or_create_successful_replay() {
        for (signal_name, expected) in [
            ("partial_success", SamplingToolOutcomeKind::Failure),
            ("success", SamplingToolOutcomeKind::Failure),
            ("skipped", SamplingToolOutcomeKind::Failure),
            ("failure", SamplingToolOutcomeKind::Failure),
            ("blocked", SamplingToolOutcomeKind::Blocked),
            ("timeout", SamplingToolOutcomeKind::Timeout),
            (
                "recoverable_cancellation",
                SamplingToolOutcomeKind::RecoverableCancellation,
            ),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let collector = control.collector(&baselines);
            let tool = ToolName::plain("exec_command");
            let payload = validation_proof_payload();
            let registration =
                collector.register_deterministic_tool_call(&tool, &payload, "failed");
            collector.record_validation_workspace_revision(&tool, &payload, 0);
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(json!({"outcome": signal_name})),
                &ResponseInputItem::FunctionCallOutput {
                    call_id: "failed".to_string(),
                    output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                        r#"{"failure_signature":"failed-check"}"#.to_string(),
                    ),
                },
                false,
            );
            assert_eq!(collector.snapshot()[0].kind, expected, "{signal_name}");
            control.settle(&baselines, &collector, &settled(0));
            assert_ne!(
                control
                    .evaluate_convergence(&baselines, &collector, &settled(0))
                    .continuation,
                ContinuationDisposition::TerminalCompletionRequired,
                "{signal_name}",
            );
            let retry = control.collector(&control.baselines(0));
            assert!(
                retry
                    .register_deterministic_tool_call(&tool, &payload, "retry")
                    .replayed_success
                    .is_none()
            );
        }
    }

    #[test]
    fn protocol_continuation_preserves_sampling_without_new_tool_or_state_evidence() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines(7);
        let collector = control.collector(&baselines);
        for (mutation_revision, has_pending_input) in [(7, false), (7, true), (8, false)] {
            let request = control.continuation_generation_request(
                &baselines,
                &collector,
                &settled(mutation_revision),
                has_pending_input,
            );
            assert_eq!(
                request.sampling,
                SamplingGenerationDisposition::DecisionBearing
            );
            assert_eq!(
                request.timing_disposition(),
                TurnTimingGenerationDisposition::DecisionBearing
            );
            assert!(!request.terminal_completion_only);
        }
    }

    #[test]
    fn non_failure_outcomes_do_not_reopen_failures() {
        for kind in [
            SamplingToolOutcomeKind::Success,
            SamplingToolOutcomeKind::Yielded,
            SamplingToolOutcomeKind::Unknown,
        ] {
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            control.settle(&baseline, &collector_with(kind), &settled(0));
            assert!(control.unresolved_failures.is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn nested_code_mode_failure_overrides_successful_cell_and_retains_evidence() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let outer_ordinal = collector
            .register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Custom {
                    input: "try { await tools.update_plan({}); } catch {}".to_string(),
                },
                "exec-call",
            )
            .ordinal;
        let nested_plan = plan(&[StepStatus::Pending]);
        let source_evidence = json!({
            "owner": "planning-architecture-runtime",
            "omitted_relationships": 0,
        });
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell-1",
            tool_name: &ToolName::plain("update_plan"),
            payload: &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::skipped(Some(
                ToolOutputSkipDisposition::BlockingRequiredOperation,
            )),
            signal: Some(&json!({
                "kind": "plan_update",
                "outcome": "skipped",
                "plan": nested_plan,
                "source_closure_established": true,
                "source_closure": source_evidence,
                "failure": {
                    "fingerprint": "nested.plan.blocked",
                    "retryable": false,
                },
            })),
            result: &json!({"message": "nested tool was blocked"}),
            canonical_artifact_required: true,
        });
        collector.push(SamplingToolOutcome::plain(
            outer_ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));

        let outcomes = collector.snapshot();
        let nested = outcomes
            .iter()
            .find(|outcome| outcome.nested_in_code_mode)
            .expect("nested outcome should reach the control");
        assert_eq!(nested.kind, SamplingToolOutcomeKind::Skipped);
        assert_eq!(
            nested.skip_disposition,
            Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
        );
        assert_eq!(nested.plan.as_ref(), Some(&nested_plan));
        assert_eq!(nested.source_evidence.as_ref(), Some(&source_evidence));
        assert_eq!(
            nested.failure_fingerprint.as_deref(),
            Some("nested.plan.blocked")
        );
        assert!(nested.canonical_artifact_required);
        assert!(
            collector
                .deterministic_cycle_key()
                .is_some_and(|key| key.starts_with("NestedToolFailure:nested.plan.blocked:"))
        );

        let settled_state = settled(0);
        control.settle(&baseline, &collector, &settled_state);
        assert_eq!(
            control.evaluate_convergence(&baseline, &collector, &settled_state),
            SamplingConvergenceDecision::default()
        );
        let first_retry = control.collector(&baseline);
        let repeated_registration = first_retry.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Custom {
                input: "try { await tools.update_plan({}); } catch {}".to_string(),
            },
            "first-retry-exec-call",
        );
        assert!(
            repeated_registration.suppressed_failure.is_none(),
            "an outer code-mode call must run again because its nested dependencies can change"
        );

        let changed_action = control.collector(&baseline);
        assert!(
            changed_action
                .register_deterministic_tool_call(
                    &ToolName::plain("exec"),
                    &ToolPayload::Custom {
                        input: "await tools.update_plan({changed: true});".to_string(),
                    },
                    "changed-action-exec-call",
                )
                .suppressed_failure
                .is_none()
        );

        let changed_state = control.baselines(1);
        let changed = control.collector(&changed_state);
        assert!(
            changed
                .register_deterministic_tool_call(
                    &ToolName::plain("exec"),
                    &ToolPayload::Custom {
                        input: "try { await tools.update_plan({}); } catch {}".to_string(),
                    },
                    "changed-state-exec-call",
                )
                .suppressed_failure
                .is_none()
        );
    }

    #[test]
    fn validation_must_follow_the_last_mutation_but_allows_reads() {
        let mut validated_after_mutation = TurnExecutionControl::new();
        settle_plan(
            &mut validated_after_mutation,
            plan(&[StepStatus::Completed]),
        );
        let baselines = validated_after_mutation.baselines(0);
        let collector = validated_after_mutation.collector(&baselines);
        record_invocation_result(
            &collector,
            ToolName::plain("apply_patch"),
            ToolPayload::Custom {
                input: "*** Begin Patch\n*** End Patch".to_string(),
            },
            "mutation-before-validation",
            ToolOutputOutcome::Success,
        );
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            validation_proof_payload(),
            "validation-after-mutation",
            ToolOutputOutcome::Success,
        );
        collector.record_validation_workspace_revision(
            &ToolName::plain("exec_command"),
            &validation_proof_payload(),
            1,
        );
        let mutation_settled = settled(1);
        validated_after_mutation.settle(&baselines, &collector, &mutation_settled);
        assert!(collector.fresh_successful_validation().is_some());
        assert_eq!(
            validated_after_mutation
                .evaluate_convergence(&baselines, &collector, &mutation_settled)
                .continuation,
            ContinuationDisposition::ModelRequired
        );

        let mut mutated_after_validation = TurnExecutionControl::new();
        settle_plan(
            &mut mutated_after_validation,
            plan(&[StepStatus::Completed]),
        );
        let baselines = mutated_after_validation.baselines(0);
        let collector = mutated_after_validation.collector(&baselines);
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            validation_proof_payload(),
            "validation-before-mutation",
            ToolOutputOutcome::Success,
        );
        record_invocation_result(
            &collector,
            ToolName::plain("apply_patch"),
            ToolPayload::Custom {
                input: "*** Begin Patch\n*** End Patch".to_string(),
            },
            "mutation-after-validation",
            ToolOutputOutcome::Success,
        );
        let mutation_settled = settled(1);
        mutated_after_validation.settle(&baselines, &collector, &mutation_settled);
        assert!(collector.fresh_successful_validation().is_none());

        let mut observed_after_validation = TurnExecutionControl::new();
        settle_plan(
            &mut observed_after_validation,
            plan(&[StepStatus::Completed]),
        );
        let baselines = observed_after_validation.baselines(0);
        let collector = recorded_validation_collector(
            &observed_after_validation,
            &baselines,
            ToolOutputOutcome::Success,
        );
        record_invocation_result(
            &collector,
            ToolName::plain("read_tool_output"),
            ToolPayload::Function {
                arguments: r#"{"artifact_id":"artifact-1"}"#.to_string(),
            },
            "observation-after-validation",
            ToolOutputOutcome::Success,
        );
        let settled_state = settled(0);
        observed_after_validation.settle(&baselines, &collector, &settled_state);
        assert!(collector.fresh_successful_validation().is_some());
        assert_eq!(
            observed_after_validation
                .evaluate_convergence(&baselines, &collector, &settled_state)
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn unrelated_validation_does_not_resolve_a_failed_target() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let failed_baselines = control.baselines(0);
        let failed =
            recorded_validation_collector(&control, &failed_baselines, ToolOutputOutcome::Failure);
        control.settle(&failed_baselines, &failed, &settled(0));

        let baselines = control.baselines(0);
        let unrelated = control.collector(&baselines);
        record_invocation_result(
            &unrelated,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: json!({"cmd": "python -m unittest unrelated_test -q"}).to_string(),
            },
            "unrelated-validation",
            ToolOutputOutcome::Success,
        );
        control.settle(&baselines, &unrelated, &settled(0));
        assert!(!control.unresolved_failures.is_empty());

        let recovered =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &recovered, &settled(0));
        assert!(control.unresolved_failures.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &recovered, &settled(0))
                .continuation,
            ContinuationDisposition::ModelRequired,
        );
    }

    #[test]
    fn validation_at_an_older_workspace_revision_cannot_finalize_or_resolve_failure() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let baselines = control.baselines(0);
        let failed =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Failure);
        control.settle(&baselines, &failed, &settled(0));

        let validation =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        // A shell edit or concurrent mutation can advance the revision without
        // an apply_patch ordinal in this collector.
        control.settle(&baselines, &validation, &settled(1));
        assert!(!control.unresolved_failures.is_empty());
        assert_ne!(
            control
                .evaluate_convergence(&baselines, &validation, &settled(1))
                .continuation,
            ContinuationDisposition::TerminalCompletionRequired,
        );
    }

    #[test]
    fn forced_validation_retry_resolves_its_failed_target() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let baselines = control.baselines(0);
        let failed =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Failure);
        control.settle(&baselines, &failed, &settled(0));
        let retry = control.collector(&baselines);
        let ToolPayload::Function { arguments } = validation_proof_payload() else {
            panic!("validation payload");
        };
        let mut arguments: Value = serde_json::from_str(&arguments).expect("validation arguments");
        arguments["force_fresh"] = json!(true);
        record_invocation_result(
            &retry,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: arguments.to_string(),
            },
            "forced-retry",
            ToolOutputOutcome::Success,
        );
        control.settle(&baselines, &retry, &settled(0));
        assert!(control.unresolved_failures.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &retry, &settled(0))
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn successful_read_retry_resolves_failure_before_later_validation() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        for outcome in [ToolOutputOutcome::Failure, ToolOutputOutcome::Success] {
            let baselines = control.baselines(0);
            let collector = control.collector(&baselines);
            record_invocation_result(
                &collector,
                ToolName::plain("exec_command"),
                ToolPayload::Function {
                    arguments: json!({"cmd": "cat src/lib.rs"}).to_string(),
                },
                "source-read",
                outcome,
            );
            control.settle(&baselines, &collector, &settled(0));
        }
        let baselines = control.baselines(0);
        let validation =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &validation, &settled(0));
        assert!(control.unresolved_failures.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &validation, &settled(0))
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn harmless_reads_and_split_final_checks_preserve_validation() {
        for commands in [
            vec![json!({"cmd": "cat src/lib.rs"})],
            vec![json!({"kind": "argv", "program": "cat", "args": ["src/lib.rs"]})],
            vec![
                json!({"cmd": "git diff --check"}),
                json!({"cmd": "git status --short"}),
            ],
            vec![
                json!({"kind": "argv", "program": "git", "args": ["diff", "--check"]}),
                json!({"kind": "argv", "program": "git", "args": ["status", "--short"]}),
            ],
            vec![json!({"cmd": "git diff --check && git status --short"}); 2],
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::Completed]));
            let baselines = control.baselines(0);
            let collector =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            for (index, command) in commands.iter().enumerate() {
                record_invocation_result(
                    &collector,
                    ToolName::plain("exec_command"),
                    ToolPayload::Function {
                        arguments: command.to_string(),
                    },
                    &format!("observation-{index}"),
                    ToolOutputOutcome::Success,
                );
            }
            control.settle(&baselines, &collector, &settled(0));
            assert!(
                collector.fresh_successful_validation().is_some(),
                "{commands:?}"
            );
            assert_eq!(
                control
                    .evaluate_convergence(&baselines, &collector, &settled(0))
                    .continuation,
                ContinuationDisposition::ModelRequired,
                "{commands:?}",
            );
        }
    }

    #[test]
    fn cargo_validation_recovery_requires_matching_context_and_executed_target() {
        for nested in [false, true] {
            for scenario in [
                "covered",
                "other target",
                "missing output",
                "other cwd",
                "filtered",
                "stale",
                "failed",
                "yielded",
            ] {
                let mut control = TurnExecutionControl::new();
                let failed_baselines = control.baselines(0);
                let failed = control.collector(&failed_baselines);
                record_invocation_result(
                    &failed,
                    ToolName::plain("exec_command"),
                    ToolPayload::Function {
                        arguments: json!({"cmd": "cargo test --offline --locked --jobs 6 --test regression", "workdir": "/repo"}).to_string(),
                    },
                    "failed-focused-test",
                    ToolOutputOutcome::Failure,
                );
                control.settle(&failed_baselines, &failed, &settled(0));
                // A real edit advances the revision; no plan is required for
                // this small fix, but baseline tests alone must not end a task.
                let baselines = control.baselines(1);
                let collector = control.collector(&baselines);
                let mut args = vec!["test", "--offline", "--locked", "--jobs", "6"];
                if scenario == "filtered" {
                    args.push("some_filter");
                }
                let payload = ToolPayload::Function {
                    arguments: json!({"kind": "argv", "program": "cargo", "args": args,
                        "workdir": if scenario == "other cwd" { "/other" } else { "/repo" },
                        "force_fresh": true, "yield_time_ms": 1000, "max_output_tokens": 3000})
                    .to_string(),
                };
                let target = if scenario == "other target" {
                    "unrelated"
                } else {
                    "regression"
                };
                let output = if scenario == "missing output" {
                    String::new()
                } else {
                    format!(
                        "     Running tests/{target}.rs (target\\debug\\deps\\{target}-0123456789abcdef.exe)\ntest result: ok. 4 passed; 0 failed\n"
                    )
                };
                let outcome = match scenario {
                    "failed" => ToolOutputOutcome::Failure,
                    "yielded" => ToolOutputOutcome::Yielded,
                    _ => ToolOutputOutcome::Success,
                };
                collector.record_validation_workspace_revision(
                    &ToolName::plain("exec_command"),
                    &payload,
                    if scenario == "stale" { 0 } else { 1 },
                );
                if nested {
                    collector.record_code_mode_result(CodeModeToolResult {
                        cell_id: "recovery",
                        tool_name: &ToolName::plain("exec_command"),
                        payload: &payload,
                        source_dependencies: None,
                        outcome_context: ToolOutputOutcomeContext::new(outcome),
                        signal: None,
                        result: &json!({"output": output}),
                        canonical_artifact_required: false,
                    });
                } else {
                    let registration = collector.register_deterministic_tool_call(
                        &ToolName::plain("exec_command"),
                        &payload,
                        "recovery",
                    );
                    collector.record_response_result(
                        registration.ordinal,
                        ToolOutputOutcomeContext::new(outcome),
                        None,
                        &ResponseInputItem::FunctionCallOutput {
                            call_id: "recovery".to_string(),
                            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                                output,
                            ),
                        },
                        false,
                    );
                }
                control.settle(&baselines, &collector, &settled(1));
                let completed = scenario == "covered";
                assert_eq!(
                    control.unresolved_failures.is_empty(),
                    completed,
                    "{scenario}, nested={nested}"
                );
                assert_eq!(
                    control
                        .evaluate_convergence(&baselines, &collector, &settled(1))
                        .continuation,
                    ContinuationDisposition::ModelRequired,
                    "{scenario}, nested={nested}"
                );
            }
        }
    }

    #[test]
    fn status_only_plan_update_retains_unfinished_mutation_obligation() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        collector.push(SamplingToolOutcome::from_signal(
            0,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            Some(&json!({
                "kind": "plan_update",
                "plan": null,
                "unfinished_mutation_obligation": true,
            })),
        ));

        assert!(collector.snapshot()[0].unfinished_mutation_obligation);
        control.settle(&baseline, &collector, &settled(0));
    }

    #[test]
    fn nested_code_mode_success_drives_plan_and_artifact_classification() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let outer_ordinal = collector
            .register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Custom {
                    input: "await tools.update_plan({});".to_string(),
                },
                "exec-call",
            )
            .ordinal;
        let nested_plan = plan(&[StepStatus::InProgress]);
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell-1",
            tool_name: &ToolName::plain("update_plan"),
            payload: &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
            source_dependencies: None,
            outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            signal: Some(&json!({
                "kind": "plan_update",
                "plan": nested_plan,
                "source_evidence": { "identity": "source-v1" },
            })),
            result: &json!({"message": "plan updated"}),
            canonical_artifact_required: true,
        });
        collector.push(SamplingToolOutcome::plain(
            outer_ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));

        assert_eq!(
            collector.generation_purpose(&baseline, &settled(0), false, false,),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
        control.settle(&baseline, &collector, &settled(0));
    }
}
