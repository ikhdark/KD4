use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_config::config_toml::ReasoningPhaseEfforts;
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

use crate::tools::handlers::command_shape::CommandInvocation;
use crate::turn_diff_tracker::ValidationFreshnessStatus;
use crate::turn_timing::TurnTimingState;
use crate::validation_admission::ValidationClassification;
use crate::validation_admission::classify_validation;

pub(crate) type SamplingReasoningPhase = ReasoningPolicyPhase;
pub(crate) type SamplingRequestPolicySource = ReasoningPolicySource;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ContinuationDisposition {
    #[default]
    ModelRequired,
    SurfaceExistingResult,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenerationRequestDisposition {
    pub(crate) purpose: Option<TurnTimingGenerationPurpose>,
    pub(crate) sampling: SamplingGenerationDisposition,
    pub(crate) relevant_state_fingerprint: String,
}

impl GenerationRequestDisposition {
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
                debug_assert_eq!(
                    proof.exact_action,
                    ResidualDeterministicAction::CompleteProtocolTurn
                );
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
    validation_status: ValidationFreshnessStatus,
    validation_revision: Option<u64>,
    plan_revision: u64,
    input_revision: u64,
}

impl SamplingRequestBaselines {
    fn revision_key(&self) -> String {
        format!(
            "mutation={};validation_status={:?};validation_revision={:?};plan={};input={}",
            self.mutation_revision,
            self.validation_status,
            self.validation_revision,
            self.plan_revision,
            self.input_revision,
        )
    }

    pub(crate) fn relevant_state_fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(self.revision_key().as_bytes()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SamplingRequestSettledState {
    pub(crate) mutation_revision: u64,
    pub(crate) validation_status: ValidationFreshnessStatus,
    pub(crate) validation_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SamplingToolOutcomeKind {
    Success,
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
        }
    }

    fn plain(ordinal: u64, kind: SamplingToolOutcomeKind, plan: Option<UpdatePlanArgs>) -> Self {
        let outcome = match kind {
            SamplingToolOutcomeKind::Success => ToolOutputOutcome::Success,
            SamplingToolOutcomeKind::Timeout => ToolOutputOutcome::TimedOut,
            SamplingToolOutcomeKind::Skipped => ToolOutputOutcome::Skipped,
            SamplingToolOutcomeKind::Failure
            | SamplingToolOutcomeKind::Blocked
            | SamplingToolOutcomeKind::RecoverableCancellation => ToolOutputOutcome::Failure,
        };
        Self::from_signal(ordinal, ToolOutputOutcomeContext::new(outcome), plan, None)
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
        SamplingToolOutcomeKind::Success => false,
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
struct WaitConvergenceHandle {
    fingerprint: String,
    observations: u32,
    enforcement_reported: bool,
    retained_result: AuthoritativeWaitOwnerResult,
}

struct DeterministicDispatchLedger {
    blocked_wait_gate: Option<BlockedWaitGate>,
    timing: Arc<TurnTimingState>,
}

impl DeterministicDispatchLedger {
    fn new(timing: Arc<TurnTimingState>) -> Self {
        Self {
            blocked_wait_gate: None,
            timing,
        }
    }
}

#[derive(Default)]
struct SamplingRequestSignalState {
    outcomes: Vec<SamplingToolOutcome>,
    registered_count: usize,
    wait_call_count: usize,
    saw_artifact_read: bool,
    saw_validation: bool,
    saw_mutation: bool,
    saw_coordination: bool,
    direct_wait_agent_count: usize,
    direct_code_mode_exec_count: usize,
    code_mode_nested_tool_count: usize,
    authoritative_wait_observations: Vec<AuthoritativeWaitObservation>,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingOwnerDrainedContinuation {
    pub(crate) preserved_content: Vec<Value>,
    pub(crate) receipt: TurnTimingDeterministicContinuationReceipt,
}

#[derive(Clone, Default)]
pub(crate) struct SamplingRequestSignalCollector {
    next_ordinal: Arc<AtomicU64>,
    state: Arc<Mutex<SamplingRequestSignalState>>,
    dispatch_ledger: Option<Arc<Mutex<DeterministicDispatchLedger>>>,
}

pub(crate) struct SamplingToolCallRegistration {
    pub(crate) ordinal: u64,
    pub(crate) blocked_wait_guard: Option<BlockedWaitGuard>,
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
        let direct_code_mode_exec = crate::tools::code_mode::is_exec_tool_name(tool_name);
        let action_identity = deterministic_action_identity(tool_name, payload);
        let validation = is_validation_invocation(tool_name, payload);
        let blocked_wait_guard = action_identity.as_ref().and_then(|action_identity| {
            let ledger = self.dispatch_ledger.as_ref()?;
            ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked_wait_gate
                .as_ref()
                .filter(|gate| gate.action_identity == *action_identity)
                .map(|gate| gate.guard.clone())
        });

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count = state.registered_count.saturating_add(1);
        if wait {
            state.wait_call_count = state.wait_call_count.saturating_add(1);
        }
        if direct_code_mode_exec {
            state.direct_code_mode_exec_count = state.direct_code_mode_exec_count.saturating_add(1);
        }
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= validation;
        state.saw_mutation |= is_mutation_tool(tool_name);
        state.saw_coordination |= is_coordination_tool(tool_name);

        SamplingToolCallRegistration {
            ordinal,
            blocked_wait_guard,
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

    pub(crate) fn record_suppressed_result(&self, ordinal: u64, _response: &ResponseInputItem) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));
    }

    pub(crate) fn record_accepted_deterministic_continuation_receipts(
        &self,
        receipts: &[TurnTimingDeterministicContinuationReceipt],
    ) {
        if receipts.is_empty() {
            return;
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

    pub(crate) fn record_code_mode_result(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        signal: Option<&Value>,
        result: Value,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        if tool_name.namespace.is_some() || tool_name.name != "wait" {
            return;
        }
        if let Some(observation) = authoritative_wait_observation(
            "code_mode_cell",
            tool_name,
            payload,
            signal,
            Some(&result),
        ) {
            state.authoritative_wait_observations.push(observation);
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

    pub(crate) fn record_failure_with_mutation(&self, ordinal: u64, _mutation_advanced: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Failure,
            None,
        ));
    }

    pub(crate) fn record_response_result_with_mutation(
        &self,
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        signal: Option<Value>,
        _response: &ResponseInputItem,
        _mutation_advanced: bool,
    ) {
        let plan = sampling_plan(signal.as_ref());
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outcomes
            .push(SamplingToolOutcome::from_signal(
                ordinal,
                outcome_context,
                plan,
                signal.as_ref(),
            ));
    }

    fn deterministic_cycle_key(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.registered_count == 0 {
            return Some("empty".to_string());
        }
        None
    }

    pub(crate) fn is_wait_only(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count > 0 && state.wait_call_count == state.registered_count
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
        let validation_failed = matches!(
            settled.validation_status,
            ValidationFreshnessStatus::FailedAfterLastMutation
                | ValidationFreshnessStatus::TimedOut
        );

        // Mixed generations use the protocol's conservative precedence. The
        // initial/compaction cases are selected by the caller before this
        // post-tool classifier runs.
        if has_pending_input {
            Some(TurnTimingGenerationPurpose::InitialReasoning)
        } else if state.saw_mutation || settled.mutation_revision > baselines.mutation_revision {
            Some(if observed_failure || validation_failed {
                TurnTimingGenerationPurpose::Repair
            } else {
                TurnTimingGenerationPurpose::ImplementationDecision
            })
        } else if state.saw_validation
            || settled.validation_status != baselines.validation_status
            || settled.validation_revision != baselines.validation_revision
        {
            Some(if observed_failure || validation_failed {
                TurnTimingGenerationPurpose::FailureDiagnosis
            } else {
                TurnTimingGenerationPurpose::ValidationInterpretation
            })
        } else if state.saw_coordination {
            Some(TurnTimingGenerationPurpose::Coordination)
        } else if state.registered_count > 0 && state.wait_call_count == state.registered_count {
            Some(TurnTimingGenerationPurpose::Wait)
        } else if state.saw_artifact_read {
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        } else if observed_failure || validation_failed {
            Some(TurnTimingGenerationPurpose::FailureDiagnosis)
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
        if settled.validation_status != baselines.validation_status
            || settled.validation_revision != baselines.validation_revision
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
            _ => match outcome {
                "success" => SamplingToolOutcomeKind::Success,
                _ => SamplingToolOutcomeKind::Failure,
            },
        });
    match outcome {
        ToolOutputOutcome::Success => signalled.unwrap_or(SamplingToolOutcomeKind::Success),
        ToolOutputOutcome::Failure => signalled.unwrap_or(SamplingToolOutcomeKind::Failure),
        ToolOutputOutcome::TimedOut => SamplingToolOutcomeKind::Timeout,
        ToolOutputOutcome::Skipped => SamplingToolOutcomeKind::Skipped,
    }
}

fn sampling_plan(signal: Option<&Value>) -> Option<UpdatePlanArgs> {
    signal
        .filter(|value| value.get("kind").and_then(Value::as_str) == Some("plan_update"))
        .and_then(|value| value.get("plan"))
        .and_then(|value| serde_json::from_value::<UpdatePlanArgs>(value.clone()).ok())
}

fn deterministic_action_identity(tool_name: &ToolName, payload: &ToolPayload) -> Option<String> {
    if !tool_name_matches(tool_name, "wait") && !tool_name_matches(tool_name, "wait_agent") {
        return None;
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let arguments = serde_json::from_str::<Value>(arguments).ok()?;
    if arguments.get("force_fresh").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let arguments = serde_json::to_string(&canonicalize_json(arguments)).ok()?;
    let action_class = serde_json::to_string(tool_name).ok()?;
    Some(format!("{action_class}\n{arguments}"))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json(value)))
                    .collect(),
            )
        }
        value => value,
    }
}

fn canonical_response_body(response: &ResponseInputItem) -> Option<Value> {
    let mut value = serde_json::to_value(response).ok()?;
    if let Value::Object(object) = &mut value {
        object.remove("call_id");
    }
    Some(canonicalize_json(value))
}

fn canonical_authoritative_result(response: &ResponseInputItem) -> Option<Value> {
    response_output_text(response)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .map(canonicalize_json)
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
    let action = canonical_tool_payload(payload);
    let result = canonicalize_json(result?.clone());
    let action_identity = deterministic_action_identity(tool_name, payload)?;
    let identity = serde_json::to_vec(&serde_json::json!({
        "adapter": expected_adapter,
        "disposition": disposition,
        "owner": owner,
        "state_revision": state_revision,
        "action": action,
        "receipt_identity": (disposition == AuthoritativeWaitDisposition::Terminal)
            .then_some(receipt_identity),
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

fn canonical_tool_payload(payload: &ToolPayload) -> Value {
    let (kind, value) = match payload {
        ToolPayload::Function { arguments } => (
            "function",
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.clone())),
        ),
        ToolPayload::ToolSearch { arguments } => {
            ("tool_search", Value::String(arguments.query.clone()))
        }
        ToolPayload::Custom { input } => ("custom", Value::String(input.clone())),
    };
    serde_json::json!({
        "kind": kind,
        "value": canonicalize_json(value),
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
    last_cycle: Option<String>,
    last_state_revision: Option<String>,
    directive_issued: bool,
    proven_loop_active: bool,
    wait_convergence: Option<WaitConvergenceHandle>,
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
            last_cycle: None,
            last_state_revision: None,
            directive_issued: false,
            proven_loop_active: false,
            wait_convergence: None,
        }
    }

    pub(crate) fn baselines(
        &self,
        mutation_revision: u64,
        validation_status: ValidationFreshnessStatus,
        validation_revision: Option<u64>,
    ) -> SamplingRequestBaselines {
        SamplingRequestBaselines {
            mutation_revision,
            validation_status,
            validation_revision,
            plan_revision: self.plan_revision,
            input_revision: self.input_revision,
        }
    }

    pub(crate) fn collector(
        &self,
        _baselines: &SamplingRequestBaselines,
    ) -> SamplingRequestSignalCollector {
        SamplingRequestSignalCollector {
            next_ordinal: Arc::new(AtomicU64::new(0)),
            state: Arc::new(Mutex::new(SamplingRequestSignalState::default())),
            dispatch_ledger: Some(Arc::clone(&self.dispatch_ledger)),
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
        if !protocol_requests_resample
            || has_pending_input
            || settled.mutation_revision != baselines.mutation_revision
            || settled.validation_status != baselines.validation_status
            || settled.validation_revision != baselines.validation_revision
            || self.plan_revision != baselines.plan_revision
            || self.input_revision != baselines.input_revision
        {
            return None;
        }

        let state = collector
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Any tool result is new evidence that the model must interpret. The
        // residual low-effort path is only the protocol-required resample
        // after a response that made no tool call and changed no relevant
        // state; its exact action is therefore already known.
        if state.registered_count != 0 || !state.outcomes.is_empty() {
            return None;
        }

        Some(ResidualDeterministicSamplingProof {
            relevant_state_fingerprint: relevant_state_fingerprint.to_string(),
            exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
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
            || settled.validation_status != baselines.validation_status
            || settled.validation_revision != baselines.validation_revision
            || self.plan_revision != baselines.plan_revision
            || self.input_revision != baselines.input_revision
        {
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        }

        if let Some(observation) = collector.authoritative_wait_observation() {
            let fingerprint = format!(
                "{:x}",
                Sha256::digest(
                    format!("{}\0{}", settled_revision, observation.identity).as_bytes()
                )
            );
            self.consecutive_no_progress = 0;
            self.last_cycle = None;
            self.last_state_revision = Some(settled_revision);
            self.directive_issued = false;
            self.proven_loop_active = false;
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
            if self
                .wait_convergence
                .as_ref()
                .is_some_and(|handle| handle.fingerprint == fingerprint)
            {
                if let Some(handle) = self.wait_convergence.as_mut() {
                    handle.observations = handle.observations.saturating_add(1);
                }
            } else {
                self.wait_convergence = Some(WaitConvergenceHandle {
                    fingerprint,
                    observations: 1,
                    enforcement_reported: false,
                    retained_result: observation.result.clone(),
                });
            }
            let Some(handle) = self.wait_convergence.as_mut() else {
                return SamplingConvergenceDecision::default();
            };
            if handle.observations >= 2 {
                let activated = !handle.enforcement_reported;
                handle.enforcement_reported = true;
                return match observation.disposition {
                    AuthoritativeWaitDisposition::Terminal => SamplingConvergenceDecision {
                        continuation: ContinuationDisposition::SurfaceExistingResult,
                        proven_loop_activated: activated,
                        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
                            handle.retained_result.clone(),
                        )),
                        ..Default::default()
                    },
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
                            proven_loop_activated: activated,
                            authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
                                observation.result,
                            )),
                        }
                    }
                };
            }
            return SamplingConvergenceDecision::default();
        }
        self.wait_convergence = None;

        let Some(cycle) = collector.deterministic_cycle_key() else {
            // Missing or ambiguous structured identity is possible progress.
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        };

        let repeated_cycle = self.last_cycle.as_deref() == Some(cycle.as_str())
            && self.last_state_revision.as_deref() == Some(settled_revision.as_str());
        if repeated_cycle || cycle == "empty" {
            self.consecutive_no_progress = self.consecutive_no_progress.saturating_add(1);
        } else {
            // A new deterministic outcome is new evidence, not no progress.
            self.consecutive_no_progress = 0;
            self.directive_issued = false;
            self.proven_loop_active = false;
        }
        let cycle_is_nonempty = cycle != "empty";
        self.last_cycle = Some(cycle);
        self.last_state_revision = Some(settled_revision);

        if self.consecutive_no_progress < 3 {
            return SamplingConvergenceDecision::default();
        }

        let proven_loop_activated = self.directive_issued
            && repeated_cycle
            && cycle_is_nonempty
            && !self.proven_loop_active;
        if proven_loop_activated {
            self.proven_loop_active = true;
        }
        self.directive_issued = true;
        let directive = if self.consecutive_no_progress == 3 {
            "Convergence required: the last three generations produced no structured state progress. Use a new hypothesis, a state-changing action, a narrower observation, or truthfully complete. Do not repeat an equivalent action against unchanged state."
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
        self.last_cycle = None;
        self.last_state_revision = None;
        self.directive_issued = false;
        self.proven_loop_active = false;
        self.wait_convergence = None;
    }

    fn settled_revision_key(&self, settled: &SamplingRequestSettledState) -> String {
        format!(
            "mutation={};validation_status={:?};validation_revision={:?};plan={};input={}",
            settled.mutation_revision,
            settled.validation_status,
            settled.validation_revision,
            self.plan_revision,
            self.input_revision,
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
                .is_none_or(|current| !plans_semantically_equal(current, plan))
        });
        if let Some(plan) = changed_plan.as_ref() {
            self.plan = Some(plan.clone());
            self.plan_revision = self.plan_revision.saturating_add(1);
        }
        if let Some(failure) = outcomes
            .iter()
            .filter(|outcome| outcome.is_failure_evidence())
            .min_by_key(|outcome| outcome.ordinal)
        {
            let trigger = match failure.kind {
                SamplingToolOutcomeKind::Failure => ReasoningPolicyTrigger::ToolFailed,
                SamplingToolOutcomeKind::Blocked => ReasoningPolicyTrigger::ToolBlocked,
                SamplingToolOutcomeKind::Timeout => ReasoningPolicyTrigger::ToolTimedOut,
                SamplingToolOutcomeKind::RecoverableCancellation => {
                    ReasoningPolicyTrigger::ToolCancelled
                }
                SamplingToolOutcomeKind::Skipped => ReasoningPolicyTrigger::ToolBlocked,
                SamplingToolOutcomeKind::Success => unreachable!("success is not a failure"),
            };
            self.transition_to(SamplingReasoningPhase::Diagnose, trigger);
            return;
        }
        let validation_changed = settled.validation_status != baselines.validation_status;
        if validation_changed
            && matches!(
                settled.validation_status,
                ValidationFreshnessStatus::FailedAfterLastMutation
                    | ValidationFreshnessStatus::TimedOut
            )
        {
            self.transition_to(
                SamplingReasoningPhase::Diagnose,
                if settled.validation_status == ValidationFreshnessStatus::TimedOut {
                    ReasoningPolicyTrigger::ValidationTimedOut
                } else {
                    ReasoningPolicyTrigger::ValidationFailed
                },
            );
            return;
        }
        let fresh_validation = settled.validation_revision != baselines.validation_revision
            && settled.validation_revision == Some(settled.mutation_revision)
            && settled.validation_status == ValidationFreshnessStatus::PassedAfterLastMutation;
        if fresh_validation {
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
            outcome.kind == SamplingToolOutcomeKind::Success && outcome.plan.is_none()
        }) {
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
        let configured_effort = configured_override.clone().unwrap_or(ReasoningEffort::Low);
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
        .or(Some(selected))
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
    model_info
        .supported_reasoning_levels
        .get(
            model_info
                .supported_reasoning_levels
                .len()
                .saturating_sub(1)
                / 2,
        )
        .map(|preset| preset.effort.clone())
        .or_else(|| model_info.default_reasoning_level.clone())
        .or(Some(selected))
}

fn plan_is_unfinished(plan: &UpdatePlanArgs) -> bool {
    !plan.plan.is_empty()
        && plan.plan.iter().any(|item| {
            !matches!(
                item.status,
                StepStatus::Passed | StepStatus::Skipped | StepStatus::Completed
            )
        })
}

fn plans_semantically_equal(left: &UpdatePlanArgs, right: &UpdatePlanArgs) -> bool {
    left.plan == right.plan
}

fn phase_for_plan(plan: &UpdatePlanArgs) -> SamplingReasoningPhase {
    if plan
        .plan
        .iter()
        .any(|item| item.status == StepStatus::Blocked)
    {
        SamplingReasoningPhase::Diagnose
    } else if plan.plan.iter().all(|item| {
        matches!(
            item.status,
            StepStatus::Passed | StepStatus::Skipped | StepStatus::Completed
        )
    }) {
        SamplingReasoningPhase::Finalize
    } else if plan
        .plan
        .iter()
        .any(|item| item.status == StepStatus::Implemented)
    {
        SamplingReasoningPhase::Verify
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
                    id: Some(format!("step-{index}")),
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

    fn collector_with_read_and_plan(plan: UpdatePlanArgs) -> SamplingRequestSignalCollector {
        let collector = collector_with(SamplingToolOutcomeKind::Success);
        collector.push(SamplingToolOutcome::plain(
            1,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        collector
    }

    fn settled(
        mutation_revision: u64,
        validation_status: ValidationFreshnessStatus,
        validation_revision: Option<u64>,
    ) -> SamplingRequestSettledState {
        SamplingRequestSettledState {
            mutation_revision,
            validation_status,
            validation_revision,
        }
    }

    fn settle_plan(governor: &mut SamplingReasoningGovernor, plan: UpdatePlanArgs) {
        let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(
            0,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        governor.settle(
            &baselines,
            &collector,
            &settled(0, ValidationFreshnessStatus::None, None),
        );
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
    fn deterministic_protocol_fallback_alone_uses_the_lowest_supported_override() {
        let model = model(
            &[ReasoningEffort::Medium, ReasoningEffort::High],
            ReasoningEffort::High,
        );
        let config = config();
        let residual = SamplingGenerationDisposition::ResidualDeterministic(
            ResidualDeterministicSamplingProof {
                relevant_state_fingerprint: "state".to_string(),
                exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
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
                exact_action: ResidualDeterministicAction::CompleteProtocolTurn,
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
    fn optimization_priority_elides_proven_residual_generation_only_for_unchanged_state() {
        let governor = SamplingReasoningGovernor::new(Some(&ReasoningPhaseEfforts::default()));
        let baselines = governor.baselines(7, ValidationFreshnessStatus::None, None);
        let unchanged = settled(7, ValidationFreshnessStatus::None, None);

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

        let changed = settled(8, ValidationFreshnessStatus::None, None);
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
    fn earliest_failure_wins_and_validation_failure_is_a_fallback() {
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
        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector,
            &settled(
                0,
                ValidationFreshnessStatus::FailedAfterLastMutation,
                Some(0),
            ),
        );
        assert_eq!(governor.phase(), Some(SamplingReasoningPhase::Diagnose));
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::ToolBlocked);

        let mut validation_only = SamplingReasoningGovernor::new(Some(&config));
        let baseline = validation_only.baselines(0, ValidationFreshnessStatus::None, None);
        validation_only.settle(
            &baseline,
            &SamplingRequestSignalCollector::default(),
            &settled(
                0,
                ValidationFreshnessStatus::FailedAfterLastMutation,
                Some(0),
            ),
        );
        assert_eq!(
            validation_only.trigger(),
            ReasoningPolicyTrigger::ValidationFailed
        );
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

        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Inspect);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::Low)
        );

        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &SamplingRequestSignalCollector::default(),
            &settled(1, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::High)
        );

        let baseline = governor.baselines(1, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Failure),
            &settled(1, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
        assert_eq!(
            governor
                .resolve_policy(Some(&config), None, &model)
                .effective_effort,
            Some(ReasoningEffort::High)
        );

        let baseline = governor.baselines(1, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                1,
                ValidationFreshnessStatus::PassedAfterLastMutation,
                Some(1),
            ),
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
            let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
            governor.settle(
                &baseline,
                &collector_with(SamplingToolOutcomeKind::Success),
                &settled(0, ValidationFreshnessStatus::None, None),
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
    fn plan_precedence_and_terminal_statuses_are_normalized() {
        assert_eq!(
            phase_for_plan(&plan(&[StepStatus::Blocked, StepStatus::Implemented])),
            SamplingReasoningPhase::Diagnose
        );
        assert_eq!(
            phase_for_plan(&plan(&[StepStatus::Implemented, StepStatus::InProgress])),
            SamplingReasoningPhase::Verify
        );
        assert_eq!(
            phase_for_plan(&plan(&[
                StepStatus::Passed,
                StepStatus::Skipped,
                StepStatus::Completed,
            ])),
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
            let baselines = governor.baselines(1, ValidationFreshnessStatus::None, None);
            governor.settle(
                &baselines,
                &collector_with_read_and(outcome),
                &settled(
                    2,
                    ValidationFreshnessStatus::PassedAfterLastMutation,
                    Some(2),
                ),
            );
            assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
            assert_eq!(governor.trigger(), expected_trigger);
        }
    }

    #[test]
    fn changed_validation_dominates_a_competing_read() {
        let config = config();
        let cases = [
            (
                ValidationFreshnessStatus::FailedAfterLastMutation,
                ReasoningPolicyTrigger::ValidationFailed,
            ),
            (
                ValidationFreshnessStatus::TimedOut,
                ReasoningPolicyTrigger::ValidationTimedOut,
            ),
        ];

        for (validation_status, expected_trigger) in cases {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            governor.phase = SamplingReasoningPhase::Finalize;
            let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
            governor.settle(
                &baselines,
                &collector_with(SamplingToolOutcomeKind::Success),
                &settled(0, validation_status, Some(0)),
            );
            assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
            assert_eq!(governor.trigger(), expected_trigger);
        }
    }

    #[test]
    fn fresh_validation_uses_final_revision_and_active_plan_state() {
        let config = config();
        let mut no_plan = SamplingReasoningGovernor::new(Some(&config));
        let baseline = no_plan.baselines(1, ValidationFreshnessStatus::None, None);
        no_plan.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                1,
                ValidationFreshnessStatus::PassedAfterLastMutation,
                Some(1),
            ),
        );
        assert_eq!(no_plan.phase, SamplingReasoningPhase::Finalize);
        assert_eq!(no_plan.trigger(), ReasoningPolicyTrigger::ValidationPassed);

        let mut active_plan = SamplingReasoningGovernor::new(Some(&config));
        settle_plan(&mut active_plan, plan(&[StepStatus::InProgress]));
        let baseline = active_plan.baselines(1, ValidationFreshnessStatus::None, None);
        active_plan.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                1,
                ValidationFreshnessStatus::PassedAfterLastMutation,
                Some(1),
            ),
        );
        assert_eq!(active_plan.phase, SamplingReasoningPhase::Verify);
        assert_eq!(
            active_plan.trigger(),
            ReasoningPolicyTrigger::ValidationPassed
        );
    }

    #[test]
    fn validation_then_concurrent_mutation_is_stale_but_final_revision_validation_is_fresh() {
        let config = config();
        let mut stale = SamplingReasoningGovernor::new(Some(&config));
        let baseline = stale.baselines(1, ValidationFreshnessStatus::None, None);
        stale.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                2,
                ValidationFreshnessStatus::StaleAfterLastMutation,
                Some(1),
            ),
        );
        assert_eq!(stale.phase, SamplingReasoningPhase::Implement);
        assert_eq!(stale.trigger(), ReasoningPolicyTrigger::WorkspaceMutation);

        let mut fresh = SamplingReasoningGovernor::new(Some(&config));
        let baseline = fresh.baselines(1, ValidationFreshnessStatus::None, None);
        fresh.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                2,
                ValidationFreshnessStatus::PassedAfterLastMutation,
                Some(2),
            ),
        );
        assert_eq!(fresh.phase, SamplingReasoningPhase::Finalize);
        assert_eq!(fresh.trigger(), ReasoningPolicyTrigger::ValidationPassed);
    }

    #[test]
    fn request_baselines_prevent_stale_mutation_validation_and_plan_signals() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        settle_plan(&mut governor, plan(&[StepStatus::InProgress]));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(
            4,
            ValidationFreshnessStatus::FailedAfterLastMutation,
            Some(4),
        );
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(
                4,
                ValidationFreshnessStatus::FailedAfterLastMutation,
                Some(4),
            ),
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
        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with_read_and_plan(plan(&[StepStatus::InProgress])),
            &settled(0, ValidationFreshnessStatus::None, None),
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
        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);

        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::Success),
            &settled(1, ValidationFreshnessStatus::StaleAfterLastMutation, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Implement);

        governor.host_diagnose();
        settle_plan(&mut governor, plan(&[StepStatus::Implemented]));
        assert_eq!(governor.phase, SamplingReasoningPhase::Verify);
    }

    #[test]
    fn plan_tool_call_ordinal_wins_independent_of_completion_order() {
        let config = config();
        for reverse_completion in [false, true] {
            let mut governor = SamplingReasoningGovernor::new(Some(&config));
            let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
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
                Some(plan(&[StepStatus::Implemented])),
            );
            if reverse_completion {
                collector.push(second_outcome);
                collector.push(first_outcome);
            } else {
                collector.push(first_outcome);
                collector.push(second_outcome);
            }
            governor.settle(
                &baseline,
                &collector,
                &settled(0, ValidationFreshnessStatus::None, None),
            );
            assert_eq!(governor.phase, SamplingReasoningPhase::Verify);
        }
    }

    #[test]
    fn recoverable_cancellation_diagnoses_and_no_signal_retains() {
        let config = config();
        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
        governor.settle(
            &baseline,
            &SamplingRequestSignalCollector::default(),
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);

        governor.settle(
            &baseline,
            &collector_with(SamplingToolOutcomeKind::RecoverableCancellation),
            &settled(0, ValidationFreshnessStatus::None, None),
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
            let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
            let collector = SamplingRequestSignalCollector::default();
            let ordinal = collector.register_tool_call();
            collector.push(SamplingToolOutcome::from_signal(
                ordinal,
                ToolOutputOutcomeContext::skipped(disposition),
                None,
                None,
            ));
            governor.settle(
                &baseline,
                &collector,
                &settled(0, ValidationFreshnessStatus::None, None),
            );
            assert_eq!(governor.phase, SamplingReasoningPhase::Finalize);
        }

        let mut governor = SamplingReasoningGovernor::new(Some(&config));
        governor.phase = SamplingReasoningPhase::Finalize;
        let baseline = governor.baselines(0, ValidationFreshnessStatus::None, None);
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
        governor.settle(
            &baseline,
            &collector,
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(governor.phase, SamplingReasoningPhase::Diagnose);
        assert_eq!(governor.trigger(), ReasoningPolicyTrigger::ToolBlocked);
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
        let baselines = governor.baselines(7, ValidationFreshnessStatus::None, None);
        let settled = SamplingRequestSettledState {
            mutation_revision: 7,
            validation_status: ValidationFreshnessStatus::None,
            validation_revision: None,
        };
        (baselines, settled)
    }

    fn authoritative_wait_collector(
        governor: &SamplingReasoningGovernor,
        baselines: &SamplingRequestBaselines,
        identity: &str,
        mixed: bool,
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
                    surfaceable_message: Some("terminal owner result".to_string()),
                },
                assignment_ids: Vec::new(),
            }];
        }
        collector
    }

    #[test]
    fn second_exact_authoritative_wait_surfaces_existing_result() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = authoritative_wait_collector(&governor, &baselines, "same", false);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );

        let second = authoritative_wait_collector(&governor, &baselines, "same", false);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &second, &settled),
            SamplingConvergenceDecision {
                continuation: ContinuationDisposition::SurfaceExistingResult,
                proven_loop_activated: true,
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
    fn authoritative_wait_enforcement_resets_on_identity_state_or_mixed_calls() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);
        let first = authoritative_wait_collector(&governor, &baselines, "first", false);
        governor.evaluate_convergence(&baselines, &first, &settled);

        let changed_identity =
            authoritative_wait_collector(&governor, &baselines, "changed", false);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &changed_identity, &settled),
            SamplingConvergenceDecision::default()
        );

        let mixed = authoritative_wait_collector(&governor, &baselines, "changed", true);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &mixed, &settled),
            SamplingConvergenceDecision::default()
        );

        let changed_baselines = governor.baselines(8, ValidationFreshnessStatus::None, None);
        let changed_settled = SamplingRequestSettledState {
            mutation_revision: 8,
            ..settled
        };
        let changed_state =
            authoritative_wait_collector(&governor, &changed_baselines, "changed", false);
        assert_eq!(
            governor.evaluate_convergence(&changed_baselines, &changed_state, &changed_settled),
            SamplingConvergenceDecision::default()
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
    fn no_progress_counter_directs_but_never_proves_an_empty_loop() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for generation in 1..=4 {
            let collector = governor.collector(&baselines);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation >= 3);
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
    fn blocked_wait_directs_main_action_on_the_second_exact_observation() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        for observation in 1..=2 {
            let collector = governor.collector(&baselines);
            blocked_wait(&collector, "receipt-1");
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert_eq!(decision.directive.is_some(), observation == 2);
            assert_eq!(decision.authoritative_wait.is_some(), observation == 2);
            assert_eq!(decision.proven_loop_activated, observation == 2);
        }
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
