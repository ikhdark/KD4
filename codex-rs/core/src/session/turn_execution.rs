use std::borrow::Cow;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_config::schema::canonicalize as canonicalize_json;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_protocol::protocol::TurnTimingGenerationDisposition;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingProgressKind;
use codex_protocol::validation::ValidationMode;
use codex_tools::ToolName;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolOutputSkipDisposition;
use codex_tools::ToolPayload;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::tool_history::SourceDependencyV1;
use crate::tool_history::source_locator_key;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::turn_timing::TurnTimingState;
use crate::validation::ValidationClassification;
use crate::validation::ValidationOperation;
use crate::validation::classify_validation;

const TURN_EFFICIENCY_TOOL_CALL_THRESHOLD: usize = 8;
const TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL: u64 = 500;

/// Most paths one completion message names; the rest are counted.
const MAX_LISTED_COMPLETION_PATHS: usize = 10;

/// Joins at most ten paths and counts the rest, so a large change set cannot
/// flood a completion message.
pub(crate) fn bounded_path_list(paths: &[String]) -> String {
    let listed = paths
        .iter()
        .take(MAX_LISTED_COMPLETION_PATHS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    match paths.len().saturating_sub(MAX_LISTED_COMPLETION_PATHS) {
        0 => listed,
        more => format!("{listed}, and {more} more"),
    }
}



const RECENT_CYCLE_LIMIT: usize = 32;


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
    attributed_mutation_revision: u64,
    unproven_command_revision: u64,
    plan_revision: u64,
    input_revision: u64,
    instruction_revision: u64,
    tool_exposure_revision: u64,
    evidence_fingerprint: String,
}

impl SamplingRequestBaselines {
    pub(crate) fn set_attributed_mutation_revision(&mut self, revision: u64) {
        self.attributed_mutation_revision = revision;
    }

    pub(crate) fn set_unproven_command_revision(&mut self, revision: u64) {
        self.unproven_command_revision = revision;
    }

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
        // Authority revisions govern admission; observed evidence governs the
        // next decision too. Do not confuse this semantic identity with an
        // exact provider-request digest.
        let state = format!(
            "v2;{};evidence={}",
            self.revision_key(),
            self.evidence_fingerprint,
        );
        hex::encode(Sha256::digest(state.as_bytes()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SamplingRequestSettledState {
    pub(crate) mutation_revision: u64,
    pub(crate) attributed_mutation_revision: u64,
    pub(crate) unproven_command_revision: u64,
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
    source_artifact_id: Option<String>,
    failure_fingerprint: Option<String>,
    failure_is_terminal: bool,
    failure_diagnosis_reused: bool,
    canonical_artifact_required: bool,
    nested_in_code_mode: bool,
    code_mode_cell_id: Option<String>,
    wraps_nested_terminal: bool,
    tests_executed: bool,
    compile_validation: bool,
    validation_failure_excerpt: Option<String>,
    process_observation_progress: bool,
    validation_mutation_revision: Option<u64>,
    background_process_id: Option<u64>,
    observed_process_id: Option<u64>,
}

#[derive(Clone, Debug)]
struct PendingValidation {
    execution_order: (u64, u64),
    check: String,
}

/// Preserve the process identity from an actual poll, without coverage claims.
pub(crate) fn validation_scope_signal(
    tool_name: &ToolName,
    payload: &ToolPayload,
    signal: Option<Value>,
    _dependencies: Option<&BTreeSet<SourceDependencyV1>>,
    _default_cwd: &std::path::Path,
    _environment_id: &str,
) -> Option<Value> {
    if !tool_name_matches(tool_name, "write_stdin") {
        return signal;
    }
    let arguments = canonical_tool_action(payload).value;
    let mut signal = signal.unwrap_or_else(|| serde_json::json!({}));
    if let Some(process_id) = arguments.get("session_id").and_then(Value::as_u64) {
        signal["observed_process_id"] = serde_json::json!(process_id);
    }
    Some(signal)
}

// Locators preserve case; compare with the platform's case-folding key.
fn source_scope_contains(scope: &SourceDependencyV1, path: &str) -> bool {
    let scope_key = source_locator_key(&scope.path);
    let scope_path: &str = &scope_key;
    let path_key = source_locator_key(path);
    let path: &str = &path_key;
    path == scope_path
        || scope.recursive
            && path
                .strip_prefix(scope_path)
                .is_some_and(|suffix| scope_path.ends_with('/') || suffix.starts_with('/'))
}

fn documentation_only_path(path: &std::path::Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let components = normalized.split('/').collect::<Vec<_>>();
    // This repository's prompt crate embeds templates into runtime requests.
    // Instruction files also control the harness; neither is a README.
    let embedded_runtime_input = [
        "codex-rs/ext/web-search/web_run_description.md",
        "codex-rs/ext/image-generation/imagegen_description.md",
    ]
    .iter()
    .any(|input| normalized == *input || normalized.ends_with(&format!("/{input}")));
    let runtime_input = embedded_runtime_input
        || components
            .iter()
            .any(|component| matches!(*component, "prompts" | "templates"))
        || components.last().is_some_and(|name| {
            ["AGENTS.md", "AGENTS.override.md", "SKILL.md"]
                .iter()
                .any(|instruction| name.eq_ignore_ascii_case(instruction))
        });
    !runtime_input
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(extension.to_ascii_lowercase().as_str(), "md" | "rst")
            })
}

impl SamplingToolOutcome {
    fn from_signal(
        ordinal: u64,
        outcome_context: ToolOutputOutcomeContext,
        plan: Option<UpdatePlanArgs>,
        signal: Option<&Value>,
    ) -> Self {
        let refused = signal.is_some_and(|signal| signal["validation_policy_refusal"] == true);
        let signal = signal.filter(|_| !refused);
        let kind = if refused {
            SamplingToolOutcomeKind::Skipped
        } else {
            sampling_tool_outcome_kind(outcome_context.outcome, signal)
        };
        Self {
            ordinal,
            kind,
            skip_disposition: outcome_context.skip_disposition,
            plan,
            source_evidence: sampling_source_evidence(signal),
            source_artifact_id: signal
                .and_then(|signal| signal["source_artifact_id"].as_str())
                .map(str::to_string),
            failure_fingerprint: sampling_failure_fingerprint(signal),
            failure_is_terminal: sampling_failure_is_terminal(signal),
            failure_diagnosis_reused: false,
            canonical_artifact_required: false,
            nested_in_code_mode: false,
            code_mode_cell_id: None,
            wraps_nested_terminal: signal
                .and_then(|signal| signal.get("nested_ordinal"))
                .and_then(Value::as_u64)
                .is_some(),
            tests_executed: signal.is_some_and(|signal| signal["command_validation"]["tests"] == true),
            compile_validation: signal
                .is_some_and(|signal| signal["command_validation"]["compile"] == true),
            validation_failure_excerpt: None,
            process_observation_progress: signal
                .is_some_and(|signal| signal["process_observation_progress"] == true),
            validation_mutation_revision: signal
                .and_then(|signal| signal["validation_mutation_revision"].as_u64()),
            background_process_id: signal
                .and_then(|signal| signal["background_process_id"].as_u64()),
            observed_process_id: signal.and_then(|signal| signal["observed_process_id"].as_u64()),
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
struct RepeatedFailureGate {
    state_revision: String,
    action_identity: String,
    failure_fingerprint: String,
}



#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuredActionClass {
    BroadSource,
    PreciseSource,
    InvalidArguments,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StructuredActionIdentity {
    identity: String,
    evidence_identity: String,
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
    failure_fingerprint: Option<String>,
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
    argument_syntax_failures: BTreeMap<String, String>,

    timing: Arc<TurnTimingState>,
}

impl DeterministicDispatchLedger {
    fn new(timing: Arc<TurnTimingState>) -> Self {
        Self {
            blocked_wait_gate: None,
            repeated_failure_gate: None,
            argument_syntax_failures: BTreeMap::new(),

            timing,
        }
    }
}

#[derive(Default)]
struct SamplingRequestSignalState {
    runtime_semantics: HashMap<ToolName, crate::tools::registry::ToolSemanticCapabilities>,
    outcomes: Vec<SamplingToolOutcome>,
    structured_actions: BTreeMap<u64, StructuredActionIdentity>,
    evidence_items: BTreeMap<u64, String>,





    validation_ordinals: BTreeSet<u64>,
    validation_check_identities: BTreeMap<u64, String>,
    validation_launch_identities: BTreeMap<u64, String>,
    validation_check_descriptions: BTreeMap<u64, String>,
    test_validation_ordinals: BTreeSet<u64>,
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
    explicit_completion: Option<(u64, String)>,
    conflicting_explicit_completions: bool,
    code_mode_nested_tool_count: usize,
    code_mode_call_ordinals: BTreeMap<String, u64>,
    code_mode_cell_owners: BTreeMap<String, u64>,
    code_mode_source_dependencies: BTreeMap<String, BTreeSet<SourceDependencyV1>>,
    authoritative_wait_observations: Vec<AuthoritativeWaitObservation>,
    /// Wake-event identities of child progress the agent wait owner delivered.
    agent_deliveries: Vec<String>,
    child_runtime_ms: u64,
    child_runtime_sample_count: usize,
    child_runtime_by_call: BTreeMap<String, u64>,
    call_ordinals: BTreeMap<String, u64>,
    delivered_output_bytes: usize,
}

impl SamplingRequestSignalState {
    fn record_validation_check(&mut self, ordinal: u64, tool_name: &ToolName, arguments: &Value) {
        // Freshness requests and display controls do not identify a different
        // check. Keep this separate from identities eligible for result replay.
        let mut arguments = arguments.clone();
        if let Some(arguments) = arguments.as_object_mut() {
            for key in [
                "force_fresh",
                "max_output_tokens",
                "yield_time_ms",
                "validation",
            ] {
                arguments.remove(key);
            }
        }
        if let Some(identity) = serialized_evidence_identity(&(tool_name, &arguments)) {
            self.validation_launch_identities
                .insert(ordinal, identity.clone());
            self.validation_check_identities.insert(ordinal, identity);
        }
        self.validation_check_descriptions.insert(
            ordinal,
            bounded_validation_detail(&format!("{}: {arguments}", tool_name.name)),
        );
    }

    fn observe_command_validation(&mut self, ordinal: u64, signal: Option<&Value>) {
        let Some(validation) = signal.and_then(|signal| signal.get("command_validation")) else {
            return;
        };
        if let Some(context) = validation.get("execution_context").filter(|context| {
            context["cwd"].is_string()
                && context["environment_id"].is_string()
                && context["environment_fingerprint"].is_string()
                && context["command"]
                    .as_array()
                    .is_some_and(|argv| !argv.is_empty())
        }) && let Some(identity) = serialized_evidence_identity(context)
        {
            self.validation_check_identities.insert(ordinal, identity);
            self.validation_check_descriptions.insert(
                ordinal,
                bounded_validation_detail(&format!("Execution: {context}")),
            );
        }
        if validation["validation"] == true {
            self.saw_validation = true;
            self.validation_ordinals.insert(ordinal);
        }
        if validation["tests"] == true {
            self.test_validation_ordinals.insert(ordinal);
        }
    }
    fn wrapper_ordinals(&self) -> BTreeSet<u64> {
        self.outcomes
            .iter()
            .filter_map(|outcome| outcome.code_mode_cell_id.as_ref())
            .filter_map(|cell| self.code_mode_cell_owners.get(cell))
            .copied()
            .filter(|ordinal| {
                self.outcomes.iter().any(|outcome| {
                    outcome.ordinal == *ordinal
                        && (matches!(
                            outcome.kind,
                            SamplingToolOutcomeKind::Success | SamplingToolOutcomeKind::Yielded
                        ) || outcome.wraps_nested_terminal)
                })
            })
            .collect()
    }

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
    request_ordinal: u64,
}

pub(crate) struct SamplingToolCallRegistration {
    pub(crate) ordinal: u64,
    pub(crate) blocked_wait_guard: Option<BlockedWaitGuard>,
    pub(crate) suppressed_failure: Option<SuppressedFailureGuard>,

}

/// Shared only while consuming one settled request, never across tool execution.
pub(crate) struct SamplingRequestAnalysis<'a> {
    collector: &'a SamplingRequestSignalCollector,
    cycle: OnceLock<Option<DeterministicCycle>>,
}

impl SamplingRequestAnalysis<'_> {
    fn deterministic_cycle(&self) -> Option<&DeterministicCycle> {
        self.cycle
            .get_or_init(|| self.collector.deterministic_cycle())
            .as_ref()
    }
}

impl SamplingRequestSignalCollector {
    pub(crate) fn analyze_settled_request(&self) -> SamplingRequestAnalysis<'_> {
        SamplingRequestAnalysis {
            collector: self,
            cycle: OnceLock::new(),
        }
    }

    pub(crate) fn register_runtime_semantics(
        &self,
        tool_name: &ToolName,
        semantics: crate::tools::registry::ToolSemanticCapabilities,
    ) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_semantics
            .insert(tool_name.clone(), semantics);
    }

    fn runtime_semantics(
        &self,
        tool_name: &ToolName,
    ) -> crate::tools::registry::ToolSemanticCapabilities {
        if let Some(semantics) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_semantics
            .get(tool_name)
            .copied()
        {
            return semantics;
        }
        // Standalone collectors may have no registered router. Production
        // always installs capabilities from the actual admitted runtime.
        crate::tools::registry::ToolSemanticCapabilities::default()
    }

    fn validation_semantics(
        &self,
        tool_name: &ToolName,
        arguments: &Value,
    ) -> (bool, bool, bool, bool) {
        let Some(format) = self.runtime_semantics(tool_name).command else {
            return (false, false, false, false);
        };
        let wire_name = format.canonical_name();
        let (validation, proof, tests) = validation_status_from_arguments(&wire_name, arguments);
        (
            validation,
            proof,
            tests,
            final_diff_status_from_arguments(&wire_name, arguments),
        )
    }

    pub(crate) fn completion_evidence_key(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let evidence = state
            .outcomes
            .iter()

            // Unrelated reads are not progress against a completion blocker.
            // The turn owner separately keys the actual candidate and revisions.
            .filter(|outcome| {
                state.validation_ordinals.contains(&outcome.ordinal)
            })
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
            .then(|| hex::encode(Sha256::digest(format!("{evidence:?}").as_bytes())))
    }



    pub(crate) fn register_deterministic_tool_call_with_reuse(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        current_call_id: &str,
        failure_reuse: crate::tools::registry::TerminalFailureReuse,
    ) -> SamplingToolCallRegistration {
        let ordinal = self.next_ordinal.fetch_add(1, Ordering::Relaxed);
        let live_process_poll = tool_name_matches(tool_name, "write_stdin");
        let direct_code_mode_exec = crate::tools::code_mode::is_exec_tool_name(tool_name);
        let canonical = canonical_tool_action(payload);
        let wait = is_wait_tool(tool_name)
            || (live_process_poll
                && canonical
                    .value
                    .get("chars")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty));
        let action_identity = deterministic_action_identity(tool_name, &canonical);
        let structured_action =
            structured_action_identity_from_canonical(tool_name, payload, &canonical);
        let (validation, _validation_proof, test_execution, final_verification) =
            self.validation_semantics(tool_name, &canonical.value);
        let semantics = self.runtime_semantics(tool_name);
        let mutation = semantics.mutation;

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
                    .filter(|_| failure_reuse != crate::tools::registry::TerminalFailureReuse::Never)
                    .and_then(|action| {
                        ledger
                            .repeated_failure_gate
                            .as_ref()
                            .filter(|gate| gate.state_revision == self.request_state_revision)
                            .filter(|gate| gate.action_identity == action.identity)
                            .map(|gate| SuppressedFailureGuard {
                                failure_fingerprint: gate.failure_fingerprint.clone(),
                            })
                            .or_else(|| {
                                (failure_reuse == crate::tools::registry::TerminalFailureReuse::RequestRevisionAndJsonSyntax
                                    && action.class == StructuredActionClass::InvalidArguments)
                                    .then(|| ledger.argument_syntax_failures.get(&action.identity))
                                    .flatten()
                                    .map(|fingerprint| SuppressedFailureGuard {
                                        failure_fingerprint: fingerprint.clone(),
                                    })
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
        state
            .call_ordinals
            .insert(current_call_id.to_string(), ordinal);

        if wait {
            state.wait_call_count = state.wait_call_count.saturating_add(1);
        }
        if live_process_poll {
            state.process_monitor_ordinals.insert(ordinal);
        }
        if direct_code_mode_exec {
            state.direct_code_mode_exec_count = state.direct_code_mode_exec_count.saturating_add(1);
            state
                .code_mode_call_ordinals
                .insert(current_call_id.to_string(), ordinal);
        } else if crate::tools::code_mode::is_orchestration_tool_name(tool_name)
            && let Some(cell_id) = canonical.value.get("cell_id").and_then(Value::as_str)
        {
            state
                .code_mode_cell_owners
                .insert(cell_id.to_string(), ordinal);
        }
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= semantics.coordination;
        if semantics.command.is_some() || live_process_poll {
            state.record_validation_check(ordinal, tool_name, &canonical.value);
        }
        if validation {
            state.validation_ordinals.insert(ordinal);
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

    /// A call contributes at most one timing sample, even when its completion
    /// is observed again. The call ID also binds mixed-request validation costs.
    pub(crate) fn record_child_runtime_for_call(&self, call_id: &str, runtime_ms: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.child_runtime_by_call.contains_key(call_id) {
            return;
        }
        state
            .child_runtime_by_call
            .insert(call_id.to_string(), runtime_ms);
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
        let result = canonical_authoritative_result(response);
        let deliveries = result
            .as_ref()
            .map(delivered_agent_progress)
            .unwrap_or_default();
        if !deliveries.is_empty() {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .agent_deliveries
                .extend(deliveries);
        }
        self.record_registered_owner_result(
            "multi_agent_v2",
            tool_name,
            payload,
            signal,
            result.as_ref(),
            false,
        );
    }

    pub(crate) fn record_registered_owner_result(
        &self,
        adapter: &str,
        tool_name: &ToolName,
        payload: &ToolPayload,
        signal: Option<&Value>,
        result: Option<&Value>,
        nested: bool,
    ) {
        // A nested export completes its own operation, not the enclosing
        // cell's assignment. A worker's accepted `deliver: true` report is its
        // declared final result: `authoritative_wait_observation` surfaces it
        // only when that cell's other tool calls were plan updates and every
        // outcome succeeded, the same proof the direct path requires.
        if nested && adapter == "agent_job_csv_export" {
            return;
        }
        let Some(observation) =
            authoritative_wait_observation(adapter, tool_name, payload, signal, result)
        else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !nested {
            state.direct_wait_agent_count = state.direct_wait_agent_count.saturating_add(1);
        }
        state.authoritative_wait_observations.push(observation);
    }

    pub(crate) fn record_code_mode_parent(&self, cell_id: &str, parent_call_id: Option<&str>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ordinal) = parent_call_id
            .and_then(|parent| state.code_mode_call_ordinals.get(parent))
            .copied()
        {
            state
                .code_mode_cell_owners
                .insert(cell_id.to_string(), ordinal);
        }
    }


    pub(crate) fn record_code_mode_result_for_call(
        &self,
        call_id: Option<&str>,
        result: CodeModeToolResult<'_>,
    ) {
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
        if outcome.is_failure_evidence() {
            outcome.validation_failure_excerpt = validation_failure_excerpt(result);
        }
        if outcome.is_failure_evidence() && outcome.failure_fingerprint.is_none() {
            outcome.failure_fingerprint = Some(code_mode_result_failure_fingerprint(
                tool_name, payload, result,
            ));
        }
        outcome.canonical_artifact_required = canonical_artifact_required;
        outcome.nested_in_code_mode = true;
        outcome.code_mode_cell_id = Some(cell_id.to_string());
        let canonical = canonical_tool_action(payload);
        let structured_action =
            structured_action_identity_from_canonical(tool_name, payload, &canonical);
        let (validation, _validation_proof, test_execution, final_verification) =
            self.validation_semantics(tool_name, &canonical.value);
        let semantics = self.runtime_semantics(tool_name);
        let mutation = semantics.mutation;
        let evidence_identity = outcome
            .source_evidence
            .as_ref()
            .and_then(value_evidence_identity)
            .or_else(|| value_evidence_identity(result));
        // As on the direct path, only the registered agent wait owner delivers
        // child progress; a same-named tool without that semantics does not.
        let agent_deliveries = if semantics.coordination
            && tool_name_matches(tool_name, "wait_agent")
            && outcome.kind == SamplingToolOutcomeKind::Success
        {
            delivered_agent_progress(result)
        } else {
            Vec::new()
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        state.agent_deliveries.extend(agent_deliveries);
        if let Some(call_id) = call_id {
            state.call_ordinals.insert(call_id.to_string(), ordinal);
        }
        if is_wait_tool(tool_name)
            || (tool_name_matches(tool_name, "write_stdin")
                && canonical
                    .value
                    .get("chars")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty))
        {
            state.wait_call_count = state.wait_call_count.saturating_add(1);
        }
        if tool_name_matches(tool_name, "write_stdin") {
            state.process_monitor_ordinals.insert(ordinal);
        }

        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= semantics.coordination;
        if semantics.command.is_some() || tool_name_matches(tool_name, "write_stdin") {
            state.record_validation_check(ordinal, tool_name, &canonical.value);
        }
        state.observe_command_validation(ordinal, signal);
        if validation {
            state.validation_ordinals.insert(ordinal);
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
        if outcome.is_failure_evidence() && signal.is_none() {
            // A carrier/handler error is not an observed process exit.
        }
        state.outcomes.push(outcome);
        if let Some(structured_action) = structured_action {
            state.structured_actions.insert(ordinal, structured_action);
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

    /// Ordinal already registered for `call_id`, for an owner that must attach
    /// evidence to a call it recorded rather than minting a new identity.
    pub(crate) fn ordinal_for_call(&self, call_id: &str) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .call_ordinals
            .get(call_id)
            .copied()
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
        outcome.code_mode_cell_id = Some(cell_id.to_string());
        let canonical = payload.map(canonical_tool_action);
        let structured_action = payload
            .zip(canonical.as_ref())
            .and_then(|(payload, canonical)| {
                structured_action_identity_from_canonical(tool_name, payload, canonical)
            });
        let (validation, _validation_proof, test_execution, final_verification) = canonical
            .as_ref()
            .map(|canonical| self.validation_semantics(tool_name, &canonical.value))
            .unwrap_or_default();
        let semantics = self.runtime_semantics(tool_name);
        let mutation = payload.is_some() && semantics.mutation;
        let coordination = payload.is_some() && semantics.coordination;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.code_mode_nested_tool_count = state.code_mode_nested_tool_count.saturating_add(1);
        state.saw_artifact_read |= tool_name_matches(tool_name, "read_tool_output");
        state.saw_validation |= validation;
        state.saw_mutation |= mutation;
        state.saw_coordination |= coordination;
        if semantics.command.is_some()
            && let Some(canonical) = canonical.as_ref()
        {
            state.record_validation_check(ordinal, tool_name, &canonical.value);
        }
        if validation {
            state.validation_ordinals.insert(ordinal);
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
    }

    fn authoritative_wait_observation(&self) -> Option<AuthoritativeWaitObservation> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let plan_calls = state
            .outcomes
            .iter()
            .filter(|outcome| {
                outcome.kind == SamplingToolOutcomeKind::Success && outcome.plan.is_some()
            })
            .count();
        let observation = state.authoritative_wait_observations.first()?;
        let owner_calls = state.authoritative_wait_observations.len();
        // Identical owner/revision/action/acceptance receipts can be joined
        // without another semantic decision. Different owners or partial
        // receipts require an explicit parent (for example an opt-in delivery
        // cell that awaits every dependency and constructs the final result).
        if !state
            .authoritative_wait_observations
            .iter()
            .all(|other| other == observation)
        {
            return None;
        }
        let direct_owner = state.registered_count == owner_calls + plan_calls
            && state.direct_wait_agent_count == owner_calls
            && state.direct_code_mode_exec_count == 0
            && state.code_mode_nested_tool_count == 0;
        let code_mode_owner = state.registered_count == 1
            && state.direct_wait_agent_count == 0
            && state.direct_code_mode_exec_count == 1
            && state.code_mode_nested_tool_count == owner_calls + plan_calls;
        if !(direct_owner || code_mode_owner) {
            return None;
        }
        if observation.disposition == AuthoritativeWaitDisposition::Terminal
            && !matches!(
                observation.result.adapter.as_str(),
                "multi_agent_v2" | "code_mode_cell"
            )
        {
            let expected = state
                .registered_count
                .saturating_add(state.code_mode_nested_tool_count);
            let observed = state
                .outcomes
                .iter()
                .map(|outcome| outcome.ordinal)
                .collect::<BTreeSet<_>>();
            if state.outcomes.len() != expected
                || observed.len() != expected
                || state.outcomes.iter().any(|outcome| {
                    outcome.kind != SamplingToolOutcomeKind::Success
                        || outcome.canonical_artifact_required
                        || outcome.background_process_id.is_some()
                })
            {
                return None;
            }
        }
        Some(observation.clone())
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
            "direct_tool.{}",
            hex::encode(Sha256::digest(
                crate::tools::context::normalize_tool_failure_text(failure).as_bytes()
            ))
        ));
        outcome.failure_is_terminal = failure_is_terminal;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A handler error has no native execution result. Preserve the tool
        // failure, but do not manufacture an executed validation failure from
        // its launch-time command classification.
        state.validation_ordinals.remove(&ordinal);
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
        if signal.as_ref().is_some_and(|signal| signal["validation_policy_refusal"] == true) {
            // Policy denial did not execute. It is neither a failed check nor
            // busy admission, and cannot create repair or replay authority.
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.validation_ordinals.remove(&ordinal);
            state.test_validation_ordinals.remove(&ordinal);
            let mut outcome = SamplingToolOutcome::from_signal(
                ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Skipped), None, None,
            );
            outcome.kind = SamplingToolOutcomeKind::Skipped;
            state.outcomes.push(outcome);
            return;
        }
        let plan = sampling_plan(signal.as_ref());
        let mut outcome =
            SamplingToolOutcome::from_signal(ordinal, outcome_context, plan, signal.as_ref());
        if outcome.is_failure_evidence()
            && let Some(text) = response_output_text(response)
        {
            outcome.validation_failure_excerpt = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| validation_failure_excerpt(&value))
                .or_else(|| Some(bounded_validation_detail(&text)));
        }
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
        state.observe_command_validation(ordinal, signal.as_ref());
        if outcome.kind == SamplingToolOutcomeKind::Success
            && (state
                .code_mode_call_ordinals
                .values()
                .any(|value| *value == ordinal)
                || state
                    .code_mode_cell_owners
                    .values()
                    .any(|value| *value == ordinal))
            && let Some(message) = signal
                .as_ref()
                .and_then(|signal| signal.get("explicit_completion_message"))
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        {
            if state
                .explicit_completion
                .as_ref()
                .is_some_and(|(owner, _)| *owner != ordinal)
            {
                state.conflicting_explicit_completions = true;
            }
            state.explicit_completion = Some((ordinal, message.to_string()));
        }

        state.saw_canonical_artifact_requirement |= canonical_artifact_required;
        state.delivered_output_bytes = state
            .delivered_output_bytes
            .saturating_add(response_replay_text_size(response).unwrap_or(0));
        state.outcomes.push(outcome);
        if let Some(evidence_identity) = evidence_identity {
            state.evidence_items.insert(ordinal, evidence_identity);
        }
    }







    fn explicit_completion(&self) -> Option<AuthoritativeWaitOwnerResult> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ordinal, message) = state.explicit_completion.as_ref()?;
        let admitted_cell = |outcome: &SamplingToolOutcome| {
            outcome.nested_in_code_mode
                && outcome
                    .code_mode_cell_id
                    .as_ref()
                    .and_then(|cell| state.code_mode_cell_owners.get(cell))
                    == Some(ordinal)
        };
        // A model-authored delivery intent is an explicit final response, not a
        // guess that an arbitrary successful tool completed the user's task.
        // Every registered sibling and nested call must be accounted for. A
        // yielded command is settled only by a later successful observation of
        // that same process; its original running receipt remains evidence.
        // A count alone would let duplicate outcomes hide pending work.
        let expected = state
            .registered_count
            .saturating_add(state.code_mode_nested_tool_count);
        let observed = state
            .outcomes
            .iter()
            .map(|outcome| outcome.ordinal)
            .collect::<BTreeSet<_>>();
        if state.conflicting_explicit_completions
            || state.outcomes.len() != expected
            || observed.len() != expected
            || state.outcomes.iter().any(|outcome| {
                let successful_terminal = outcome.kind == SamplingToolOutcomeKind::Success
                    && outcome.background_process_id.is_none();
                let consumed_process = outcome.kind == SamplingToolOutcomeKind::Yielded
                    && outcome.background_process_id.is_some_and(|process_id| {
                        state.outcomes.iter().any(|later| {
                            later.ordinal > outcome.ordinal
                                && later.observed_process_id == Some(process_id)
                                && later.kind == SamplingToolOutcomeKind::Success
                                && later.background_process_id.is_none()
                                && (!later.canonical_artifact_required || admitted_cell(later))
                        })
                    });
                (!successful_terminal && !consumed_process)
                        // The cell owner admits nested evidence using recovered
                        // coverage, which survives empty-yield packet rollover.
                        || (outcome.canonical_artifact_required && !admitted_cell(outcome))
            })
            || state
                .outcomes
                .iter()
                .filter(|outcome| outcome.ordinal == *ordinal)
                .count()
                != 1
        {
            return None;
        }
        Some(AuthoritativeWaitOwnerResult {
            adapter: "code_mode_delivery".to_string(),
            value: serde_json::json!({"message": message}),
            surfaceable_message: Some(message.clone()),
        })
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
                failure_fingerprint: None,
                repeated_failure: None,
            });
        }
        let wrappers = state.wrapper_ordinals();
        let expected_outcomes =
            state.registered_count + state.code_mode_nested_tool_count - wrappers.len();
        let outcomes = state
            .outcomes
            .iter()
            .filter(|outcome| !wrappers.contains(&outcome.ordinal))
            .collect::<Vec<_>>();
        let failures = outcomes
            .iter()
            .copied()
            .filter(|outcome| outcome.is_failure_evidence())
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            let suppressible_failure = failures.iter().all(|outcome| outcome.failure_is_terminal);
            let failure_only = outcomes.len() == expected_outcomes
                && outcomes.iter().all(|outcome| outcome.is_failure_evidence());
            let nested_only = failures.iter().all(|outcome| outcome.nested_in_code_mode);
            let mut fingerprints = failures
                .iter()
                .map(|outcome| outcome.failure_fingerprint.as_deref())
                .collect::<Option<Vec<_>>>()?;
            fingerprints.sort_unstable();
            fingerprints.dedup();
            let failure_fingerprint = fingerprints.into_iter().collect::<Vec<_>>().join("|");
            // Nested diagnostics belong to the actual nested action, never the
            // surrounding script. A script can contain other effectful work,
            // so it must not become a failure-suppression target.
            let repeated_action_identity =
                if failures.len() == 1 && !failures[0].nested_in_code_mode {
                    state
                        .structured_actions
                        .get(&failures[0].ordinal)
                        .map(|action| action.identity.clone())
                } else {
                    None
                };
            let mut failure_action_bindings = failures
                .iter()
                .map(|outcome| {
                    let action_identity = state
                        .structured_actions
                        .get(&outcome.ordinal)
                        .map(|action| action.evidence_identity.clone())?;
                    let fingerprint = outcome.failure_fingerprint.as_deref()?.to_string();
                    Some((action_identity, fingerprint))
                })
                .collect::<Option<Vec<_>>>()?;
            failure_action_bindings.sort_unstable();
            let failure_action_identity = hex::encode(Sha256::digest(
                serde_json::to_vec(&failure_action_bindings).ok()?,
            ));
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
                failure_fingerprint: Some(failure_fingerprint.clone()),
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
        if !outcomes.is_empty()
            && outcomes
                .iter()
                .all(|outcome| state.process_monitor_ordinals.contains(&outcome.ordinal))
        {
            return None;
        }
        if outcomes.len() != expected_outcomes
            || !outcomes
                .iter()
                .all(|outcome| outcome.kind == SamplingToolOutcomeKind::Success)
        {
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
                if action.class == StructuredActionClass::InvalidArguments {
                    return None;
                }
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
            .map(|(_, action, evidence)| format!("{}:{evidence}", action.evidence_identity))
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
            failure_fingerprint: None,
            repeated_failure: None,
        })
    }

    pub(crate) fn is_wait_only(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let calls = state
            .registered_count
            .saturating_sub(state.direct_code_mode_exec_count)
            .saturating_add(state.code_mode_nested_tool_count);
        calls > 0 && state.wait_call_count == calls
    }


    pub(crate) fn observed_successful_process_monitor(&self) -> bool {
        self.monitoring_only_generation(false)
    }

    pub(crate) fn observed_yielded_execution(&self) -> bool {
        self.monitoring_only_generation(true)
    }

    fn monitoring_only_generation(&self, yielded_only: bool) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wrappers = state.wrapper_ordinals();
        let expected = state.registered_count + state.code_mode_nested_tool_count - wrappers.len();
        let outcomes = state
            .outcomes
            .iter()
            .filter(|outcome| !wrappers.contains(&outcome.ordinal))
            .collect::<Vec<_>>();
        expected > 0
            && outcomes.len() == expected
            && outcomes.iter().all(|outcome| {
                let eligible = if yielded_only {
                    outcome.kind == SamplingToolOutcomeKind::Yielded
                } else {
                    state.process_monitor_ordinals.contains(&outcome.ordinal)
                        && matches!(
                            outcome.kind,
                            SamplingToolOutcomeKind::Success | SamplingToolOutcomeKind::Yielded
                        )
                };
                eligible && outcome.process_observation_progress
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
        // post-tool classifier runs. Commands proven read-only at admission
        // still advance `mutation_revision` for invalidation, not this label.
        if has_pending_input {
            Some(TurnTimingGenerationPurpose::InitialReasoning)
        } else if state.saw_mutation
            || settled.attributed_mutation_revision > baselines.attributed_mutation_revision
            || settled.unproven_command_revision > baselines.unproven_command_revision
        {
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
        } else if state.wait_call_count > 0
            && state.wait_call_count
                == state
                    .registered_count
                    .saturating_sub(state.direct_code_mode_exec_count)
                    .saturating_add(state.code_mode_nested_tool_count)
        {
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

    /// Cardinality is independent of fingerprint presence: a known empty list
    /// has a valid fingerprint, while an incomplete observation is unknown.
    pub(crate) fn has_no_structured_actions(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.next_ordinal.load(Ordering::Acquire) == 0 && state.structured_actions.is_empty()
    }

    /// Identity of ordered tool actions, independent of outcomes and call IDs.
    /// Missing identities are unknown, not proof of equal actions.
    pub(crate) fn structured_action_fingerprint(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.structured_actions.len()
            != usize::try_from(self.next_ordinal.load(Ordering::Acquire)).ok()?
        {
            return None;
        }
        // Malformed arguments keep an identity only for argument-error
        // diagnosis; they are not a comparable structured action.
        if state
            .structured_actions
            .values()
            .any(|action| action.class == StructuredActionClass::InvalidArguments)
        {
            return None;
        }
        serialized_evidence_identity(
            &state
                .structured_actions
                .values()
                .map(|action| action.identity.as_str())
                .collect::<Vec<_>>(),
        )
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
        ToolPayload::Function { arguments } => {
            match crate::tools::handlers::parsed_function_argument_value(arguments).unwrap_or_else(
                || serde_json::from_str::<Value>(arguments).map_err(|err| err.to_string()),
            ) {
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
            }
        }
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


fn structured_action_identity_from_canonical(
    tool_name: &ToolName,
    payload: &ToolPayload,
    canonical: &CanonicalToolAction,
) -> Option<StructuredActionIdentity> {
    if canonical.identity_payload.is_none() {
        // Retain invalid bytes only for argument-error diagnosis. Do not give
        // malformed input a successful replay or wait identity.
        let identity = hex::encode(Sha256::digest(
            serde_json::to_vec(&(tool_name, "invalid_function_arguments", &canonical.value))
                .ok()?,
        ));
        return Some(StructuredActionIdentity {
            evidence_identity: identity.clone(),
            identity,
            class: StructuredActionClass::InvalidArguments,
        });
    }
    // Fresh execution must bypass both replay lookup and storage, including
    // consecutive calls that all explicitly request force_fresh.
    if canonical.value.get("force_fresh").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let class = source_invocation_class_from_canonical(tool_name, payload, canonical);
    let normalized_read = if tool_name_matches(tool_name, "read_file") {
        Some(crate::tools::handlers::canonical_read_file_arguments(
            canonical.identity_payload.as_deref()?,
        )?)
    } else {
        None
    };
    let normalized_payload = normalized_read
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .ok()?;
    let action = serde_json::to_string(&(
        tool_name,
        normalized_payload
            .as_deref()
            .or(canonical.identity_payload.as_deref())?,
    ))
    .ok()?;
    let identity = hex::encode(Sha256::digest(action.as_bytes()));
    let mut evidence_arguments = normalized_read.unwrap_or_else(|| canonical.value.clone());
    if ["exec_command", "shell_command", "write_stdin"]
        .iter()
        .any(|name| tool_name_matches(tool_name, name))
        && let Some(arguments) = evidence_arguments.as_object_mut()
    {
        arguments.remove("max_output_tokens");
        arguments.remove("yield_time_ms");
    }
    let evidence_identity = hex::encode(Sha256::digest(
        serde_json::to_vec(&(tool_name, evidence_arguments)).ok()?,
    ));
    Some(StructuredActionIdentity {
        identity,
        evidence_identity,
        class,
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
    Some(hex::encode(writer.0.finalize()))
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



fn value_evidence_identity(value: &Value) -> Option<String> {
    serialized_evidence_identity(&canonicalize_json(value))
}

fn bounded_validation_detail(text: &str) -> String {
    let mut end = text.len().min(1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut detail = text[..end].to_string();
    if end < text.len() {
        detail.push('…');
    }
    detail
}

fn validation_failure_excerpt(value: &Value) -> Option<String> {
    let excerpt = ["stderr", "error", "output", "message"]
        .into_iter()
        .find_map(|key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .map(bounded_validation_detail);
    // Name where the complete output is, so it is read rather than sought
    // through a guessed status tool (audit 2026-10-22 #29).
    let artifact = value
        .get("raw_output_artifact_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(|id| format!("full output: read_tool_output artifact_id {id}"));
    match (excerpt, artifact) {
        (Some(excerpt), Some(artifact)) => Some(format!("{excerpt}; {artifact}")),
        (excerpt, artifact) => excerpt.or(artifact),
    }
}








fn validation_site_key(path: &std::path::Path) -> String {
    source_locator_key(&SourceDependencyV1::new(path, false).path).into_owned()
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
    let mut diagnostic_result = result.clone();
    // Only these diagnostic strings admit volatile producer framing. Invocation
    // arguments and structured application values are never diagnostic prose.
    if let Some(fields) = diagnostic_result.as_object_mut() {
        for key in ["error", "message", "diagnostic"] {
            if let Some(Value::String(text)) = fields.get_mut(key) {
                *text = crate::tools::context::normalize_tool_failure_text(text);
            }
        }
    }
    let canonical = serde_json::to_string(&serde_json::json!({
        "tool_name": tool_name,
        "payload": canonical_tool_payload(&action),
        "result": canonicalize_json(&diagnostic_result),
    }))
    .unwrap_or_default();
    format!(
        "code_mode.nested_tool.{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    )
}

fn canonical_response_body(response: &ResponseInputItem) -> Option<Value> {
    let mut value = serde_json::to_value(response).ok()?;
    if let Value::Object(object) = &mut value {
        object.remove("call_id");
    }
    Some(canonicalize_json(&value))
}

pub(crate) fn canonical_authoritative_result(response: &ResponseInputItem) -> Option<Value> {
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
    if !matches!(expected_adapter, "multi_agent_v2" | "code_mode_cell")
        && (receipt_identity.is_empty() || surfaceable_message.is_none())
    {
        return None;
    }
    let action = canonical_tool_action(payload);
    let result = canonicalize_json(result?);
    let action_identity = format!(
        "{}\n{}",
        serde_json::to_string(tool_name).ok()?,
        action.identity_payload.as_deref()?,
    );
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
        identity: hex::encode(Sha256::digest(identity)),
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

/// Child progress that the agent wait owner delivered in `typed_deltas`. The
/// task store owns which observation kinds are meaningful progress: admission,
/// start and host-only diagnostics are lifecycle bookkeeping. Wake event IDs
/// let an explicit-cursor re-read of the same events stay non-novel.
fn delivered_agent_progress(result: &Value) -> Vec<String> {
    result
        .get("typed_deltas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|delta| {
            delta.get("reason").is_some_and(|reason| {
                codex_agent_task_store::ObservationKind::deserialize(reason)
                    .is_ok_and(codex_agent_task_store::ObservationKind::is_meaningful_progress)
            })
        })
        .filter_map(|delta| delta.get("event_id").and_then(Value::as_str))
        .map(|event_id| format!("agent_event:{event_id}"))
        .collect()
}

fn canonical_tool_payload(action: &CanonicalToolAction) -> Value {
    serde_json::json!({
        "kind": action.kind,
        "value": action.value,
    })
}

fn command_invocation(tool_name: &ToolName, arguments: &Value) -> Option<CommandInvocation> {
    if tool_name.namespace.is_some() {
        return None;
    }
    // Decode native wire formats only. Scheduling and proof eligibility come
    // from the registered runtime's CommandArgumentFormat, not its alias.
    let script_field = match tool_name.name.as_str() {
        "shell_command" => "command",
        "exec_command" | "unified_exec" => "cmd",
        _ => return None,
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
            true,
            leaves.iter().any(|leaf| {
                leaf.operation == ValidationOperation::Test
                    && leaf.mode
                        == codex_shell_command::validation::ValidationExecutionMode::Execution
            }),
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


fn source_invocation_class_from_canonical(
    tool_name: &ToolName,
    payload: &ToolPayload,
    canonical: &CanonicalToolAction,
) -> StructuredActionClass {
    if tool_name_matches(tool_name, "read_file") {
        return StructuredActionClass::PreciseSource;
    }
    if ["read_tool_output", "list_files"]
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

#[derive(Clone, Default, Eq, PartialEq, Serialize)]
struct DeliveredSourceCoverage {
    ranges: Vec<(u64, u64)>,
    query_proofs: BTreeSet<String>,
}

impl DeliveredSourceCoverage {
    fn merge(&mut self, other: &Self) -> bool {
        let previous = self.clone();
        self.ranges.extend_from_slice(&other.ranges);
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(start, end) in &self.ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        self.ranges = merged;
        self.query_proofs.extend(other.query_proofs.iter().cloned());
        *self != previous
    }
}



pub(crate) struct TurnExecutionControl {
    issued_directives: BTreeSet<String>,
    validation_mode: ValidationMode,



    /// Completed generations since the last one that produced new evidence
    /// (including child progress delivered to this thread), a workspace
    /// mutation, or a plan/input change.
    continuations_without_progress: u32,
    /// Wake events already credited as delivered child progress this turn.
    /// They reset only the soft no-progress streak, not the evidence digest.
    delivered_agent_events: BTreeSet<String>,
    plan: Option<crate::plan_store::PlanExecutionSnapshot>,
    /// An inherited completed checklist is not a final-answer hint for new input.
    plan_updated_since_input: bool,
    plan_revision: u64,
    input_revision: u64,
    /// Passive cycle identity excludes reports, unlike completion/dispatch authority.
    instruction_revision: u64,
    dispatch_ledger: Arc<Mutex<DeterministicDispatchLedger>>,
    consecutive_no_progress: u32,
    consecutive_obligation_no_progress: u32,
    recent_cycles: VecDeque<String>,
    last_state_revision: Option<String>,
    directive_issued: bool,
    proven_loop_active: bool,

    turn_efficiency_guard: Option<TurnEfficiencyGuardHandle>,
    turn_efficiency_tool_calls: usize,
    turn_efficiency_child_runtime_ms: u64,
    budget_progress_evidence: BTreeSet<String>,
    delivered_coverage: BTreeMap<String, DeliveredSourceCoverage>,
    /// Latest native snapshot per locator; avoid scanning every file on each read.
    evidence_fingerprint: OnceLock<String>,
    artifact_source_coverage: BTreeMap<String, BTreeSet<String>>,
    /// The turn diff tracker's map, adopted at each settlement boundary by
    /// `retain_validation_across_changes`. Validation bookkeeping below is kept
    /// in validation revisions; callers pass mutation revisions, which also
    /// advance for evidence-only observations.
    /// Validation revision proven by this turn's last passing validation.
    validated_mutation_revision: Option<u64>,
    /// The last fresh passing execution's coverage entry, if it has one.
    /// Re-proving its dependencies does not rewrite the execution revision.
    /// Last terminal result for each check, retained only within this turn.
    failed_validation_checks: BTreeSet<String>,
    validation_check_descriptions: BTreeMap<String, String>,
    validation_check_orders: BTreeMap<String, (u64, u64)>,
    next_validation_request: AtomicU64,
    pending_validation_processes: BTreeMap<u64, PendingValidation>,

}





#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct SamplingConvergenceDecision {
    pub(crate) continuation: ContinuationDisposition,
    pub(crate) directive: Option<String>,
    pub(crate) proven_loop_activated: bool,
    pub(crate) authoritative_wait: Option<AuthoritativeWaitResolution>,
}

impl TurnExecutionControl {

    pub(crate) fn new_with_timing(timing: Arc<TurnTimingState>) -> Self {
        Self {
            issued_directives: BTreeSet::new(),
            validation_mode: ValidationMode::Full,



            continuations_without_progress: 0,
            delivered_agent_events: BTreeSet::new(),
            plan: None,
            plan_updated_since_input: false,
            plan_revision: 0,
            input_revision: 0,
            instruction_revision: 0,
            dispatch_ledger: Arc::new(Mutex::new(DeterministicDispatchLedger::new(timing))),
            consecutive_no_progress: 0,
            consecutive_obligation_no_progress: 0,
            recent_cycles: VecDeque::new(),
            last_state_revision: None,
            directive_issued: false,
            proven_loop_active: false,

            turn_efficiency_guard: None,
            turn_efficiency_tool_calls: 0,
            turn_efficiency_child_runtime_ms: 0,
            budget_progress_evidence: BTreeSet::new(),
            delivered_coverage: BTreeMap::new(),
            evidence_fingerprint: OnceLock::new(),
            artifact_source_coverage: BTreeMap::new(),
            validated_mutation_revision: None,
            failed_validation_checks: BTreeSet::new(),
            validation_check_descriptions: BTreeMap::new(),

            validation_check_orders: BTreeMap::new(),
            next_validation_request: AtomicU64::new(0),
            pending_validation_processes: BTreeMap::new(),

        }
    }

    pub(crate) fn set_validation_policy(&mut self, mode: ValidationMode) {
        self.validation_mode = mode;
    }

    pub(crate) fn validation_repair_available(&self) -> bool {
        self.validation_mode != ValidationMode::Off
    }

    pub(crate) fn with_active_plan(
        mut self,
        plan: Option<crate::plan_store::PlanExecutionSnapshot>,
    ) -> Self {
        self.refresh_plan(plan);
        self.plan_updated_since_input = false;
        self
    }

    /// Called at the settlement boundary, after publication tasks have settled.
    /// Tool observation ordinals never choose the authoritative checklist.
    pub(crate) fn refresh_plan(&mut self, plan: Option<crate::plan_store::PlanExecutionSnapshot>) {
        if self.plan != plan {
            self.plan_revision = self.plan_revision.saturating_add(1);
            self.plan_updated_since_input = plan.is_some();
        }
        self.plan = plan;
    }

    /// Validation settles against source revisions: an uncertain command whose
    /// Git-visible workspace (outside Git, its bounded workspace tree) was
    /// unchanged advances only the mutation revision.
    pub(crate) fn validation_revision_at(&self, mutation_revision: u64) -> u64 {
        mutation_revision
    }

    /// Whether this turn changed sources by the settled `mutation_revision`.
    /// Each turn's diff tracker starts at validation revision zero. Patches,
    /// declared writes, workspace changes no command declared, and command
    /// effects neither a Git baseline nor a bounded non-Git tree capture could
    /// rule out advance it; an uncertain command whose Git-visible workspace
    /// (outside Git, its bounded workspace tree) was unchanged does not.
    /// Without a change, every current failure was observed against the
    /// turn-start sources.
    pub(crate) fn changed_sources(&self, mutation_revision: u64) -> bool {
        self.validation_revision_at(mutation_revision) != 0
    }















    /// Mechanical reasons this turn is not a verified completion: workspace
    /// changes after its last passing validation, not checklist accounting.
    /// The revision is the last settled request's validation revision. The turn
    /// owner separately reports the post-validation boundary of a mutating finalizer.
    fn completion_assessment(
        &self,
        _settled_mutation_revision: u64,
    ) -> codex_protocol::protocol::TurnCompletionAssessment {
        codex_protocol::protocol::TurnCompletionAssessment {
            failed_checks: self.failed_validation_checks.iter().map(|check| {
                self.validation_check_descriptions.get(check).cloned().unwrap_or_else(|| check.clone())
            }).collect(),
            ..Default::default()
        }
    }


    pub(crate) fn completion_assessment_with_runtime_inputs(
        &self,
        settled_mutation_revision: u64,
        _changed_paths: Option<&[(String, std::path::PathBuf)]>,
        _has_untracked_changes: bool,
        _runtime_inputs: &[(String, std::path::PathBuf)],
    ) -> codex_protocol::protocol::TurnCompletionAssessment {
        self.completion_assessment(settled_mutation_revision)
    }




    /// Owner-derived checklist accounting only, never proof of task completion.
    pub(crate) fn plan_completed(&self) -> bool {
        self.plan_updated_since_input
            && self
                .plan
                .as_ref()
                .is_some_and(|plan| plan.has_steps && plan.obligations.unresolved.is_empty())
    }



    /// Share one novelty decision between the emergency allowance and telemetry.
    /// New call IDs, replayed results, and repeated validation/failure evidence
    /// must not be reported as progress just because an observation occurred.
    pub(crate) fn observe_progress(
        &mut self,
        baselines: &SamplingRequestBaselines,
        signals: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> Vec<TurnTimingProgressKind> {

        let state = signals
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut progress = Vec::new();
        if settled.attributed_mutation_revision != baselines.attributed_mutation_revision {
            progress.push(TurnTimingProgressKind::WorkspaceMutation);
        }
        let wrappers = state.wrapper_ordinals();
        let mut process_progress = false;
        for outcome in &state.outcomes {

            if outcome.failure_diagnosis_reused
                || wrappers.contains(&outcome.ordinal)
                || outcome.kind == SamplingToolOutcomeKind::Skipped
            {
                continue;
            }
            if outcome.kind == SamplingToolOutcomeKind::Yielded {
                // Monitoring can advance without a terminal source result. Keep
                // the existing generation refund and soft convergence in agreement.
                process_progress |= outcome.process_observation_progress;
                continue;
            }
            if let Some(novel) = outcome.source_evidence.as_ref().and_then(|evidence| {
                self.observe_delivered_coverage(
                    evidence,
                    outcome.source_artifact_id.as_deref(),
                    outcome.validation_mutation_revision.unwrap_or(baselines.mutation_revision),
                )
            }) {
                if novel {
                    progress.push(TurnTimingProgressKind::NewSourceEvidence);
                }
                if !outcome.is_failure_evidence() {
                    continue;
                }
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
                // Producer-scoped coverage is independent of execution/presentation.
                // Unscoped legacy evidence remains bound to its semantic action:
                // a command marker alone cannot establish common provenance.
                let evidence = if outcome.is_failure_evidence() {
                    format!("failure:{evidence}")
                } else if !outcome.source_evidence.as_ref().is_some_and(|evidence| {
                    evidence.get("source").is_some()
                        && evidence.get("scope").is_some()
                        && evidence.get("identity").is_some()
                }) {
                    format!(
                        "source:{}:{evidence}",
                        state
                            .structured_actions
                            .get(&outcome.ordinal)
                            .map(|action| action.evidence_identity.as_str())
                            .unwrap_or_default()
                    )
                } else {
                    format!("content:{evidence}")
                };
                if self.budget_progress_evidence.insert(evidence) {
                    self.evidence_fingerprint.take();
                    if state.validation_ordinals.contains(&outcome.ordinal) {
                        progress.push(TurnTimingProgressKind::ValidationResult);
                    }
                    if outcome.is_failure_evidence() {
                        progress.push(TurnTimingProgressKind::FailureObservation);
                    } else if !state.validation_ordinals.contains(&outcome.ordinal) {
                        progress.push(TurnTimingProgressKind::NewSourceEvidence);
                    }
                }
            }
        }
        progress.sort_by_key(|kind| *kind as u8);
        progress.dedup();

        // A coordinating thread's evidence arrives as child progress, not as
        // its own reads. Like process monitoring, novel delivered progress ends
        // the soft no-progress streak without claiming a source-evidence kind;
        // empty or re-read waits still count toward the intervention.
        let mut agent_delivery_progress = false;
        for event in &state.agent_deliveries {
            agent_delivery_progress |= self.delivered_agent_events.insert(event.clone());
        }
        if !progress.is_empty() || process_progress || agent_delivery_progress {
            self.continuations_without_progress = 0;
        } else {
            self.continuations_without_progress =
                self.continuations_without_progress.saturating_add(1);
        }
        progress
    }

    /// Native file snapshots and their retained artifacts share delivered bytes,
    /// not freshness. A new source hash or path always starts independent coverage.
    fn observe_delivered_coverage(
        &mut self,
        evidence: &Value,
        artifact_id: Option<&str>,
        _mutation_revision: u64,
    ) -> Option<bool> {
        let source = evidence["source"].as_str()?;
        if !matches!(source, "read_file" | "artifact") {
            return None;
        }
        let identity = evidence.get("identity")?;
        let sha256 = identity["sha256"].as_str()?;
        let scope = evidence.get("scope")?;
        let ranges = identity["ranges"]
            .as_array()?
            .iter()
            .map(|range| {
                let range = range.as_array()?;
                if range.len() != 2 {
                    return None;
                }
                let start = range[0].as_u64()?;
                let end = range[1].as_u64()?;
                (start <= end).then_some((start, end))
            })
            .collect::<Option<Vec<_>>>()?;
        let values = identity["values"].as_array()?;
        let key = value_evidence_identity(&serde_json::json!([source, scope, sha256]))?;
        let delivered = DeliveredSourceCoverage {
            ranges,
            query_proofs: values.iter().filter_map(value_evidence_identity).collect(),
        };
        let sources = self
            .artifact_source_coverage
            .get(&key)
            .cloned()
            .unwrap_or_default();
        // Alias propagation can change the digest even without novel source bytes.
        self.evidence_fingerprint.take();
        // Other reads may have filled holes since this artifact was created.
        // Import their coverage before deciding whether recovery adds evidence.
        if source == "artifact" {
            for source in &sources {
                if let Some(coverage) = self.delivered_coverage.get(source).cloned() {
                    self.delivered_coverage
                        .entry(key.clone())
                        .or_default()
                        .merge(&coverage);
                }
            }
        }
        let novel = self
            .delivered_coverage
            .entry(key.clone())
            .or_default()
            .merge(&delivered);
        if source == "read_file" {
            if let Some(id) = artifact_id {
                let artifact_key =
                    value_evidence_identity(&serde_json::json!(["artifact", id, sha256]))?;
                let coverage = self.delivered_coverage.get(&key)?.clone();
                self.delivered_coverage
                    .entry(artifact_key.clone())
                    .or_default()
                    .merge(&coverage);
                self.artifact_source_coverage
                    .entry(artifact_key)
                    .or_default()
                    .insert(key);
            }
        } else {
            // Recovery delivered the exact bytes of these authenticated snapshots.
            // It does not refresh their paths or authorize replay of stale files.
            for source in sources {
                self.delivered_coverage
                    .entry(source)
                    .or_default()
                    .merge(&delivered);
            }
        }
        Some(novel)
    }



    pub(crate) fn baselines_with_tool_exposure_revision(
        &self,
        mutation_revision: u64,
        tool_exposure_revision: u64,
    ) -> SamplingRequestBaselines {
        SamplingRequestBaselines {
            mutation_revision,
            attributed_mutation_revision: mutation_revision,
            unproven_command_revision: 0,
            plan_revision: self.plan_revision,
            input_revision: self.input_revision,
            instruction_revision: self.instruction_revision,
            tool_exposure_revision,
            evidence_fingerprint: self.evidence_fingerprint.get_or_init(|| {
                #[expect(
                    clippy::expect_used,
                    reason = "derived evidence serialization contains only strings, string-keyed maps, collections, and integer ranges"
                )]
                let evidence = serde_json::to_vec(&(
                    &self.budget_progress_evidence,
                    &self.delivered_coverage,
                ))
                .expect("ordered evidence contains only JSON-serializable values");
                hex::encode(Sha256::digest(evidence))
            }).clone(),
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
            request_ordinal: self.next_validation_request.fetch_add(1, Ordering::Relaxed),
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


    pub(crate) fn continuation_generation_request_with_analysis(
        &self,
        baselines: &SamplingRequestBaselines,
        analysis: &SamplingRequestAnalysis<'_>,
        settled: &SamplingRequestSettledState,
        has_pending_input: bool,
    ) -> GenerationRequestDisposition {
        let collector = analysis.collector;
        let relevant_state_fingerprint = self
            .baselines_with_tool_exposure_revision(
                settled.mutation_revision,
                settled.tool_exposure_revision,
            )
            .relevant_state_fingerprint();
        GenerationRequestDisposition {
            // A server-requested continuation can finish reasoning or issue a
            // tool even when the preceding response changed no tracked state.
            // Only an explicit owner result may complete work without sampling.
            purpose: collector.generation_purpose(baselines, settled, has_pending_input, false),
            sampling: SamplingGenerationDisposition::DecisionBearing,
            relevant_state_fingerprint,
            failure_fingerprint: analysis
                .deterministic_cycle()
                .and_then(|cycle| cycle.failure_fingerprint.clone()),
            terminal_completion_only: false,
        }
    }


    pub(crate) fn accepted_context_input(&mut self, instructions_changed: bool) {
        self.plan_updated_since_input = false;
        self.issued_directives.clear();

        self.input_revision = self.input_revision.saturating_add(1);
        // Without new instructions, accepted input is a child's or peer's
        // delivered report or final answer: new information that ends a soft
        // no-progress streak. Only new instructions re-arm the one-shot
        // intervention or reset passive cycle identity.
        self.continuations_without_progress = 0;
        if instructions_changed {
            self.instruction_revision = self.instruction_revision.saturating_add(1);

            self.reset_convergence();
        }
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timing = Arc::clone(&ledger.timing);

        let syntax_failures = std::mem::take(&mut ledger.argument_syntax_failures);
        *ledger = DeterministicDispatchLedger::new(timing);

        ledger.argument_syntax_failures = syntax_failures;
        drop(ledger);
    }

    /// Existing durable call/result pairs are the failure-memory journal.
    /// Reparse with this build before retaining a versioned syntax diagnosis.
    /// We do not rehydrate state-sensitive schema/plan/artifact failures.
    pub(crate) fn rehydrate_argument_failures(
        &self,
        items: &[codex_protocol::models::ResponseItem],
    ) {
        use codex_protocol::models::ResponseItem;
        let recent = &items[items.len().saturating_sub(256)..];
        let mut calls = BTreeMap::new();
        let mut diagnoses = BTreeMap::new();
        for item in recent {
            match item {
                ResponseItem::FunctionCall {
                    name,
                    namespace: None,
                    arguments,
                    call_id,
                    ..
                } => {
                    let payload = ToolPayload::Function {
                        arguments: arguments.clone(),
                    };
                    let canonical = canonical_tool_action(&payload);
                    if canonical.identity_payload.is_none()
                        && let Some(action) = structured_action_identity_from_canonical(
                            &ToolName::plain(name.clone()),
                            &payload,
                            &canonical,
                        )
                    {
                        calls.insert(call_id, action.identity);
                    }
                }
                ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } if output.success == Some(false) => {
                    if let Some(identity) = calls.get(call_id) {
                        diagnoses.insert(
                            identity.clone(),
                            format!("json-argument-syntax-v1:{identity}"),
                        );
                    }
                }
                _ => {}
            }
        }
        self.dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .argument_syntax_failures = diagnoses;
    }





    pub(crate) fn admit_directive(&mut self, directive: &str) -> bool {
        self.issued_directives.insert(directive.to_string())
    }


    pub(crate) fn evaluate_convergence_with_analysis(
        &mut self,
        baselines: &SamplingRequestBaselines,
        analysis: &SamplingRequestAnalysis<'_>,
        settled: &SamplingRequestSettledState,
    ) -> SamplingConvergenceDecision {
        let collector = analysis.collector;
        // Required validation remains turn-owned across request collectors.
        // Background services without validation ownership do not gate delivery.
        if self
            .pending_validation_processes
            .values()
            .next().is_some()
        {
            return SamplingConvergenceDecision::default();
        }
        if self.input_revision == baselines.input_revision
            && let Some(result) = collector.explicit_completion()
        {
            return SamplingConvergenceDecision {
                continuation: ContinuationDisposition::SurfaceExistingResult,
                authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(result)),
                ..Default::default()
            };
        }
        // Cache freshness remains conservative. Convergence only credits
        // changes attributed to a patch or known command mutation.
        // Presentation/write-consistency revisions are not execution progress.
        let settled_revision = format!(
            "mutation={};input={};tool_exposure={}",
            settled.attributed_mutation_revision,
            self.instruction_revision,
            settled.tool_exposure_revision,
        );
        // Validation proves only its observed execution. It cannot prove that
        // all user-requested changes, checks, or child lifecycle actions are done.
        if settled.attributed_mutation_revision != baselines.attributed_mutation_revision
            || self.instruction_revision != baselines.instruction_revision
            || settled.tool_exposure_revision != baselines.tool_exposure_revision
        {
            self.continuations_without_progress = 0;
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        }

        // Reports invalidate old completion/dispatch proof, but are not proof
        // that an unchanged action/result became new evidence. Do not record
        // the in-flight observation against a context it never sampled.
        if self.input_revision != baselines.input_revision {
            return SamplingConvergenceDecision::default();
        }

        if self
            .last_state_revision
            .as_deref()
            .is_some_and(|previous| previous != settled_revision)
        {
            self.reset_convergence();
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
            .then(|| analysis.deterministic_cycle())
            .flatten();
        let request_deterministic_cycle = request_cycle.as_ref().map(|cycle| cycle.key.clone());
        let repeated_cycle = request_cycle
            .as_ref()
            .is_some_and(|cycle| self.recent_cycles.contains(&cycle.key));
        // Keep a bounded set of observations, not just the adjacent one.
        // Relevant state changes above invalidate the entire window.
        if let Some(cycle) = request_cycle.as_ref()
            && cycle.kind != DeterministicCycleKind::Empty
        {
            self.recent_cycles.retain(|key| key != &cycle.key);
            self.recent_cycles.push_back(cycle.key.clone());
            if self.recent_cycles.len() > RECENT_CYCLE_LIMIT {
                self.recent_cycles.pop_front();
            }
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
            self.recent_cycles.clear();
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
                        proven_loop_activated: false,
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
                        proven_loop_activated: false,
                        authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
                            observation.result,
                        )),
                    }
                }
            };
        }

        if collector.suppressed_blocked_wait() {
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
            // Unknown semantics cannot prove a repeat, but do not erase the
            // independent soft-advisory accounting or prior observations.
            self.reset_cycle_advisory();
            self.last_state_revision = Some(settled_revision);
            return SamplingConvergenceDecision::default();
        };
        if cycle.kind == DeterministicCycleKind::Empty {
            // A tool-free continuation is a protocol/model signal, not an
            // action/result cycle. It provides no semantic identity that the
            // host can prove repeated, so it must never spend the convergence
            // budget or escalate tool restrictions.
            self.reset_cycle_advisory();
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
                state_revision: self.settled_revision_key(settled),
                action_identity: action_identity.clone(),
                failure_fingerprint: failure_fingerprint.clone(),
            });
        }
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
            "Convergence escalation: structured state still has not changed, so repeating the same action unchanged adds no evidence. Choose a new hypothesis, change state, narrow the observation, or truthfully complete; a no-progress count alone never ends the task."
        };
        SamplingConvergenceDecision {
            continuation: ContinuationDisposition::ModelRequired,
            directive: Some(directive.to_string()),
            proven_loop_activated,
            authoritative_wait: None,
        }
    }

    fn reset_convergence(&mut self) {
        self.recent_cycles.clear();
        self.last_state_revision = None;
        self.reset_cycle_advisory();
    }

    fn reset_cycle_advisory(&mut self) {
        self.consecutive_no_progress = 0;
        self.consecutive_obligation_no_progress = 0;
        self.directive_issued = false;
        self.proven_loop_active = false;

        self.reset_turn_efficiency_guard();
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

    /// Retain only validation whose full dependency graph is disjoint from
    /// exact effects recorded by the existing turn diff owner. Package names
    /// and net-diff equality alone are not independence proofs. Each settlement
    /// first adopts the tracker's validation revisions; `revision` is the settled
    /// mutation revision.


    pub(crate) fn settle(
        &mut self,
        _baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) {
        // Keep only the latest terminal observation for each check in this turn.
        {
            let state = collector.state.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut outcomes = state.outcomes.iter().collect::<Vec<_>>();
            outcomes.sort_by_key(|outcome| outcome.ordinal);
            for outcome in outcomes {
                let pending = outcome.observed_process_id
                    .and_then(|id| self.pending_validation_processes.get(&id).cloned());
                let check = pending.as_ref().map(|pending| pending.check.clone())
                    .or_else(|| state.validation_check_identities.get(&outcome.ordinal).cloned());
                if let Some(check) = check.filter(|_| pending.is_some()
                    || state.validation_ordinals.contains(&outcome.ordinal))
                {
                    let order = pending.as_ref().map(|pending| pending.execution_order)
                        .unwrap_or((collector.request_ordinal, outcome.ordinal));
                    if pending.is_none()
                        && self.validation_check_orders.get(&check).is_none_or(|previous| *previous <= order)
                        && let Some(description) = state.validation_check_descriptions.get(&outcome.ordinal)
                    {
                        let call = state.call_ordinals.iter().find_map(|(call, ordinal)| {
                            (*ordinal == outcome.ordinal).then_some(call.as_str())
                        }).unwrap_or("unavailable");
                        self.validation_check_descriptions.insert(check.clone(), format!("call {call}; {description}"));
                    }
                    if let Some(id) = outcome.background_process_id {
                        self.pending_validation_processes.insert(id, PendingValidation {
                            execution_order: order,
                            check,
                        });
                        continue;
                    }
                    if self.validation_check_orders.get(&check).is_none_or(|previous| *previous <= order)
                        && matches!(outcome.kind, SamplingToolOutcomeKind::Success
                            | SamplingToolOutcomeKind::Failure | SamplingToolOutcomeKind::Timeout
                            | SamplingToolOutcomeKind::RecoverableCancellation)
                    {
                        self.validation_check_orders.insert(check.clone(), order);
                        if outcome.kind == SamplingToolOutcomeKind::Success {
                            self.failed_validation_checks.remove(&check);
                            self.validated_mutation_revision = Some(settled.mutation_revision);
                        } else {
                            self.failed_validation_checks.insert(check.clone());
                            if let Some(excerpt) = &outcome.validation_failure_excerpt {
                                self.validation_check_descriptions.entry(check).or_default()
                                    .push_str(&format!("; result: {excerpt}"));
                            }
                        }
                    }
                }
                if outcome.kind != SamplingToolOutcomeKind::Yielded
                    && let Some(id) = outcome.observed_process_id
                {
                    self.pending_validation_processes.remove(&id);
                }
            }
        }

    }
}


