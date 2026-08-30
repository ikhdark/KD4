use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_config::config_toml::ReasoningPhaseEfforts;
use codex_config::schema::canonicalize as canonicalize_json;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::ReasoningPolicyHistory;
use codex_protocol::protocol::ReasoningPolicyPhase;
use codex_protocol::protocol::ReasoningPolicySnapshot;
use codex_protocol::protocol::ReasoningPolicySource;
use codex_protocol::protocol::ReasoningPolicyTrigger;
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
use crate::validation_admission::classify_validation;

pub(crate) type SamplingReasoningPhase = ReasoningPolicyPhase;
pub(crate) type SamplingRequestPolicySource = ReasoningPolicySource;

const TURN_EFFICIENCY_TOOL_CALL_THRESHOLD: usize = 8;
const TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL: u64 = 500;
const DISTINCT_FAILURE_RECOVERY_ADVISORY_THRESHOLD: u32 = 2;

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
            SamplingGenerationDisposition::ResidualDeterministic(proof) => {
                debug_assert_eq!(
                    proof.relevant_state_fingerprint,
                    self.relevant_state_fingerprint
                );
                debug_assert!(matches!(
                    proof.exact_action,
                    ResidualDeterministicAction::CompleteProtocolTurn
                        | ResidualDeterministicAction::RequireChangedContinuation
                ));
                TurnTimingGenerationDisposition::Deterministic
            }
        }
    }

    /// Returns true when the governor has proved that the protocol-requested
    /// continuation has exactly one host-owned outcome. In that case another
    /// model generation cannot add a decision and must be elided.
    pub(crate) fn completes_protocol_turn_deterministically(&self) -> bool {
        matches!(
            &self.sampling,
            SamplingGenerationDisposition::ResidualDeterministic(
                ResidualDeterministicSamplingProof {
                    exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
                    ..
                }
            )
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SamplingGenerationDisposition {
    DecisionBearing,
    ResidualDeterministic(ResidualDeterministicSamplingProof),
}

impl SamplingGenerationDisposition {
    pub(crate) fn is_residual_deterministic(&self) -> bool {
        matches!(self, Self::ResidualDeterministic(_))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResidualDeterministicSamplingProof {
    relevant_state_fingerprint: String,
    exact_action: ResidualDeterministicAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResidualDeterministicAction {
    CompleteProtocolTurn,
    RequireChangedContinuation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SamplingRequestPolicy {
    pub(crate) phase: Option<SamplingReasoningPhase>,
    pub(crate) configured_effort: Option<ReasoningEffort>,
    pub(crate) effective_effort: Option<ReasoningEffort>,
    pub(crate) request_effort: Option<ReasoningEffort>,
    pub(crate) source: SamplingRequestPolicySource,
}

#[derive(Default)]
struct ReasoningPolicyRecorderState {
    entries: VecDeque<ReasoningPolicySnapshot>,
    total_entries: u64,
    finalized: bool,
}

/// Records the exact resolved request policies. A single mutex makes deduplication,
/// sequencing, and retention one atomic operation without burdening the turn stream.
#[derive(Clone)]
pub(crate) struct ReasoningPolicyRecorder {
    enabled: bool,
    state: Arc<Mutex<ReasoningPolicyRecorderState>>,
}

impl ReasoningPolicyRecorder {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: Arc::new(Mutex::new(ReasoningPolicyRecorderState::default())),
        }
    }

    pub(crate) fn append(
        &self,
        policy: &SamplingRequestPolicy,
        model: String,
        trigger: ReasoningPolicyTrigger,
    ) -> Option<ReasoningPolicySnapshot> {
        if !self.enabled {
            return None;
        }
        let phase = policy.phase?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.finalized {
            return None;
        }
        let candidate = ReasoningPolicySnapshot {
            sequence: state.total_entries.saturating_add(1),
            timestamp: unix_timestamp_millis(),
            phase,
            configured_effort: policy.configured_effort.clone(),
            effective_effort: policy.effective_effort.clone(),
            request_effort: policy.request_effort.clone(),
            source: policy.source,
            model,
            trigger,
        };
        if state.entries.back().is_some_and(|previous| {
            previous.phase == candidate.phase
                && previous.configured_effort == candidate.configured_effort
                && previous.effective_effort == candidate.effective_effort
                && previous.request_effort == candidate.request_effort
                && previous.source == candidate.source
                && previous.model == candidate.model
                && previous.trigger == candidate.trigger
        }) {
            return None;
        }
        state.total_entries = candidate.sequence;
        if state.entries.len() == 64 {
            state.entries.pop_front();
        }
        state.entries.push_back(candidate.clone());
        Some(candidate)
    }

    pub(crate) fn take_summary(&self, turn_id: String) -> Option<ReasoningPolicyHistory> {
        if !self.enabled {
            return None;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.finalized {
            return None;
        }
        state.finalized = true;
        Some(ReasoningPolicyHistory {
            turn_id,
            truncated: state.total_entries > state.entries.len() as u64,
            total_entries: state.total_entries,
            entries: state.entries.drain(..).collect(),
        })
    }
}

fn unix_timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
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
    source_closure_established: bool,
    source_evidence: Option<Value>,
    unfinished_mutation_obligation: bool,
    failure_fingerprint: Option<String>,
    failure_is_terminal: bool,
    failure_diagnosis_reused: bool,
    canonical_artifact_required: bool,
    nested_in_code_mode: bool,
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
            source_closure_established: sampling_source_closure_established(signal),
            source_evidence: sampling_source_evidence(signal),
            unfinished_mutation_obligation: sampling_unfinished_mutation_obligation(signal),
            failure_fingerprint: sampling_failure_fingerprint(signal),
            failure_is_terminal: sampling_failure_is_terminal(signal),
            failure_diagnosis_reused: false,
            canonical_artifact_required: false,
            nested_in_code_mode: false,
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

    fn is_generic_success(&self) -> bool {
        self.kind == SamplingToolOutcomeKind::Success
            && self.plan.is_none()
            && !self.source_closure_established
            && self.source_evidence.is_none()
            && !self.unfinished_mutation_obligation
            && !self.canonical_artifact_required
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
struct RepeatedFailureGate {
    state_revision: String,
    action_identity: String,
    failure_fingerprint: String,
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
    timing: Arc<TurnTimingState>,
}

impl DeterministicDispatchLedger {
    fn new(timing: Arc<TurnTimingState>) -> Self {
        Self {
            blocked_wait_gate: None,
            repeated_failure_gate: None,
            timing,
        }
    }
}

#[derive(Default)]
struct SamplingRequestSignalState {
    outcomes: Vec<SamplingToolOutcome>,
    structured_actions: BTreeMap<u64, StructuredActionIdentity>,
    evidence_items: BTreeMap<u64, String>,
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
}

pub(crate) struct SamplingToolCallRegistration {
    pub(crate) ordinal: u64,
    pub(crate) blocked_wait_guard: Option<BlockedWaitGuard>,
    pub(crate) suppressed_failure: Option<SuppressedFailureGuard>,
}

impl SamplingRequestSignalCollector {
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
        let (action_identity, structured_action) = action_identities(tool_name, payload);
        let validation = is_validation_invocation(tool_name, payload);
        let (blocked_wait_guard, suppressed_failure) = self
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
                (blocked_wait_guard, suppressed_failure)
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
        state.saw_mutation |= is_mutation_tool(tool_name);
        state.saw_coordination |= is_coordination_tool(tool_name);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
        }

        SamplingToolCallRegistration {
            ordinal,
            blocked_wait_guard,
            suppressed_failure,
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
    }

    fn turn_efficiency_sample(&self) -> (usize, u64) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.registered_count, state.child_runtime_ms)
    }

    pub(crate) fn record_suppressed_result(&self, ordinal: u64, response: &ResponseInputItem) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Success,
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
        outcome.nested_in_code_mode = true;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
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
        let structured_action = structured_action_identity(tool_name, payload);
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
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.saw_validation |= is_validation_invocation(tool_name, payload);
        state.saw_mutation |= is_mutation_tool(tool_name);
        state.saw_coordination |= is_coordination_tool(tool_name);
        if let Some(source_dependencies) = source_dependencies {
            let accumulated = state
                .code_mode_source_dependencies
                .entry(cell_id.to_string())
                .or_insert_with(|| source_dependencies.clone());
            if accumulated.is_empty() || source_dependencies.is_empty() {
                // Empty means a workspace-observing nested tool could not be
                // scoped. Preserve that fail-closed state for the whole cell.
                accumulated.clear();
            } else {
                accumulated.extend(source_dependencies);
            }
        }
        state.outcomes.push(outcome);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
        }
        if let Some(evidence_identity) = evidence_identity {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
        if tool_name.namespace.is_some() || tool_name.name != "wait" {
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
        let structured_action =
            payload.and_then(|payload| structured_action_identity(tool_name, payload));
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        if let Some(payload) = payload {
            state.saw_validation |= is_validation_invocation(tool_name, payload);
            state.saw_mutation |= is_mutation_tool(tool_name);
            state.saw_coordination |= is_coordination_tool(tool_name);
        }
        if let Some(source_dependencies) = source_dependencies {
            let accumulated = state
                .code_mode_source_dependencies
                .entry(cell_id.to_string())
                .or_insert_with(|| source_dependencies.clone());
            if accumulated.is_empty() || source_dependencies.is_empty() {
                accumulated.clear();
            } else {
                accumulated.extend(source_dependencies);
            }
        }
        state.outcomes.push(outcome);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
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
        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.outcomes.push(outcome);
        if let Some(evidence_identity) = evidence_identity {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
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
        let outer_code_mode_success = outer_code_mode_outcomes
            .next()
            .is_some_and(|outcome| outcome.kind == SamplingToolOutcomeKind::Success)
            && outer_code_mode_outcomes.next().is_none();
        let code_mode_owned = state.code_mode_nested_tool_count > 0
            && state.registered_count == 1
            && state.direct_code_mode_exec_count == 1
            && outer_code_mode_success;
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

        let semantic_evidence_only = outcomes
            .iter()
            .all(|outcome| outcome.source_evidence.is_some())
            && !state.saw_validation
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
        let action_evidence = ordered
            .iter()
            .map(|(_, action, evidence)| format!("{}:{evidence}", action.identity))
            .collect::<Vec<_>>()
            .join("|");
        let semantic_evidence = (all_broad_source || semantic_evidence_only).then(|| {
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

    fn saw_validation(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .saw_validation
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
        } else if state.saw_artifact_read
            || state.saw_canonical_artifact_requirement
            || state
                .structured_actions
                .values()
                .any(|action| action.class == StructuredActionClass::BroadSource)
        {
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        } else if observed_new_failure {
            Some(TurnTimingGenerationPurpose::FailureDiagnosis)
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
        if state.saw_validation {
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
        ToolOutputOutcome::Failure => signalled.unwrap_or(SamplingToolOutcomeKind::Failure),
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

fn sampling_source_closure_established(signal: Option<&Value>) -> bool {
    signal
        .and_then(|value| value.get("source_closure_established"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
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
    signal
        .and_then(|value| {
            value
                .get("failure_signature")
                .and_then(Value::as_str)
                .or_else(|| {
                    value
                        .get("failure")
                        .and_then(|failure| failure.get("fingerprint"))
                        .and_then(Value::as_str)
                })
        })
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_owned)
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
    let class = source_invocation_class_from_canonical(tool_name, payload, canonical);
    let action =
        serde_json::to_string(&(tool_name, canonical.identity_payload.as_deref()?)).ok()?;
    let identity = format!("{:x}", Sha256::digest(action.as_bytes()));
    Some(StructuredActionIdentity { identity, class })
}

fn response_evidence_identity(response: &ResponseInputItem) -> Option<String> {
    let canonical = match response {
        ResponseInputItem::Message {
            role,
            content,
            phase,
        } => serde_json::to_vec(&(role, content, phase)).ok()?,
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            serde_json::to_vec(output).ok()?
        }
        ResponseInputItem::McpToolCallOutput { output, .. } => serde_json::to_vec(output).ok()?,
        ResponseInputItem::ToolSearchOutput {
            status,
            execution,
            tools,
            omitted_result_count,
            ..
        } => serde_json::to_vec(&(status, execution, tools, omitted_result_count)).ok()?,
    };
    Some(format!("{:x}", Sha256::digest(canonical)))
}

fn value_evidence_identity(value: &Value) -> Option<String> {
    let canonical = serde_json::to_vec(&canonicalize_json(value)).ok()?;
    Some(format!("{:x}", Sha256::digest(canonical)))
}

fn terminal_failure_can_be_reused_without_dispatch(tool_name: &ToolName) -> bool {
    // Keep reuse limited to deterministic local state transitions and artifact reads whose
    // producer explicitly classified the failure as terminal. Process, filesystem search, and
    // MCP failures can recover while their arguments and request revision remain unchanged.
    tool_name_matches(tool_name, "update_plan") || tool_name_matches(tool_name, "read_tool_output")
}

fn response_failure_fingerprint(response: &ResponseInputItem) -> Option<String> {
    let value =
        response_output_text(response).and_then(|text| serde_json::from_str::<Value>(text).ok())?;
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
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .map(|value| canonicalize_json(&value))
        .or_else(|| canonical_response_body(response))
}

fn response_output_text(response: &ResponseInputItem) -> Option<&str> {
    let output = match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output,
        _ => return None,
    };
    match &output.body {
        codex_protocol::models::FunctionCallOutputBody::Text(text) => Some(text),
        codex_protocol::models::FunctionCallOutputBody::ContentItems(_) => None,
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

fn is_validation_invocation(tool_name: &ToolName, payload: &ToolPayload) -> bool {
    if !is_validation_tool(tool_name) {
        return false;
    }
    let ToolPayload::Function { arguments } = payload else {
        return false;
    };
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    let script_field = if tool_name_matches(tool_name, "shell_command") {
        "command"
    } else {
        "cmd"
    };
    let script = arguments.get(script_field).and_then(Value::as_str);
    let kind = arguments.get("kind").and_then(Value::as_str);
    let program = arguments.get("program").and_then(Value::as_str);
    let args = arguments.get("args").and_then(Value::as_array).map(|args| {
        args.iter()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    });
    let script_body = arguments.get("script_body").and_then(Value::as_str);
    let Ok(invocation) = CommandInvocation::from_parts(
        tool_name.name.as_str(),
        script_field,
        script,
        kind,
        program,
        args.as_deref(),
        script_body,
    ) else {
        return false;
    };
    matches!(
        classify_validation(&invocation),
        ValidationClassification::Validation { leaves, .. } if !leaves.is_empty()
    )
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
    let arguments = &canonical.value;
    let program = arguments.get("program").and_then(Value::as_str);
    let args = arguments.get("args").and_then(Value::as_array);
    if let Some(program) = program {
        let program = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        let args = args
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        return classify_read_only_evidence_program(&program, &args);
    }
    let script = ["cmd", "command", "script_body"]
        .iter()
        .find_map(|field| arguments.get(*field).and_then(Value::as_str));
    let Some(script) = script.map(str::trim) else {
        return StructuredActionClass::Other;
    };
    if script
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '>' | '\n' | '\r'))
    {
        return StructuredActionClass::Other;
    }
    let words = script.split_whitespace().collect::<Vec<_>>();
    let Some((program, args)) = words.split_first() else {
        return StructuredActionClass::Other;
    };
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    classify_read_only_evidence_program(&program, args)
}

fn classify_read_only_evidence_program(program: &str, args: &[&str]) -> StructuredActionClass {
    match program {
        "rg" if args.contains(&"--files") => StructuredActionClass::BroadSource,
        "rg" | "grep" | "findstr" if !args.is_empty() => StructuredActionClass::PreciseSource,
        "cat" | "type" | "head" | "tail" | "bat" if !args.is_empty() => {
            StructuredActionClass::PreciseSource
        }
        "git" => match args
            .first()
            .map(|subcommand| subcommand.to_ascii_lowercase())
        {
            Some(subcommand) if matches!(subcommand.as_str(), "status" | "log" | "ls-files") => {
                StructuredActionClass::BroadSource
            }
            Some(subcommand)
                if matches!(subcommand.as_str(), "diff" | "show" | "grep" | "blame") =>
            {
                StructuredActionClass::PreciseSource
            }
            _ => StructuredActionClass::Other,
        },
        _ => StructuredActionClass::Other,
    }
}

fn tool_name_matches(tool_name: &ToolName, candidate: &str) -> bool {
    tool_name.name == candidate || tool_name.name.ends_with(&format!("__{candidate}"))
}

pub(crate) struct SamplingReasoningGovernor {
    enabled: bool,
    phase: SamplingReasoningPhase,
    trigger: ReasoningPolicyTrigger,
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
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SamplingConvergenceDecision {
    pub(crate) continuation: ContinuationDisposition,
    pub(crate) directive: Option<String>,
    pub(crate) proven_loop_activated: bool,
    pub(crate) authoritative_wait: Option<AuthoritativeWaitResolution>,
}

impl SamplingReasoningGovernor {
    #[cfg(test)]
    pub(crate) fn new(config: Option<&ReasoningPhaseEfforts>) -> Self {
        Self::new_with_timing(config, Arc::new(TurnTimingState::default()))
    }

    pub(crate) fn new_with_timing(
        config: Option<&ReasoningPhaseEfforts>,
        timing: Arc<TurnTimingState>,
    ) -> Self {
        Self {
            enabled: config.is_some(),
            phase: SamplingReasoningPhase::Orient,
            trigger: ReasoningPolicyTrigger::UserInput,
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
        }
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
        protocol_requests_resample: bool,
    ) -> GenerationRequestDisposition {
        let relevant_state_fingerprint = format!(
            "{:x}",
            Sha256::digest(self.settled_revision_key(settled).as_bytes())
        );
        let residual_proof = self.residual_deterministic_sampling_proof(
            baselines,
            collector,
            settled,
            has_pending_input,
            protocol_requests_resample,
            &relevant_state_fingerprint,
        );
        GenerationRequestDisposition {
            // Owners drain before returning a tool result. Once execution has
            // returned ambiguous or new evidence, unknown cases must fail open.
            purpose: collector.generation_purpose(
                baselines,
                settled,
                has_pending_input,
                residual_proof.is_some(),
            ),
            sampling: residual_proof
                .map(SamplingGenerationDisposition::ResidualDeterministic)
                .unwrap_or(SamplingGenerationDisposition::DecisionBearing),
            relevant_state_fingerprint,
            failure_fingerprint: collector.failure_fingerprint(),
            terminal_completion_only: false,
        }
    }

    fn residual_deterministic_sampling_proof(
        &self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
        has_pending_input: bool,
        protocol_requests_resample: bool,
        relevant_state_fingerprint: &str,
    ) -> Option<ResidualDeterministicSamplingProof> {
        if has_pending_input
            || settled.mutation_revision != baselines.mutation_revision
            || self.plan_revision != baselines.plan_revision
            || self.input_revision != baselines.input_revision
            || settled.tool_exposure_revision != baselines.tool_exposure_revision
        {
            return None;
        }

        let state = collector
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.registered_count == 0 && state.outcomes.is_empty() {
            if !protocol_requests_resample {
                return None;
            }
            return Some(ResidualDeterministicSamplingProof {
                relevant_state_fingerprint: relevant_state_fingerprint.to_string(),
                exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
            });
        }
        drop(state);

        let cycle = collector.deterministic_cycle()?;
        let settled_revision = self.settled_revision_key(settled);
        // Broad source passes and failures start the no-progress sequence on
        // their first observation because they cannot advance state. They
        // still require one model interpretation before an identical second
        // observation can become residual-deterministic. Successful
        // structured evidence starts at zero, so its exact repetition reaches
        // the same two-observation fixed point at one.
        let fixed_point_observed = match cycle.kind {
            DeterministicCycleKind::BroadSourcePass
            | DeterministicCycleKind::ToolFailure
            | DeterministicCycleKind::NestedToolFailure => self.consecutive_no_progress > 1,
            DeterministicCycleKind::Empty => false,
            _ => self.consecutive_no_progress > 0,
        };
        (fixed_point_observed
            && self.last_cycle.as_deref() == Some(cycle.key.as_str())
            && self.last_state_revision.as_deref() == Some(settled_revision.as_str()))
        .then(|| ResidualDeterministicSamplingProof {
            relevant_state_fingerprint: relevant_state_fingerprint.to_string(),
            exact_action: ResidualDeterministicAction::RequireChangedContinuation,
        })
    }

    #[cfg(test)]
    pub(crate) fn resolve_policy(
        &self,
        config: Option<&ReasoningPhaseEfforts>,
        turn_fallback: Option<ReasoningEffort>,
        model_info: &ModelInfo,
    ) -> SamplingRequestPolicy {
        resolve_request_policy(
            self.enabled.then_some(self.phase),
            config,
            turn_fallback,
            model_info,
        )
    }

    pub(crate) fn phase(&self) -> Option<SamplingReasoningPhase> {
        self.enabled.then_some(self.phase)
    }

    pub(crate) fn trigger(&self) -> ReasoningPolicyTrigger {
        self.trigger
    }

    pub(crate) fn accepted_user_input(&mut self) {
        self.input_revision = self.input_revision.saturating_add(1);
        self.reset_convergence();
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timing = Arc::clone(&ledger.timing);
        *ledger = DeterministicDispatchLedger::new(timing);
        drop(ledger);
        self.transition_to(
            SamplingReasoningPhase::Orient,
            ReasoningPolicyTrigger::UserInput,
        );
    }

    pub(crate) fn host_diagnose(&mut self) {
        self.transition_to(
            SamplingReasoningPhase::Diagnose,
            ReasoningPolicyTrigger::HostOverride,
        );
    }

    #[cfg(test)]
    pub(crate) fn host_mutation(&mut self) {
        self.transition_to(
            SamplingReasoningPhase::Implement,
            ReasoningPolicyTrigger::WorkspaceMutation,
        );
    }

    /// Records a host continuation that intentionally preserves the current phase.
    pub(crate) fn host_retain(&self) {}

    fn transition_to(&mut self, phase: SamplingReasoningPhase, trigger: ReasoningPolicyTrigger) {
        if self.enabled {
            self.phase = phase;
            self.trigger = trigger;
        }
    }

    pub(crate) fn evaluate_convergence(
        &mut self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> SamplingConvergenceDecision {
        let settled_revision = self.settled_revision_key(settled);
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
        let (request_tool_calls, request_child_runtime_ms) = if efficiency_sample_eligible {
            collector.turn_efficiency_sample()
        } else {
            (0, 0)
        };
        let request_runtime_limit_ms = u64::try_from(request_tool_calls)
            .unwrap_or(u64::MAX)
            .saturating_mul(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        let request_has_negligible_runtime =
            request_tool_calls > 0 && request_child_runtime_ms <= request_runtime_limit_ms;
        let request_cycle = efficiency_sample_eligible
            .then(|| collector.deterministic_cycle())
            .flatten();
        if request_tool_calls > 0 && !request_has_negligible_runtime {
            // A child that performed substantive work may have learned new
            // information even when host-visible state and normalized output
            // identity are unchanged. Treat that observation as progress and
            // fail open instead of spending the non-progress budget.
            self.reset_convergence();
            self.reset_turn_efficiency_guard();
            self.reset_distinct_failure_recovery();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        }
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
        if self.turn_efficiency_guard.as_ref().is_some_and(|guard| {
            guard.settled_revision != settled_revision || !request_has_negligible_runtime
        }) {
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
                continuation: ContinuationDisposition::TerminalCompletionRequired,
                directive: Some(
                    "Turn-efficiency guard: the same deterministic tool cycle repeated after the consolidation directive while state stayed unchanged. Complete now from the returned evidence; do not call another tool."
                        .to_string(),
                ),
                proven_loop_activated: true,
                authoritative_wait: None,
            };
        }

        self.turn_efficiency_tool_calls = self
            .turn_efficiency_tool_calls
            .saturating_add(request_tool_calls);
        self.turn_efficiency_child_runtime_ms = self
            .turn_efficiency_child_runtime_ms
            .saturating_add(request_child_runtime_ms);
        let negligible_runtime_limit_ms = u64::try_from(self.turn_efficiency_tool_calls)
            .unwrap_or(u64::MAX)
            .saturating_mul(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL);
        let exceeds_turn_efficiency_guard = request_tool_calls > 0
            && repeated_cycle
            && self.turn_efficiency_tool_calls >= TURN_EFFICIENCY_TOOL_CALL_THRESHOLD
            && self.turn_efficiency_child_runtime_ms <= negligible_runtime_limit_ms;
        if exceeds_turn_efficiency_guard && self.turn_efficiency_guard.is_none() {
            // The first high-volume negligible-runtime observation is only a
            // consolidation signal. Preserve its exact cycle and settled state
            // so only a subsequent identical deterministic cycle can enforce
            // the terminal boundary.
            self.turn_efficiency_guard = Some(TurnEfficiencyGuardHandle {
                settled_revision: settled_revision.clone(),
                deterministic_cycle: request_deterministic_cycle,
            });
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = true;
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::ModelRequired,
                directive: Some(
                    "Turn-efficiency guard: this turn accumulated many tool calls with negligible child runtime while state stayed unchanged. Consolidate the returned evidence now; use another tool only when additional evidence or state change is necessary."
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
                continuation: ContinuationDisposition::TerminalCompletionRequired,
                directive: Some(
                    "Convergence enforced: the unchanged authoritative wait was repeated after its blocker was surfaced. No further tools are available for this turn. Act from existing evidence in the final response and truthfully report the blocker if it remains unresolved."
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
        // requested two-observation fixed point. Ambiguous or merely empty
        // cycles retain the more conservative three-observation threshold.
        let threshold = if repeated_cycle { 1 } else { 3 };
        if self.consecutive_no_progress < threshold
            && self.consecutive_obligation_no_progress < threshold
        {
            return SamplingConvergenceDecision::default();
        }

        // Even an exact repeated failure gets one model-visible convergence
        // advisory before tools are removed. This keeps the terminal boundary
        // deterministic without turning the second failed recovery attempt
        // directly into forced completion.
        let proven_loop_activated =
            self.directive_issued && repeated_cycle && !self.proven_loop_active;
        if proven_loop_activated {
            self.proven_loop_active = true;
        }
        self.directive_issued = true;
        let directive = if proven_loop_activated {
            "Convergence enforced: an ordered deterministic action/result cycle repeated after the convergence directive against identical state. No further tools are available for this turn. Provide the final response now using existing evidence, truthfully reporting any blocker or incomplete validation."
        } else if cycle.kind == DeterministicCycleKind::BroadSourcePass {
            "Convergence required: the broad source pass repeated against the same obligation and returned the same evidence for the same action. Before another broad source pass, change the active obligation, provide a new evidence identity, or choose a materially different action; otherwise synthesize the result, begin implementation, or truthfully complete."
        } else if self.consecutive_no_progress == threshold
            || self.consecutive_obligation_no_progress == threshold
        {
            "Convergence required: repeated deterministic evidence produced no obligation-level state progress. Synthesize from existing evidence, take a state-changing action, or truthfully complete instead of exploring again."
        } else if self.proven_loop_active {
            "Convergence escalation: an ordered deterministic action/result cycle has repeated after the convergence directive against identical state. Do not repeat it. Change the hypothesis or state, narrow the observation, or truthfully complete; existing task lifecycle rules still govern termination."
        } else {
            "Convergence escalation: structured state still has not changed. Equivalent completed actions remain blocked. Choose a new hypothesis, change state, narrow the observation, or truthfully complete; a no-progress count alone never ends the task."
        };
        SamplingConvergenceDecision {
            continuation: if proven_loop_activated {
                ContinuationDisposition::TerminalCompletionRequired
            } else {
                ContinuationDisposition::ModelRequired
            },
            directive: Some(directive.to_string()),
            proven_loop_activated,
            authoritative_wait: None,
        }
    }

    fn reset_convergence(&mut self) {
        self.consecutive_no_progress = 0;
        self.consecutive_obligation_no_progress = 0;
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

    fn settled_revision_key(&self, settled: &SamplingRequestSettledState) -> String {
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
        if !self.enabled {
            return;
        }
        let outcomes = collector.snapshot();
        let latest_plan = outcomes
            .iter()
            .filter(|outcome| outcome.kind == SamplingToolOutcomeKind::Success)
            .filter_map(|outcome| outcome.plan.as_ref().map(|plan| (outcome.ordinal, plan)))
            .max_by_key(|(ordinal, _)| *ordinal)
            .map(|(_, plan)| plan.clone());
        let changed_plan = latest_plan.filter(|plan| {
            self.plan
                .as_ref()
                .is_none_or(|current| current.plan != plan.plan)
        });
        if let Some(plan) = changed_plan.as_ref() {
            self.plan = Some(plan.clone());
            self.plan_revision = self.plan_revision.saturating_add(1);
        }
        let saw_validation = collector.saw_validation();
        if let Some(failure) = outcomes
            .iter()
            .filter(|outcome| outcome.is_failure_evidence())
            .min_by_key(|outcome| outcome.ordinal)
        {
            let trigger = if saw_validation {
                if failure.kind == SamplingToolOutcomeKind::Timeout {
                    ReasoningPolicyTrigger::ValidationTimedOut
                } else {
                    ReasoningPolicyTrigger::ValidationFailed
                }
            } else {
                match failure.kind {
                    SamplingToolOutcomeKind::Failure => ReasoningPolicyTrigger::ToolFailed,
                    SamplingToolOutcomeKind::Blocked => ReasoningPolicyTrigger::ToolBlocked,
                    SamplingToolOutcomeKind::Timeout => ReasoningPolicyTrigger::ToolTimedOut,
                    SamplingToolOutcomeKind::RecoverableCancellation => {
                        ReasoningPolicyTrigger::ToolCancelled
                    }
                    SamplingToolOutcomeKind::Skipped => ReasoningPolicyTrigger::ToolBlocked,
                    SamplingToolOutcomeKind::Yielded => unreachable!("yielded is not a failure"),
                    SamplingToolOutcomeKind::Unknown => unreachable!("unknown is not a failure"),
                    SamplingToolOutcomeKind::Success => unreachable!("success is not a failure"),
                }
            };
            self.transition_to(SamplingReasoningPhase::Diagnose, trigger);
            return;
        }
        if saw_validation {
            self.transition_to(
                if self.plan.as_ref().is_some_and(plan_is_unfinished) {
                    SamplingReasoningPhase::Verify
                } else {
                    SamplingReasoningPhase::Finalize
                },
                ReasoningPolicyTrigger::ValidationPassed,
            );
            return;
        }
        if settled.mutation_revision > baselines.mutation_revision {
            self.transition_to(
                SamplingReasoningPhase::Implement,
                ReasoningPolicyTrigger::WorkspaceMutation,
            );
            return;
        }
        if self.plan_revision > baselines.plan_revision
            && let Some(plan) = changed_plan.as_ref()
        {
            self.transition_to(phase_for_plan(plan), ReasoningPolicyTrigger::PlanUpdated);
            return;
        }
        if outcomes.iter().any(|outcome| {
            outcome.kind == SamplingToolOutcomeKind::Success
                && outcome.unfinished_mutation_obligation
        }) {
            self.transition_to(
                SamplingReasoningPhase::Implement,
                ReasoningPolicyTrigger::PlanUpdated,
            );
            return;
        }
        if outcomes.iter().any(SamplingToolOutcome::is_generic_success) {
            let next_phase = match self.phase {
                SamplingReasoningPhase::Orient => SamplingReasoningPhase::Inspect,
                SamplingReasoningPhase::Inspect => SamplingReasoningPhase::Inspect,
                SamplingReasoningPhase::Implement => SamplingReasoningPhase::Implement,
                SamplingReasoningPhase::Diagnose => SamplingReasoningPhase::Diagnose,
                SamplingReasoningPhase::Verify => SamplingReasoningPhase::Verify,
                SamplingReasoningPhase::Finalize => SamplingReasoningPhase::Finalize,
            };
            self.transition_to(next_phase, ReasoningPolicyTrigger::ReadOnlyToolSuccess);
        }
    }
}

pub(crate) fn resolve_request_policy(
    phase: Option<SamplingReasoningPhase>,
    config: Option<&ReasoningPhaseEfforts>,
    turn_fallback: Option<ReasoningEffort>,
    model_info: &ModelInfo,
) -> SamplingRequestPolicy {
    resolve_request_policy_for_generation(
        phase,
        config,
        turn_fallback,
        model_info,
        &SamplingGenerationDisposition::DecisionBearing,
    )
}

pub(crate) fn resolve_request_policy_for_generation(
    phase: Option<SamplingReasoningPhase>,
    config: Option<&ReasoningPhaseEfforts>,
    turn_fallback: Option<ReasoningEffort>,
    model_info: &ModelInfo,
    sampling: &SamplingGenerationDisposition,
) -> SamplingRequestPolicy {
    if sampling.is_residual_deterministic() {
        let configured_override =
            config.and_then(|config| config.deterministic_continuation.clone());
        let configured_effort = configured_override
            .clone()
            .or(turn_fallback)
            .unwrap_or(ReasoningEffort::Low);
        let effective_effort = lowest_supported_equivalent(configured_effort.clone(), model_info);
        return SamplingRequestPolicy {
            phase,
            configured_effort: Some(configured_effort),
            request_effort: request_effort(effective_effort.clone()),
            effective_effort,
            source: if configured_override.is_some() {
                SamplingRequestPolicySource::PhaseOverride
            } else {
                SamplingRequestPolicySource::TurnFallback
            },
        };
    }
    let (configured_effort, source) = match (config, phase) {
        (Some(config), Some(phase)) => {
            let override_effort = match phase {
                SamplingReasoningPhase::Orient => config.orient.clone(),
                SamplingReasoningPhase::Inspect => config.inspect.clone(),
                SamplingReasoningPhase::Implement => config.implement.clone(),
                SamplingReasoningPhase::Diagnose => config.diagnose.clone(),
                SamplingReasoningPhase::Verify => config.verify.clone(),
                SamplingReasoningPhase::Finalize => config.finalize.clone(),
            };
            let source = if override_effort.is_some() {
                SamplingRequestPolicySource::PhaseOverride
            } else {
                SamplingRequestPolicySource::TurnFallback
            };
            (override_effort.or(turn_fallback), source)
        }
        _ => (turn_fallback, SamplingRequestPolicySource::TurnFallback),
    };
    let effective_effort = if config.is_some() && phase.is_some() {
        supported_effort(configured_effort.clone(), model_info)
    } else {
        configured_effort
            .clone()
            .or_else(|| supported_effort(None, model_info))
    };
    SamplingRequestPolicy {
        phase,
        configured_effort,
        request_effort: request_effort(effective_effort.clone()),
        effective_effort,
        source,
    }
}

fn lowest_supported_equivalent(
    selected: ReasoningEffort,
    model_info: &ModelInfo,
) -> Option<ReasoningEffort> {
    if !model_info.supports_reasoning_summaries {
        return None;
    }
    if model_info
        .supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == selected)
    {
        return Some(selected);
    }
    model_info
        .supported_reasoning_levels
        .iter()
        .min_by_key(|preset| reasoning_effort_rank(&preset.effort))
        .map(|preset| preset.effort.clone())
        .or_else(|| model_info.default_reasoning_level.clone())
}

fn reasoning_effort_rank(effort: &ReasoningEffort) -> u8 {
    match effort {
        ReasoningEffort::None => 0,
        ReasoningEffort::Minimal => 1,
        ReasoningEffort::Low => 2,
        ReasoningEffort::Medium => 3,
        ReasoningEffort::High => 4,
        ReasoningEffort::XHigh => 5,
        ReasoningEffort::Max => 6,
        ReasoningEffort::Ultra => 7,
        ReasoningEffort::Custom(_) => 8,
    }
}

fn request_effort(effort: Option<ReasoningEffort>) -> Option<ReasoningEffort> {
    effort.map(|effort| match effort {
        ReasoningEffort::Ultra => ReasoningEffort::Max,
        effort => effort,
    })
}

fn supported_effort(
    selected: Option<ReasoningEffort>,
    model_info: &ModelInfo,
) -> Option<ReasoningEffort> {
    if !model_info.supports_reasoning_summaries {
        return None;
    }
    let Some(selected) = selected else {
        return model_info.default_reasoning_level.clone();
    };
    if model_info
        .supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == selected)
    {
        return Some(selected);
    }
    let selected_rank = reasoning_effort_rank(&selected);
    model_info
        .supported_reasoning_levels
        .iter()
        .min_by_key(|preset| {
            let rank = reasoning_effort_rank(&preset.effort);
            (rank.abs_diff(selected_rank), rank)
        })
        .map(|preset| preset.effort.clone())
        .or_else(|| model_info.default_reasoning_level.clone())
}

fn plan_is_unfinished(plan: &UpdatePlanArgs) -> bool {
    !plan.plan.is_empty()
        && plan
            .plan
            .iter()
            .any(|item| item.status != StepStatus::Completed)
}

fn phase_for_plan(plan: &UpdatePlanArgs) -> SamplingReasoningPhase {
    if plan
        .plan
        .iter()
        .all(|item| item.status == StepStatus::Completed)
    {
        SamplingReasoningPhase::Finalize
    } else if plan
        .plan
        .iter()
        .any(|item| item.status == StepStatus::InProgress)
    {
        SamplingReasoningPhase::Implement
    } else {
        SamplingReasoningPhase::Inspect
    }
}

#[cfg(test)]
mod tests {
    use codex_protocol::openai_models::ReasoningEffortPreset;
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::protocol::DeterministicContinuationClass;
    use codex_protocol::protocol::DeterministicContinuationHostAction;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn config() -> ReasoningPhaseEfforts {
        ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::Medium),
            inspect: Some(ReasoningEffort::Low),
            implement: Some(ReasoningEffort::High),
            diagnose: Some(ReasoningEffort::High),
            verify: Some(ReasoningEffort::Low),
            finalize: Some(ReasoningEffort::Low),
            deterministic_continuation: Some(ReasoningEffort::Low),
        }
    }

    fn model(levels: &[ReasoningEffort], default: ReasoningEffort) -> ModelInfo {
        let mut model: ModelInfo = serde_json::from_value(json!({
            "slug": "test-model",
            "display_name": "test-model",
            "description": "test",
            "default_reasoning_level": default,
            "supported_reasoning_levels": [],
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1,
            "upgrade": null,
            "base_instructions": "base",
            "model_messages": null,
            "supports_reasoning_summaries": true,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10000},
            "supports_parallel_tool_calls": true,
            "supports_image_detail_original": false,
            "context_window": 10000,
            "auto_compact_token_limit": null,
            "experimental_supported_tools": []
        }))
        .expect("model info");
        model.supported_reasoning_levels = levels
            .iter()
            .cloned()
            .map(|effort| ReasoningEffortPreset {
                description: effort.to_string(),
                effort,
            })
            .collect();
        model
    }

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
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn collector_with(outcome: SamplingToolOutcomeKind) -> SamplingRequestSignalCollector {
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(0, outcome, None));
        collector
    }

    fn collector_with_read_and(outcome: SamplingToolOutcomeKind) -> SamplingRequestSignalCollector {
        let collector = collector_with(SamplingToolOutcomeKind::Success);
        collector.push(SamplingToolOutcome::plain(1, outcome, None));
        collector
    }

    #[test]
    fn yielded_tool_output_is_resumable_and_not_failure_evidence() {
        let kind = sampling_tool_outcome_kind(ToolOutputOutcome::Yielded, None);

        assert_eq!(kind, SamplingToolOutcomeKind::Yielded);
        assert!(!outcome_reopens_failure_evidence(kind, None));
    }

    #[test]
    fn unknown_tool_outcome_is_not_failure_evidence() {
        let signal = json!({"outcome": "partial_success"});
        let outcome = SamplingToolOutcome::from_signal(
            0,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            None,
            Some(&signal),
        );

        assert_eq!(outcome.kind, SamplingToolOutcomeKind::Unknown);
        assert!(!outcome.is_failure_evidence());
        assert!(!outcome.failure_is_terminal);
    }

    fn collector_with_read_and_plan(plan: UpdatePlanArgs) -> SamplingRequestSignalCollector {
        let collector = collector_with(SamplingToolOutcomeKind::Success);
        collector.push(SamplingToolOutcome::plain(
            1,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        collector
    }

    fn settled(mutation_revision: u64) -> SamplingRequestSettledState {
        SamplingRequestSettledState {
            mutation_revision,
            tool_exposure_revision: 0,
        }
    }

    fn validation_collector(outcome: SamplingToolOutcomeKind) -> SamplingRequestSignalCollector {
        let collector = collector_with(outcome);
        collector
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .saw_validation = true;
        collector
    }

    fn settle_plan(governor: &mut SamplingReasoningGovernor, plan: UpdatePlanArgs) {
        let baselines = governor.baselines(0);
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(
            0,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        governor.settle(&baselines, &collector, &settled(0));
    }

    #[test]
    fn resolver_preserves_raw_configuration_and_normalizes_the_request_once() {
        let ultra_only = model(&[ReasoningEffort::Ultra], ReasoningEffort::Ultra);
        let config = ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let policy = resolve_request_policy(
            Some(SamplingReasoningPhase::Orient),
            Some(&config),
            Some(ReasoningEffort::Low),
            &ultra_only,
        );

        assert_eq!(policy.configured_effort, Some(ReasoningEffort::High));
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::Ultra));
        assert_eq!(policy.request_effort, Some(ReasoningEffort::Max));

        let mut without_reasoning = ultra_only;
        without_reasoning.supports_reasoning_summaries = false;
        let no_reasoning = resolve_request_policy(
            Some(SamplingReasoningPhase::Orient),
            Some(&config),
            Some(ReasoningEffort::Low),
            &without_reasoning,
        );
        assert_eq!(no_reasoning.effective_effort, None);
        assert_eq!(no_reasoning.request_effort, None);
    }

    #[test]
    fn deterministic_changed_continuation_uses_the_lowest_supported_override() {
        let model = model(
            &[ReasoningEffort::Medium, ReasoningEffort::High],
            ReasoningEffort::High,
        );
        let config = config();
        let residual = SamplingGenerationDisposition::ResidualDeterministic(
            ResidualDeterministicSamplingProof {
                relevant_state_fingerprint: "state".to_string(),
                exact_action: ResidualDeterministicAction::RequireChangedContinuation,
            },
        );

        let deterministic = resolve_request_policy_for_generation(
            Some(SamplingReasoningPhase::Finalize),
            Some(&config),
            Some(ReasoningEffort::High),
            &model,
            &residual,
        );
        assert_eq!(deterministic.configured_effort, Some(ReasoningEffort::Low));
        assert_eq!(
            deterministic.effective_effort,
            Some(ReasoningEffort::Medium)
        );

        for phase in [
            SamplingReasoningPhase::Implement,
            SamplingReasoningPhase::Diagnose,
            SamplingReasoningPhase::Verify,
            SamplingReasoningPhase::Finalize,
        ] {
            let ordinary = resolve_request_policy_for_generation(
                Some(phase),
                Some(&config),
                Some(ReasoningEffort::High),
                &model,
                &SamplingGenerationDisposition::DecisionBearing,
            );
            let expected = match phase {
                SamplingReasoningPhase::Verify | SamplingReasoningPhase::Finalize => {
                    ReasoningEffort::Medium
                }
                SamplingReasoningPhase::Implement | SamplingReasoningPhase::Diagnose => {
                    ReasoningEffort::High
                }
                SamplingReasoningPhase::Orient | SamplingReasoningPhase::Inspect => unreachable!(),
            };
            assert_eq!(ordinary.effective_effort, Some(expected));
        }
    }

    #[test]
    fn residual_deterministic_sampling_defaults_to_low_without_an_override() {
        let model = model(
            &[ReasoningEffort::Low, ReasoningEffort::High],
            ReasoningEffort::High,
        );
        let residual = SamplingGenerationDisposition::ResidualDeterministic(
            ResidualDeterministicSamplingProof {
                relevant_state_fingerprint: "state".to_string(),
                exact_action: ResidualDeterministicAction::RequireChangedContinuation,
            },
        );

        let policy = resolve_request_policy_for_generation(
            Some(SamplingReasoningPhase::Finalize),
            Some(&ReasoningPhaseEfforts::default()),
            Some(ReasoningEffort::High),
            &model,
            &residual,
        );

        assert_eq!(policy.configured_effort, Some(ReasoningEffort::Low));
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::Low));
        assert_eq!(policy.source, SamplingRequestPolicySource::TurnFallback);
    }

    #[test]
    fn kd4_latency_stable_continuation_protocol_terminal_elides_sampling_only_when_unchanged() {
        let governor = SamplingReasoningGovernor::new(Some(&ReasoningPhaseEfforts::default()));
        let baselines = governor.baselines(7);
        let unchanged = settled(7);

        let empty_collector = governor.collector(&baselines);
        let proven = governor.continuation_generation_request(
            &baselines,
            &empty_collector,
            &unchanged,
            false,
            true,
        );
        assert!(matches!(
            &proven.sampling,
            SamplingGenerationDisposition::ResidualDeterministic(_)
        ));
        assert!(proven.completes_protocol_turn_deterministically());
        assert_eq!(
            proven.timing_disposition(),
            TurnTimingGenerationDisposition::Deterministic
        );

        let with_tool_evidence = governor.collector(&baselines);
        with_tool_evidence.register_tool_call();
        let tool_result = governor.continuation_generation_request(
            &baselines,
            &with_tool_evidence,
            &unchanged,
            false,
            true,
        );
        assert_eq!(
            tool_result.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
        assert!(!tool_result.completes_protocol_turn_deterministically());
        assert_eq!(
            tool_result.timing_disposition(),
            TurnTimingGenerationDisposition::DecisionBearing
        );

        let with_input = governor.continuation_generation_request(
            &baselines,
            &empty_collector,
            &unchanged,
            true,
            true,
        );
        assert_eq!(
            with_input.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );

        let changed = settled(8);
        let changed_state = governor.continuation_generation_request(
            &baselines,
            &empty_collector,
            &changed,
            false,
            true,
        );
        assert_eq!(
            changed_state.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
    }

    #[test]
    fn resolver_uses_model_default_when_effort_is_omitted() {
        let default_high = model(&[ReasoningEffort::High], ReasoningEffort::High);

        let policy = resolve_request_policy(None, None, None, &default_high);

        assert_eq!(policy.configured_effort, None);
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::High));
        assert_eq!(policy.request_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn recorder_deduplicates_bounds_history_and_finalizes_once() {
        let model = model(&[ReasoningEffort::Medium], ReasoningEffort::Medium);
        let policy = resolve_request_policy(
            Some(SamplingReasoningPhase::Orient),
            Some(&config()),
            None,
            &model,
        );
        let recorder = ReasoningPolicyRecorder::new(true);

        assert!(
            recorder
                .append(
                    &policy,
                    "test-model".into(),
                    ReasoningPolicyTrigger::UserInput
                )
                .is_some()
        );
        assert!(
            recorder
                .append(
                    &policy,
                    "test-model".into(),
                    ReasoningPolicyTrigger::UserInput
                )
                .is_none()
        );
        for index in 0..64 {
            let trigger = if index % 2 == 0 {
                ReasoningPolicyTrigger::ReadOnlyToolSuccess
            } else {
                ReasoningPolicyTrigger::WorkspaceMutation
            };
            assert!(
                recorder
                    .append(&policy, "test-model".into(), trigger)
                    .is_some()
            );
        }

        let summary = recorder.take_summary("turn-1".into()).expect("summary");
        assert_eq!(summary.total_entries, 65);
        assert!(summary.truncated);
        assert_eq!(summary.entries.len(), 64);
        assert_eq!(summary.entries.first().map(|entry| entry.sequence), Some(2));
        assert_eq!(summary.entries.last().map(|entry| entry.sequence), Some(65));
        assert!(recorder.take_summary("turn-1".into()).is_none());
    }

    #[test]
    fn earliest_failure_wins() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(
            2,
            SamplingToolOutcomeKind::Failure,
            None,
        ));
        collector.push(SamplingToolOutcome::plain(
            1,
            SamplingToolOutcomeKind::Blocked,
            None,
        ));
        let baseline = governor.baselines(0);
        governor.settle(&baseline, &collector, &settled(0));
        assert_eq!(governor.phase(), Some(SamplingReasoningPhase::Diagnose));
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::ToolBlocked);
    }

    #[test]
    fn disabled_empty_partial_and_full_policies_have_expected_semantics() {
        let model = model(
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            ReasoningEffort::Medium,
        );
        let disabled = SamplingReasoningGovernor::new(None);
        assert_eq!(
            disabled.resolve_policy(None, Some(ReasoningEffort::High), &model),
            SamplingRequestPolicy {
                phase: None,
                configured_effort: Some(ReasoningEffort::High),
                effective_effort: Some(ReasoningEffort::High),
                request_effort: Some(ReasoningEffort::High),
                source: SamplingRequestPolicySource::TurnFallback,
            }
        );

        let empty_config = ReasoningPhaseEfforts::default();
        let governed = SamplingReasoningGovernor::new(Some(&empty_config));
        assert_eq!(
            governed.resolve_policy(Some(&empty_config), Some(ReasoningEffort::Medium), &model,),
            SamplingRequestPolicy {
                phase: Some(SamplingReasoningPhase::Orient),
                configured_effort: Some(ReasoningEffort::Medium),
                effective_effort: Some(ReasoningEffort::Medium),
                request_effort: Some(ReasoningEffort::Medium),
                source: SamplingRequestPolicySource::TurnFallback,
            }
        );

        let partial = ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        assert_eq!(
            governed
                .resolve_policy(Some(&partial), Some(ReasoningEffort::High), &model)
                .source,
            SamplingRequestPolicySource::PhaseOverride
        );
        assert_eq!(
            governed
                .resolve_policy(Some(&config()), Some(ReasoningEffort::Low), &model)
                .effective_effort,
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn unsupported_override_falls_back_once_and_preserves_override_source() {
        let config = ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let governor = SamplingReasoningGovernor::new(Some(&config));
        let medium_only = model(&[ReasoningEffort::Medium], ReasoningEffort::Medium);
        let policy =
            governor.resolve_policy(Some(&config), Some(ReasoningEffort::Low), &medium_only);
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::Medium));
        assert_eq!(policy.source, SamplingRequestPolicySource::PhaseOverride);

        let low_only = model(&[ReasoningEffort::Low], ReasoningEffort::Low);
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::Medium));
        assert_eq!(
            governor
                .resolve_policy(Some(&config), Some(ReasoningEffort::Low), &low_only)
                .effective_effort,
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn global_effort_survives_unoverridden_phases_and_deterministic_continuation() {
        let phase_efforts = ReasoningPhaseEfforts::default();
        let model = model(
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            ReasoningEffort::Medium,
        );

        for phase in [
            SamplingReasoningPhase::Orient,
            SamplingReasoningPhase::Inspect,
            SamplingReasoningPhase::Implement,
            SamplingReasoningPhase::Diagnose,
            SamplingReasoningPhase::Verify,
            SamplingReasoningPhase::Finalize,
        ] {
            let policy = resolve_request_policy(
                Some(phase),
                Some(&phase_efforts),
                Some(ReasoningEffort::High),
                &model,
            );
            assert_eq!(policy.configured_effort, Some(ReasoningEffort::High));
            assert_eq!(policy.effective_effort, Some(ReasoningEffort::High));
            assert_eq!(policy.source, SamplingRequestPolicySource::TurnFallback);
        }

        let deterministic = SamplingGenerationDisposition::ResidualDeterministic(
            ResidualDeterministicSamplingProof {
                relevant_state_fingerprint: "same-state".to_string(),
                exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
            },
        );
        let policy = resolve_request_policy_for_generation(
            Some(SamplingReasoningPhase::Finalize),
            Some(&phase_efforts),
            Some(ReasoningEffort::High),
            &model,
            &deterministic,
        );
        assert_eq!(policy.configured_effort, Some(ReasoningEffort::High));
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::High));
        assert_eq!(policy.source, SamplingRequestPolicySource::TurnFallback);

        let explicit = ReasoningPhaseEfforts {
            deterministic_continuation: Some(ReasoningEffort::Low),
            ..ReasoningPhaseEfforts::default()
        };
        let policy = resolve_request_policy_for_generation(
            Some(SamplingReasoningPhase::Finalize),
            Some(&explicit),
            Some(ReasoningEffort::High),
            &model,
            &deterministic,
        );
        assert_eq!(policy.configured_effort, Some(ReasoningEffort::Low));
        assert_eq!(policy.source, SamplingRequestPolicySource::PhaseOverride);
    }

    #[test]
    fn unsupported_effort_without_capabilities_is_not_emitted() {
        let mut model_without_capabilities = model(&[], ReasoningEffort::Medium);
        model_without_capabilities.default_reasoning_level = None;

        assert_eq!(
            supported_effort(Some(ReasoningEffort::High), &model_without_capabilities),
            None
        );
        assert_eq!(
            lowest_supported_equivalent(ReasoningEffort::High, &model_without_capabilities),
            None
        );
    }

    #[test]
    fn unsupported_effort_fallback_is_order_independent() {
        let ascending = model(
            &[ReasoningEffort::Low, ReasoningEffort::High],
            ReasoningEffort::Low,
        );
        let descending = model(
            &[ReasoningEffort::High, ReasoningEffort::Low],
            ReasoningEffort::Low,
        );

        assert_eq!(
            supported_effort(Some(ReasoningEffort::Medium), &ascending),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            supported_effort(Some(ReasoningEffort::Medium), &descending),
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn deferred_tool_activation_changes_relevant_state_identity() {
        let governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines_with_tool_exposure_revision(0, 4);
        let settled = SamplingRequestSettledState {
            mutation_revision: 0,
            tool_exposure_revision: 5,
        };

        assert_ne!(
            baselines.revision_key(),
            governor.settled_revision_key(&settled)
        );
    }

    #[test]
    fn turn_fallback_reports_raw_effort_after_model_normalization() {
        let ultra_only = model(&[ReasoningEffort::Ultra], ReasoningEffort::Ultra);

        let policy = resolve_request_policy(
            Some(SamplingReasoningPhase::Orient),
            Some(&ReasoningPhaseEfforts::default()),
            Some(ReasoningEffort::High),
            &ultra_only,
        );

        assert_eq!(policy.configured_effort, Some(ReasoningEffort::High));
        assert_eq!(policy.effective_effort, Some(ReasoningEffort::Ultra));
        assert_eq!(policy.request_effort, Some(ReasoningEffort::Max));
    }

    #[test]
    fn scripted_phase_and_effort_flow_is_deterministic() {
        let config = config();
        let model = model(
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            ReasoningEffort::Medium,
        );
        let mut governor = SamplingReasoningGovernor::new(Some(&config));

        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::Medium)
        );

        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(0),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Inspect);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::Low)
        );

        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &SamplingRequestSignalCollector::default(),
            &settled(1),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::High)
        );

        let baseline = governor.baselines(1);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Failure),
            &settled(1),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::High)
        );

        let baseline = governor.baselines(1);
        governor.settle(
            &baseline,
            &validation_collector(SamplingToolOutcomeKind::Success),
            &settled(1),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::Low)
        );
    }

    #[test]
    fn read_success_retains_sticky_phases_and_selects_their_explicit_effort() {
        let config = config();
        let model = model(
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            ReasoningEffort::Medium,
        );
        let cases = [
            (
                SamplingReasoningPhase::Orient,
                SamplingReasoningPhase::Inspect,
                config.inspect.clone(),
            ),
            (
                SamplingReasoningPhase::Inspect,
                SamplingReasoningPhase::Inspect,
                config.inspect.clone(),
            ),
            (
                SamplingReasoningPhase::Implement,
                SamplingReasoningPhase::Implement,
                config.implement.clone(),
            ),
            (
                SamplingReasoningPhase::Diagnose,
                SamplingReasoningPhase::Diagnose,
                config.diagnose.clone(),
            ),
            (
                SamplingReasoningPhase::Verify,
                SamplingReasoningPhase::Verify,
                config.verify.clone(),
            ),
            (
                SamplingReasoningPhase::Finalize,
                SamplingReasoningPhase::Finalize,
                config.finalize.clone(),
            ),
        ];

        for (starting_phase, resulting_phase, expected_configured_effort) in cases {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            governor.phase = starting_phase;
            let baseline = governor.baselines(0);
            governor.settle(
                &baseline,
                &collector_with(SamplingToolOutcomeKind::Success),
                &settled(0),
            );

            assert_eq!(
                governor.phase, resulting_phase,
                "starting at {starting_phase:?}"
            );
            assert_eq!(
                governor.trigger(),
                ReasoningPolicyTrigger::ReadOnlyToolSuccess,
                "starting at {starting_phase:?}"
            );
            let policy = governor.resolve_policy(Some(&config), None, &model);
            assert_eq!(
                policy.configured_effort, expected_configured_effort,
                "starting at {starting_phase:?}"
            );
            assert_eq!(
                policy.source,
                SamplingRequestPolicySource::PhaseOverride,
                "starting at {starting_phase:?}"
            );
        }
    }

    #[test]
    fn plan_statuses_select_the_reasoning_phase() {
        assert_eq!(
            phase_for_plan(&plan(&[StepStatus::Completed, StepStatus::Completed])),
            SamplingReasoningPhase::Finalize
        );
        assert_eq!(
            phase_for_plan(&plan(&[StepStatus::InProgress, StepStatus::Pending])),
            SamplingReasoningPhase::Implement
        );
        assert_eq!(
            phase_for_plan(&plan(&[StepStatus::Pending])),
            SamplingReasoningPhase::Inspect
        );
    }

    #[test]
    fn tool_failures_dominate_a_competing_read() {
        let config = config();
        let cases = [
            (
                SamplingToolOutcomeKind::Failure,
                ReasoningPolicyTrigger::ToolFailed,
            ),
            (
                SamplingToolOutcomeKind::Blocked,
                ReasoningPolicyTrigger::ToolBlocked,
            ),
            (
                SamplingToolOutcomeKind::Timeout,
                ReasoningPolicyTrigger::ToolTimedOut,
            ),
            (
                SamplingToolOutcomeKind::RecoverableCancellation,
                ReasoningPolicyTrigger::ToolCancelled,
            ),
        ];

        for (outcome, expected_trigger) in cases {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            governor.phase = SamplingReasoningPhase::Finalize;
            let baselines = governor.baselines(1);
            governor.settle(&baselines, &collector_with_read_and(outcome), &settled(2));
            assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
            assert_eq!(governor.trigger(), expected_trigger);
        }
    }

    #[test]
    fn current_validation_failure_dominates_a_competing_read() {
        let config = config();
        let cases = [
            (
                SamplingToolOutcomeKind::Failure,
                ReasoningPolicyTrigger::ValidationFailed,
            ),
            (
                SamplingToolOutcomeKind::Timeout,
                ReasoningPolicyTrigger::ValidationTimedOut,
            ),
        ];

        for (outcome, expected_trigger) in cases {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            governor.phase = SamplingReasoningPhase::Finalize;
            let baselines = governor.baselines(0);
            governor.settle(&baselines, &validation_collector(outcome), &settled(0));
            assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
            assert_eq!(governor.trigger(), expected_trigger);
        }
    }

    #[test]
    fn fresh_validation_uses_final_revision_and_active_plan_state() {
        let config = config();
        let mut no_plan = SamplingReasoningGovernor::new(Some(&config));
        let baseline = no_plan.baselines(1);
        no_plan.settle(
            &baseline,
            &validation_collector(SamplingToolOutcomeKind::Success),
            &settled(1),
        );
        assert_eq!(no_plan.phase, SamplingReasoningPhase::Finalize);
        assert_eq!(no_plan.trigger(), ReasoningPolicyTrigger::ValidationPassed);

        let mut active_plan = SamplingReasoningGovernor::new(Some(&config));
        settle_plan(&mut active_plan, plan(&[StepStatus::InProgress]));
        let baseline = active_plan.baselines(1);
        active_plan.settle(
            &baseline,
            &validation_collector(SamplingToolOutcomeKind::Success),
            &settled(1),
        );
        assert_eq!(active_plan.phase, SamplingReasoningPhase::Verify);
        assert_eq!(
            active_plan.trigger(),
            ReasoningPolicyTrigger::ValidationPassed
        );
    }

    #[test]
    fn unchanged_read_does_not_invent_validation_or_plan_signals() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        settle_plan(&mut governor, plan(&[StepStatus::InProgress]));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(4);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(4),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);
        assert_eq!(
            governor.trigger(),
            ReasoningPolicyTrigger::ReadOnlyToolSuccess
        );
    }

    #[test]
    fn changed_plan_dominates_a_competing_read() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &collector_with_read_and_plan(plan(&[StepStatus::InProgress])),
            &settled(0),
        );

        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::PlanUpdated);
    }

    #[test]
    fn unchanged_plan_is_not_a_transition_and_diagnose_stays_sticky() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        let original = plan(&[StepStatus::InProgress]);
        settle_plan(&mut governor, original.clone());
        governor.host_diagnose();
        let mut repeated = original;
        repeated.explanation = Some("different explanation only".to_string());
        settle_plan(&mut governor, repeated);
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
    }

    #[test]
    fn diagnose_stickiness_and_explicit_exits_are_deterministic() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.host_diagnose();
        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(0),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);

        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(1),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);

        governor.host_diagnose();
        settle_plan(&mut governor, plan(&[StepStatus::InProgress]));
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
    }

    #[test]
    fn plan_tool_call_ordinal_wins_independent_of_completion_order() {
        let config = config();
        for reverse_completion in [false, true] {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            let baseline = governor.baselines(0);
            let collector = SamplingRequestSignalCollector::default();
            let first = collector.register_tool_call();
            let second = collector.register_tool_call();
            let first_outcome = SamplingToolOutcome::plain(
                first,
                SamplingToolOutcomeKind::Success,
                Some(plan(&[StepStatus::InProgress])),
            );
            let second_outcome = SamplingToolOutcome::plain(
                second,
                SamplingToolOutcomeKind::Success,
                Some(plan(&[StepStatus::Completed])),
            );
            if reverse_completion {
                collector.push(second_outcome);
                collector.push(first_outcome);
            } else {
                collector.push(first_outcome);
                collector.push(second_outcome);
            }
            governor.settle(&baseline, &collector, &settled(0));
            assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);
        }
    }

    #[test]
    fn recoverable_cancellation_diagnoses_and_no_signal_retains() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0);
        governor.settle(
            &baseline,
            &SamplingRequestSignalCollector::default(),
            &settled(0),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);

        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::RecoverableCancellation),
            &settled(0),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
    }

    #[test]
    fn only_blocking_skips_reopen_failure_evidence() {
        let config = config();
        for disposition in [
            None,
            Some(ToolOutputSkipDisposition::Deferred),
            Some(ToolOutputSkipDisposition::Suppressed),
            Some(ToolOutputSkipDisposition::NotApplicable),
        ] {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            governor.phase = SamplingReasoningPhase::Finalize;
            let baseline = governor.baselines(0);
            let collector = SamplingRequestSignalCollector::default();
            let ordinal = collector.register_tool_call();
            collector.push(SamplingToolOutcome::from_signal(
                ordinal,
                ToolOutputOutcomeContext::skipped(disposition),
                None,
                None,
            ));
            governor.settle(&baseline, &collector, &settled(0));
            assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);
        }

        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0);
        let collector = SamplingRequestSignalCollector::default();
        let ordinal = collector.register_tool_call();
        collector.push(SamplingToolOutcome::from_signal(
            ordinal,
            ToolOutputOutcomeContext::skipped(Some(
                ToolOutputSkipDisposition::BlockingRequiredOperation,
            )),
            None,
            None,
        ));
        governor.settle(&baseline, &collector, &settled(0));
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::ToolBlocked);
    }

    #[test]
    fn nested_code_mode_failure_overrides_successful_cell_and_retains_evidence() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        let baseline = governor.baselines(0);
        let collector = governor.collector(&baseline);
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
            .expect("nested outcome should reach the governor");
        assert_eq!(nested.kind, SamplingToolOutcomeKind::Skipped);
        assert_eq!(
            nested.skip_disposition,
            Some(ToolOutputSkipDisposition::BlockingRequiredOperation)
        );
        assert_eq!(nested.plan.as_ref(), Some(&nested_plan));
        assert!(nested.source_closure_established);
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
        governor.settle(&baseline, &collector, &settled_state);
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::ToolBlocked);

        assert_eq!(
            governor.evaluate_convergence(&baseline, &collector, &settled_state),
            SamplingConvergenceDecision::default()
        );
        let first_retry = governor.collector(&baseline);
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

        let changed_action = governor.collector(&baseline);
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

        let changed_state = governor.baselines(1);
        let changed = governor.collector(&changed_state);
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
            let mut governor = SamplingReasoningGovernor::new(None);
            let baseline = governor.baselines(0);
            let settled_state = settled(0);
            let first = governor.collector(&baseline);
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
                governor.evaluate_convergence(&baseline, &first, &settled_state),
                SamplingConvergenceDecision::default()
            );
            let retry = governor.collector(&baseline);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let baseline = governor.baselines(0);
        let settled_state = settled(0);
        let first = governor.collector(&baseline);
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
            governor.evaluate_convergence(&baseline, &first, &settled_state),
            SamplingConvergenceDecision::default()
        );
        let retry = governor.collector(&baseline);
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
                .expect("scoped cell dependencies")
                .len(),
            2
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
    fn nested_code_mode_observation_borrows_result_value() {
        let collector = SamplingRequestSignalCollector::default();
        let tool_name = ToolName::plain("read_tool_output");
        let payload = ToolPayload::Function {
            arguments: "{}".to_string(),
        };
        let nested_result = json!({
            "artifact_id": "artifact-1",
            "output": "retained nested evidence",
        });
        let output = nested_result["output"]
            .as_str()
            .expect("nested output string");
        let output_ptr = output.as_ptr();

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
        assert_eq!(
            nested_result["output"].as_str().map(str::as_ptr),
            Some(output_ptr)
        );
    }

    #[test]
    fn unfinished_mutation_obligation_moves_status_only_plan_update_to_implement() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0);
        let collector = governor.collector(&baseline);
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
        governor.settle(&baseline, &collector, &settled(0));

        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::PlanUpdated);
    }

    #[test]
    fn nested_code_mode_success_drives_plan_and_artifact_classification() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        let baseline = governor.baselines(0);
        let collector = governor.collector(&baseline);
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
        governor.settle(&baseline, &collector, &settled(0));
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::PlanUpdated);
    }

    #[test]
    fn user_and_host_continuations_follow_declared_precedence() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));

        for starting_phase in [
            SamplingReasoningPhase::Diagnose,
            SamplingReasoningPhase::Verify,
            SamplingReasoningPhase::Finalize,
        ] {
            governor.phase = starting_phase;
            governor.host_mutation();
            assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
            assert_eq!(
                governor.trigger(),
                ReasoningPolicyTrigger::WorkspaceMutation
            );
        }

        for starting_phase in [
            SamplingReasoningPhase::Inspect,
            SamplingReasoningPhase::Implement,
            SamplingReasoningPhase::Diagnose,
            SamplingReasoningPhase::Verify,
            SamplingReasoningPhase::Finalize,
        ] {
            governor.phase = starting_phase;
            governor.accepted_user_input();
            assert_eq!(governor.phase, SamplingReasoningPhase::Orient);
            assert_eq!(governor.trigger(), ReasoningPolicyTrigger::UserInput);
        }

        governor.host_diagnose();
        governor.host_retain();
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);

        let disabled_config = None;
        let mut disabled = SamplingReasoningGovernor::new(disabled_config);
        disabled.host_diagnose();
        disabled.host_mutation();
        disabled.accepted_user_input();
        assert_eq!(disabled.phase, SamplingReasoningPhase::Orient);
    }

    fn unchanged_state(
        governor: &SamplingReasoningGovernor,
    ) -> (SamplingRequestBaselines, SamplingRequestSettledState) {
        let baselines = governor.baselines(7);
        let settled = SamplingRequestSettledState {
            mutation_revision: 7,
            tool_exposure_revision: 0,
        };
        (baselines, settled)
    }

    fn authoritative_wait_collector(
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        identity: &str,
        mixed: bool,
        surfaceable_message: Option<&str>,
    ) -> SamplingRequestSignalCollector {
        let collector = governor.collector(baselines);
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
    fn first_exact_authoritative_wait_with_designated_surface_surfaces_existing_result() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = authoritative_wait_collector(
            &governor,
            &baselines,
            "same",
            false,
            Some("terminal owner result"),
        );
        assert_eq!(
            governor.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision {
                continuation: ContinuationDisposition::SurfaceExistingResult,
                proven_loop_activated: false,
                authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
                    AuthoritativeWaitOwnerResult {
                        adapter: "multi_agent_v2".to_string(),
                        value: json!({"message": "terminal owner result"}),
                        surfaceable_message: Some("terminal owner result".to_string()),
                    }
                )),
                ..Default::default()
            }
        );
    }

    #[test]
    fn kd4_latency_stable_continuation_terminal_wait_gets_one_tool_free_completion() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = authoritative_wait_collector(&governor, &baselines, "same", false, None);
        let decision = governor.evaluate_convergence(&baselines, &first, &settled);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );
        assert!(decision.directive.is_some());
        assert!(decision.proven_loop_activated);
        assert!(matches!(
            decision.authoritative_wait,
            Some(AuthoritativeWaitResolution::Terminal(_))
        ));

        let completion = governor
            .continuation_generation_request(&baselines, &first, &settled, false, false)
            .require_terminal_completion();
        assert!(completion.terminal_completion_only);
        assert_eq!(
            completion.sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
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
    fn exact_authoritative_wait_is_immediate_while_mixed_calls_fail_open() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
        let first = authoritative_wait_collector(&governor, &baselines, "first", false, None);
        assert_eq!(
            governor
                .evaluate_convergence(&baselines, &first, &settled)
                .continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );

        let changed_identity =
            authoritative_wait_collector(&governor, &baselines, "changed", false, None);
        assert_eq!(
            governor
                .evaluate_convergence(&baselines, &changed_identity, &settled)
                .continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );

        let mixed = authoritative_wait_collector(&governor, &baselines, "changed", true, None);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &mixed, &settled),
            SamplingConvergenceDecision::default()
        );

        let changed_baselines = governor.baselines(8);
        let changed_settled = SamplingRequestSettledState {
            mutation_revision: 8,
            ..settled
        };
        let changed_state =
            authoritative_wait_collector(&governor, &changed_baselines, "changed", false, None);
        assert_eq!(
            governor
                .evaluate_convergence(&changed_baselines, &changed_state, &changed_settled)
                .continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );
    }

    #[test]
    fn mixed_generation_purpose_uses_conservative_precedence() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
        let classify =
            |configure: fn(&mut SamplingRequestSignalState), has_pending_input, terminal| {
                let collector = governor.collector(&baselines);
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

    fn read_only_pass_collector(
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        arguments: &str,
        evidence: &str,
    ) -> (SamplingRequestSignalCollector, SamplingToolCallRegistration) {
        let collector = governor.collector(baselines);
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
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        arguments: &str,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = governor.collector(baselines);
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
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = governor.collector(baselines);
        for ordinal in 0..TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            let call_id = format!("exec-call-{ordinal}");
            let arguments = format!(r#"{{"command":"inspect-{ordinal}"}}"#);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Function { arguments },
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

    fn residual_tool_collector(
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        receipt_state: &str,
        evidence: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = governor.collector(baselines);
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
    fn direct_canonical_artifact_requirement_reaches_governor() {
        let governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0);
        let settled = settled(0);
        let collector = governor.collector(&baselines);
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
        assert!(!outcomes[0].is_generic_success());
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = residual_tool_collector(&governor, &baselines, "revision-1", "same");
        assert_eq!(
            governor.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );
        assert_eq!(
            governor
                .continuation_generation_request(&baselines, &first, &settled, false, false,)
                .sampling,
            SamplingGenerationDisposition::DecisionBearing
        );

        let repeated = residual_tool_collector(&governor, &baselines, "revision-1", "same");
        let repeated_decision = governor.evaluate_convergence(&baselines, &repeated, &settled);
        assert_eq!(
            repeated_decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(repeated_decision.directive.is_some());
        let repeated_request =
            governor.continuation_generation_request(&baselines, &repeated, &settled, false, false);
        assert!(matches!(
            &repeated_request.sampling,
            SamplingGenerationDisposition::ResidualDeterministic(
                ResidualDeterministicSamplingProof {
                    exact_action: ResidualDeterministicAction::RequireChangedContinuation,
                    ..
                }
            )
        ));
        let changed_evidence =
            residual_tool_collector(&governor, &baselines, "revision-2", "changed");
        assert!(
            governor
                .evaluate_convergence(&baselines, &changed_evidence, &settled)
                .directive
                .is_none()
        );
        assert_eq!(
            governor
                .continuation_generation_request(
                    &baselines,
                    &changed_evidence,
                    &settled,
                    false,
                    false,
                )
                .sampling,
            SamplingGenerationDisposition::DecisionBearing
        );
    }

    #[test]
    fn repeated_read_only_pass_activates_loop_guard_but_redispatches_exact_action() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
        let arguments =
            r#"{"artifact_id":"artifact-1","selectors":[{"kind":"lines","start":1,"end":1}]}"#;

        for generation in 1..=2 {
            let (collector, _) =
                read_only_pass_collector(&governor, &baselines, arguments, "same-evidence");
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation > 1);
            if generation > 1 {
                assert_eq!(
                    governor
                        .continuation_generation_request(
                            &baselines, &collector, &settled, false, false,
                        )
                        .timing_disposition(),
                    TurnTimingGenerationDisposition::Deterministic
                );
            }
        }
    }

    #[test]
    fn repeated_broad_source_directive_does_not_claim_the_pass_was_suppressed() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
        let arguments = r#"{"artifact_id":"artifact-1"}"#;

        let mut directive = None;
        for _ in 1..=2 {
            let (collector, _) =
                read_only_pass_collector(&governor, &baselines, arguments, "same-evidence");
            directive = governor
                .evaluate_convergence(&baselines, &collector, &settled)
                .directive;
        }

        let directive =
            directive.expect("a repeated broad source pass issues a convergence directive");
        assert!(
            directive.starts_with("Convergence required: the broad source pass repeated"),
            "unexpected directive: {directive}"
        );
        assert!(
            !directive.contains("suppress"),
            "the directive must not promise suppression the host does not perform: {directive}"
        );
    }

    #[test]
    fn semantic_evidence_converges_across_different_read_paths_and_presentations() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
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
            let collector = governor.collector(&baselines);
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
                    "semantic_evidence": crate::tools::context::semantic_evidence_for_command_output(
                        presentation.as_bytes()
                    ),
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
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation > 0);
        }
    }

    #[test]
    fn broad_source_cycle_preserves_evidence_multiplicity() {
        fn collector_with_reads(
            governor: &SamplingReasoningGovernor,
            baselines: &SamplingRequestBaselines,
            count: usize,
        ) -> SamplingRequestSignalCollector {
            let collector = governor.collector(baselines);
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

        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let one_read = collector_with_reads(&governor, &baselines, 1);
        let two_reads = collector_with_reads(&governor, &baselines, 2);

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
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        fingerprint: &str,
    ) -> SamplingRequestSignalCollector {
        direct_failure_collector_for_artifact(governor, baselines, "artifact-1", fingerprint)
    }

    fn direct_failure_collector_for_artifact(
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        artifact_id: &str,
        fingerprint: &str,
    ) -> SamplingRequestSignalCollector {
        let collector = governor.collector(baselines);
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
    fn stable_continuation_failure_gets_an_advisory_before_tool_free_completion() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=3 {
            let collector = direct_failure_collector(&governor, &baselines, "io.locked");
            assert!(
                collector
                    .deterministic_cycle_key()
                    .is_some_and(|key| key.starts_with("ToolFailure:io.locked:"))
            );
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation >= 2);
            assert_eq!(decision.proven_loop_activated, generation == 3);
            assert_eq!(
                decision.continuation,
                if generation == 3 {
                    ContinuationDisposition::TerminalCompletionRequired
                } else {
                    ContinuationDisposition::ModelRequired
                }
            );
            if generation == 3 {
                let completion = governor
                    .continuation_generation_request(&baselines, &collector, &settled, false, false)
                    .require_terminal_completion();
                assert!(completion.terminal_completion_only);
            }
        }
    }

    #[test]
    fn write_stdin_failures_are_retried_for_live_process_state() {
        let governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0);
        let collector = governor.collector(&baselines);
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
            .expect("governor collector has a dispatch ledger")
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
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let collector = governor.collector(&baselines);
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
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let collector = governor.collector(&baselines);
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
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let collector = governor.collector(&baselines);
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
            &successful_tool_response(
                "cargo-failure",
                r#"{"failure_signature":"validation-failure-v1:stable-diagnostic"}"#,
            ),
            false,
        );

        assert!(collector.deterministic_cycle_key().is_some_and(|key| {
            key.starts_with("ToolFailure:validation-failure-v1:stable-diagnostic:")
        }));
    }

    #[test]
    fn direct_tool_errors_receive_a_stable_failure_fingerprint() {
        let governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0);
        let collector = governor.collector(&baselines);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0);
        let settled_state = settled(0);
        let payload = ToolPayload::Function {
            arguments:
                r#"{"artifact_id":"expired","selectors":[{"kind":"lines","start":1,"end":1}]}"#
                    .to_string(),
        };
        let first = governor.collector(&baselines);
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
            governor.evaluate_convergence(&baselines, &first, &settled_state),
            SamplingConvergenceDecision::default()
        );
        assert!(
            governor
                .collector(&baselines)
                .register_deterministic_tool_call(
                    &ToolName::plain("read_tool_output"),
                    &payload,
                    "expired-artifact-retry",
                )
                .suppressed_failure
                .is_some(),
            "a terminal model-visible failure should suppress the exact retry"
        );

        let changed_payload = ToolPayload::Function {
            arguments:
                r#"{"artifact_id":"replacement","selectors":[{"kind":"lines","start":1,"end":1}]}"#
                    .to_string(),
        };
        assert!(
            governor
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
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first = direct_failure_collector(&governor, &baselines, "io.locked");
        let changed = direct_failure_collector(&governor, &baselines, "schema.invalid");

        assert_ne!(
            first.deterministic_cycle_key(),
            changed.deterministic_cycle_key()
        );
    }

    #[test]
    fn failure_signature_convergence_does_not_equate_changed_actions() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first =
            direct_failure_collector_for_artifact(&governor, &baselines, "artifact-1", "io.locked");
        let changed =
            direct_failure_collector_for_artifact(&governor, &baselines, "artifact-2", "io.locked");

        assert_ne!(
            first.deterministic_cycle_key(),
            changed.deterministic_cycle_key()
        );
    }

    #[test]
    fn distinct_failure_strategies_allow_narrow_successful_recovery() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for (generation, (artifact_id, fingerprint)) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ]
        .into_iter()
        .enumerate()
        {
            let collector = direct_failure_collector_for_artifact(
                &governor,
                &baselines,
                artifact_id,
                fingerprint,
            );
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert_eq!(decision.directive.is_some(), generation == 1);
            assert!(!decision.proven_loop_activated);
        }

        let narrower_success = structured_tool_pass_collector(
            &governor,
            &baselines,
            r#"{"command":"inspect-narrower-input"}"#,
            "narrower-evidence",
        );
        assert_eq!(
            governor.evaluate_convergence(&baselines, &narrower_success, &settled),
            SamplingConvergenceDecision::default()
        );
    }

    #[test]
    fn meaningful_child_runtime_does_not_spend_distinct_failure_recovery_budget() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for (artifact_id, fingerprint) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ] {
            let collector = direct_failure_collector_for_artifact(
                &governor,
                &baselines,
                artifact_id,
                fingerprint,
            );
            collector
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL + 1);
            assert_eq!(
                governor.evaluate_convergence(&baselines, &collector, &settled),
                SamplingConvergenceDecision::default()
            );
        }
    }

    #[test]
    fn successful_result_resets_distinct_failure_recovery_budget() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first =
            direct_failure_collector_for_artifact(&governor, &baselines, "artifact-1", "io.locked");
        assert_eq!(
            governor.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );

        let success = structured_tool_pass_collector(
            &governor,
            &baselines,
            r#"{"command":"inspect-new-evidence"}"#,
            "new-evidence",
        );
        assert_eq!(
            governor.evaluate_convergence(&baselines, &success, &settled),
            SamplingConvergenceDecision::default()
        );

        let after_success = direct_failure_collector_for_artifact(
            &governor,
            &baselines,
            "artifact-2",
            "schema.invalid",
        );
        assert_eq!(
            governor.evaluate_convergence(&baselines, &after_success, &settled),
            SamplingConvergenceDecision::default()
        );

        let recovery = direct_failure_collector_for_artifact(
            &governor,
            &baselines,
            "artifact-3",
            "permission.denied",
        );
        assert_eq!(
            governor.evaluate_convergence(&baselines, &recovery, &settled),
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

        let mut mixed_governor = SamplingReasoningGovernor::new(None);
        let (mixed_baselines, mixed_settled) = unchanged_state(&mixed_governor);
        let first = direct_failure_collector_for_artifact(
            &mixed_governor,
            &mixed_baselines,
            "mixed-artifact-1",
            "io.locked",
        );
        let _ = mixed_governor.evaluate_convergence(&mixed_baselines, &first, &mixed_settled);

        let mixed = mixed_governor.collector(&mixed_baselines);
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
            mixed_governor.evaluate_convergence(&mixed_baselines, &mixed, &mixed_settled,),
            SamplingConvergenceDecision::default()
        );

        let after_mixed = direct_failure_collector_for_artifact(
            &mixed_governor,
            &mixed_baselines,
            "mixed-artifact-3",
            "permission.denied",
        );
        assert_eq!(
            mixed_governor.evaluate_convergence(&mixed_baselines, &after_mixed, &mixed_settled,),
            SamplingConvergenceDecision::default()
        );
    }

    #[test]
    fn multi_failure_cycle_retains_action_to_failure_pairing() {
        fn collector_for(
            governor: &SamplingReasoningGovernor,
            baselines: &SamplingRequestBaselines,
            failures: &[(&str, &str)],
        ) -> SamplingRequestSignalCollector {
            let collector = governor.collector(baselines);
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

        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first = collector_for(
            &governor,
            &baselines,
            &[("artifact-a", "failure-a"), ("artifact-b", "failure-b")],
        );
        let swapped = collector_for(
            &governor,
            &baselines,
            &[("artifact-a", "failure-b"), ("artifact-b", "failure-a")],
        );

        assert_ne!(
            first.deterministic_cycle_key(),
            swapped.deterministic_cycle_key()
        );

        let reversed = collector_for(
            &governor,
            &baselines,
            &[("artifact-b", "failure-b"), ("artifact-a", "failure-a")],
        );
        assert_eq!(
            first.deterministic_cycle_key(),
            reversed.deterministic_cycle_key()
        );

        let duplicated = collector_for(
            &governor,
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=3 {
            let arguments = format!(
                r#"{{"artifact_id":"artifact-{generation}","selectors":[{{"kind":"lines","start":1,"end":1}}]}}"#
            );
            let evidence = format!("evidence-{generation}");
            let (collector, _) =
                read_only_pass_collector(&governor, &baselines, &arguments, &evidence);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
        }
    }

    #[test]
    fn repeated_structured_tool_pass_converges_without_resetting_history() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=3 {
            let collector = structured_tool_pass_collector(
                &governor,
                &baselines,
                r#"{"command":"inspect"}"#,
                "same-result",
            );
            assert!(collector.deterministic_cycle_key().is_some());
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation >= 2);
        }
    }

    #[test]
    fn turn_efficiency_repeated_high_tool_count_cycle_requires_terminal_completion() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = high_volume_tool_pass_collector(&governor, &baselines, "same-result");
        first.record_child_runtime(
            TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL
                * u64::try_from(TURN_EFFICIENCY_TOOL_CALL_THRESHOLD).unwrap(),
        );
        let initial = governor.evaluate_convergence(&baselines, &first, &settled);
        assert_eq!(initial.continuation, ContinuationDisposition::ModelRequired);
        assert!(initial.directive.is_none());
        assert!(!initial.proven_loop_activated);

        let repeated = high_volume_tool_pass_collector(&governor, &baselines, "same-result");
        repeated.record_child_runtime(
            TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL
                * u64::try_from(TURN_EFFICIENCY_TOOL_CALL_THRESHOLD).unwrap(),
        );
        let advisory = governor.evaluate_convergence(&baselines, &repeated, &settled);
        assert_eq!(
            advisory.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(advisory.directive.is_some());
        assert!(!advisory.proven_loop_activated);

        let repeated_after_advisory =
            high_volume_tool_pass_collector(&governor, &baselines, "same-result");
        repeated_after_advisory.record_child_runtime(
            TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL
                * u64::try_from(TURN_EFFICIENCY_TOOL_CALL_THRESHOLD).unwrap(),
        );
        let terminal =
            governor.evaluate_convergence(&baselines, &repeated_after_advisory, &settled);
        assert_eq!(
            terminal.continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );
        assert!(terminal.directive.is_some());
        assert!(terminal.proven_loop_activated);
    }

    #[test]
    fn changing_sequential_tiny_calls_do_not_request_consolidation() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=9 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&governor, &baselines, &arguments, &evidence);
            collector.record_child_runtime(100);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=TURN_EFFICIENCY_TOOL_CALL_THRESHOLD * 2 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&governor, &baselines, &arguments, &evidence);
            collector.record_child_runtime(100);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=TURN_EFFICIENCY_TOOL_CALL_THRESHOLD + 1 {
            let arguments = format!(r#"{{"command":"inspect-{generation}"}}"#);
            let evidence = format!("evidence-{generation}");
            let collector =
                structured_tool_pass_collector(&governor, &baselines, &arguments, &evidence);
            collector
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL + 1);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
        }
    }

    #[test]
    fn proven_loop_requests_one_shot_terminal_completion() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=3 {
            let collector = structured_tool_pass_collector(
                &governor,
                &baselines,
                r#"{"command":"inspect"}"#,
                "same-result",
            );
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            if generation < 3 {
                assert_eq!(
                    decision.continuation,
                    ContinuationDisposition::ModelRequired
                );
                continue;
            }

            assert!(decision.proven_loop_activated);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::TerminalCompletionRequired
            );
            let request = governor
                .continuation_generation_request(&baselines, &collector, &settled, false, false)
                .require_terminal_completion();
            assert!(request.terminal_completion_only);
            assert_eq!(
                request.purpose,
                Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning)
            );
            assert_eq!(
                request.sampling,
                SamplingGenerationDisposition::DecisionBearing
            );
        }
    }

    #[test]
    fn uncertain_dispatch_identity_fails_open() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        for _ in 0..2 {
            let collector = governor.collector(&baselines);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for _ in 1..=4 {
            let collector = governor.collector(&baselines);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
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
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let collector = governor.collector(&baselines);
        blocked_wait(&collector, "receipt-1");
        let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
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

        let repeated = governor.collector(&baselines);
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
    fn repeated_suppressed_blocked_wait_reaches_terminal_completion() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = governor.collector(&baselines);
        blocked_wait(&first, "receipt-1");
        let _ = governor.evaluate_convergence(&baselines, &first, &settled);

        let tool_name = ToolName::plain("wait_agent");
        let payload = ToolPayload::Function {
            arguments: r#"{"cursor":"cursor-1"}"#.to_string(),
        };
        let collector = governor.collector(&baselines);
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

        let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
        assert!(decision.proven_loop_activated);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::TerminalCompletionRequired
        );
    }

    #[test]
    fn blocked_wait_receipt_change_does_not_reset_owner_revision_convergence() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = governor.collector(&baselines);
        blocked_wait(&first, "receipt-1");
        let _ = governor.evaluate_convergence(&baselines, &first, &settled);
        let changed = governor.collector(&baselines);
        blocked_wait(&changed, "receipt-2");
        let decision = governor.evaluate_convergence(&baselines, &changed, &settled);
        assert_eq!(
            decision.continuation,
            ContinuationDisposition::ModelRequired
        );
        assert!(decision.directive.is_some());
        let guard = governor
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked_wait_gate
            .clone()
            .expect("blocked wait gate");
        assert_eq!(guard.guard.owner, "owner-1");
        assert_eq!(guard.guard.state_revision, "revision-1");
    }
}
