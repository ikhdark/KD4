use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
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
use codex_shell_command::is_safe_command::is_known_safe_command;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::ToolPayload;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::watch;

use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::source_closure::AuthoritativeSourceBasis;
use crate::tools::handlers::source_closure::ObligationIdentity;
use crate::tools::handlers::source_closure::SourceEvidenceRange;
use crate::turn_diff_tracker::ValidationFreshnessStatus;
use crate::turn_timing::PreEditReopenReason;
use crate::turn_timing::TurnTimingState;
use crate::validation_admission::PreEditValidationClass;
use crate::validation_admission::classify_pre_edit_validation;

pub(crate) type SamplingReasoningPhase = ReasoningPolicyPhase;
pub(crate) type SamplingRequestPolicySource = ReasoningPolicySource;

/// A closed, bounded set of evidence questions that may be active together.
/// The enum order is the deterministic primary-obligation priority.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum EvidenceObligation {
    Owner,
    GoverningInstructions,
    CallerOrContractClosure,
    ImplementationQuestion,
    FailureCause,
    FocusedValidationRoute,
    FocusedValidationProof,
    TerminalProof,
}

impl EvidenceObligation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::GoverningInstructions => "governing_instructions",
            Self::CallerOrContractClosure => "caller_or_contract_closure",
            Self::ImplementationQuestion => "implementation_question",
            Self::FailureCause => "failure_cause",
            Self::FocusedValidationRoute => "focused_validation_route",
            Self::FocusedValidationProof => "focused_validation_proof",
            Self::TerminalProof => "terminal_proof",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "governing_instructions" => Some(Self::GoverningInstructions),
            "caller_or_contract_closure" => Some(Self::CallerOrContractClosure),
            "implementation_question" => Some(Self::ImplementationQuestion),
            "failure_cause" => Some(Self::FailureCause),
            "focused_validation_route" => Some(Self::FocusedValidationRoute),
            "focused_validation_proof" => Some(Self::FocusedValidationProof),
            "terminal_proof" => Some(Self::TerminalProof),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EvidenceObligationSet(BTreeSet<EvidenceObligation>);

impl EvidenceObligationSet {
    fn discovery() -> Self {
        Self(BTreeSet::from([
            EvidenceObligation::Owner,
            EvidenceObligation::GoverningInstructions,
            EvidenceObligation::CallerOrContractClosure,
            EvidenceObligation::FocusedValidationRoute,
        ]))
    }

    pub(crate) fn primary(&self) -> Option<EvidenceObligation> {
        self.0.first().copied()
    }

    fn insert(&mut self, obligation: EvidenceObligation) {
        self.0.insert(obligation);
    }

    fn remove(&mut self, obligation: EvidenceObligation) {
        self.0.remove(&obligation);
    }

    fn remove_all(&mut self, obligations: &Self) {
        self.0
            .retain(|obligation| !obligations.0.contains(obligation));
    }

    fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }

    fn extend(&mut self, other: &Self) {
        self.0.extend(other.0.iter().copied());
    }

    fn from_signal_array(value: Option<&Value>) -> Self {
        let obligations = value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(EvidenceObligation::parse)
            .collect();
        Self(obligations)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EvidenceOperationRelationship {
    KnownAdvances(EvidenceObligationSet),
    KnownNoAdvance,
    Unknown,
}

impl EvidenceOperationRelationship {
    fn from_signal(signal: Option<&Value>) -> Self {
        let Some(relationship) = signal.and_then(|value| value.get("relationship")) else {
            return Self::Unknown;
        };
        match relationship.get("kind").and_then(Value::as_str) {
            Some("known_advances") => Self::KnownAdvances(
                EvidenceObligationSet::from_signal_array(relationship.get("obligations")),
            ),
            Some("known_no_advance") => Self::KnownNoAdvance,
            _ => Self::Unknown,
        }
    }
}

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
    mutation_obligation_revision: u64,
    input_revision: u64,
    source_closure_identity: Option<String>,
}

impl SamplingRequestBaselines {
    fn revision_key(&self) -> String {
        format!(
            "mutation={};validation_status={:?};validation_revision={:?};plan={};mutation_obligation={};input={};source={:?}",
            self.mutation_revision,
            self.validation_status,
            self.validation_revision,
            self.plan_revision,
            self.mutation_obligation_revision,
            self.input_revision,
            self.source_closure_identity,
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
    pub(crate) source_closure_identity: Option<String>,
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
    advances: EvidenceObligationSet,
    reopens: EvidenceObligationSet,
    relationship: EvidenceOperationRelationship,
}

impl SamplingToolOutcome {
    fn from_signal(
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        plan: Option<UpdatePlanArgs>,
        signal: Option<&Value>,
    ) -> Self {
        let kind = sampling_tool_outcome_kind(outcome_context.outcome, signal);
        let mut reopens =
            EvidenceObligationSet::from_signal_array(signal.and_then(|value| value.get("reopens")));
        if outcome_reopens_failure_evidence(kind, outcome_context.skip_disposition)
            && reopens.0.is_empty()
        {
            reopens.extend(&validation_failure_obligation_delta(signal));
            if reopens.0.is_empty() {
                reopens.insert(EvidenceObligation::FailureCause);
            }
        }
        Self {
            ordinal,
            kind,
            skip_disposition: outcome_context.skip_disposition,
            plan,
            advances: EvidenceObligationSet::from_signal_array(
                signal.and_then(|value| value.get("advances")),
            ),
            reopens,
            relationship: EvidenceOperationRelationship::from_signal(signal),
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

/// Maps only producer-supplied structured validation classifications. Rendered
/// stderr and arbitrary summaries never participate in this decision.
fn validation_failure_obligation_delta(signal: Option<&Value>) -> EvidenceObligationSet {
    let mut obligations = EvidenceObligationSet::default();
    match signal
        .and_then(|value| value.get("validation_failure_class"))
        .and_then(Value::as_str)
    {
        Some("caller_or_contract") => {
            obligations.insert(EvidenceObligation::CallerOrContractClosure);
            obligations.insert(EvidenceObligation::ImplementationQuestion);
        }
        Some("compile_or_type") => {
            obligations.insert(EvidenceObligation::ImplementationQuestion);
        }
        Some("platform") => {
            obligations.insert(EvidenceObligation::ImplementationQuestion);
            obligations.insert(EvidenceObligation::FailureCause);
        }
        Some("owner_contradiction") => {
            obligations.insert(EvidenceObligation::Owner);
        }
        Some("unclassified") => {
            obligations.insert(EvidenceObligation::FailureCause);
        }
        _ => {}
    }
    obligations
}

#[derive(Clone, Debug)]
struct PendingDeterministicDispatch {
    key: String,
    locator: bool,
}

#[derive(Clone, Debug)]
struct DeterministicCycleEntry {
    ordinal: u64,
    key: String,
    outcome: String,
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

#[derive(Clone, Debug)]
struct DeterministicDispatchRecord {
    original_call_id: String,
    original_result_identity: String,
    original_execution_status: String,
    state_revision: Option<String>,
    artifact: DeterministicReplayArtifact,
    response: ResponseInputItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreDispatchSuppressionKind {
    StaleFinalValidation,
}

impl PreDispatchSuppressionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::StaleFinalValidation => "stale_final_validation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreDispatchSuppression {
    pub(crate) kind: PreDispatchSuppressionKind,
    pub(crate) reason: String,
    pub(crate) mutation_obligation_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValidationSuppressionDisposition {
    Deferred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ValidationSuppressionOutcome {
    NotExecuted,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeferredValidationSuppressionResult<'a> {
    kind: &'static str,
    disposition: ValidationSuppressionDisposition,
    outcome: ValidationSuppressionOutcome,
    reason_code: &'static str,
    suppression: &'static str,
    reason: &'a str,
    mutation_obligation_revision: u64,
}

impl PreDispatchSuppression {
    pub(crate) fn deferred_validation_result(&self) -> DeferredValidationSuppressionResult<'_> {
        DeferredValidationSuppressionResult {
            kind: "validation_suppression",
            disposition: ValidationSuppressionDisposition::Deferred,
            outcome: ValidationSuppressionOutcome::NotExecuted,
            reason_code: "pending_mutation_obligation",
            suppression: self.kind.as_str(),
            reason: &self.reason,
            mutation_obligation_revision: self.mutation_obligation_revision,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreDispatchAdvisory {
    pub(crate) reason: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeterministicReplayArtifact {
    pub(crate) artifact_id: String,
    pub(crate) canonical_sha256: String,
    pub(crate) canonical_bytes: u64,
}

struct DeterministicDispatchLedger {
    completed: BTreeMap<String, DeterministicDispatchRecord>,
    active_obligations: EvidenceObligationSet,
    pre_edit: PreEditConvergence,
    blocked_wait_gate: Option<BlockedWaitGate>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PreEditConvergenceState {
    #[default]
    OwnerUnresolved,
    OwnerResolved,
    ClosureIncomplete,
    ImplementationReady,
    ImplementationStarted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceOwnerReceiptState {
    OwnerUnresolved,
    OwnerResolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceClosureReceiptState {
    BundleIncomplete,
    BundleReady,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct OwnerEvidenceReceiptV2 {
    pub(crate) receipt_id: String,
    pub(crate) task_contract_epoch: String,
    pub(crate) owner_id: Option<String>,
    pub(crate) source_snapshot_identity: String,
    pub(crate) instruction_snapshot_identity: InstructionSnapshotIdentity,
    pub(crate) closure_contract_revision: String,
    pub(crate) owner_state: SourceOwnerReceiptState,
    pub(crate) closure_state: SourceClosureReceiptState,
    pub(crate) unresolved_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PreEditConvergenceSeed {
    pub(crate) active: bool,
    pub(crate) owner_candidates: Vec<String>,
    pub(crate) instruction_snapshot_identity: InstructionSnapshotIdentity,
    pub(crate) task_mandated_proof: bool,
}

/// Exact identity of the frozen instructions visible to the model for this
/// convergence cycle. `Unknown` is fail-closed compatibility state for callers
/// that cannot prove a semantic snapshot; it replaces the legacy `"captured"`
/// sentinel rather than treating that sentinel as a digest.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "sha256")]
pub(crate) enum InstructionSnapshotIdentity {
    #[default]
    Unknown,
    Empty,
    Loaded(String),
}

impl InstructionSnapshotIdentity {
    pub(crate) fn loaded(digest: [u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self::Loaded(encoded)
    }

    fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreEditObligationKind {
    Primary,
    Caller,
    Test,
    Contract,
    Generated,
    Platform,
    Truncation,
    Ambiguity,
    TaskProof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RangedEvidenceCoverage {
    source_basis: AuthoritativeSourceBasis,
    obligation_identity: ObligationIdentity,
    observed_ranges: Vec<SourceEvidenceRange>,
}

struct PreEditConvergence {
    active: bool,
    state: PreEditConvergenceState,
    owner_candidates: BTreeSet<String>,
    authoritative_owner: Option<String>,
    primary_surface: Option<String>,
    instruction_snapshot_identity: InstructionSnapshotIdentity,
    mechanism_evidence: bool,
    closure: BTreeSet<String>,
    obligations: BTreeMap<String, PreEditObligationKind>,
    required_path_ranges: BTreeMap<String, Vec<SourceEvidenceRange>>,
    ranged_evidence: BTreeMap<String, RangedEvidenceCoverage>,
    bundle_question_details: BTreeMap<String, String>,
    validation_route: Option<String>,
    receipt: Option<OwnerEvidenceReceiptV2>,
    locator_request_identity: Option<String>,
    last_source_snapshot: Option<String>,
    readiness_handoff_pending: bool,
    readiness_handoff_emitted: bool,
    workspace_untouched: bool,
    first_successful_mutation: bool,
    unfinished_mutation_obligation: Option<bool>,
    unfinished_mutation_obligation_revision: u64,
    timing: Arc<TurnTimingState>,
}

impl PreEditConvergence {
    fn new(seed: PreEditConvergenceSeed, timing: Arc<TurnTimingState>) -> Self {
        let active = seed.active;
        if active {
            timing.activate_pre_edit_convergence();
        }
        Self {
            active,
            state: PreEditConvergenceState::OwnerUnresolved,
            owner_candidates: seed
                .owner_candidates
                .into_iter()
                .map(|path| normalize_source_path(&path))
                .filter(|path| !path.is_empty())
                .take(8)
                .collect(),
            authoritative_owner: None,
            primary_surface: None,
            instruction_snapshot_identity: seed.instruction_snapshot_identity,
            mechanism_evidence: false,
            closure: BTreeSet::new(),
            obligations: if seed.task_mandated_proof {
                BTreeMap::from([(
                    "task-mandated-proof".to_string(),
                    PreEditObligationKind::TaskProof,
                )])
            } else {
                BTreeMap::new()
            },
            required_path_ranges: BTreeMap::new(),
            ranged_evidence: BTreeMap::new(),
            bundle_question_details: BTreeMap::new(),
            validation_route: None,
            receipt: None,
            locator_request_identity: None,
            last_source_snapshot: None,
            readiness_handoff_pending: false,
            readiness_handoff_emitted: false,
            workspace_untouched: true,
            first_successful_mutation: false,
            unfinished_mutation_obligation: None,
            unfinished_mutation_obligation_revision: 0,
            timing,
        }
    }

    fn owner_resolved(&self) -> bool {
        self.authoritative_owner.is_some()
    }

    fn is_ready(&self) -> bool {
        self.state == PreEditConvergenceState::ImplementationReady
    }

    fn validation_suppression(
        &self,
        class: PreEditValidationClass,
    ) -> Option<PreDispatchSuppression> {
        (self.active
            && self.workspace_untouched
            && self.is_ready()
            && self.unfinished_mutation_obligation == Some(true)
            && class == PreEditValidationClass::KnownFinalCeremony)
            .then(|| PreDispatchSuppression {
                kind: PreDispatchSuppressionKind::StaleFinalValidation,
                reason: "final validation was not run because a proven mutation obligation remains pending"
                    .to_string(),
                mutation_obligation_revision: self.unfinished_mutation_obligation_revision,
            })
    }

    fn refresh_readiness(&mut self) {
        if !self.active
            || !self.workspace_untouched
            || !self.owner_resolved()
            || self.primary_surface.is_none()
            || !self.instruction_snapshot_identity.is_known()
        {
            return;
        }
        self.state = if !self.obligations.is_empty() || !self.mechanism_evidence {
            PreEditConvergenceState::ClosureIncomplete
        } else {
            PreEditConvergenceState::ImplementationReady
        };
        if self.state == PreEditConvergenceState::ImplementationReady
            && !self.readiness_handoff_emitted
            && !self.readiness_handoff_pending
        {
            self.readiness_handoff_pending = true;
            self.timing.record_pre_edit_implementation_ready();
        }
    }

    fn reopen(&mut self, reason: PreEditReopenReason) {
        if self.is_ready() || self.readiness_handoff_emitted {
            self.timing.record_pre_edit_reopen(reason);
        }
        self.readiness_handoff_pending = false;
        self.readiness_handoff_emitted = false;
        if self.owner_resolved() {
            self.state = PreEditConvergenceState::ClosureIncomplete;
        }
    }

    fn record_mutation_advance(&mut self, successful: bool) {
        if !self.active {
            return;
        }
        self.workspace_untouched = false;
        self.readiness_handoff_pending = false;
        self.obligations.insert(
            "source-revision-after-mutation".to_string(),
            PreEditObligationKind::Ambiguity,
        );
        if successful && !self.first_successful_mutation {
            self.first_successful_mutation = true;
            self.state = PreEditConvergenceState::ImplementationStarted;
            self.timing.record_pre_edit_first_successful_mutation();
        } else if !self.first_successful_mutation {
            self.reopen(PreEditReopenReason::SourceRevision);
        }
    }

    fn source_advisory(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
    ) -> Option<PreDispatchAdvisory> {
        if !self.active
            || !self.workspace_untouched
            || !self.is_ready()
            || !self.owner_resolved()
            || !self.obligations.is_empty()
        {
            return None;
        }
        if self.source_operation_relationship(tool_name, payload)
            != EvidenceOperationRelationship::KnownNoAdvance
        {
            return None;
        }
        Some(PreDispatchAdvisory {
            reason: "Pre-edit convergence classifies this source operation as unsupported expansion after authoritative owner and hydration-complete closure resolution. Execution remains advisory-only."
                .to_string(),
        })
    }

    fn source_operation_relationship(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
    ) -> EvidenceOperationRelationship {
        let Some(operation) = source_operation(tool_name, payload) else {
            return EvidenceOperationRelationship::Unknown;
        };
        let known_no_advance = match operation {
            SourceOperation::ArtifactRecovery => false,
            SourceOperation::Read { path, force_fresh } => {
                !force_fresh && !self.source_target_is_supported(&path)
            }
            SourceOperation::Search {
                paths,
                force_fresh,
                introduces_uncertainty,
            } => {
                !force_fresh
                    && !introduces_uncertainty
                    && (paths.is_empty()
                        || paths
                            .iter()
                            .all(|path| !self.source_target_is_supported(path)))
            }
            SourceOperation::Locate {
                path_anchor,
                force_fresh,
                introduces_uncertainty,
            } => {
                path_anchor.is_some_and(|path| !self.source_target_is_supported(&path))
                    && !force_fresh
                    && !introduces_uncertainty
            }
        };
        if known_no_advance {
            EvidenceOperationRelationship::KnownNoAdvance
        } else {
            EvidenceOperationRelationship::Unknown
        }
    }

    fn source_target_is_supported(&self, target: &str) -> bool {
        let target = normalize_source_path(target);
        self.owner_candidates.contains(&target)
            || self.closure.iter().any(|path| {
                path == &target
                    || path.starts_with(&format!("{target}/"))
                    || target.starts_with(&format!("{path}/"))
            })
            || self
                .obligations
                .keys()
                .any(|obligation| obligation_path(obligation) == Some(target.as_str()))
    }

    fn apply_source_signal(&mut self, signal: &Value) {
        if !self.active || signal.get("kind").and_then(Value::as_str) != Some("source_evidence") {
            return;
        }
        if signal.get("progress_only").and_then(Value::as_bool) == Some(true) {
            return;
        }
        if signal.get("owner_state").is_some() {
            self.apply_owner_bundle_signal(signal);
            return;
        }
        match signal.get("operation").and_then(Value::as_str) {
            Some("locate_task") => self.apply_locator_signal(signal),
            Some("read_file_span") => self.apply_read_signal(signal),
            Some("search_source") => self.apply_search_signal(signal),
            _ => {}
        }
    }

    fn apply_plan_signal(&mut self, signal: &Value) {
        if signal.get("kind").and_then(Value::as_str) != Some("plan_update") {
            return;
        }
        let unfinished = signal
            .get("unfinished_mutation_obligation")
            .and_then(Value::as_bool);
        self.unfinished_mutation_obligation = unfinished;
        self.unfinished_mutation_obligation_revision = self
            .unfinished_mutation_obligation_revision
            .saturating_add(1);
    }

    fn apply_locator_signal(&mut self, signal: &Value) {
        if signal.get("owner_state").is_some() {
            self.apply_owner_bundle_signal(signal);
            return;
        }
        let Some(result) = signal.get("result").and_then(Value::as_object) else {
            return;
        };
        let material_before = (
            self.authoritative_owner.clone(),
            self.primary_surface.clone(),
            self.mechanism_evidence,
            self.obligations.clone(),
            self.validation_route.clone(),
        );
        let snapshot = signal
            .get("snapshot_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let locator_request_identity = signal
            .get("locator_request_identity")
            .or_else(|| signal.get("request_identity"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let prior_snapshot = self.last_source_snapshot.clone();
        if self.is_ready()
            && self.last_source_snapshot.is_some()
            && snapshot != self.last_source_snapshot
        {
            self.reopen(PreEditReopenReason::SourceRevision);
        }
        self.last_source_snapshot = snapshot.clone();

        let routing = result.get("routing").and_then(Value::as_object);
        let status = routing
            .and_then(|routing| routing.get("status"))
            .and_then(Value::as_str);
        let owner = routing
            .and_then(|routing| routing.get("owner_id"))
            .and_then(Value::as_str);
        let primary = result
            .get("primary")
            .and_then(Value::as_object)
            .and_then(|primary| primary.get("path"))
            .and_then(Value::as_str)
            .map(normalize_source_path);
        if status != Some("selected") || owner.is_none() || primary.is_none() {
            if self.is_ready() {
                self.reopen(PreEditReopenReason::NewAmbiguity);
            }
            return;
        }
        let owner = owner.unwrap_or_default().to_string();
        let primary = primary.unwrap_or_default();
        let evidence_basis_rebound = prior_snapshot != snapshot
            || self
                .authoritative_owner
                .as_ref()
                .is_some_and(|current| current != &owner)
            || self.locator_request_identity != locator_request_identity;
        if evidence_basis_rebound {
            self.ranged_evidence.clear();
            self.required_path_ranges.clear();
        }
        if self
            .authoritative_owner
            .as_ref()
            .is_some_and(|current| current != &owner)
            || self
                .primary_surface
                .as_ref()
                .is_some_and(|current| current != &primary)
        {
            self.reopen(PreEditReopenReason::ContradictoryEvidence);
        }
        let newly_resolved = !self.owner_resolved();
        self.authoritative_owner = Some(owner);
        self.locator_request_identity = locator_request_identity;
        self.primary_surface = Some(primary.clone());
        self.state = PreEditConvergenceState::OwnerResolved;
        self.closure.insert(primary.clone());

        let neighborhood_paths = value_paths(result.get("source_neighborhoods"));
        self.mechanism_evidence = neighborhood_paths.iter().any(|path| path == &primary)
            || result
                .get("primary")
                .and_then(|value| value.get("resolution"))
                .and_then(Value::as_str)
                .is_some_and(|resolution| resolution == "exact");
        self.closure.extend(neighborhood_paths.iter().cloned());
        self.closure
            .extend(value_paths(result.get("relationships")));
        self.closure.extend(value_paths(result.get("contracts")));
        self.closure.extend(value_paths(result.get("tests")));
        self.closure.extend(value_paths(result.get("instructions")));

        self.obligations.retain(|_, kind| {
            matches!(
                *kind,
                PreEditObligationKind::TaskProof | PreEditObligationKind::Truncation
            )
        });
        for unresolved in result
            .get("unresolved")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(16)
        {
            self.obligations.insert(
                format!("ambiguity:{unresolved}"),
                PreEditObligationKind::Ambiguity,
            );
        }
        for truncation in result
            .get("truncation")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(8)
        {
            let collection = truncation
                .get("collection")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            self.obligations.insert(
                format!("truncation:{collection}"),
                PreEditObligationKind::Truncation,
            );
        }
        let previous_required_ranges = std::mem::take(&mut self.required_path_ranges);
        if let Some(range) = source_evidence_range(result.get("primary")) {
            self.required_path_ranges
                .entry(primary)
                .or_default()
                .push(range);
        }
        self.add_path_obligations(
            result.get("relationships"),
            &neighborhood_paths,
            PreEditObligationKind::Caller,
            |role| role.contains("caller") || role.contains("consumer"),
        );
        self.add_path_obligations(
            result.get("contracts"),
            &neighborhood_paths,
            PreEditObligationKind::Contract,
            |role| role == "contract",
        );
        self.add_path_obligations(
            result.get("contracts"),
            &neighborhood_paths,
            PreEditObligationKind::Generated,
            |role| role.contains("generated"),
        );
        canonicalize_required_ranges(&mut self.required_path_ranges);
        self.ranged_evidence.retain(|path, coverage| {
            self.required_path_ranges.get(path)
                == Some(&coverage.obligation_identity.required_ranges)
                && previous_required_ranges.get(path)
                    == Some(&coverage.obligation_identity.required_ranges)
        });
        if let Some(validation) = result.get("validation").and_then(Value::as_array) {
            for route in validation.iter().take(8) {
                let role = route
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if role.contains("platform")
                    || role.contains("schema")
                    || role.contains("generated")
                {
                    self.validation_route = route
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    if self.validation_route.is_none() {
                        self.obligations.insert(
                            format!("validation:{role}"),
                            PreEditObligationKind::Platform,
                        );
                    }
                }
            }
        }
        if newly_resolved {
            self.timing.record_pre_edit_owner_resolved();
        }
        let material_after = (
            self.authoritative_owner.clone(),
            self.primary_surface.clone(),
            self.mechanism_evidence,
            self.obligations.clone(),
            self.validation_route.clone(),
        );
        if material_before != material_after {
            self.timing.record_pre_edit_material_evidence();
        }
        self.refresh_readiness();
    }

    fn apply_owner_bundle_signal(&mut self, signal: &Value) {
        let material_before = (
            self.authoritative_owner.clone(),
            self.primary_surface.clone(),
            self.mechanism_evidence,
            self.obligations.clone(),
            self.validation_route.clone(),
        );
        let owner_state = match signal.get("owner_state").and_then(Value::as_str) {
            Some("owner_resolved") => SourceOwnerReceiptState::OwnerResolved,
            _ => SourceOwnerReceiptState::OwnerUnresolved,
        };
        let closure_state = match signal.get("closure_state").and_then(Value::as_str) {
            Some("bundle_ready") => SourceClosureReceiptState::BundleReady,
            _ => SourceClosureReceiptState::BundleIncomplete,
        };
        let snapshot = signal
            .get("snapshot_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let locator_request_identity = signal
            .get("locator_request_identity")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let unresolved_ids = signal
            .get("unresolved_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>();
        self.bundle_question_details = signal
            .get("unresolved_questions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|question| {
                Some((
                    question.get("id")?.as_str()?.to_string(),
                    question.get("detail")?.as_str()?.trim().to_string(),
                ))
            })
            .filter(|(id, detail)| unresolved_ids.contains(id) && !detail.is_empty())
            .collect();
        let receipt = OwnerEvidenceReceiptV2 {
            receipt_id: signal
                .get("receipt_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            task_contract_epoch: signal
                .get("task_contract_epoch")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            owner_id: signal
                .get("owner_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            source_snapshot_identity: snapshot.clone(),
            instruction_snapshot_identity: self.instruction_snapshot_identity.clone(),
            closure_contract_revision: signal
                .get("closure_contract_revision")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            owner_state,
            closure_state,
            unresolved_ids: unresolved_ids.clone(),
        };
        let receipt_epoch_changed = self.receipt.as_ref().is_some_and(|current| {
            current.task_contract_epoch != receipt.task_contract_epoch
                || current.closure_contract_revision != receipt.closure_contract_revision
        });
        let evidence_basis_rebound = self.last_source_snapshot.as_deref()
            != Some(snapshot.as_str())
            || self.locator_request_identity != locator_request_identity
            || self.authoritative_owner.as_deref() != receipt.owner_id.as_deref()
            || receipt_epoch_changed;
        if evidence_basis_rebound {
            self.ranged_evidence.clear();
            self.required_path_ranges.clear();
        }
        if self.is_ready()
            && ((self.last_source_snapshot.is_some()
                && self.last_source_snapshot.as_deref() != Some(snapshot.as_str()))
                || receipt_epoch_changed)
        {
            self.reopen(PreEditReopenReason::SourceRevision);
        }
        self.last_source_snapshot = Some(snapshot);
        self.locator_request_identity = locator_request_identity;
        self.receipt = Some(receipt.clone());
        if owner_state != SourceOwnerReceiptState::OwnerResolved || receipt.owner_id.is_none() {
            if self.is_ready() {
                self.reopen(PreEditReopenReason::NewAmbiguity);
            }
            self.state = PreEditConvergenceState::OwnerUnresolved;
            return;
        }

        let newly_resolved = self.authoritative_owner.is_none();
        let Some(owner) = receipt.owner_id else {
            return;
        };
        if self
            .authoritative_owner
            .as_ref()
            .is_some_and(|current| current != &owner)
        {
            self.reopen(PreEditReopenReason::ContradictoryEvidence);
        }
        self.authoritative_owner = Some(owner);
        if let Some(primary) = signal
            .get("primary_path")
            .and_then(Value::as_str)
            .map(normalize_source_path)
        {
            self.primary_surface = Some(primary.clone());
            self.closure.insert(primary);
        }
        let materialized_paths = signal
            .get("materialized_paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(normalize_source_path)
            .collect::<BTreeSet<_>>();
        self.mechanism_evidence = self
            .primary_surface
            .as_ref()
            .is_some_and(|primary| materialized_paths.contains(primary));
        self.closure.extend(materialized_paths.iter().cloned());
        self.validation_route = signal
            .get("validation_route")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        self.obligations.retain(|_, kind| {
            matches!(
                *kind,
                PreEditObligationKind::TaskProof | PreEditObligationKind::Truncation
            )
        });
        let previous_required_ranges = std::mem::take(&mut self.required_path_ranges);
        for unresolved_id in unresolved_ids {
            self.obligations.insert(
                format!("bundle:{unresolved_id}"),
                PreEditObligationKind::Ambiguity,
            );
        }
        for section in signal
            .get("required_source_sections")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(path) = section.get("path").and_then(Value::as_str) else {
                continue;
            };
            let path = normalize_source_path(path);
            if path.is_empty() || materialized_paths.contains(&path) {
                continue;
            }
            let kind = match section.get("obligation_kind").and_then(Value::as_str) {
                Some("primary") => PreEditObligationKind::Primary,
                Some("caller") => PreEditObligationKind::Caller,
                Some("test") => PreEditObligationKind::Test,
                Some("contract") => PreEditObligationKind::Contract,
                Some("generated") => PreEditObligationKind::Generated,
                _ => continue,
            };
            self.obligations.insert(format!("path:{path}"), kind);
            if let Some(range) = source_evidence_range(Some(section)) {
                self.required_path_ranges
                    .entry(path)
                    .or_default()
                    .push(range);
            }
        }
        canonicalize_required_ranges(&mut self.required_path_ranges);
        self.ranged_evidence.retain(|path, coverage| {
            let obligation_kind = self.obligations.get(&format!("path:{path}")).copied();
            self.required_path_ranges.get(path)
                == Some(&coverage.obligation_identity.required_ranges)
                && previous_required_ranges.get(path)
                    == Some(&coverage.obligation_identity.required_ranges)
                && obligation_kind.map(pre_edit_obligation_kind_name)
                    == Some(coverage.obligation_identity.obligation_kind.as_str())
        });
        self.state = PreEditConvergenceState::OwnerResolved;
        if newly_resolved {
            self.timing.record_pre_edit_owner_resolved();
        }
        if closure_state == SourceClosureReceiptState::BundleReady && self.obligations.is_empty() {
            self.mechanism_evidence = true;
        }
        let material_after = (
            self.authoritative_owner.clone(),
            self.primary_surface.clone(),
            self.mechanism_evidence,
            self.obligations.clone(),
            self.validation_route.clone(),
        );
        if material_before != material_after {
            self.timing.record_pre_edit_material_evidence();
        }
        self.refresh_readiness();
    }

    fn add_path_obligations(
        &mut self,
        evidence: Option<&Value>,
        represented_paths: &BTreeSet<String>,
        kind: PreEditObligationKind,
        relevant: impl Fn(&str) -> bool,
    ) {
        for item in evidence
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(16)
        {
            let Some(path) = item.get("path").and_then(Value::as_str) else {
                continue;
            };
            let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
            let path = normalize_source_path(path);
            if relevant(role) && !represented_paths.contains(&path) {
                self.obligations.insert(format!("path:{path}"), kind);
                if let Some(range) = source_evidence_range(Some(item)) {
                    self.required_path_ranges
                        .entry(path)
                        .or_default()
                        .push(range);
                }
            }
        }
    }

    fn apply_read_signal(&mut self, signal: &Value) {
        let Some(path) = signal.get("path").and_then(Value::as_str) else {
            return;
        };
        let path = normalize_source_path(path);
        let obligations_before = self.obligations.clone();
        let truncated = signal
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mechanism_before = self.mechanism_evidence;
        let required_ranges = self
            .required_path_ranges
            .get(&path)
            .cloned()
            .unwrap_or_default();
        let ranged_evidence_complete = !truncated
            && !required_ranges.is_empty()
            && self.record_ranged_read(&path, &required_ranges, signal);
        let grants_path_credit =
            !truncated && (required_ranges.is_empty() || ranged_evidence_complete);
        if grants_path_credit {
            if self.owner_resolved() && self.source_target_is_supported(&path) {
                self.closure.insert(path.clone());
            }
            if self.primary_surface.as_deref() == Some(path.as_str()) {
                self.mechanism_evidence = true;
            }
            self.obligations.remove(&format!("path:{path}"));
            self.obligations.remove(&format!("truncation:{path}"));
            self.obligations.remove(&format!("hydration:{path}"));
            if !self
                .obligations
                .keys()
                .any(|obligation| obligation.starts_with("hydration:"))
            {
                self.obligations.remove("hydration-status:search");
            }
        } else {
            if truncated {
                if self.is_ready() {
                    self.reopen(PreEditReopenReason::IncompleteEvidence);
                }
                self.obligations.insert(
                    format!("truncation:{path}"),
                    PreEditObligationKind::Truncation,
                );
            }
        }
        if obligations_before != self.obligations || mechanism_before != self.mechanism_evidence {
            self.timing.record_pre_edit_material_evidence();
        }
        self.refresh_readiness();
    }

    fn record_ranged_read(
        &mut self,
        path: &str,
        required_ranges: &[SourceEvidenceRange],
        signal: &Value,
    ) -> bool {
        let Some(basis_value) = signal.get("authoritative_source_basis") else {
            self.ranged_evidence.remove(path);
            return false;
        };
        if basis_value.is_null() {
            self.ranged_evidence.remove(path);
            return false;
        }
        let Ok(mut source_basis) =
            serde_json::from_value::<AuthoritativeSourceBasis>(basis_value.clone())
        else {
            self.ranged_evidence.remove(path);
            return false;
        };
        let Some(obligation_value) = signal.get("obligation_identity") else {
            self.ranged_evidence.remove(path);
            return false;
        };
        let Ok(mut obligation_identity) =
            serde_json::from_value::<ObligationIdentity>(obligation_value.clone())
        else {
            self.ranged_evidence.remove(path);
            return false;
        };
        source_basis.path = normalize_source_path(&source_basis.path);
        canonicalize_ranges(&mut obligation_identity.required_ranges);
        let full_file_sha256 = signal
            .get("full_file_sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let obligation_kind = self.obligations.get(&format!("path:{path}")).copied();
        let context_matches = source_basis.path == path
            && !source_basis.source_snapshot_identity.is_empty()
            && !obligation_identity.locator_request_identity.is_empty()
            && !obligation_identity.owner_id.is_empty()
            && !obligation_identity.task_contract_epoch.is_empty()
            && !full_file_sha256.is_empty()
            && full_file_sha256 == source_basis.file_content_hash
            && self.last_source_snapshot.as_deref()
                == Some(source_basis.source_snapshot_identity.as_str())
            && self.locator_request_identity.as_deref()
                == Some(obligation_identity.locator_request_identity.as_str())
            && self.authoritative_owner.as_deref() == Some(obligation_identity.owner_id.as_str())
            && self.receipt.as_ref().is_some_and(|receipt| {
                receipt.owner_state == SourceOwnerReceiptState::OwnerResolved
                    && receipt.owner_id.as_deref() == Some(obligation_identity.owner_id.as_str())
                    && receipt.task_contract_epoch == obligation_identity.task_contract_epoch
            })
            && obligation_kind.map(pre_edit_obligation_kind_name)
                == Some(obligation_identity.obligation_kind.as_str())
            && obligation_identity.required_ranges == required_ranges;
        if !context_matches {
            self.ranged_evidence.remove(path);
            return false;
        }
        let (Some(start_line), Some(end_line)) = (
            signal.get("start_line").and_then(Value::as_u64),
            signal.get("end_line").and_then(Value::as_u64),
        ) else {
            return false;
        };
        let (Ok(start_line), Ok(end_line)) =
            (usize::try_from(start_line), usize::try_from(end_line))
        else {
            return false;
        };
        let total_lines = signal
            .get("total_lines")
            .and_then(Value::as_u64)
            .and_then(|total| usize::try_from(total).ok());
        if start_line == 0
            || end_line < start_line
            || total_lines.is_some_and(|total| end_line > total)
        {
            return false;
        }
        let coverage = self
            .ranged_evidence
            .entry(path.to_string())
            .or_insert_with(|| RangedEvidenceCoverage {
                source_basis: source_basis.clone(),
                obligation_identity: obligation_identity.clone(),
                observed_ranges: Vec::new(),
            });
        if coverage.source_basis != source_basis
            || coverage.obligation_identity != obligation_identity
        {
            *coverage = RangedEvidenceCoverage {
                source_basis,
                obligation_identity,
                observed_ranges: Vec::new(),
            };
        }
        coverage.observed_ranges.push(SourceEvidenceRange {
            start_line,
            end_line,
        });
        canonicalize_ranges(&mut coverage.observed_ranges);
        ranges_cover(&coverage.observed_ranges, required_ranges)
    }

    fn apply_search_signal(&mut self, signal: &Value) {
        let obligations_before = self.obligations.clone();
        let closure_before = self.closure.clone();
        let mechanism_before = self.mechanism_evidence;
        let match_paths = signal_string_set(signal.get("match_paths"));
        let match_count = signal
            .get("match_count")
            .and_then(Value::as_u64)
            .unwrap_or(match_paths.len() as u64);
        let mut hydrated_paths = signal_string_set(signal.get("hydrated_paths"));
        if let Some(path) = signal.get("hydrated_path").and_then(Value::as_str) {
            let path = normalize_source_path(path);
            if !path.is_empty() {
                hydrated_paths.insert(path);
            }
        }
        let coverage_incomplete = signal
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !signal
                .get("coverage_complete")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            || !signal
                .get("index_complete")
                .and_then(Value::as_bool)
                .unwrap_or(true);
        if coverage_incomplete {
            if self.is_ready() {
                self.reopen(PreEditReopenReason::IncompleteEvidence);
            }
            self.obligations.insert(
                "truncation:search".to_string(),
                PreEditObligationKind::Truncation,
            );
        }

        let hydration_status = signal
            .get("hydration_status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let omission_count = signal
            .get("hydration_omission_count")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let hydration_truncated = signal
            .get("hydration_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status_is_exact = matches!(
            hydration_status,
            "hydrated_authoritative_definition"
                | "hydrated_structured_context"
                | "hydrated_deterministic_window"
                | "hydrated_bounded_packet"
        );
        let hydration_complete = if match_count == 0 {
            hydration_status == "skipped_no_unique_match"
                && !coverage_incomplete
                && omission_count == 0
        } else {
            status_is_exact
                && omission_count == 0
                && !hydration_truncated
                && match_count > 0
                && !match_paths.is_empty()
                && match_paths.is_subset(&hydrated_paths)
        };

        if coverage_incomplete || !hydration_complete {
            self.obligations.insert(
                "truncation:search".to_string(),
                PreEditObligationKind::Truncation,
            );
        } else {
            self.obligations.remove("truncation:search");
        }

        for path in &hydrated_paths {
            self.closure.insert(path.clone());
            if !self.required_path_ranges.contains_key(path) {
                self.obligations.remove(&format!("path:{path}"));
                self.obligations.remove(&format!("truncation:{path}"));
                if self.primary_surface.as_deref() == Some(path.as_str()) {
                    self.mechanism_evidence = true;
                }
            }
            self.obligations.remove(&format!("hydration:{path}"));
        }
        if hydration_complete {
            if !self
                .obligations
                .keys()
                .any(|obligation| obligation.starts_with("hydration:"))
            {
                self.obligations.remove("hydration-status:search");
            }
            if let Some(resolved_detail) = signal
                .get("resolved_question_detail")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|detail| !detail.is_empty())
            {
                let resolved_ids = self
                    .bundle_question_details
                    .iter()
                    .filter(|(_, detail)| detail.trim() == resolved_detail)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                for id in resolved_ids {
                    self.bundle_question_details.remove(&id);
                    self.obligations.remove(&format!("bundle:{id}"));
                }
            }
        } else if match_count > 0 {
            if self.is_ready() {
                self.reopen(PreEditReopenReason::IncompleteEvidence);
            }
            self.obligations.insert(
                "hydration-status:search".to_string(),
                PreEditObligationKind::Truncation,
            );
            for path in match_paths.difference(&hydrated_paths) {
                self.obligations.insert(
                    format!("hydration:{path}"),
                    PreEditObligationKind::Truncation,
                );
            }
        }
        if obligations_before != self.obligations
            || closure_before != self.closure
            || mechanism_before != self.mechanism_evidence
        {
            self.timing.record_pre_edit_material_evidence();
        }
        self.refresh_readiness();
    }

    fn take_readiness_handoff(&mut self) -> Option<String> {
        if !self.readiness_handoff_pending || !self.workspace_untouched {
            return None;
        }
        self.readiness_handoff_pending = false;
        self.readiness_handoff_emitted = true;
        self.timing.record_pre_edit_material_evidence();
        Some(format!(
            "Pre-edit convergence reached: authoritative owner `{}` and implementation surface `{}` are established; implementation-affecting closure is complete. Proceed with the pending mutation. Routine post-edit validation routing does not block this edit.",
            self.authoritative_owner.as_deref().unwrap_or("unknown"),
            self.primary_surface.as_deref().unwrap_or("unknown"),
        ))
    }

    fn apply_validation_result(&mut self, class: PreEditValidationClass, success: bool) {
        if !self.active || !self.workspace_untouched || !success {
            return;
        }
        let before = self.obligations.len();
        self.obligations.remove("task-mandated-proof");
        if class == PreEditValidationClass::FocusedImplementationEvidence {
            self.obligations
                .retain(|_, kind| *kind != PreEditObligationKind::Platform);
            self.validation_route = Some("focused-pre-edit-evidence".to_string());
        }
        if before != self.obligations.len() {
            self.timing.record_pre_edit_material_evidence();
        }
        self.refresh_readiness();
    }
}

enum SourceOperation {
    Locate {
        path_anchor: Option<String>,
        force_fresh: bool,
        introduces_uncertainty: bool,
    },
    Read {
        path: String,
        force_fresh: bool,
    },
    Search {
        paths: Vec<String>,
        force_fresh: bool,
        introduces_uncertainty: bool,
    },
    ArtifactRecovery,
}

fn source_operation(tool_name: &ToolName, payload: &ToolPayload) -> Option<SourceOperation> {
    if tool_name_matches(tool_name, "read_tool_output") {
        return Some(SourceOperation::ArtifactRecovery);
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let arguments = serde_json::from_str::<Value>(arguments).ok()?;
    if tool_name_matches(tool_name, "locate_task") {
        return Some(SourceOperation::Locate {
            path_anchor: arguments
                .get("path_anchor")
                .and_then(Value::as_str)
                .map(normalize_source_path),
            force_fresh: arguments
                .get("force_fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            introduces_uncertainty: source_question_introduces_uncertainty(&arguments),
        });
    }
    if tool_name_matches(tool_name, "read_file_span") {
        return Some(SourceOperation::Read {
            path: normalize_source_path(arguments.get("path")?.as_str()?),
            force_fresh: arguments
                .get("force_fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    if tool_name_matches(tool_name, "search_source") {
        let paths = arguments
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(normalize_source_path)
                    .filter(|path| !path.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        return Some(SourceOperation::Search {
            paths,
            force_fresh: arguments
                .get("force_fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            introduces_uncertainty: source_question_introduces_uncertainty(&arguments),
        });
    }
    None
}

fn source_question_introduces_uncertainty(arguments: &Value) -> bool {
    arguments
        .get("source_question")
        .and_then(Value::as_object)
        .and_then(|question| question.get("detail"))
        .and_then(Value::as_str)
        .is_some_and(|detail| !detail.trim().is_empty())
}

fn value_paths(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .map(normalize_source_path)
        .collect()
}

fn signal_string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(normalize_source_path)
        .filter(|value| !value.is_empty())
        .collect()
}

fn source_evidence_range(value: Option<&Value>) -> Option<SourceEvidenceRange> {
    let span = value?.get("span")?;
    let start_line = usize::try_from(span.get("start_line")?.as_u64()?).ok()?;
    let end_line = usize::try_from(span.get("end_line")?.as_u64()?).ok()?;
    (start_line > 0 && end_line >= start_line).then_some(SourceEvidenceRange {
        start_line,
        end_line,
    })
}

fn canonicalize_required_ranges(required_ranges: &mut BTreeMap<String, Vec<SourceEvidenceRange>>) {
    for ranges in required_ranges.values_mut() {
        canonicalize_ranges(ranges);
    }
    required_ranges.retain(|_, ranges| !ranges.is_empty());
}

fn canonicalize_ranges(ranges: &mut Vec<SourceEvidenceRange>) {
    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    let mut merged: Vec<SourceEvidenceRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(previous) = merged.last_mut()
            && range.start_line <= previous.end_line.saturating_add(1)
        {
            previous.end_line = previous.end_line.max(range.end_line);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn ranges_cover(observed: &[SourceEvidenceRange], required: &[SourceEvidenceRange]) -> bool {
    required.iter().all(|required_range| {
        observed.iter().any(|observed_range| {
            observed_range.start_line <= required_range.start_line
                && observed_range.end_line >= required_range.end_line
        })
    })
}

fn pre_edit_obligation_kind_name(kind: PreEditObligationKind) -> &'static str {
    match kind {
        PreEditObligationKind::Primary => "primary",
        PreEditObligationKind::Caller => "caller",
        PreEditObligationKind::Test => "test",
        PreEditObligationKind::Contract => "contract",
        PreEditObligationKind::Generated => "generated",
        PreEditObligationKind::Platform => "platform",
        PreEditObligationKind::Truncation => "truncation",
        PreEditObligationKind::Ambiguity => "ambiguity",
        PreEditObligationKind::TaskProof => "task_proof",
    }
}

fn normalize_source_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn obligation_path(obligation: &str) -> Option<&str> {
    obligation
        .strip_prefix("path:")
        .or_else(|| obligation.strip_prefix("hydration:"))
}

impl DeterministicDispatchLedger {
    fn new(seed: PreEditConvergenceSeed, timing: Arc<TurnTimingState>) -> Self {
        let active_obligations = if seed.active {
            EvidenceObligationSet::discovery()
        } else {
            EvidenceObligationSet::default()
        };
        Self {
            completed: BTreeMap::new(),
            active_obligations,
            pre_edit: PreEditConvergence::new(seed, timing),
            blocked_wait_gate: None,
        }
    }
}

#[derive(Default)]
struct SamplingRequestSignalState {
    outcomes: Vec<SamplingToolOutcome>,
    pending_dispatches: BTreeMap<u64, PendingDeterministicDispatch>,
    pending_pre_edit_validation: BTreeMap<u64, PreEditValidationClass>,
    deterministic_cycle: Vec<DeterministicCycleEntry>,
    registered_count: usize,
    wait_call_count: usize,
    has_unknown_identity: bool,
    saw_source_discovery: bool,
    saw_source_evidence: bool,
    saw_artifact_read: bool,
    saw_validation: bool,
    saw_mutation: bool,
    saw_coordination: bool,
    direct_wait_agent_count: usize,
    direct_code_mode_exec_count: usize,
    code_mode_nested_tool_count: usize,
    authoritative_wait_observations: Vec<AuthoritativeWaitObservation>,
    same_generation_reads: BTreeMap<String, SameGenerationReadFlight>,
}

#[derive(Clone)]
struct SameGenerationReadFlight {
    leader_ordinal: u64,
    sender: watch::Sender<SameGenerationReadOutcome>,
}

#[derive(Clone, Debug)]
enum SameGenerationReadOutcome {
    Pending,
    Shareable(Box<SameGenerationSharedRead>),
    RetryIndependently,
}

#[derive(Clone, Debug)]
pub(crate) struct SameGenerationSharedRead {
    pub(crate) authoritative_execution_identity: String,
    response: ResponseInputItem,
    pub(crate) transitions_committed: bool,
    pub(crate) joined_projection_supported: bool,
}

impl SameGenerationSharedRead {
    pub(crate) fn project_for_call(&self, call_id: &str) -> Option<ResponseInputItem> {
        if !self.transitions_committed || !self.joined_projection_supported {
            return None;
        }
        match &self.response {
            ResponseInputItem::FunctionCallOutput { output, .. } => {
                Some(ResponseInputItem::FunctionCallOutput {
                    call_id: call_id.to_string(),
                    output: output.clone(),
                })
            }
            _ => None,
        }
    }
}

pub(crate) struct SameGenerationSingleflightLeader {
    key: String,
    pub(crate) ordinal: u64,
    pub(crate) authoritative_execution_identity: String,
    sender: watch::Sender<SameGenerationReadOutcome>,
    state: Arc<Mutex<SamplingRequestSignalState>>,
    resolved: Arc<AtomicBool>,
}

impl SameGenerationSingleflightLeader {
    fn resolve(&self, outcome: SameGenerationReadOutcome) {
        if self.resolved.swap(true, Ordering::AcqRel) {
            return;
        }
        self.sender.send_replace(outcome);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .same_generation_reads
            .get(&self.key)
            .is_some_and(|flight| flight.leader_ordinal == self.ordinal)
        {
            state.same_generation_reads.remove(&self.key);
        }
    }
}

impl Drop for SameGenerationSingleflightLeader {
    fn drop(&mut self) {
        self.resolve(SameGenerationReadOutcome::RetryIndependently);
    }
}

pub(crate) struct SameGenerationSingleflightFollower {
    pub(crate) leader_ordinal: u64,
    pub(crate) authoritative_execution_identity: String,
    receiver: watch::Receiver<SameGenerationReadOutcome>,
}

impl SameGenerationSingleflightFollower {
    pub(crate) async fn wait_for_shareable(
        &mut self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Option<SameGenerationSharedRead> {
        loop {
            match self.receiver.borrow().clone() {
                SameGenerationReadOutcome::Pending => {}
                SameGenerationReadOutcome::Shareable(shared) => return Some(*shared),
                SameGenerationReadOutcome::RetryIndependently => return None,
            }
            tokio::select! {
                _ = cancellation.cancelled() => return None,
                changed = self.receiver.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                }
            }
        }
    }
}

pub(crate) enum SameGenerationSingleflightRegistration {
    Leader(SameGenerationSingleflightLeader),
    Follower(SameGenerationSingleflightFollower),
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
    state_revision: Option<String>,
    dispatch_ledger: Option<Arc<Mutex<DeterministicDispatchLedger>>>,
}

pub(crate) struct SamplingToolCallRegistration {
    pub(crate) ordinal: u64,
    pub(crate) suppression: Option<PreDispatchSuppression>,
    pub(crate) blocked_wait_guard: Option<BlockedWaitGuard>,
    pub(crate) replay_response: Option<ResponseInputItem>,
    pub(crate) replay_artifact: Option<DeterministicReplayArtifact>,
    pub(crate) replay_fallback_key: Option<String>,
    pub(crate) replay_fallback_locator: bool,
    pub(crate) repeated_discovery: bool,
    pub(crate) discovery_after_owner_resolution: bool,
    pub(crate) source_advisory: Option<PreDispatchAdvisory>,
    pub(crate) same_generation_singleflight: Option<SameGenerationSingleflightRegistration>,
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
        state.has_unknown_identity = true;
        ordinal
    }

    pub(crate) fn register_deterministic_tool_call(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        current_call_id: &str,
    ) -> SamplingToolCallRegistration {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let discovery = is_owner_discovery_tool(tool_name);
        let wait = is_wait_tool(tool_name);
        let direct_code_mode_exec = crate::tools::code_mode::is_exec_tool_name(tool_name);
        let identity =
            deterministic_dispatch_identity(tool_name, payload, self.state_revision.as_deref());
        let action_identity = deterministic_action_identity(tool_name, payload);
        let pre_edit_validation = pre_edit_validation_class(tool_name, payload);

        let mut repeated_discovery = false;
        let mut discovery_after_owner_resolution = false;
        let mut prior_outcome = None;
        let mut replay_response = None;
        let mut replay_artifact = None;
        let suppression = pre_edit_validation.and_then(|class| {
            let ledger = self.dispatch_ledger.as_ref()?;
            ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pre_edit
                .validation_suppression(class)
        });
        let mut source_advisory = None;
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
        if let (Some(identity), Some(ledger)) = (identity.as_ref(), self.dispatch_ledger.as_ref()) {
            let ledger = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prior = ledger.completed.get(identity).cloned();
            prior_outcome = prior.as_ref().and_then(|record| {
                serde_json::to_value(&record.response)
                    .ok()
                    .map(canonicalize_json)
                    .and_then(|value| serde_json::to_string(&value).ok())
            });
            repeated_discovery = prior_outcome.is_some() && discovery;
            discovery_after_owner_resolution = discovery && ledger.pre_edit.owner_resolved();
            if let Some(prior) = prior {
                replay_response = Some(provenance_preserving_replay(&prior, current_call_id));
                replay_artifact = Some(prior.artifact);
            } else if suppression.is_none() {
                source_advisory = ledger.pre_edit.source_advisory(tool_name, payload);
            }
        }

        let signal_state = Arc::clone(&self.state);
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
        state.saw_source_discovery |= discovery;
        state.saw_source_evidence |= is_source_evidence_tool(tool_name);
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= pre_edit_validation.is_some();
        state.saw_mutation |= is_mutation_tool(tool_name);
        state.saw_coordination |= is_coordination_tool(tool_name);
        if let Some(class) = pre_edit_validation {
            state.pending_pre_edit_validation.insert(ordinal, class);
        }
        let same_generation_singleflight = if replay_response.is_none()
            && let Some(key) = identity.as_ref()
        {
            let authoritative_execution_identity =
                format!("sha256:{:x}", Sha256::digest(key.as_bytes()));
            if let Some(flight) = state.same_generation_reads.get(key) {
                Some(SameGenerationSingleflightRegistration::Follower(
                    SameGenerationSingleflightFollower {
                        leader_ordinal: flight.leader_ordinal,
                        authoritative_execution_identity,
                        receiver: flight.sender.subscribe(),
                    },
                ))
            } else {
                let (sender, _receiver) = watch::channel(SameGenerationReadOutcome::Pending);
                state.same_generation_reads.insert(
                    key.clone(),
                    SameGenerationReadFlight {
                        leader_ordinal: ordinal,
                        sender: sender.clone(),
                    },
                );
                Some(SameGenerationSingleflightRegistration::Leader(
                    SameGenerationSingleflightLeader {
                        key: key.clone(),
                        ordinal,
                        authoritative_execution_identity,
                        sender,
                        state: signal_state,
                        resolved: Arc::new(AtomicBool::new(false)),
                    },
                ))
            }
        } else {
            None
        };
        let replay_fallback_key = replay_response.as_ref().and(identity.clone());
        match (identity, prior_outcome, replay_response.is_some()) {
            (Some(key), Some(outcome), true) => {
                state.deterministic_cycle.push(DeterministicCycleEntry {
                    ordinal,
                    key,
                    outcome,
                });
                state.outcomes.push(SamplingToolOutcome::plain(
                    ordinal,
                    SamplingToolOutcomeKind::Success,
                    None,
                ));
            }
            (Some(key), prior_outcome, false) => {
                if let Some(outcome) = prior_outcome {
                    state.deterministic_cycle.push(DeterministicCycleEntry {
                        ordinal,
                        key: key.clone(),
                        outcome,
                    });
                }
                state.pending_dispatches.insert(
                    ordinal,
                    PendingDeterministicDispatch {
                        key,
                        locator: discovery,
                    },
                );
            }
            (None, _, _) => state.has_unknown_identity = true,
            (Some(_), None, true) => unreachable!("replay records always have a response body"),
        }

        SamplingToolCallRegistration {
            ordinal,
            suppression,
            blocked_wait_guard,
            replay_response,
            replay_artifact,
            replay_fallback_key,
            replay_fallback_locator: discovery,
            repeated_discovery,
            discovery_after_owner_resolution,
            source_advisory,
            same_generation_singleflight,
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

    pub(crate) fn record_suppressed_result(&self, ordinal: u64, response: &ResponseInputItem) {
        let outcome =
            canonical_response_body(response).and_then(|value| serde_json::to_string(&value).ok());
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = state.pending_dispatches.remove(&ordinal);
        state.pending_pre_edit_validation.remove(&ordinal);
        match (pending, outcome) {
            (Some(pending), Some(outcome)) => {
                state.deterministic_cycle.push(DeterministicCycleEntry {
                    ordinal,
                    key: pending.key,
                    outcome,
                });
            }
            (Some(_), None) => state.has_unknown_identity = true,
            (None, _) => {}
        }
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));
    }

    pub(crate) fn record_deferred_validation_result(
        &self,
        ordinal: u64,
        response: &ResponseInputItem,
    ) {
        let outcome =
            canonical_response_body(response).and_then(|value| serde_json::to_string(&value).ok());
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = state.pending_dispatches.remove(&ordinal);
        state.pending_pre_edit_validation.remove(&ordinal);
        match (pending, outcome) {
            (Some(pending), Some(outcome)) => {
                state.deterministic_cycle.push(DeterministicCycleEntry {
                    ordinal,
                    key: pending.key,
                    outcome,
                });
            }
            (Some(_), None) => state.has_unknown_identity = true,
            (None, _) => {}
        }
        state.outcomes.push(SamplingToolOutcome::from_signal(
            ordinal,
            ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Deferred)),
            None,
            None,
        ));
    }

    pub(crate) fn record_source_advisory_physical_operation(&self) {
        if let Some(ledger) = &self.dispatch_ledger {
            ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pre_edit
                .timing
                .record_pre_edit_broad_discovery_after_ready();
        }
    }

    pub(crate) fn complete_same_generation_leader(
        &self,
        leader: &SameGenerationSingleflightLeader,
        outcome_context: ToolOutputOutcomeContext,
        response: &ResponseInputItem,
    ) {
        if same_generation_response_is_safely_shareable(outcome_context, response) {
            leader.resolve(SameGenerationReadOutcome::Shareable(Box::new(
                SameGenerationSharedRead {
                    authoritative_execution_identity: leader
                        .authoritative_execution_identity
                        .clone(),
                    response: response.clone(),
                    transitions_committed: true,
                    joined_projection_supported: true,
                },
            )));
        } else {
            leader.resolve(SameGenerationReadOutcome::RetryIndependently);
        }
    }

    pub(crate) fn fail_same_generation_leader(&self, leader: &SameGenerationSingleflightLeader) {
        leader.resolve(SameGenerationReadOutcome::RetryIndependently);
    }

    pub(crate) fn record_joined_same_generation_result(
        &self,
        ordinal: u64,
        response: &ResponseInputItem,
        authoritative_execution_identity: &str,
    ) {
        let outcome = canonical_response_body(response).and_then(|result| {
            serde_json::to_string(&canonicalize_json(serde_json::json!({
                "disposition": "joined_reused",
                "authoritative_execution_identity": authoritative_execution_identity,
                "result": result,
            })))
            .ok()
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = state.pending_dispatches.remove(&ordinal);
        state.pending_pre_edit_validation.remove(&ordinal);
        match (pending, outcome) {
            (Some(pending), Some(outcome)) => {
                state.deterministic_cycle.push(DeterministicCycleEntry {
                    ordinal,
                    key: pending.key,
                    outcome,
                });
            }
            (Some(_), None) => state.has_unknown_identity = true,
            (None, _) => {}
        }
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));
    }

    pub(crate) fn activate_replay_fallback(&self, ordinal: u64, key: String, locator: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.outcomes.retain(|outcome| outcome.ordinal != ordinal);
        state
            .deterministic_cycle
            .retain(|entry| entry.ordinal != ordinal);
        state
            .pending_dispatches
            .insert(ordinal, PendingDeterministicDispatch { key, locator });
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
                .pre_edit
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

    pub(crate) fn record_failure_with_mutation(&self, ordinal: u64, mutation_advanced: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_dispatches.remove(&ordinal);
        state.pending_pre_edit_validation.remove(&ordinal);
        state.has_unknown_identity = true;
        state.outcomes.push(SamplingToolOutcome::plain(
            ordinal,
            SamplingToolOutcomeKind::Failure,
            None,
        ));
        drop(state);
        if let Some(ledger) = self.dispatch_ledger.as_ref() {
            let mut ledger = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if mutation_advanced {
                ledger.pre_edit.record_mutation_advance(false);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn record_response_result(
        &self,
        ordinal: u64,
        success: bool,
        signal: Option<Value>,
        response: &ResponseInputItem,
    ) {
        self.record_response_result_with_mutation(
            ordinal,
            ToolOutputOutcomeContext::new(if success {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            }),
            signal,
            response,
            false,
        );
    }

    pub(crate) fn record_response_result_with_mutation(
        &self,
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        signal: Option<Value>,
        response: &ResponseInputItem,
        mutation_advanced: bool,
    ) {
        let kind = sampling_tool_outcome_kind(outcome_context.outcome, signal.as_ref());
        let plan = sampling_plan(signal.as_ref());
        let outcome =
            canonical_response_body(response).and_then(|value| serde_json::to_string(&value).ok());
        let (pending, pre_edit_validation) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pending = state.pending_dispatches.remove(&ordinal);
            let pre_edit_validation = state.pending_pre_edit_validation.remove(&ordinal);
            match (pending.as_ref(), outcome.as_ref()) {
                (Some(pending), Some(outcome)) => {
                    state.deterministic_cycle.push(DeterministicCycleEntry {
                        ordinal,
                        key: pending.key.clone(),
                        outcome: outcome.clone(),
                    });
                }
                (Some(_), None) => state.has_unknown_identity = true,
                (None, _) => {}
            }
            state.outcomes.push(SamplingToolOutcome::from_signal(
                ordinal,
                outcome_context,
                plan,
                signal.as_ref(),
            ));
            (pending, pre_edit_validation)
        };

        if let Some(ledger) = self.dispatch_ledger.as_ref() {
            let mut ledger = ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if kind == SamplingToolOutcomeKind::Success
                && outcome_context.skip_disposition.is_none()
                && let (Some(pending), Some(outcome)) = (pending, outcome)
                && let Some(artifact) = projection_artifact_identity(response)
            {
                let record = DeterministicDispatchRecord {
                    original_call_id: response_input_call_id(response).unwrap_or_default(),
                    original_result_identity: format!(
                        "sha256:{:x}",
                        Sha256::digest(outcome.as_bytes())
                    ),
                    original_execution_status: response_execution_status(response)
                        .unwrap_or_else(|| "unknown".to_string()),
                    state_revision: self.state_revision.clone(),
                    artifact,
                    response: response.clone(),
                };
                let locator_reusable = !pending.locator
                    || signal
                        .as_ref()
                        .and_then(|value| value.get("locator_reusable"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if locator_reusable {
                    ledger.completed.insert(pending.key.clone(), record.clone());
                }
                if pending.locator
                    && locator_reusable
                    && let Some(dependency_identity) = signal
                        .as_ref()
                        .and_then(|value| value.get("source_dependency_identity"))
                        .and_then(Value::as_str)
                    && let Some(alias) =
                        locator_post_result_alias(&pending.key, dependency_identity)
                {
                    ledger.completed.insert(alias, record);
                }
            }
            if mutation_advanced {
                ledger
                    .pre_edit
                    .record_mutation_advance(outcome_context.outcome == ToolOutputOutcome::Success);
            }
            if let Some(class) = pre_edit_validation {
                match outcome_context.outcome {
                    ToolOutputOutcome::Success => {
                        ledger.pre_edit.apply_validation_result(class, true)
                    }
                    ToolOutputOutcome::Failure | ToolOutputOutcome::TimedOut => {
                        ledger.pre_edit.apply_validation_result(class, false);
                    }
                    ToolOutputOutcome::Skipped
                        if outcome_context.skip_disposition
                            == Some(ToolOutputSkipDisposition::BlockingRequiredOperation) =>
                    {
                        ledger.pre_edit.apply_validation_result(class, false);
                    }
                    ToolOutputOutcome::Skipped => {}
                }
            }
            if kind == SamplingToolOutcomeKind::Success
                && let Some(signal) = signal.as_ref()
            {
                ledger.pre_edit.apply_source_signal(signal);
                ledger.pre_edit.apply_plan_signal(signal);
            }
        }
    }

    fn deterministic_cycle_key(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.registered_count == 0 {
            return Some("empty".to_string());
        }
        if state.has_unknown_identity || state.deterministic_cycle.len() != state.registered_count {
            return None;
        }
        let mut entries = state.deterministic_cycle.clone();
        entries.sort_by_key(|entry| entry.ordinal);
        serde_json::to_string(
            &entries
                .into_iter()
                .map(|entry| (entry.key, entry.outcome))
                .collect::<Vec<_>>(),
        )
        .ok()
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
        } else if state.saw_source_discovery
            || state.saw_source_evidence
            || settled.source_closure_identity != baselines.source_closure_identity
        {
            Some(TurnTimingGenerationPurpose::SourceEvidenceInterpretation)
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
        if state.saw_source_discovery {
            progress.push(TurnTimingProgressKind::SourceClosure);
        }
        if settled.source_closure_identity != baselines.source_closure_identity
            && !progress.contains(&TurnTimingProgressKind::SourceClosure)
        {
            progress.push(TurnTimingProgressKind::SourceClosure);
        }
        if state.saw_source_evidence {
            // A named source read is progress even when it does not mutate the
            // workspace. No reasoning text is inspected.
            progress.push(TurnTimingProgressKind::NewNamedEvidence);
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

fn deterministic_dispatch_identity(
    tool_name: &ToolName,
    payload: &ToolPayload,
    state_revision: Option<&str>,
) -> Option<String> {
    if !supports_deterministic_identity(tool_name) {
        return None;
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let revision = state_revision?;
    let arguments = serde_json::from_str::<Value>(arguments).ok()?;
    if arguments.get("force_fresh").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let required_identity_field = if tool_name_matches(tool_name, "locate_task") {
        "task"
    } else if tool_name_matches(tool_name, "read_file_span") {
        "path"
    } else if tool_name_matches(tool_name, "search_source") {
        "query"
    } else {
        return None;
    };
    if arguments
        .get(required_identity_field)
        .and_then(Value::as_str)
        .is_none_or(|value| value.trim().is_empty())
    {
        return None;
    }
    let arguments = serde_json::to_string(&canonicalize_json(arguments)).ok()?;
    let action_class = serde_json::to_string(tool_name).ok()?;
    Some(format!("{action_class}\n{arguments}\n{revision}"))
}

fn deterministic_action_identity(tool_name: &ToolName, payload: &ToolPayload) -> Option<String> {
    if !supports_deterministic_identity(tool_name)
        && !tool_name_matches(tool_name, "wait")
        && !tool_name_matches(tool_name, "wait_agent")
    {
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

fn locator_post_result_alias(key: &str, source_dependency_identity: &str) -> Option<String> {
    let (prefix, _) = key.rsplit_once(";source=")?;
    Some(format!(
        "{prefix};source={:?}",
        Some(source_dependency_identity)
    ))
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

fn response_input_call_id(response: &ResponseInputItem) -> Option<String> {
    match response {
        ResponseInputItem::FunctionCallOutput { call_id, .. }
        | ResponseInputItem::CustomToolCallOutput { call_id, .. }
        | ResponseInputItem::McpToolCallOutput { call_id, .. } => Some(call_id.clone()),
        ResponseInputItem::ToolSearchOutput { call_id, .. } => Some(call_id.clone()),
        ResponseInputItem::Message { .. } => None,
    }
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

fn response_execution_status(response: &ResponseInputItem) -> Option<String> {
    serde_json::from_str::<Value>(response_output_text(response)?)
        .ok()?
        .get("outcome")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn projection_artifact_identity(
    response: &ResponseInputItem,
) -> Option<DeterministicReplayArtifact> {
    let envelope = serde_json::from_str::<Value>(response_output_text(response)?).ok()?;
    if envelope.get("version").and_then(Value::as_u64) != Some(1)
        || envelope.get("canonical_complete").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let artifact_id = envelope.get("artifact_id")?.as_str()?.to_string();
    let canonical_sha256 = envelope.get("canonical_sha256")?.as_str()?.to_string();
    let canonical_bytes = envelope.get("canonical_bytes")?.as_u64()?;
    if artifact_id.is_empty() || canonical_sha256.len() != 64 {
        return None;
    }
    Some(DeterministicReplayArtifact {
        artifact_id,
        canonical_sha256,
        canonical_bytes,
    })
}

fn same_generation_response_is_safely_shareable(
    outcome_context: ToolOutputOutcomeContext,
    response: &ResponseInputItem,
) -> bool {
    outcome_context.outcome == ToolOutputOutcome::Success
        && outcome_context.skip_disposition.is_none()
        && projection_artifact_identity(response).is_some()
        && matches!(
            response,
            ResponseInputItem::FunctionCallOutput { output, .. }
                if output.success == Some(true)
        )
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

fn provenance_preserving_replay(
    prior: &DeterministicDispatchRecord,
    current_call_id: &str,
) -> ResponseInputItem {
    let body = serde_json::json!({
        "version": 1,
        "status": "not_modified",
        "original_call_id": prior.original_call_id,
        "original_result_identity": prior.original_result_identity,
        "original_execution_status": prior.original_execution_status,
        "state_revision": prior.state_revision,
        "artifact_id": prior.artifact.artifact_id,
        "canonical_sha256": prior.artifact.canonical_sha256,
        "canonical_bytes": prior.artifact.canonical_bytes,
    })
    .to_string();
    let success = match &prior.response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => output.success,
        _ => None,
    };
    ResponseInputItem::FunctionCallOutput {
        call_id: current_call_id.to_string(),
        output: codex_protocol::models::FunctionCallOutputPayload {
            body: codex_protocol::models::FunctionCallOutputBody::Text(body),
            success,
        },
    }
}

fn supports_deterministic_identity(tool_name: &ToolName) -> bool {
    ["locate_task", "read_file_span", "search_source"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
}

fn pre_edit_validation_class(
    tool_name: &ToolName,
    payload: &ToolPayload,
) -> Option<PreEditValidationClass> {
    if !is_validation_tool(tool_name) {
        return None;
    }
    let ToolPayload::Function { arguments } = payload else {
        return None;
    };
    let arguments = serde_json::from_str::<Value>(arguments).ok()?;
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
    let invocation = CommandInvocation::from_parts(
        tool_name.name.as_str(),
        script_field,
        script,
        kind,
        program,
        args.as_deref(),
        script_body,
    )
    .ok()?;
    classify_pre_edit_validation(&invocation)
}

fn is_owner_discovery_tool(tool_name: &ToolName) -> bool {
    tool_name_matches(tool_name, "locate_task")
}

fn is_wait_tool(tool_name: &ToolName) -> bool {
    tool_name_matches(tool_name, "wait")
}

fn is_source_evidence_tool(tool_name: &ToolName) -> bool {
    ["locate_task", "read_file_span", "search_source"]
        .iter()
        .any(|candidate| tool_name_matches(tool_name, candidate))
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

pub(crate) fn is_accepted_mutating_operation(
    class: TypedToolClass,
    external_mutation_intent: ExternalMutationIntent,
    tool_name: &ToolName,
    payload: &ToolPayload,
) -> bool {
    if is_mutation_tool(tool_name) {
        return true;
    }
    match class {
        TypedToolClass::Shell => !shell_payload_is_known_read_only(payload),
        TypedToolClass::StructuredEdit => true,
        TypedToolClass::DynamicExternal => {
            external_mutation_intent == ExternalMutationIntent::MayMutate
        }
        TypedToolClass::Unknown => false,
        TypedToolClass::AgentCommunication
        | TypedToolClass::OwnTask
        | TypedToolClass::RootTaskControl
        | TypedToolClass::ReadSearch
        | TypedToolClass::CodeModeControl
        | TypedToolClass::Diff => false,
    }
}

fn shell_payload_is_known_read_only(payload: &ToolPayload) -> bool {
    let ToolPayload::Function { arguments } = payload else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if let Some(program) = object.get("program").and_then(Value::as_str) {
        let mut command = vec![program.to_string()];
        let Some(args) = object.get("args").and_then(Value::as_array) else {
            return is_known_safe_command(&command);
        };
        let Some(args) = args.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
            return false;
        };
        command.extend(args.into_iter().map(str::to_string));
        return is_known_safe_command(&command);
    }
    let script = object
        .get("command")
        .or_else(|| object.get("cmd"))
        .or_else(|| object.get("script_body"))
        .and_then(Value::as_str);
    let Some(script) = script else {
        return false;
    };
    let command = if cfg!(windows) {
        vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ]
    } else {
        vec!["bash".to_string(), "-lc".to_string(), script.to_string()]
    };
    is_known_safe_command(&command)
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
    pub(crate) readiness_handoff: bool,
    pub(crate) authoritative_wait: Option<AuthoritativeWaitResolution>,
}

impl SamplingReasoningGovernor {
    #[cfg(test)]
    pub(crate) fn new(config: Option<&ReasoningPhaseEfforts>) -> Self {
        Self::new_with_pre_edit(
            config,
            PreEditConvergenceSeed::default(),
            Arc::new(TurnTimingState::default()),
        )
    }

    pub(crate) fn new_with_pre_edit(
        config: Option<&ReasoningPhaseEfforts>,
        seed: PreEditConvergenceSeed,
        timing: Arc<TurnTimingState>,
    ) -> Self {
        Self {
            enabled: config.is_some(),
            phase: SamplingReasoningPhase::Orient,
            trigger: ReasoningPolicyTrigger::UserInput,
            plan: None,
            plan_revision: 0,
            input_revision: 0,
            dispatch_ledger: Arc::new(Mutex::new(DeterministicDispatchLedger::new(seed, timing))),
            consecutive_no_progress: 0,
            last_cycle: None,
            last_state_revision: None,
            directive_issued: false,
            proven_loop_active: false,
            wait_convergence: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn baselines(
        &self,
        mutation_revision: u64,
        validation_status: ValidationFreshnessStatus,
        validation_revision: Option<u64>,
    ) -> SamplingRequestBaselines {
        self.baselines_with_source_identity(
            mutation_revision,
            validation_status,
            validation_revision,
            None,
        )
    }

    pub(crate) fn baselines_with_source_identity(
        &self,
        mutation_revision: u64,
        validation_status: ValidationFreshnessStatus,
        validation_revision: Option<u64>,
        source_closure_identity: Option<String>,
    ) -> SamplingRequestBaselines {
        let mutation_obligation_revision = self.current_mutation_obligation_revision();
        SamplingRequestBaselines {
            mutation_revision,
            validation_status,
            validation_revision,
            plan_revision: self.plan_revision,
            mutation_obligation_revision,
            input_revision: self.input_revision,
            source_closure_identity,
        }
    }

    pub(crate) fn collector(
        &self,
        baselines: &SamplingRequestBaselines,
    ) -> SamplingRequestSignalCollector {
        SamplingRequestSignalCollector {
            next_ordinal: Arc::new(AtomicU64::new(0)),
            state: Arc::new(Mutex::new(SamplingRequestSignalState::default())),
            state_revision: Some(baselines.revision_key()),
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
            || settled.source_closure_identity != baselines.source_closure_identity
            || self.plan_revision != baselines.plan_revision
            || self.current_mutation_obligation_revision() != baselines.mutation_obligation_revision
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
        if state.registered_count != 0
            || !state.outcomes.is_empty()
            || !state.pending_dispatches.is_empty()
            || !state.pending_pre_edit_validation.is_empty()
            || state.has_unknown_identity
        {
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

    pub(crate) fn model_evidence_guidance(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let primary = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_obligations
            .primary()
            .map(EvidenceObligation::as_str)
            .unwrap_or("none");
        Some(format!(
            "next_evidence_obligation: {primary}\nblocked_by: none"
        ))
    }

    #[cfg(test)]
    pub(crate) fn accepted_user_input(&mut self) {
        self.accepted_user_input_with_seed(PreEditConvergenceSeed::default());
    }

    pub(crate) fn accepted_user_input_with_seed(&mut self, seed: PreEditConvergenceSeed) {
        self.input_revision = self.input_revision.saturating_add(1);
        self.reset_convergence();
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.pre_edit.is_ready() {
            ledger
                .pre_edit
                .timing
                .record_pre_edit_reopen(PreEditReopenReason::UserSteering);
        }
        let timing = Arc::clone(&ledger.pre_edit.timing);
        *ledger = DeterministicDispatchLedger::new(seed, timing);
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
        self.dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pre_edit
            .record_mutation_advance(true);
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
        let readiness_handoff = {
            self.dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pre_edit
                .take_readiness_handoff()
        };
        if let Some(directive) = readiness_handoff {
            self.wait_convergence = None;
            self.transition_to(
                SamplingReasoningPhase::Implement,
                ReasoningPolicyTrigger::HostOverride,
            );
            return SamplingConvergenceDecision {
                directive: Some(directive),
                readiness_handoff: true,
                ..Default::default()
            };
        }
        let settled_revision = self.settled_revision_key(settled);
        if settled.mutation_revision != baselines.mutation_revision
            || settled.validation_status != baselines.validation_status
            || settled.validation_revision != baselines.validation_revision
            || settled.source_closure_identity != baselines.source_closure_identity
            || self.plan_revision != baselines.plan_revision
            || self.current_mutation_obligation_revision() != baselines.mutation_obligation_revision
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
                            ..Default::default()
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
            readiness_handoff: false,
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
        let mutation_obligation_revision = self.current_mutation_obligation_revision();
        format!(
            "mutation={};validation_status={:?};validation_revision={:?};plan={};mutation_obligation={};input={};source={:?}",
            settled.mutation_revision,
            settled.validation_status,
            settled.validation_revision,
            self.plan_revision,
            mutation_obligation_revision,
            self.input_revision,
            settled.source_closure_identity,
        )
    }

    fn current_mutation_obligation_revision(&self) -> u64 {
        self.dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pre_edit
            .unfinished_mutation_obligation_revision
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
        self.settle_evidence_obligations(baselines, settled, &outcomes);
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

    fn settle_evidence_obligations(
        &self,
        baselines: &SamplingRequestBaselines,
        settled: &SamplingRequestSettledState,
        outcomes: &[SamplingToolOutcome],
    ) {
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active_before = ledger.active_obligations.clone();
        let mut advances = EvidenceObligationSet::default();
        let mut reopens = EvidenceObligationSet::default();

        // Every result in a parallel batch is evaluated against the same
        // pre-batch set. Only after collecting all deltas do we apply their
        // union, allowing independent operations to close independent work.
        for outcome in outcomes {
            if outcome.kind == SamplingToolOutcomeKind::Success {
                advances.extend(&outcome.advances);
                if let EvidenceOperationRelationship::KnownAdvances(related) = &outcome.relationship
                {
                    advances.extend(related);
                }
            }
            reopens.extend(&outcome.reopens);
        }
        let effective_advances = advances.intersection(&active_before);
        ledger.active_obligations.remove_all(&effective_advances);
        ledger.active_obligations.extend(&reopens);

        if settled.mutation_revision > baselines.mutation_revision {
            ledger
                .active_obligations
                .remove(EvidenceObligation::ImplementationQuestion);
            ledger
                .active_obligations
                .remove(EvidenceObligation::FailureCause);
            ledger
                .active_obligations
                .insert(EvidenceObligation::FocusedValidationProof);
            ledger
                .active_obligations
                .insert(EvidenceObligation::TerminalProof);
        }

        let fresh_validation = settled.validation_revision != baselines.validation_revision
            && settled.validation_revision == Some(settled.mutation_revision)
            && settled.validation_status == ValidationFreshnessStatus::PassedAfterLastMutation;
        if fresh_validation {
            ledger
                .active_obligations
                .remove(EvidenceObligation::FocusedValidationProof);
            ledger
                .active_obligations
                .insert(EvidenceObligation::TerminalProof);
        } else if matches!(
            settled.validation_status,
            ValidationFreshnessStatus::FailedAfterLastMutation
                | ValidationFreshnessStatus::TimedOut
        ) && settled.validation_status != baselines.validation_status
            && reopens.0.is_empty()
        {
            ledger
                .active_obligations
                .insert(EvidenceObligation::FailureCause);
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

    #[test]
    fn accepted_mutation_classification_reuses_tool_and_command_safety_contracts() {
        let read_only_shell = ToolPayload::Function {
            arguments: json!({"program": "rg", "args": ["needle", "src"]}).to_string(),
        };
        assert!(!is_accepted_mutating_operation(
            TypedToolClass::Shell,
            ExternalMutationIntent::MayMutate,
            &ToolName::plain("shell_command"),
            &read_only_shell,
        ));

        let potentially_mutating_shell = ToolPayload::Function {
            arguments: json!({"program": "cargo", "args": ["check"]}).to_string(),
        };
        assert!(is_accepted_mutating_operation(
            TypedToolClass::Shell,
            ExternalMutationIntent::MayMutate,
            &ToolName::plain("shell_command"),
            &potentially_mutating_shell,
        ));
        assert!(is_accepted_mutating_operation(
            TypedToolClass::DynamicExternal,
            ExternalMutationIntent::MayMutate,
            &ToolName::namespaced("external", "write"),
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        ));
        assert!(!is_accepted_mutating_operation(
            TypedToolClass::DynamicExternal,
            ExternalMutationIntent::ProvenReadOnly,
            &ToolName::namespaced("external", "read"),
            &ToolPayload::Function {
                arguments: "{}".to_string(),
            },
        ));
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
            source_closure_identity: None,
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

    fn active_pre_edit(
        owner_candidates: &[&str],
        task_mandated_proof: bool,
    ) -> (PreEditConvergence, Arc<TurnTimingState>) {
        let timing = Arc::new(TurnTimingState::default());
        timing.mark_turn_started();
        let convergence = PreEditConvergence::new(
            PreEditConvergenceSeed {
                active: true,
                owner_candidates: owner_candidates
                    .iter()
                    .map(|candidate| (*candidate).to_string())
                    .collect(),
                instruction_snapshot_identity: InstructionSnapshotIdentity::loaded([1; 32]),
                task_mandated_proof,
            },
            Arc::clone(&timing),
        );
        (convergence, timing)
    }

    fn owner_bundle_signal(
        owner_state: &str,
        closure_state: &str,
        unresolved_ids: &[&str],
    ) -> Value {
        json!({
            "kind": "source_evidence",
            "operation": "locate_task",
            "snapshot_id": "snapshot-v1",
            "locator_request_identity": "locator-v1",
            "receipt_id": "receipt-v1",
            "task_contract_epoch": "contract-1",
            "closure_contract_revision": "source_closure_v2",
            "owner_state": owner_state,
            "closure_state": closure_state,
            "owner_id": (owner_state == "owner_resolved").then_some("core-runtime"),
            "primary_path": "src/owner.rs",
            "materialized_paths": ["src/owner.rs"],
            "unresolved_ids": unresolved_ids,
        })
    }

    fn ranged_owner_bundle(locator_request_identity: &str) -> Value {
        let mut signal = owner_bundle_signal("owner_resolved", "bundle_incomplete", &[]);
        signal["locator_request_identity"] = json!(locator_request_identity);
        signal["required_source_sections"] = json!([{
            "path": "src/caller.rs",
            "obligation_kind": "caller",
            "span": {"start_line": 1, "end_line": 100},
        }]);
        signal
    }

    fn ranged_read_signal(
        start_line: usize,
        end_line: usize,
        file_content_hash: &str,
        locator_request_identity: &str,
        reset_generation: u64,
    ) -> Value {
        json!({
            "kind": "source_evidence",
            "operation": "read_file_span",
            "path": "src/caller.rs",
            "start_line": start_line,
            "end_line": end_line,
            "total_lines": 150,
            "truncated": false,
            "full_file_sha256": file_content_hash,
            "authoritative_source_basis": {
                "path": "src/caller.rs",
                "file_content_hash": file_content_hash,
                "source_snapshot_identity": "snapshot-v1",
            },
            "obligation_identity": {
                "locator_request_identity": locator_request_identity,
                "owner_id": "core-runtime",
                "task_contract_epoch": "contract-1",
                "obligation_kind": "caller",
                "required_ranges": [{"start_line": 1, "end_line": 100}],
                "reset_generation": reset_generation,
            },
        })
    }

    fn source_payload(arguments: Value) -> ToolPayload {
        ToolPayload::Function {
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn unknown_instruction_snapshot_identity_cannot_complete_convergence() {
        let timing = Arc::new(TurnTimingState::default());
        let mut convergence = PreEditConvergence::new(
            PreEditConvergenceSeed {
                active: true,
                owner_candidates: vec!["src/owner.rs".to_string()],
                instruction_snapshot_identity: InstructionSnapshotIdentity::Unknown,
                task_mandated_proof: false,
            },
            Arc::clone(&timing),
        );
        convergence.authoritative_owner = Some("core".to_string());
        convergence.primary_surface = Some("src/owner.rs".to_string());
        convergence.mechanism_evidence = true;
        convergence.refresh_readiness();
        assert_ne!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );

        convergence.instruction_snapshot_identity = InstructionSnapshotIdentity::Empty;
        convergence.refresh_readiness();
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence
                .receipt
                .expect("owner evidence receipt")
                .instruction_snapshot_identity,
            InstructionSnapshotIdentity::Empty
        );
    }

    #[test]
    fn explicit_owner_candidate_uses_bounded_read_then_typed_fast_path() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        assert_eq!(convergence.state, PreEditConvergenceState::OwnerUnresolved);
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("read_file_span"),
                &source_payload(json!({"path": "src/owner.rs"})),
            ),
            None
        );
        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "read_file_span",
            "path": "src/owner.rs",
            "truncated": false,
        }));
        assert_eq!(convergence.state, PreEditConvergenceState::OwnerUnresolved);
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("search_source"),
                &source_payload(json!({"query": "owner", "paths": []})),
            ),
            None
        );
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("locate_task"),
                &source_payload(json!({
                    "task": "confirm owner",
                    "path_anchor": "src/owner.rs",
                })),
            ),
            None
        );

        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
        assert_eq!(
            convergence.authoritative_owner.as_deref(),
            Some("core-runtime")
        );
        assert_eq!(convergence.primary_surface.as_deref(), Some("src/owner.rs"));
    }

    #[test]
    fn unknown_owner_requires_typed_locator_resolution_and_rejects_incomplete_output() {
        let (mut convergence, _) = active_pre_edit(&[], false);
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("read_file_span"),
                &source_payload(json!({"path": "src/guess.rs"})),
            ),
            None
        );
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("locate_task"),
                &source_payload(json!({"task": "find the implementation owner"})),
            ),
            None
        );
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_unresolved",
            "bundle_incomplete",
            &["owner-ambiguous"],
        ));
        assert_eq!(convergence.state, PreEditConvergenceState::OwnerUnresolved);
        assert!(convergence.authoritative_owner.is_none());

        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
    }

    #[test]
    fn complete_focused_search_retires_matching_bundle_question() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        let mut bundle =
            owner_bundle_signal("owner_resolved", "bundle_incomplete", &["question-1"]);
        bundle["unresolved_questions"] = json!([{
            "id": "question-1",
            "category": "source_gap",
            "detail": "find the unknown caller",
        }]);
        convergence.apply_source_signal(&bundle);
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ClosureIncomplete
        );
        assert!(convergence.obligations.contains_key("bundle:question-1"));

        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "search_source",
            "match_count": 0,
            "match_paths": [],
            "truncated": false,
            "coverage_complete": true,
            "index_complete": true,
            "hydration_status": "skipped_no_unique_match",
            "hydrated_paths": [],
            "hydration_omission_count": 0,
            "hydration_truncated": false,
            "resolved_question_detail": "find the unknown caller",
        }));

        assert!(!convergence.obligations.contains_key("bundle:question-1"));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
    }

    #[test]
    fn owner_bundle_receipt_is_replaced_on_contract_epoch_or_snapshot_change() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        let first = convergence.receipt.clone().expect("first receipt");

        let mut changed_epoch = owner_bundle_signal("owner_resolved", "bundle_ready", &[]);
        changed_epoch["receipt_id"] = json!("receipt-v2");
        changed_epoch["task_contract_epoch"] = json!("contract-2");
        convergence.apply_source_signal(&changed_epoch);
        let second = convergence.receipt.clone().expect("replacement receipt");
        assert_ne!(first.receipt_id, second.receipt_id);
        assert_eq!(second.task_contract_epoch, "contract-2");

        let mut changed_snapshot = changed_epoch;
        changed_snapshot["receipt_id"] = json!("receipt-v3");
        changed_snapshot["snapshot_id"] = json!("snapshot-v2");
        convergence.apply_source_signal(&changed_snapshot);
        let third = convergence.receipt.expect("snapshot replacement receipt");
        assert_eq!(third.source_snapshot_identity, "snapshot-v2");
        assert_eq!(third.receipt_id, "receipt-v3");
    }

    #[test]
    fn compatible_adjacent_reads_accumulate_across_progress_only_receipts() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        let bundle = ranged_owner_bundle("locator-v1");
        convergence.apply_source_signal(&bundle);

        convergence.apply_source_signal(&ranged_read_signal(1, 50, "hash-a", "locator-v1", 1));
        assert!(convergence.obligations.contains_key("path:src/caller.rs"));
        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "locate_task",
            "owner_state": "owner_resolved",
            "evidence_revision": 10,
            "progress_only": true,
        }));
        assert_eq!(
            convergence.ranged_evidence["src/caller.rs"].observed_ranges,
            vec![SourceEvidenceRange {
                start_line: 1,
                end_line: 50,
            }]
        );

        let mut progress_receipt = bundle;
        progress_receipt["receipt_id"] = json!("receipt-progress-2");
        progress_receipt["evidence_revision"] = json!(11);
        convergence.apply_source_signal(&progress_receipt);
        convergence.apply_source_signal(&ranged_read_signal(51, 100, "hash-a", "locator-v1", 1));

        assert!(!convergence.obligations.contains_key("path:src/caller.rs"));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
    }

    #[test]
    fn ranged_reads_fail_closed_without_one_stable_evidence_basis() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&ranged_owner_bundle("locator-v1"));

        let mut missing_basis = ranged_read_signal(1, 50, "hash-a", "locator-v1", 1);
        missing_basis["authoritative_source_basis"] = Value::Null;
        convergence.apply_source_signal(&missing_basis);
        assert!(convergence.ranged_evidence.is_empty());

        let mut mismatched_hash = ranged_read_signal(1, 50, "hash-a", "locator-v1", 1);
        mismatched_hash["full_file_sha256"] = json!("hash-b");
        convergence.apply_source_signal(&mismatched_hash);
        assert!(convergence.ranged_evidence.is_empty());

        convergence.apply_source_signal(&ranged_read_signal(1, 50, "hash-a", "locator-v1", 1));
        convergence.apply_source_signal(&ranged_read_signal(51, 100, "hash-b", "locator-v1", 2));
        assert!(convergence.obligations.contains_key("path:src/caller.rs"));
        assert_eq!(
            convergence.ranged_evidence["src/caller.rs"].observed_ranges,
            vec![SourceEvidenceRange {
                start_line: 51,
                end_line: 100,
            }],
            "revision B must replace, never merge with, revision A coverage"
        );
    }

    #[test]
    fn stale_late_read_is_ignored_after_obligation_rebinding() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&ranged_owner_bundle("locator-v1"));
        convergence.apply_source_signal(&ranged_read_signal(1, 50, "hash-a", "locator-v1", 1));

        convergence.apply_source_signal(&ranged_owner_bundle("locator-v2"));
        assert!(convergence.ranged_evidence.is_empty());
        convergence.apply_source_signal(&ranged_read_signal(51, 100, "hash-a", "locator-v1", 1));

        assert!(convergence.ranged_evidence.is_empty());
        assert!(convergence.obligations.contains_key("path:src/caller.rs"));
    }

    #[test]
    fn implementation_affecting_closure_blocks_but_routine_validation_route_does_not() {
        let (mut ordinary, _) = active_pre_edit(&["src/owner.rs"], false);
        ordinary.apply_source_signal(&owner_bundle_signal("owner_resolved", "bundle_ready", &[]));
        assert_eq!(ordinary.validation_route, None);
        assert_eq!(ordinary.state, PreEditConvergenceState::ImplementationReady);

        let (mut closure, _) = active_pre_edit(&["src/owner.rs"], false);
        closure.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "locate_task",
            "snapshot_id": "snapshot-v1",
            "result": {
                "routing": {"status": "selected", "owner_id": "core-runtime"},
                "primary": {"path": "src/owner.rs", "resolution": "exact"},
                "source_neighborhoods": [{"path": "src/owner.rs", "role": "primary"}],
                "relationships": [{"path": "src/caller.rs", "role": "direct_caller"}],
                "contracts": [
                    {"path": "src/contract.rs", "role": "contract"},
                    {"path": "schema/generated.json", "role": "generated_mirror"}
                ],
                "validation": [{"role": "platform_schema"}],
                "tests": [],
                "instructions": [],
                "unresolved": [],
                "truncation": [],
            }
        }));
        assert_eq!(closure.state, PreEditConvergenceState::ClosureIncomplete);
        assert!(
            closure
                .obligations
                .values()
                .any(|kind| *kind == PreEditObligationKind::Caller)
        );
        assert!(
            closure
                .obligations
                .values()
                .any(|kind| *kind == PreEditObligationKind::Contract)
        );
        assert!(
            closure
                .obligations
                .values()
                .any(|kind| *kind == PreEditObligationKind::Generated)
        );
        assert!(
            closure
                .obligations
                .values()
                .any(|kind| *kind == PreEditObligationKind::Platform)
        );

        let (mut task_proof, _) = active_pre_edit(&["src/owner.rs"], true);
        task_proof.apply_source_signal(&owner_bundle_signal("owner_resolved", "bundle_ready", &[]));
        assert_eq!(task_proof.state, PreEditConvergenceState::ClosureIncomplete);
        task_proof.apply_validation_result(PreEditValidationClass::OtherKnownValidation, true);
        assert_eq!(
            task_proof.state,
            PreEditConvergenceState::ImplementationReady
        );
    }

    #[test]
    fn pre_edit_validation_allows_evidence_suppresses_only_stale_final_ceremony_and_reopens_after_mutation()
     {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence
                .validation_suppression(PreEditValidationClass::FocusedImplementationEvidence),
            None
        );
        assert_eq!(
            convergence.validation_suppression(PreEditValidationClass::OtherKnownValidation),
            None
        );
        assert_eq!(
            convergence.validation_suppression(PreEditValidationClass::KnownFinalCeremony),
            None,
            "mutation-oriented readiness without a durable unfinished mutation obligation must fail open"
        );
        convergence.apply_plan_signal(&json!({
            "kind": "plan_update",
            "unfinished_mutation_obligation": true,
        }));
        let suppression = convergence
            .validation_suppression(PreEditValidationClass::KnownFinalCeremony)
            .expect("a proven pending mutation defers final ceremony");
        let result = serde_json::to_value(suppression.deferred_validation_result())
            .expect("typed suppression result serializes");
        assert_eq!(result["disposition"], "deferred");
        assert_eq!(result["outcome"], "not_executed");
        assert!(result.get("success").is_none());
        assert!(result.get("exit_code").is_none());
        let proven_revision = convergence.unfinished_mutation_obligation_revision;
        convergence.apply_plan_signal(&json!({
            "kind": "plan_update",
            "unfinished_mutation_obligation": null,
        }));
        assert_eq!(
            convergence.validation_suppression(PreEditValidationClass::KnownFinalCeremony),
            None,
            "an uncertain status-only or no-op update must fail open"
        );
        assert!(convergence.unfinished_mutation_obligation_revision > proven_revision);
        convergence.apply_plan_signal(&json!({
            "kind": "plan_update",
            "unfinished_mutation_obligation": false,
        }));
        assert_eq!(
            convergence.validation_suppression(PreEditValidationClass::KnownFinalCeremony),
            None,
            "a no-edit or terminal plan conclusion must restore normal validation"
        );
        convergence.apply_plan_signal(&json!({
            "kind": "plan_update",
            "unfinished_mutation_obligation": true,
        }));
        convergence.record_mutation_advance(false);
        assert_eq!(
            convergence.validation_suppression(PreEditValidationClass::KnownFinalCeremony),
            None
        );
    }

    #[test]
    fn deterministic_registration_observes_source_and_enforces_validation_suppression() {
        let timing = Arc::new(TurnTimingState::default());
        let governor = SamplingReasoningGovernor::new_with_pre_edit(
            Some(&config()),
            PreEditConvergenceSeed {
                active: true,
                owner_candidates: vec!["src/owner.rs".to_string()],
                instruction_snapshot_identity: InstructionSnapshotIdentity::loaded([1; 32]),
                task_mandated_proof: false,
            },
            Arc::clone(&timing),
        );
        {
            let mut ledger = governor
                .dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ledger.pre_edit.apply_source_signal(&owner_bundle_signal(
                "owner_resolved",
                "bundle_ready",
                &[],
            ));
        }
        let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
        let source_collector = governor.collector(&baselines);
        let source_registration = source_collector.register_deterministic_tool_call(
            &ToolName::plain("search_source"),
            &source_payload(json!({"query": "redundant", "paths": []})),
            "search-suppressed",
        );
        assert!(source_registration.suppression.is_none());
        assert!(source_registration.source_advisory.is_some());
        assert_eq!(
            timing
                .complete_snapshot()
                .protocol_timing()
                .pre_edit_convergence
                .expect("active convergence timing")
                .broad_discovery_after_ready,
            0,
            "classification is not a physical-operation observation"
        );
        source_collector.record_source_advisory_physical_operation();
        assert_eq!(
            timing
                .complete_snapshot()
                .protocol_timing()
                .pre_edit_convergence
                .expect("active convergence timing")
                .broad_discovery_after_ready,
            1
        );

        let validation_payload = source_payload(json!({"command": "cargo test --workspace"}));
        let no_plan_registration = governor
            .collector(&baselines)
            .register_deterministic_tool_call(
                &ToolName::plain("shell_command"),
                &validation_payload,
                "validation-fail-open",
            );
        assert!(no_plan_registration.suppression.is_none());

        governor
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pre_edit
            .apply_plan_signal(&json!({
                "kind": "plan_update",
                "unfinished_mutation_obligation": true,
            }));
        let validation_registration = governor
            .collector(&baselines)
            .register_deterministic_tool_call(
                &ToolName::plain("shell_command"),
                &validation_payload,
                "validation-suppressed",
            );
        assert_eq!(
            validation_registration
                .suppression
                .as_ref()
                .map(|suppression| suppression.kind),
            Some(PreDispatchSuppressionKind::StaleFinalValidation)
        );
    }

    #[test]
    fn post_ready_targeted_reads_are_allowed_broad_rediscovery_is_advisory_and_ambiguity_fails_open()
     {
        let (mut convergence, timing) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("read_file_span"),
                &source_payload(json!({"path": "src/owner.rs"})),
            ),
            None
        );
        assert!(
            convergence
                .source_advisory(
                    &ToolName::plain("search_source"),
                    &source_payload(json!({"query": "anything", "paths": []})),
                )
                .is_some()
        );
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("search_source"),
                &ToolPayload::Function {
                    arguments: "not-json".to_string(),
                },
            ),
            None
        );
        let recorded = timing
            .complete_snapshot()
            .protocol_timing()
            .pre_edit_convergence
            .expect("active timing");
        assert_eq!(
            recorded.broad_discovery_after_ready, 0,
            "classification alone must not report a physical operation"
        );
    }

    #[test]
    fn hydration_obligations_prevent_known_no_advance_until_exact_reads_clear_them() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        convergence
            .obligations
            .insert("path:src/lib.rs".to_string(), PreEditObligationKind::Caller);
        convergence.obligations.insert(
            "path:src/other.rs".to_string(),
            PreEditObligationKind::Caller,
        );
        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "search_source",
            "match_count": 2,
            "match_paths": ["src/lib.rs", "src/other.rs"],
            "truncated": false,
            "coverage_complete": true,
            "hydration_status": "partially_hydrated_bounded_packet",
            "hydrated_paths": ["src/lib.rs"],
            "hydration_omission_count": 1,
            "hydration_truncated": false,
        }));

        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ClosureIncomplete
        );
        assert!(convergence.closure.contains("src/lib.rs"));
        assert!(!convergence.closure.contains("src/other.rs"));
        assert!(!convergence.obligations.contains_key("path:src/lib.rs"));
        assert!(convergence.obligations.contains_key("path:src/other.rs"));
        assert!(
            convergence
                .obligations
                .contains_key("hydration:src/other.rs")
        );
        assert!(convergence.obligations.contains_key("truncation:search"));
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("search_source"),
                &source_payload(json!({"query": "anything", "paths": []})),
            ),
            None,
            "complete coverage alone must not establish KnownNoAdvance"
        );

        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ClosureIncomplete,
            "owner resolution must not erase outstanding hydration obligations"
        );
        assert!(
            convergence
                .obligations
                .contains_key("hydration:src/other.rs")
        );

        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "read_file_span",
            "path": "src/other.rs",
            "truncated": false,
        }));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ClosureIncomplete
        );
        assert!(
            !convergence
                .obligations
                .contains_key("hydration:src/other.rs")
        );
        assert!(
            !convergence
                .obligations
                .contains_key("hydration-status:search")
        );
        assert!(
            convergence.obligations.contains_key("truncation:search"),
            "incomplete hydration must retain the search truncation obligation"
        );
    }

    #[test]
    fn zero_match_complete_search_creates_no_hydration_obligation() {
        let (mut convergence, _) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        convergence.apply_source_signal(&json!({
            "kind": "source_evidence",
            "operation": "search_source",
            "match_count": 0,
            "match_paths": [],
            "truncated": false,
            "coverage_complete": true,
            "hydration_status": "skipped_no_unique_match",
            "hydrated_paths": [],
            "hydration_omission_count": 0,
            "hydration_truncated": false,
        }));
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationReady
        );
        assert!(
            !convergence
                .obligations
                .keys()
                .any(|obligation| obligation.starts_with("hydration"))
        );
    }

    #[test]
    fn readiness_handoff_uses_next_sampling_boundary_without_a_generation() {
        let timing = Arc::new(TurnTimingState::default());
        timing.mark_turn_started();
        let mut governor = SamplingReasoningGovernor::new_with_pre_edit(
            Some(&config()),
            PreEditConvergenceSeed {
                active: true,
                owner_candidates: vec!["src/owner.rs".to_string()],
                instruction_snapshot_identity: InstructionSnapshotIdentity::loaded([1; 32]),
                task_mandated_proof: false,
            },
            Arc::clone(&timing),
        );
        governor
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pre_edit
            .apply_source_signal(&owner_bundle_signal("owner_resolved", "bundle_ready", &[]));
        let generations_before = timing
            .complete_snapshot()
            .protocol_timing()
            .counters
            .logical_generation_count;
        let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
        let collector = governor.collector(&baselines);
        let decision = governor.evaluate_convergence(
            &baselines,
            &collector,
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert!(decision.readiness_handoff);
        assert!(decision.directive.is_some());
        assert_eq!(
            timing
                .complete_snapshot()
                .protocol_timing()
                .counters
                .logical_generation_count,
            generations_before
        );
        assert_eq!(
            governor
                .dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pre_edit
                .take_readiness_handoff(),
            None
        );
    }

    #[test]
    fn partial_failed_mutation_reopens_without_milestone_then_success_records_once() {
        let (mut convergence, timing) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        convergence.record_mutation_advance(false);
        assert!(!convergence.workspace_untouched);
        assert!(!convergence.first_successful_mutation);
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ClosureIncomplete
        );
        assert_eq!(
            convergence.source_advisory(
                &ToolName::plain("search_source"),
                &source_payload(json!({"query": "refresh", "paths": []})),
            ),
            None
        );
        assert!(
            timing
                .complete_snapshot()
                .protocol_timing()
                .pre_edit_convergence
                .expect("active timing")
                .first_successful_mutation_ms
                .is_none()
        );

        convergence.record_mutation_advance(true);
        let first = timing
            .complete_snapshot()
            .protocol_timing()
            .pre_edit_convergence
            .expect("active timing")
            .first_successful_mutation_ms;
        convergence.record_mutation_advance(true);
        let second = timing
            .complete_snapshot()
            .protocol_timing()
            .pre_edit_convergence
            .expect("active timing")
            .first_successful_mutation_ms;
        assert_eq!(
            convergence.state,
            PreEditConvergenceState::ImplementationStarted
        );
        assert!(first.is_some());
        assert_eq!(second, first);
    }

    #[test]
    fn every_supported_readiness_reopen_reason_is_counted() {
        let (mut convergence, timing) = active_pre_edit(&["src/owner.rs"], false);
        convergence.apply_source_signal(&owner_bundle_signal(
            "owner_resolved",
            "bundle_ready",
            &[],
        ));
        for reason in [
            PreEditReopenReason::SourceRevision,
            PreEditReopenReason::ContradictoryEvidence,
            PreEditReopenReason::IncompleteEvidence,
            PreEditReopenReason::NewAmbiguity,
            PreEditReopenReason::UserSteering,
        ] {
            convergence.state = PreEditConvergenceState::ImplementationReady;
            convergence.reopen(reason);
        }
        let recorded = timing
            .complete_snapshot()
            .protocol_timing()
            .pre_edit_convergence
            .expect("active timing");
        assert_eq!(recorded.readiness_reopen_count, 5);
        assert_eq!(recorded.reopen_reason_counts.source_revision, 1);
        assert_eq!(recorded.reopen_reason_counts.contradictory_evidence, 1);
        assert_eq!(recorded.reopen_reason_counts.incomplete_evidence, 1);
        assert_eq!(recorded.reopen_reason_counts.new_ambiguity, 1);
        assert_eq!(recorded.reopen_reason_counts.user_steering, 1);
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
    fn residual_deterministic_sampling_requires_unchanged_input_free_tool_free_state() {
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

    #[test]
    fn unanchored_source_relationships_and_structured_uncertainty_fail_open() {
        let timing = Arc::new(TurnTimingState::default());
        let mut convergence = PreEditConvergence::new(
            PreEditConvergenceSeed {
                active: true,
                owner_candidates: vec!["src/owner.rs".to_string()],
                instruction_snapshot_identity: InstructionSnapshotIdentity::loaded([2; 32]),
                task_mandated_proof: false,
            },
            timing,
        );

        let unanchored_search = ToolPayload::Function {
            arguments: json!({"query": "Owner"}).to_string(),
        };
        assert_eq!(
            convergence.source_advisory(&ToolName::plain("search_source"), &unanchored_search,),
            None,
            "an unanchored search has an unknown relationship and must execute",
        );

        convergence.authoritative_owner = Some("core".to_string());
        convergence.primary_surface = Some("src/owner.rs".to_string());
        convergence.mechanism_evidence = true;
        convergence.closure.insert("src/owner.rs".to_string());
        convergence.state = PreEditConvergenceState::ImplementationReady;

        let unanchored_locator = ToolPayload::Function {
            arguments: json!({"task": "verify the current owner"}).to_string(),
        };
        assert_eq!(
            convergence.source_advisory(&ToolName::plain("locate_task"), &unanchored_locator,),
            None,
            "an unanchored locator relationship remains unknown after closure",
        );

        let structured_uncertainty = ToolPayload::Function {
            arguments: json!({
                "task": "verify the current owner",
                "path_anchor": "outside/current/closure.rs",
                "source_question": {
                    "kind": "ambiguous_ownership",
                    "detail": "the recorded owner contradicts a new exact contract"
                },
            })
            .to_string(),
        };
        assert_eq!(
            convergence.source_advisory(&ToolName::plain("locate_task"), &structured_uncertainty,),
            None,
            "structured uncertainty must reach the locator and reopen closure",
        );

        let truncation_recovery = ToolPayload::Function {
            arguments: json!({
                "query": "Owner",
                "paths": [],
                "source_question": {
                    "kind": "incomplete_prior_result",
                    "detail": "the prior search result was truncated"
                },
            })
            .to_string(),
        };
        assert_eq!(
            convergence.source_advisory(&ToolName::plain("search_source"), &truncation_recovery,),
            None,
            "structured ambiguity or truncation recovery must remain ordinary discovery",
        );

        let proven_irrelevant_read = ToolPayload::Function {
            arguments: json!({"path": "outside/current/closure.rs"}).to_string(),
        };
        assert!(
            convergence
                .source_advisory(&ToolName::plain("read_file_span"), &proven_irrelevant_read,)
                .is_some(),
            "an exact unsupported path can still be declined as KnownNoAdvance",
        );
    }

    #[test]
    fn active_obligation_batch_applies_union_while_guidance_stays_singular() {
        let timing = Arc::new(TurnTimingState::default());
        let mut governor = SamplingReasoningGovernor::new_with_pre_edit(
            Some(&config()),
            PreEditConvergenceSeed {
                active: true,
                ..Default::default()
            },
            timing,
        );
        assert_eq!(
            governor.model_evidence_guidance().as_deref(),
            Some("next_evidence_obligation: owner\nblocked_by: none")
        );

        let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
        let batch = governor.collector(&baselines);
        for (ordinal, obligations) in [
            (0, json!(["governing_instructions"])),
            (1, json!(["caller_or_contract_closure"])),
            (2, json!(["focused_validation_route"])),
        ] {
            batch.push(SamplingToolOutcome::from_signal(
                ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None,
                Some(&json!({
                    "relationship": {
                        "kind": "known_advances",
                        "obligations": obligations,
                    }
                })),
            ));
        }
        governor.settle(
            &baselines,
            &batch,
            &settled(0, ValidationFreshnessStatus::None, None),
        );
        assert_eq!(
            governor.model_evidence_guidance().as_deref(),
            Some("next_evidence_obligation: owner\nblocked_by: none")
        );
        let active = &governor
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_obligations;
        assert_eq!(active.0, BTreeSet::from([EvidenceObligation::Owner]));
    }

    #[test]
    fn structured_validation_failures_reopen_only_supported_obligations() {
        let cases = [
            (
                "caller_or_contract",
                BTreeSet::from([
                    EvidenceObligation::CallerOrContractClosure,
                    EvidenceObligation::ImplementationQuestion,
                ]),
            ),
            (
                "compile_or_type",
                BTreeSet::from([EvidenceObligation::ImplementationQuestion]),
            ),
            (
                "platform",
                BTreeSet::from([
                    EvidenceObligation::ImplementationQuestion,
                    EvidenceObligation::FailureCause,
                ]),
            ),
            (
                "owner_contradiction",
                BTreeSet::from([EvidenceObligation::Owner]),
            ),
            (
                "unclassified",
                BTreeSet::from([EvidenceObligation::FailureCause]),
            ),
        ];
        for (class, expected) in cases {
            assert_eq!(
                validation_failure_obligation_delta(Some(&json!({
                    "validation_failure_class": class,
                    "stderr": "text that must not affect classification",
                })))
                .0,
                expected,
                "class {class}"
            );
        }
        assert_eq!(
            validation_failure_obligation_delta(Some(&json!({
                "stderr": "owner contradiction compile failure platform"
            }))),
            EvidenceObligationSet::default()
        );
    }

    fn unchanged_state(
        governor: &SamplingReasoningGovernor,
    ) -> (SamplingRequestBaselines, SamplingRequestSettledState) {
        let baselines = governor.baselines(7, ValidationFreshnessStatus::None, None);
        let settled = SamplingRequestSettledState {
            mutation_revision: 7,
            validation_status: ValidationFreshnessStatus::None,
            validation_revision: None,
            source_closure_identity: None,
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
            state.saw_source_evidence = true;
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
            state.saw_source_evidence = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(validation_and_later, false, true),
            Some(TurnTimingGenerationPurpose::ValidationInterpretation)
        );

        fn coordination_and_later(state: &mut SamplingRequestSignalState) {
            state.saw_coordination = true;
            state.saw_source_evidence = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(coordination_and_later, false, true),
            Some(TurnTimingGenerationPurpose::Coordination)
        );

        fn source_and_wait(state: &mut SamplingRequestSignalState) {
            state.saw_source_evidence = true;
            state.registered_count = 1;
            state.wait_call_count = 1;
        }
        assert_eq!(
            classify(source_and_wait, false, true),
            Some(TurnTimingGenerationPurpose::SourceEvidenceInterpretation)
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

    fn deterministic_read(
        collector: &SamplingRequestSignalCollector,
        arguments: &str,
    ) -> SamplingToolCallRegistration {
        collector.register_deterministic_tool_call(
            &ToolName::plain("read_file_span"),
            &ToolPayload::Function {
                arguments: arguments.to_string(),
            },
            "read-current",
        )
    }

    fn record_read_result(
        collector: &SamplingRequestSignalCollector,
        registration: &SamplingToolCallRegistration,
    ) {
        collector.record_response_result(
            registration.ordinal,
            true,
            None,
            &ResponseInputItem::FunctionCallOutput {
                call_id: "read-1".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    "same structured span".to_string(),
                ),
            },
        );
    }

    #[test]
    fn source_specific_semantics_keep_generic_identity_diagnostic_only() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first = governor.collector(&baselines);
        let registration = deterministic_read(&first, r#"{"path":"src/lib.rs","start_line":1}"#);
        record_read_result(&first, &registration);

        let repeated = governor.collector(&baselines);
        let _registration =
            deterministic_read(&repeated, r#"{"start_line":1,"path":"src/lib.rs"}"#);
        assert!(repeated.deterministic_cycle_key().is_some());

        let changed_revision = governor.baselines(8, ValidationFreshnessStatus::None, None);
        let changed = governor.collector(&changed_revision);
        let _registration = deterministic_read(&changed, r#"{"path":"src/lib.rs","start_line":1}"#);
        assert!(changed.deterministic_cycle_key().is_none());
    }

    #[test]
    fn exact_duplicate_with_complete_artifact_returns_not_modified_receipt() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first = governor.collector(&baselines);
        let first_registration =
            deterministic_read(&first, r#"{"path":"src/lib.rs","start_line":1}"#);
        first.record_response_result(
            first_registration.ordinal,
            true,
            None,
            &ResponseInputItem::FunctionCallOutput {
                call_id: "read-1".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    json!({
                        "version": 1,
                        "canonical_complete": true,
                        "canonical_bytes": 10,
                        "canonical_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "artifact_id": "01900000-0000-7000-8000-000000000000",
                        "outcome": "success",
                        "result": { "artifact": { "complete": true, "retained_bytes": 10 } }
                    })
                    .to_string(),
                ),
            },
        );

        let repeated = governor.collector(&baselines);
        let registration = repeated.register_deterministic_tool_call(
            &ToolName::plain("read_file_span"),
            &ToolPayload::Function {
                arguments: r#"{"start_line":1,"path":"src/lib.rs"}"#.to_string(),
            },
            "read-2",
        );

        assert!(repeated.deterministic_cycle_key().is_some());
        let ResponseInputItem::FunctionCallOutput { call_id, output } = registration
            .replay_response
            .expect("duplicate should replay")
        else {
            panic!("duplicate replay should be a function output");
        };
        assert_eq!(call_id, "read-2");
        let codex_protocol::models::FunctionCallOutputBody::Text(text) = output.body else {
            panic!("duplicate replay should be text JSON");
        };
        let replay: Value = serde_json::from_str(&text).expect("valid replay JSON");
        assert_eq!(replay["status"], "not_modified");
        assert_eq!(replay["original_call_id"], "read-1");
        assert_eq!(
            replay["artifact_id"],
            "01900000-0000-7000-8000-000000000000"
        );
        assert_eq!(replay["canonical_bytes"], 10);
    }

    #[test]
    fn failed_artifact_is_not_inserted_for_replay() {
        let governor = SamplingReasoningGovernor::new(None);
        let (baselines, _) = unchanged_state(&governor);
        let first = governor.collector(&baselines);
        let registration = deterministic_read(&first, r#"{"path":"src/lib.rs"}"#);
        first.record_response_result(
            registration.ordinal,
            false,
            None,
            &ResponseInputItem::FunctionCallOutput {
                call_id: "read-failed".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    json!({
                        "version": 1,
                        "canonical_complete": true,
                        "canonical_bytes": 10,
                        "canonical_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "artifact_id": "01900000-0000-7000-8000-000000000001",
                        "outcome": "failure"
                    })
                    .to_string(),
                ),
            },
        );

        let repeated = governor.collector(&baselines);
        let registration = deterministic_read(&repeated, r#"{"path":"src/lib.rs"}"#);
        assert!(registration.replay_response.is_none());
    }

    #[test]
    fn only_explicit_success_with_complete_projection_is_singleflight_shareable() {
        let mut output = codex_protocol::models::FunctionCallOutputPayload::from_text(
            json!({
                "version": 1,
                "canonical_complete": true,
                "canonical_bytes": 10,
                "canonical_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "artifact_id": "01900000-0000-7000-8000-000000000002",
                "outcome": "success"
            })
            .to_string(),
        );
        output.success = Some(true);
        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "shareable-read".to_string(),
            output,
        };
        assert!(same_generation_response_is_safely_shareable(
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            &response,
        ));
        for context in [
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            ToolOutputOutcomeContext::new(ToolOutputOutcome::TimedOut),
            ToolOutputOutcomeContext::skipped(Some(ToolOutputSkipDisposition::Deferred)),
        ] {
            assert!(!same_generation_response_is_safely_shareable(
                context, &response,
            ));
        }

        let mut malformed_output =
            codex_protocol::models::FunctionCallOutputPayload::from_text("not-json".to_string());
        malformed_output.success = Some(true);
        assert!(!same_generation_response_is_safely_shareable(
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            &ResponseInputItem::FunctionCallOutput {
                call_id: "malformed-read".to_string(),
                output: malformed_output,
            },
        ));
    }

    #[test]
    fn source_locator_reuse_is_deferred_to_source_preflight() {
        let governor = SamplingReasoningGovernor::new(None);
        let first_baseline = governor.baselines(7, ValidationFreshnessStatus::None, None);
        let first = governor.collector(&first_baseline);
        let first_registration = first.register_deterministic_tool_call(
            &ToolName::plain("locate_task"),
            &ToolPayload::Function {
                arguments: r#"{"task":"find owner","path_anchor":"src/lib.rs"}"#.to_string(),
            },
            "locate-1",
        );
        first.record_response_result(
            first_registration.ordinal,
            true,
            Some(json!({
                "kind": "source_evidence",
                "operation": "locate_task",
                "locator_reusable": true,
                "source_dependency_identity": "snapshot-a",
                "relationship": {
                    "kind": "known_advances",
                    "obligations": ["owner"]
                },
                "advances": ["owner"]
            })),
            &ResponseInputItem::FunctionCallOutput {
                call_id: "locate-1".to_string(),
                output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                    r#"{"process_id":"stable-17","owner":"core"}"#.to_string(),
                ),
            },
        );

        let matching_baseline = governor.baselines_with_source_identity(
            7,
            ValidationFreshnessStatus::None,
            None,
            Some("snapshot-a".to_string()),
        );
        let matching = governor.collector(&matching_baseline);
        let _matching_registration = matching.register_deterministic_tool_call(
            &ToolName::plain("locate_task"),
            &ToolPayload::Function {
                arguments: r#"{"path_anchor":"src/lib.rs","task":"find owner"}"#.to_string(),
            },
            "locate-2",
        );

        let changed_question = governor.collector(&matching_baseline);
        let _changed_question_registration = changed_question.register_deterministic_tool_call(
            &ToolName::plain("locate_task"),
            &ToolPayload::Function {
                arguments: r#"{"task":"find validation owner","path_anchor":"src/lib.rs"}"#
                    .to_string(),
            },
            "locate-3",
        );

        let changed_snapshot = governor.baselines_with_source_identity(
            7,
            ValidationFreshnessStatus::None,
            None,
            Some("snapshot-b".to_string()),
        );
        let changed_snapshot = governor.collector(&changed_snapshot);
        let _changed_snapshot_registration = changed_snapshot.register_deterministic_tool_call(
            &ToolName::plain("locate_task"),
            &ToolPayload::Function {
                arguments: r#"{"task":"find owner","path_anchor":"src/lib.rs"}"#.to_string(),
            },
            "locate-4",
        );
    }

    #[test]
    fn artifact_recovery_is_never_deterministically_suppressed() {
        assert!(!supports_deterministic_identity(&ToolName::plain(
            "read_tool_output",
        )));
        assert!(deterministic_dispatch_identity(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function {
                arguments: r#"{"artifact_id":"01900000-0000-7000-8000-000000000000","selectors":[{"kind":"bytes","start":0,"end":4}]}"#
                    .to_string(),
            },
            Some("revision"),
        )
        .is_none());
    }

    #[test]
    fn force_fresh_bypasses_deterministic_suppression_identity() {
        assert!(
            deterministic_dispatch_identity(
                &ToolName::plain("read_file_span"),
                &ToolPayload::Function {
                    arguments: r#"{"path":"src/lib.rs","force_fresh":true}"#.to_string(),
                },
                Some("revision"),
            )
            .is_none()
        );
    }

    #[test]
    fn incomplete_read_identity_is_never_singleflight_eligible() {
        assert!(
            deterministic_dispatch_identity(
                &ToolName::plain("read_file_span"),
                &ToolPayload::Function {
                    arguments: r#"{"start_line":1}"#.to_string(),
                },
                Some("revision"),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn unprojectable_duplicate_read_fails_open_instead_of_sharing() {
        let governor = SamplingReasoningGovernor::new(None);
        let baselines = governor.baselines(0, ValidationFreshnessStatus::None, None);
        let collector = governor.collector(&baselines);
        let payload = ToolPayload::Function {
            arguments: r#"{"path":"src/lib.rs","start_line":1}"#.to_string(),
        };
        let leader = collector.register_deterministic_tool_call(
            &ToolName::plain("read_file_span"),
            &payload,
            "read-leader",
        );
        let mut follower = collector.register_deterministic_tool_call(
            &ToolName::plain("read_file_span"),
            &payload,
            "read-follower",
        );
        assert!(collector.deterministic_cycle_key().is_none());
        assert!(matches!(
            leader.same_generation_singleflight.as_ref(),
            Some(SameGenerationSingleflightRegistration::Leader(_))
        ));
        assert!(matches!(
            follower.same_generation_singleflight.as_ref(),
            Some(SameGenerationSingleflightRegistration::Follower(_))
        ));

        let response = ResponseInputItem::FunctionCallOutput {
            call_id: "read-leader".to_string(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "canonical source bytes".to_string(),
            ),
        };
        collector.record_response_result(leader.ordinal, true, None, &response);
        let Some(SameGenerationSingleflightRegistration::Leader(leader_flight)) =
            leader.same_generation_singleflight.as_ref()
        else {
            panic!("first duplicate should own execution");
        };
        collector.complete_same_generation_leader(
            leader_flight,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            &response,
        );
        let Some(SameGenerationSingleflightRegistration::Follower(follower_flight)) =
            follower.same_generation_singleflight.as_mut()
        else {
            panic!("second duplicate should join");
        };
        assert!(
            follower_flight
                .wait_for_shareable(&tokio_util::sync::CancellationToken::new())
                .await
                .is_none(),
            "a successful body without the complete projection artifact contract must execute independently"
        );
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

    #[test]
    fn only_repeated_deterministic_cycle_after_directive_activates_loop_guard() {
        let mut governor = SamplingReasoningGovernor::new(None);
        let (baselines, settled) = unchanged_state(&governor);

        let first = governor.collector(&baselines);
        let registration = deterministic_read(&first, r#"{"path":"src/lib.rs"}"#);
        record_read_result(&first, &registration);
        assert_eq!(
            governor.evaluate_convergence(&baselines, &first, &settled),
            SamplingConvergenceDecision::default()
        );

        for repeated_generation in 1..=4 {
            let collector = governor.collector(&baselines);
            let _registration = deterministic_read(&collector, r#"{"path":"src/lib.rs"}"#);
            let decision = governor.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), repeated_generation >= 3);
            assert_eq!(decision.proven_loop_activated, repeated_generation == 4);
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
