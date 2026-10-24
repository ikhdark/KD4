use std::borrow::Cow;

#[cfg(test)]
#[path = "turn_execution_cycle_tests.rs"]
mod cycle_tests;
#[cfg(test)]
#[path = "turn_execution_evidence_tests.rs"]
mod evidence_tests;
#[cfg(test)]
#[path = "turn_execution_validation_evidence_tests.rs"]
mod validation_evidence_tests;
#[cfg(test)]
#[path = "turn_execution_validation_policy_tests.rs"]
mod validation_policy_tests;
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
#[cfg(test)]
use codex_protocol::plan_tool::StepStatus;
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
    #[cfg(test)]
    validation_error_sites: Option<ValidationErrorSites>,
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
            #[cfg(test)]
            validation_error_sites: None,
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
        // Standalone collector fixtures have no registered router. Production
        // always installs capabilities from the actual admitted runtime.
        #[cfg(test)]
        if tool_name.namespace.is_none() {
            use crate::tools::registry::CommandArgumentFormat;
            return crate::tools::registry::ToolSemanticCapabilities {
                mutation: matches!(tool_name.name.as_str(), "apply_patch" | "apply_patch_tool"),
                coordination: matches!(
                    tool_name.name.as_str(),
                    "spawn_agent" | "send_message" | "followup_task" | "wait_agent"
                ),
                command: match tool_name.name.as_str() {
                    "exec_command" | "unified_exec" => Some(CommandArgumentFormat::Exec),
                    "shell_command" => Some(CommandArgumentFormat::Shell),
                    _ => None,
                },
            };
        }
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

    #[cfg(test)]
    pub(crate) fn register_deterministic_tool_call(
        &self,
        tool_name: &ToolName,
        payload: &ToolPayload,
        current_call_id: &str,
    ) -> SamplingToolCallRegistration {
        // Legacy collector fixtures have no router. Ask the actual native
        // producers for their contract; production uses the registered runtime.
        use crate::tools::registry::CoreToolRuntime;
        let reuse = if tool_name_matches(tool_name, "update_plan") {
            crate::tools::handlers::PlanHandler.terminal_failure_reuse()
        } else if tool_name_matches(tool_name, "read_tool_output") {
            crate::tools::handlers::ReadToolOutputHandler.terminal_failure_reuse()
        } else {
            crate::tools::registry::TerminalFailureReuse::Never
        };
        self.register_deterministic_tool_call_with_reuse(tool_name, payload, current_call_id, reuse)
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

    #[cfg(test)]
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
        // A nested report/export completes its own operation, not the
        // enclosing cell's assignment. Only that cell may opt in to delivery
        // of its final result after all of its work has settled.
        if nested && matches!(adapter, "agent_job_report" | "agent_job_csv_export") {
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

    #[cfg(test)]
    pub(crate) fn record_code_mode_result(&self, result: CodeModeToolResult<'_>) {
        self.record_code_mode_result_for_call(None, result);
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
            #[cfg(test)]
            { outcome.validation_error_sites = validation_error_sites(result, signal); }
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



    #[cfg(test)]
    fn deterministic_cycle_key(&self) -> Option<String> {
        self.deterministic_cycle().map(|cycle| cycle.key)
    }

    #[cfg(test)]
    fn failure_fingerprint(&self) -> Option<String> {
        self.deterministic_cycle()
            .and_then(|cycle| cycle.failure_fingerprint)
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

    #[cfg(test)]
    pub(crate) fn has_process_monitor(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.process_monitor_ordinals.is_empty()
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

    #[cfg(test)]
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

/// Files a failed compile reported errors in. This locates errors; it does not
/// prove which writer caused them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(test)]
struct ValidationErrorSites {
    /// The primary file of each reported error.
    primary: BTreeSet<String>,
    /// Every file a reported error references, its primary file included.
    referenced: BTreeSet<String>,
    /// The directory relative locations resolve against, when recorded.
    cwd: Option<std::path::PathBuf>,
    /// Directories of the packages Cargo could not compile. Cargo reports a
    /// workspace member's locations relative to the workspace root, which is
    /// an ancestor of the member's directory rather than the command's.
    package_dirs: Vec<std::path::PathBuf>,
}

/// Larger error sets are never classified as external.
#[cfg(test)]
const MAX_VALIDATION_ERROR_SITE_FILES: usize = 64;

/// A chunk, a reduced projection or a partial poll cannot show every error, so
/// only the exact complete streams of an exited process are parsed.
#[cfg(test)]
fn validation_error_sites(result: &Value, signal: Option<&Value>) -> Option<ValidationErrorSites> {
    if result["streams_complete"] != true {
        return None;
    }
    let streams = [result["stdout"].as_str()?, result["stderr"].as_str()?];
    let (primary, referenced) = rustc_error_sites(streams)?;
    Some(ValidationErrorSites {
        primary,
        referenced,
        cwd: signal
            .and_then(|signal| signal["command_validation"]["execution_context"]["cwd"].as_str())
            .map(std::path::PathBuf::from),
        package_dirs: failed_package_dirs(streams),
    })
}

/// Absolute directories, from Cargo's `Checking name v1.0.0 (dir)` progress
/// lines, of the packages it reported it could not compile.
#[cfg(test)]
fn failed_package_dirs<'a>(
    streams: impl IntoIterator<Item = &'a str>,
) -> Vec<std::path::PathBuf> {
    let mut failed = BTreeSet::new();
    let mut dirs = BTreeMap::new();
    for line in streams.into_iter().flat_map(str::lines).map(str::trim) {
        if let Some(rest) = line.strip_prefix("error: could not compile `") {
            failed.extend(rest.split_once('`').map(|(name, _)| name));
        } else if let Some(rest) = line
            .strip_prefix("Compiling ")
            .or_else(|| line.strip_prefix("Checking "))
            && let Some((name, rest)) = rest.split_once(' ')
            && let Some((_, dir)) = rest.split_once(" (")
            && let Some(dir) = dir.strip_suffix(')')
        {
            dirs.insert(name, std::path::Path::new(dir));
        }
    }
    failed
        .into_iter()
        .filter_map(|name| dirs.get(name).copied())
        .filter(|dir| dir.is_absolute())
        .map(std::path::Path::to_path_buf)
        .collect()
}

/// Files of every error in rendered rustc/cargo diagnostics. Any reported error
/// without a source location (linker, build script, failed test run, internal
/// compiler error, unrecognized format) leaves the set unknown.
#[cfg(test)]
fn rustc_error_sites<'a>(
    streams: impl IntoIterator<Item = &'a str>,
) -> Option<(BTreeSet<String>, BTreeSet<String>)> {
    let mut primary = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    for stream in streams {
        // `Some(located)` inside an error diagnostic.
        let mut error = None;
        for line in stream.lines().map(str::trim_end) {
            if let Some(is_error) = rustc_diagnostic_header(line) {
                if error == Some(false) {
                    return None;
                }
                error = is_error.then_some(false);
                continue;
            }
            let Some(located) = error else {
                continue;
            };
            let trimmed = line.trim_start();
            let (is_primary, location) = match trimmed.strip_prefix("--> ") {
                Some(location) => (true, location),
                None => match trimmed.strip_prefix("::: ") {
                    Some(location) => (false, location),
                    None => continue,
                },
            };
            let file = rustc_location_file(location)?;
            if is_primary && !located {
                primary.insert(file.to_string());
                error = Some(true);
            }
            referenced.insert(file.to_string());
            if referenced.len() > MAX_VALIDATION_ERROR_SITE_FILES {
                return None;
            }
        }
        if error == Some(false) {
            return None;
        }
    }
    (!primary.is_empty()).then_some((primary, referenced))
}

/// `Some(true)` for an error header, `Some(false)` for a warning or a summary
/// of errors already reported, `None` for any other line.
#[cfg(test)]
fn rustc_diagnostic_header(line: &str) -> Option<bool> {
    let (is_error, rest) = match line.strip_prefix("error") {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix("warning")?),
    };
    let rest = match rest.strip_prefix('[') {
        Some(code) => code.split_once(']')?.1,
        None => rest,
    };
    let message = rest.strip_prefix(": ")?;
    Some(
        is_error
            && !["could not compile `", "aborting due to ", "recipe `", "Recipe `"]
                .iter()
                .any(|summary| message.starts_with(summary)),
    )
}

/// The file of `file:line:column`; Windows drive prefixes keep their `:`.
#[cfg(test)]
fn rustc_location_file(location: &str) -> Option<&str> {
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?;
    let line = parts.next()?;
    let file = parts.next()?;
    let numeric = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    (!file.is_empty() && numeric(column) && numeric(line)).then_some(file)
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

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct CompletionReadiness {
    pub(crate) mutation_revision: u64,
    pub(crate) plan_revision: Option<String>,
    pub(crate) live_work: Vec<String>,
    pub(crate) plan_work: Vec<String>,
    pub(crate) plan_requirement_ids: Vec<String>,
    pub(crate) plan_acceptance_ids: std::collections::BTreeMap<String, Vec<String>>,
    pub(crate) validation_work: Vec<String>,
    pub(crate) validation_check_ids: Vec<String>,
    pub(crate) validation_acceptance_ids: BTreeMap<String, Vec<String>>,
}

#[cfg(test)]
impl CompletionReadiness {
    pub(crate) fn remaining(&self, _blocked: bool, _blocked_ids: &[String]) -> Vec<String> {
        self.live_work.clone()
    }

}

pub(crate) struct TurnExecutionControl {
    issued_directives: BTreeSet<String>,
    validation_mode: ValidationMode,
    validation_budget_exhausted: bool,



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
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_with_timing(Arc::new(TurnTimingState::default()))
    }

    pub(crate) fn new_with_timing(timing: Arc<TurnTimingState>) -> Self {
        Self {
            issued_directives: BTreeSet::new(),
            validation_mode: ValidationMode::Full,
            validation_budget_exhausted: false,



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

    pub(crate) fn set_validation_policy(&mut self, mode: ValidationMode, exhausted: bool) {
        self.validation_mode = mode;
        self.validation_budget_exhausted = exhausted;
    }

    pub(crate) fn validation_repair_available(&self) -> bool {
        self.validation_mode != ValidationMode::Off && !self.validation_budget_exhausted
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




#[cfg(test)]
    /// Only this turn's running processes delay final delivery.
    pub(crate) fn completion_readiness(&self, mutation_revision: u64) -> CompletionReadiness {
        let mut readiness = CompletionReadiness {
            mutation_revision,
            ..Default::default()
        };
        readiness.live_work.extend(self.pending_validation_processes.iter()
            .map(|(process, pending)| format!("Consume the terminal result of existing validation {process}: {}. Use its current owner capability, not the stored process number; do not launch a duplicate.", pending.check)));
        readiness
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

    /// Production completion passes runtime instruction inputs explicitly.
    #[cfg(test)]
    pub(crate) fn completion_assessment_with_changed_paths(
        &self,
        settled_mutation_revision: u64,
        changed_paths: Option<&[(String, std::path::PathBuf)]>,
        has_untracked_changes: bool,
    ) -> codex_protocol::protocol::TurnCompletionAssessment {
        self.completion_assessment_with_runtime_inputs(
            settled_mutation_revision,
            changed_paths,
            has_untracked_changes,
            &[],
        )
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

    #[cfg(test)]
    fn completion_gaps(&self, revision: u64) -> Vec<String> {
        let report = self.completion_assessment(revision);
        report
            .failed_checks
            .into_iter()
            .chain(report.verification_gaps)
            .chain(report.advisories)
            .collect()
    }

    #[cfg(test)]
    fn completion_gaps_with_changed_paths(
        &self,
        revision: u64,
        paths: Option<&[(String, std::path::PathBuf)]>,
        untracked: bool,
    ) -> Vec<String> {
        let report = self.completion_assessment_with_changed_paths(revision, paths, untracked);
        report
            .failed_checks
            .into_iter()
            .chain(report.verification_gaps)
            .chain(report.advisories)
            .collect()
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

    #[cfg(test)]
    pub(crate) fn observe_budget_progress(
        &mut self,
        baselines: &SamplingRequestBaselines,
        signals: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> bool {
        !self
            .observe_progress(baselines, signals, settled)
            .is_empty()
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
            attributed_mutation_revision: mutation_revision,
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

    #[cfg(test)]
    pub(crate) fn continuation_generation_request(
        &self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
        has_pending_input: bool,
    ) -> GenerationRequestDisposition {
        self.continuation_generation_request_with_analysis(
            baselines,
            &collector.analyze_settled_request(),
            settled,
            has_pending_input,
        )
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

    /// Production input acceptance passes whether instructions changed.
    #[cfg(test)]
    pub(crate) fn accepted_user_input(&mut self) {
        self.accepted_context_input(true);
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

    #[cfg(test)]
    pub(crate) fn evaluate_convergence(
        &mut self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) -> SamplingConvergenceDecision {
        self.evaluate_convergence_with_analysis(
            baselines,
            &collector.analyze_settled_request(),
            settled,
        )
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
            attributed_mutation_revision: mutation_revision,
            tool_exposure_revision: 0,
        }
    }

    // Retention fixtures cannot authorize reuse: their watcher identity is invalid.
    // The freshness regression uses a real cache registration instead.


    fn record_invocation_result(
        collector: &SamplingRequestSignalCollector,
        tool_name: ToolName,
        payload: ToolPayload,
        call_id: &str,
        outcome: ToolOutputOutcome,
    ) {
        let registration =
            collector.register_deterministic_tool_call(&tool_name, &payload, call_id);
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(outcome),
            validation_invocation_status(&tool_name, &payload)
                .2
                .then(test_execution_signal),
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

    fn test_execution_signal() -> Value {
        json!({"command_validation": {"validation": true, "tests": true}})
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
        control.refresh_plan(Some(crate::plan_store::PlanExecutionSnapshot::new(
            &plan,
            &crate::plan_store::PlanLineage::default(),
        )));
        let baselines = control.baselines(0);
        let collector = SamplingRequestSignalCollector::default();
        collector.push(SamplingToolOutcome::plain(
            0,
            SamplingToolOutcomeKind::Success,
            Some(plan),
        ));
        control.settle(&baselines, &collector, &settled(0));
    }

    #[test]
    fn status_only_plan_update_refreshes_owner_revision_and_obligations() {
        let mut control = TurnExecutionControl::new();
        settle_plan(
            &mut control,
            plan(&[StepStatus::InProgress, StepStatus::Pending]),
        );
        let revision = control.plan_revision;
        settle_plan(
            &mut control,
            plan(&[StepStatus::Completed, StepStatus::Completed]),
        );
        assert!(control.plan_completed());
        assert!(control.completion_gaps(0).is_empty());
        assert_eq!(control.plan_revision, revision + 1);
        settle_plan(
            &mut control,
            plan(&[
                StepStatus::Completed,
                StepStatus::Completed,
                StepStatus::Pending,
            ]),
        );
        assert!(!control.plan_completed());
        assert_eq!(control.plan_revision, revision + 2);
    }

    #[test]
    fn plan_completion_hint_does_not_survive_new_input_or_turn_inheritance() {
        let completed = plan(&[StepStatus::Completed]);
        let snapshot =
            crate::plan_store::PlanExecutionSnapshot::new(&completed, &Default::default());
        let mut control = TurnExecutionControl::new().with_active_plan(Some(snapshot.clone()));
        assert!(!control.plan_completed());
        assert!(
            control.completion_gaps(0).is_empty(),
            "completed history is not new task debt"
        );
        settle_plan(&mut control, plan(&[StepStatus::Pending]));
        settle_plan(&mut control, completed);
        assert!(control.plan_completed());
        control.accepted_user_input();
        control.refresh_plan(Some(snapshot));
        assert!(
            !control.plan_completed(),
            "unchanged old completion cannot qualify a new request"
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
    fn progress_telemetry_fingerprints_actual_actions_not_call_ids_or_outcomes() {
        let fingerprint = |tool: &str, arguments: &str, call_id: &str| {
            let collector = SamplingRequestSignalCollector::default();
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain(tool),
                &ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
                call_id,
            );
            collector.record_failure(registration.ordinal, call_id, false);
            collector.structured_action_fingerprint()
        };
        let first = fingerprint(
            "read_file",
            r#"{"path":"a","environment_id":"local"}"#,
            "one",
        );
        assert!(first.is_some());
        assert_eq!(
            first,
            fingerprint(
                "read_file",
                r#"{"environment_id":"local","path":"a"}"#,
                "two"
            )
        );
        assert_ne!(first, fingerprint("read_file", r#"{"path":"b"}"#, "one"));
        assert_ne!(
            first,
            fingerprint(
                "list_files",
                r#"{"path":"a","environment_id":"local"}"#,
                "one"
            )
        );
        assert_eq!(fingerprint("read_file", "{", "invalid"), None);
        assert_ne!(
            first,
            SamplingRequestSignalCollector::default().structured_action_fingerprint()
        );
    }

    #[test]
    fn progress_telemetry_counts_new_source_evidence_and_repeated_read_generations() {
        let timing = Arc::new(TurnTimingState::default());
        timing.mark_turn_started();
        let mut control = TurnExecutionControl::new_with_timing(Arc::clone(&timing));
        let baselines = control.baselines(0);
        // Unscoped command output is bound to the command that produced it:
        // the same bytes from another path are new provenance, repeats are not.
        for (index, (path, text, novel)) in [
            ("a", "first", true),
            ("a", "first", false),
            ("a", "changed", true),
            ("b", "changed", true),
            ("b", "changed", false),
            ("b", "changed", false),
        ]
        .into_iter()
        .enumerate()
        {
            let evidence_before = control.baselines(0).relevant_state_fingerprint();
            let mut pending =
                (index > 0).then_some(crate::turn_timing::ContinuationCause::ToolResult);
            timing.begin_model_generation_with_metadata(
                &mut pending,
                &codex_protocol::protocol::SessionSource::Cli,
                Some(TurnTimingGenerationPurpose::ArtifactContinuation),
                TurnTimingGenerationDisposition::DecisionBearing,
                Some(baselines.relevant_state_fingerprint()),
            );
            drop(timing.begin_model_request_wait());
            let collector = control.collector(&baselines);
            let call_id = format!("read-{index}");
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("shell_command"),
                &ToolPayload::Function {
                    arguments: json!({"command": format!("cat {path}")}).to_string(),
                },
                &call_id,
            );
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                Some(crate::tools::context::semantic_evidence_sampling_signal(
                    json!(crate::tools::context::successful_command_evidence(
                        text.as_bytes(),
                        None
                    )),
                )),
                &successful_tool_response(&call_id, text),
                false,
            );
            let progress = control.observe_progress(&baselines, &collector, &settled(0));
            let evidence_after = control.baselines(0).relevant_state_fingerprint();
            assert_eq!(evidence_before != evidence_after, novel);
            assert_eq!(
                control
                    .continuation_generation_request(&baselines, &collector, &settled(0), false)
                    .relevant_state_fingerprint,
                evidence_after,
            );
            assert_eq!(
                progress,
                if novel {
                    vec![TurnTimingProgressKind::NewSourceEvidence]
                } else {
                    Vec::new()
                }
            );
            timing.record_generation_outcome(
                progress.clone(),
                collector.structured_action_fingerprint(),
                collector.has_no_structured_actions(),
                progress.is_empty(),
            );
        }
        let timing = timing.complete_snapshot().protocol_timing();
        assert_eq!(
            timing.observational_nonprogress_latency.logical_generations,
            3
        );
        assert_eq!(
            timing
                .model_requests
                .iter()
                .map(|request| request.unchanged_relevant_state)
                .collect::<Vec<_>>(),
            vec![false, true, false, false, true, true]
        );
        assert!(
            timing
                .model_requests
                .last()
                .unwrap()
                .next_structured_action_changed
        );
    }

    #[test]
    fn progress_telemetry_reports_only_novel_validation_and_failure_evidence() {
        for failed in [false, true] {
            let mut control = TurnExecutionControl::new();
            let baselines = control.baselines(0);
            for (index, (evidence, novel)) in [
                ("first result", true),
                ("first result", false),
                ("changed result", true),
                ("changed result", false),
            ]
            .into_iter()
            .enumerate()
            {
                let collector = control.collector(&baselines);
                let call_id = format!("validation-{index}");
                let registration = collector.register_deterministic_tool_call(
                    &ToolName::plain("exec_command"),
                    &validation_proof_payload(),
                    &call_id,
                );
                collector.record_response_result(
                    registration.ordinal,
                    ToolOutputOutcomeContext::new(if failed {
                        ToolOutputOutcome::Failure
                    } else {
                        ToolOutputOutcome::Success
                    }),
                    failed.then(|| json!({"failure_signature": evidence})),
                    &runner_tool_response(&call_id, evidence),
                    false,
                );
                let expected = if !novel {
                    Vec::new()
                } else if failed {
                    vec![
                        TurnTimingProgressKind::ValidationResult,
                        TurnTimingProgressKind::FailureObservation,
                    ]
                } else {
                    vec![TurnTimingProgressKind::ValidationResult]
                };
                assert_eq!(
                    control.observe_progress(&baselines, &collector, &settled(0)),
                    expected
                );
            }
        }
    }

    #[test]
    fn recovery_output_is_not_test_execution() {
        let mut control = TurnExecutionControl::new();
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("read_tool_output"),
            &ToolPayload::Function { arguments: json!({"artifact_id":"retained"}).to_string() },
            "reused-validation",
        );
        let response = runner_tool_response("reused-validation", "Ran 1 test in 0.01s\n\nOK\n");
        collector.record_response_result(registration.ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success), None, &response, false);
        assert!(
            !control
                .observe_progress(&baselines, &collector, &settled(0))
                .contains(&TurnTimingProgressKind::ValidationResult)
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
            Some(test_execution_signal()),
            &response,
            false,
        );
        assert!(
            control
                .observe_progress(&baselines, &executed, &settled(0))
                .contains(&TurnTimingProgressKind::ValidationResult)
        );
    }

    #[test]
    fn completion_evidence_tracks_result_changes_without_call_id_noise() {
        let control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let evidence = |call_id: &str, output: &str| {
            let collector = control.collector(&baseline);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                call_id,
            );
            let response = runner_tool_response(call_id, output);
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None, &response, false,
            );
            collector.completion_evidence_key()
        };
        let first = evidence("first", "observed state A");
        assert!(first.is_some());
        assert_eq!(
            first,
            evidence("different-call-id", "observed state A")
        );
        assert_ne!(first, evidence("first", "observed state B"));
    }

    #[test]
    fn deferred_tool_activation_changes_relevant_state_identity() {
        let control = TurnExecutionControl::new();
        let baselines = control.baselines_with_tool_exposure_revision(0, 4);
        let settled = SamplingRequestSettledState {
            mutation_revision: 0,
            attributed_mutation_revision: 0,
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
            // These tools never opt in to failure reuse, so the registration
            // below cannot be suppressed either way. The gate is what the
            // retryable flag controls.
            assert!(!first.state.lock().unwrap().outcomes[0].failure_is_terminal);
            assert!(
                control
                    .dispatch_ledger
                    .lock()
                    .unwrap()
                    .repeated_failure_gate
                    .is_none(),
                "retryable {tool_name} failure must not arm suppression"
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

        assert!(!first.snapshot()[0].failure_is_terminal);
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
        for nested in [false, true] {
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
            if nested {
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
            } else {
                collector.record_response_result(
                    collector.register_tool_call(),
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                    Some(signal),
                    &runner_tool_response("mcp-call", &result.to_string()),
                    false,
                );
            }
            let outcomes = collector.snapshot();
            assert_eq!(outcomes.len(), 1);
            assert_eq!(outcomes[0].kind, SamplingToolOutcomeKind::Failure);
            assert!(!outcomes[0].failure_is_terminal, "nested={nested}");
        }
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
    fn registered_semantics_classify_aliases_and_reject_name_spoofing() {
        use crate::tools::registry::CommandArgumentFormat;
        use crate::tools::registry::ToolSemanticCapabilities;
        let collector = SamplingRequestSignalCollector::default();
        let alias = ToolName::new(Some("local".to_string()), "check_project".to_string());
        collector.register_runtime_semantics(
            &alias,
            ToolSemanticCapabilities {
                command: Some(CommandArgumentFormat::Exec),
                ..Default::default()
            },
        );
        record_invocation_result(
            &collector,
            alias,
            ToolPayload::Function {
                arguments: r#"{"cmd":"cargo check -p codex-core"}"#.into(),
            },
            "alias-check",
            ToolOutputOutcome::Success,
        );
        collector.record_child_runtime_for_call("alias-check", 25);
        collector.register_runtime_semantics(
            &ToolName::plain("exec_command"),
            ToolSemanticCapabilities::default(),
        );
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: r#"{"cmd":"cargo check -p codex-core"}"#.into(),
            },
            "not-a-command-runtime",
            ToolOutputOutcome::Success,
        );
        {
            // Only the alias with command semantics is classified as validation.
            let state = collector.state.lock().unwrap();
            assert_eq!(state.validation_ordinals.len(), 1);
            assert_eq!(state.child_runtime_by_call.get("alias-check"), Some(&25));
        }
        let edit = ToolName::plain("custom_editor");
        collector.register_runtime_semantics(
            &edit,
            ToolSemanticCapabilities {
                mutation: true,
                coordination: true,
                command: None,
            },
        );
        record_invocation_result(
            &collector,
            edit,
            ToolPayload::Function {
                arguments: "{}".into(),
            },
            "custom-edit",
            ToolOutputOutcome::Success,
        );
        let state = collector.state.lock().unwrap();
        assert!(state.saw_mutation && state.saw_coordination);
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

        assert_eq!(control.validated_mutation_revision, Some(0));
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
            ("exec_command", json!({"cmd": "just test"})),
            ("exec_command", json!({"cmd": "npm run test:unit"})),
            ("exec_command", json!({"cmd": "uv run pytest -q"})),
            (
                "exec_command",
                json!({"kind": "argv", "program": "cargo", "args": ["test", "-p", "example"]}),
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
                collector.state.lock().unwrap().validation_ordinals.len(),
                1,
                "{tool}: {arguments}"
            );
            assert_eq!(
                control.validated_mutation_revision,
                Some(0),
                "{tool}: {arguments}"
            );
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
    fn masked_or_skipped_validation_cannot_create_proof() {
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
                        control.validated_mutation_revision.is_none(),
                        "{arguments}; nested={nested}"
                    );
                }
            }
        }
    }

    #[test]
    fn metadata_cannot_turn_non_validation_commands_into_proof() {
        let root = tempfile::tempdir().unwrap();
        let dependencies =
            BTreeSet::from([SourceDependencyV1::new(&root.path().join("src"), true)]);
        let tool = ToolName::plain("exec_command");
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
            let payload = ToolPayload::Function {
                arguments: json!({
                    "cmd": command,
                    "validation": {"covered_paths": ["src"]},
                })
                .to_string(),
            };
            // The dispatcher attaches attribution before recording. Known
            // dependencies let a declared path become a scope that settlement
            // would admit as proof.
            let signal = validation_scope_signal(
                &tool,
                &payload,
                None,
                Some(&dependencies),
                root.path(),
                "local",
            );
            let ordinal = collector
                .register_deterministic_tool_call(&tool, &payload, "not-validation")
                .ordinal;
            collector.record_response_result(
                ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                signal,
                &successful_tool_response("not-validation", r#"{"status":"complete"}"#),
                false,
            );
            control.settle(&baselines, &collector, &settled_state);
            assert!(
                collector
                    .state
                    .lock()
                    .unwrap()
                    .validation_ordinals
                    .is_empty(),
                "{command}"
            );
            assert!(control.validated_mutation_revision.is_none(), "{command}");
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
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "validation-cell",
                tool_name: &ToolName::plain("exec_command"),
                payload: &validation_proof_payload(),
                source_dependencies: None,
                outcome_context: ToolOutputOutcomeContext::new(outcome),
                signal: Some(&test_execution_signal()),
                // The runner-owned signal proves tests ran; this isolates the
                // outcome, not the execution evidence.
                result: &json!({
                    "exit_code": if outcome == ToolOutputOutcome::Success { 0 } else { 1 },
                    "output": "Ran 1 test in 0.001s\nOK",
                }),
                canonical_artifact_required: false,
            });
            control.settle(&baselines, &collector, &settled_state);
            assert_eq!(
                control.validated_mutation_revision,
                (outcome == ToolOutputOutcome::Success).then_some(0),
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

            assert!(control.validated_mutation_revision.is_none());
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
        assert!(control.validated_mutation_revision.is_none());
    }

    #[test]
    fn final_diff_status_observation_requires_exact_git_executable() {
        for (program, final_verification) in [
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
            assert_eq!(
                collector
                    .state
                    .lock()
                    .unwrap()
                    .final_verification_ordinals
                    .len(),
                usize::from(final_verification),
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
    fn unsafe_final_observations_are_not_classified_as_final_verification() {
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

            // Only the exact read-only pair counts as a final verification.
            assert_eq!(
                collector
                    .state
                    .lock()
                    .unwrap()
                    .final_verification_ordinals
                    .len(),
                1
            );
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
    fn a_successful_validation_after_failure_does_not_force_completion() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        let failed_baselines = control.baselines(0);
        let failed =
            recorded_validation_collector(&control, &failed_baselines, ToolOutputOutcome::Failure);
        control.settle(&failed_baselines, &failed, &settled(0));

        let recovery_baselines = control.baselines(0);
        let recovered = recorded_validation_collector(
            &control,
            &recovery_baselines,
            ToolOutputOutcome::Success,
        );
        control.settle(&recovery_baselines, &recovered, &settled(0));
        assert_eq!(control.validated_mutation_revision, Some(0));
        assert!(control.failed_validation_checks.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&recovery_baselines, &recovered, &settled(0))
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn force_fresh_commands_have_no_structured_reuse_identity() {
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
                    collector
                        .state
                        .lock()
                        .unwrap()
                        .structured_actions
                        .is_empty()
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
    fn unfinished_plan_does_not_invalidate_fresh_validation_evidence() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Completed]));
        settle_plan(&mut control, plan(&[StepStatus::InProgress]));
        let baselines = control.baselines(0);
        let settled_state = settled(0);
        let collector =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &collector, &settled_state);

        assert_eq!(control.validated_mutation_revision, Some(0));
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
            control.settle(&baselines, &collector, &settled_state);
            assert_eq!(
                control.validated_mutation_revision,
                Some(revision),
                "{scenario}"
            );
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
            attributed_mutation_revision: 7,
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
    fn identical_owner_receipts_join_but_conflicting_revisions_do_not() {
        let control = TurnExecutionControl::new();
        let (baselines, _) = unchanged_state(&control);
        let collector = authoritative_wait_collector(&control, &baselines, "same", false, None);
        {
            let mut state = collector.state.lock().unwrap();
            let duplicate = state.authoritative_wait_observations[0].clone();
            state.authoritative_wait_observations.push(duplicate);
            state.registered_count = 2;
            state.direct_wait_agent_count = 2;
        }
        assert!(collector.authoritative_wait_observation().is_some());
        collector
            .state
            .lock()
            .unwrap()
            .authoritative_wait_observations[1]
            .state_revision = "other".into();
        assert!(collector.authoritative_wait_observation().is_none());
    }

    #[test]
    fn native_receipt_requires_settled_success_not_plan_bookkeeping() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let collector =
            authoritative_wait_collector(&control, &baselines, "native", false, Some("{}"));
        collector
            .state
            .lock()
            .unwrap()
            .authoritative_wait_observations[0]
            .result
            .adapter = "agent_job_report".into();
        assert!(
            collector.authoritative_wait_observation().is_none(),
            "pending producer"
        );
        collector
            .state
            .lock()
            .unwrap()
            .outcomes
            .push(SamplingToolOutcome::plain(
                0,
                SamplingToolOutcomeKind::Success,
                None,
            ));
        assert!(collector.authoritative_wait_observation().is_some());
        {
            let mut state = collector.state.lock().unwrap();
            state.outcomes.push(SamplingToolOutcome::plain(
                0,
                SamplingToolOutcomeKind::Success,
                None,
            ));
            assert_eq!(state.outcomes.len(), 2);
        }
        assert!(
            collector.authoritative_wait_observation().is_none(),
            "duplicate is not another completion"
        );
        {
            let mut state = collector.state.lock().unwrap();
            state.outcomes.pop();
            state.outcomes[0].kind = SamplingToolOutcomeKind::Failure;
        }
        assert!(
            collector.authoritative_wait_observation().is_none(),
            "failed producer"
        );
        collector.state.lock().unwrap().outcomes[0].kind = SamplingToolOutcomeKind::Success;
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &collector, &settled)
                .continuation,
            ContinuationDisposition::SurfaceExistingResult
        );
        settle_plan(&mut control, plan(&[StepStatus::InProgress]));
        let baselines = control.baselines(settled.mutation_revision);
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &collector, &settled)
                .continuation,
            ContinuationDisposition::SurfaceExistingResult
        );
    }

    #[test]
    fn explicit_delivery_requires_consumed_process_and_complete_evidence() {
        for scenario in [
            "complete",
            "running",
            "failed",
            "unrelated",
            "missing",
            "duplicate",
            "truncated",
            "out-of-order",
        ] {
            let collector = SamplingRequestSignalCollector::default();
            {
                let mut state = collector.state.lock().unwrap();
                state.registered_count = 1;
                state.code_mode_nested_tool_count = 2;
                state.explicit_completion = Some((0, "verified answer".into()));
                let mut started =
                    SamplingToolOutcome::plain(1, SamplingToolOutcomeKind::Yielded, None);
                started.background_process_id = Some(7);
                let mut observed =
                    SamplingToolOutcome::plain(2, SamplingToolOutcomeKind::Success, None);
                observed.observed_process_id = Some(7);
                match scenario {
                    "running" => {
                        observed.kind = SamplingToolOutcomeKind::Yielded;
                        observed.background_process_id = Some(7);
                    }
                    "failed" => observed.kind = SamplingToolOutcomeKind::Failure,
                    "unrelated" => observed.observed_process_id = Some(8),
                    "duplicate" => observed.ordinal = 1,
                    "truncated" => observed.canonical_artifact_required = true,
                    "out-of-order" => {
                        started.ordinal = 2;
                        observed.ordinal = 1;
                    }
                    _ => {}
                }
                state.outcomes = vec![
                    SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None),
                    started,
                ];
                if scenario != "missing" {
                    state.outcomes.push(observed);
                }
            }
            assert_eq!(
                collector.explicit_completion().is_some(),
                scenario == "complete",
                "{scenario}"
            );
        }
    }

    #[test]
    fn completion_boundary_retains_pending_validation_across_requests() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let launch = control.collector(&baseline);
        let ordinal = launch
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                "validation",
            )
            .ordinal;
        launch.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7})),
            &successful_tool_response("validation", "running"),
            false,
        );
        control.settle(&baseline, &launch, &settled(0));
        assert_eq!(control.pending_validation_processes.len(), 1);
        let delivery = control.collector(&baseline);
        {
            let mut state = delivery.state.lock().unwrap();
            state.registered_count = 1;
            state.explicit_completion = Some((0, "answer".into()));
            state.outcomes.push(SamplingToolOutcome::plain(
                0,
                SamplingToolOutcomeKind::Success,
                None,
            ));
        }
        assert_ne!(
            control
                .evaluate_convergence(&baseline, &delivery, &settled(0))
                .continuation,
            ContinuationDisposition::SurfaceExistingResult
        );
        let poll = control.collector(&baseline);
        let ordinal = poll
            .register_deterministic_tool_call(
                &ToolName::plain("write_stdin"),
                &ToolPayload::Function {
                    arguments: json!({"session_id":7}).to_string(),
                },
                "poll",
            )
            .ordinal;
        let mut signal = test_execution_signal();
        signal["observed_process_id"] = json!(7);
        poll.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal),
            &successful_tool_response("poll", "passed"),
            false,
        );
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_processes.is_empty());
        assert_eq!(
            control
                .evaluate_convergence(&baseline, &delivery, &settled(0))
                .continuation,
            ContinuationDisposition::SurfaceExistingResult
        );
    }

    #[test]
    fn failed_check_and_open_plan_do_not_block_final_delivery() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Pending]));
        let baseline = control.baselines(0);
        let failed = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Failure);
        control.settle(&baseline, &failed, &settled(0));
        let report = control.completion_assessment(1);
        assert_eq!(report.failed_checks.len(), 1);
        assert!(report.verification_gaps.is_empty());
        assert!(report.advisories.is_empty());
        assert!(control.completion_readiness(1).remaining(false, &[]).is_empty());
    }

    #[test]
    fn completion_boundary_unrelated_tool_output_is_not_hook_repair_evidence() {
        let collector = SamplingRequestSignalCollector::default();
        let ordinal = collector
            .register_deterministic_tool_call(
                &ToolName::plain("read_file"),
                &ToolPayload::Function {
                    arguments: json!({"path":"unrelated.txt"}).to_string(),
                },
                "read",
            )
            .ordinal;
        collector.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None,
            &successful_tool_response("read", "new but unrelated"),
            false,
        );
        assert!(collector.completion_evidence_key().is_none());
    }

    #[test]
    fn direct_result_requires_unchanged_input_not_closed_plan() {
        for (status, new_input, expected) in [
            (StepStatus::Pending, false, true),
            (StepStatus::InProgress, false, true),
            (StepStatus::Completed, false, true),
            (StepStatus::Completed, true, false),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[status]));
            let (baselines, settled) = unchanged_state(&control);
            let collector = SamplingRequestSignalCollector::default();
            {
                let mut state = collector.state.lock().unwrap();
                state.registered_count = 1;
                state.explicit_completion = Some((0, "verified answer".into()));
                state.outcomes.push(SamplingToolOutcome::plain(
                    0,
                    SamplingToolOutcomeKind::Success,
                    None,
                ));
            }
            if new_input {
                control.input_revision += 1;
            }
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation == ContinuationDisposition::SurfaceExistingResult,
                expected,
                "{status:?}, new_input={new_input}"
            );
            if !expected {
                assert_eq!(
                    decision.continuation,
                    ContinuationDisposition::ModelRequired
                );
            }
        }
    }

    #[test]
    fn completion_gaps_are_reported_without_automatic_repair_directives() {
        // A native receipt always designates its text: the owner-result
        // constructor rejects a receipt-only report before an observation exists.
        for mode in ["explicit", "native-text"] {
            let mut control = TurnExecutionControl::new();
            let before = control.baselines(0);
            let validation =
                recorded_validation_collector(&control, &before, ToolOutputOutcome::Success);
            control.settle(&before, &validation, &settled(0));
            // Validation exists, but a later settled change invalidates it.
            let baselines = control.baselines(1);
            let receipt = if mode == "explicit" {
                let collector = control.collector(&baselines);
                {
                    let mut state = collector.state.lock().unwrap();
                    state.registered_count = 1;
                    state.explicit_completion = Some((0, "verified answer".into()));
                    state.outcomes.push(SamplingToolOutcome::plain(
                        0,
                        SamplingToolOutcomeKind::Success,
                        None,
                    ));
                }
                collector
            } else {
                let collector = authoritative_wait_collector(
                    &control,
                    &baselines,
                    "native",
                    false,
                    Some("verified answer"),
                );
                {
                    let mut state = collector.state.lock().unwrap();
                    state.authoritative_wait_observations[0].result.adapter =
                        "agent_job_report".into();
                    state.outcomes.push(SamplingToolOutcome::plain(
                        0,
                        SamplingToolOutcomeKind::Success,
                        None,
                    ));
                }
                collector
            };
            let stale = control.evaluate_convergence(&baselines, &receipt, &settled(1));
            let expected = ContinuationDisposition::SurfaceExistingResult;
            assert!(!control.completion_gaps(1).is_empty());
            assert_eq!(stale.continuation, expected);
            assert_eq!(stale.directive, None, "{mode}");
            let failed =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Failure);
            control.settle(&baselines, &failed, &settled(1));
            assert_eq!(
                control
                    .evaluate_convergence(&baselines, &receipt, &settled(1))
                    .continuation,
                expected,
                "failed validation remains a reportable gap, not an automatic continuation"
            );
            assert!(!control.completion_gaps(1).is_empty());
            let passed =
                recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            control.settle(&baselines, &passed, &settled(1));
            assert_eq!(
                control
                    .evaluate_convergence(&baselines, &receipt, &settled(1))
                    .continuation,
                expected,
                "fresh evidence surfaces the existing receipt"
            );
            settle_plan(&mut control, plan(&[StepStatus::Pending]));
            let pending = control.baselines(1);
            assert_eq!(
                control
                    .evaluate_convergence(&pending, &receipt, &settled(1))
                    .continuation,
                expected,
                "unfinished plan bookkeeping must not override explicit delivery: {mode}"
            );
        }
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
            attributed_mutation_revision: 8,
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
    fn cosmetic_plan_revisions_do_not_reset_repeated_read_convergence() {
        let mut control = TurnExecutionControl::new();
        let mut checklist = plan(&[StepStatus::Pending, StepStatus::Pending]);
        for generation in 0..4 {
            let (baselines, settled) = unchanged_state(&control);
            checklist.plan.reverse();
            checklist.explanation = Some(format!("presentation {generation}"));
            control.refresh_plan(Some(crate::plan_store::PlanExecutionSnapshot::new(
                &checklist,
                &crate::plan_store::PlanLineage::default(),
            )));
            let (collector, _) = read_only_pass_collector(
                &control,
                &baselines,
                r#"{"artifact_id":"same-source"}"#,
                "same-evidence",
            );
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.directive.is_some(), generation > 0);
        }
    }

    #[tokio::test]
    async fn owner_obligations_override_reordered_observations_and_suspend_in_plan_mode() {
        let store = crate::plan_store::PlanStore::default();
        let checklist = plan(&[StepStatus::Completed]);
        let mut lineage = crate::plan_store::PlanLineage::default();
        lineage.requirements.insert(
            "orphan".into(),
            crate::plan_store::PlanRequirement {
                text: "unmapped remaining obligation".into(),
                status: StepStatus::Pending,
                superseded_reason: None,
            },
        );
        store
            .restore_with_lineage(Some(checklist.clone()), Some(lineage))
            .await;
        let snapshot = store.execution_snapshot().await.unwrap();
        for reverse in [false, true] {
            let mut control = TurnExecutionControl::new().with_active_plan(Some(snapshot.clone()));
            let (baselines, settled) = unchanged_state(&control);
            let collector = control.collector(&baselines);
            for ordinal in if reverse { [100, 0] } else { [0, 100] } {
                collector.push(SamplingToolOutcome::plain(
                    ordinal,
                    SamplingToolOutcomeKind::Success,
                    Some(checklist.clone()),
                ));
            }
            control.refresh_plan(store.execution_snapshot().await);
            control.settle(&baselines, &collector, &settled);
            assert!(
                !control.plan_completed(),
                "compaction must not infer completion from checklist alone"
            );
            assert_eq!(control.plan.as_ref(), Some(&snapshot));
            assert!(control.completion_gaps(0).is_empty());
            control.refresh_plan(None); // The Plan Mode settlement path suspends execution.
            assert!(control.completion_gaps(0).is_empty());
            assert!(!control.plan_completed());
            assert_eq!(store.execution_snapshot().await, Some(snapshot.clone()));
        }
    }

    #[test]
    fn repeated_broad_source_directive_does_not_claim_the_pass_was_suppressed() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let arguments = r#"{"artifact_id":"artifact-1"}"#;

        let mut directive = None;
        for generation in 1..=2 {
            let (collector, _) =
                read_only_pass_collector(&control, &baselines, arguments, "same-evidence");
            directive = control
                .evaluate_convergence(&baselines, &collector, &settled)
                .directive;
            assert_eq!(directive.is_some(), generation > 1);
            assert_eq!(
                control
                    .continuation_generation_request(&baselines, &collector, &settled, false,)
                    .timing_disposition(),
                TurnTimingGenerationDisposition::DecisionBearing,
            );
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
    fn command_evidence_preserves_changed_presentations_of_the_same_source() {
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

        let mut cycle_keys = std::collections::HashSet::new();
        for (tool, arguments, presentation) in calls {
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
                        "identity": crate::tools::context::successful_command_evidence(presentation.as_bytes(), None),
                    },
                })),
                &successful_tool_response("semantic-call", presentation),
                false,
            );
            let current_key = collector
                .deterministic_cycle_key()
                .expect("semantic evidence cycle");
            assert!(cycle_keys.insert(current_key));
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert!(decision.directive.is_none());
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

        assert!(left_deterministic.is_some());
        assert!(left_structured.is_some());
        assert_eq!(left_deterministic, right_deterministic);
        assert_eq!(left_structured, right_structured);
        let changed = ToolPayload::Function {
            arguments: r#"{"target":"agent-2","timeout_ms":10}"#.to_string(),
        };
        let (changed_deterministic, changed_structured) = action_identities(&tool_name, &changed);
        assert_ne!(left_deterministic, changed_deterministic);
        assert_ne!(left_structured, changed_structured);
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
            attributed_mutation_revision: 1,
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
                Some(json!({"command_evidence": true, "semantic_evidence": ["identical file content"]})),
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
    fn partial_recovery_progress_tracks_bytes_independently_of_repeated_errors() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        for (start, new_source, new_failure) in [
            (0, true, true),
            (10, true, false),
            (20, true, false),
            (10, false, false),
        ] {
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("read_tool_output"),
                &ToolPayload::Function {
                    arguments: json!({"artifact_id":"snapshot", "selectors":[
                    {"kind":"bytes", "start":start, "end":start + 10}]})
                    .to_string(),
                },
                "page",
            );
            collector.record_response_result(registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(json!({"outcome":"failure", "failure_signature":"same-selection-error",
                    "semantic_evidence":{"source":"artifact", "scope":"snapshot",
                        "identity":{"sha256":"source", "ranges":[[start, start + 10]], "values":[]}}})),
                &successful_tool_response("page", "partial recovery"), false);
            let progress = control.observe_progress(&baselines, &collector, &settled);
            assert_eq!(
                progress.contains(&TurnTimingProgressKind::NewSourceEvidence),
                new_source
            );
            assert_eq!(
                progress.contains(&TurnTimingProgressKind::FailureObservation),
                new_failure
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

        // Take the contract from the real handler: a write_stdin runtime that
        // opted in to failure reuse would hit the gate installed above.
        use crate::tools::registry::CoreToolRuntime;
        let reuse = crate::tools::handlers::WriteStdinHandler::default().terminal_failure_reuse();
        assert!(
            collector
                .register_deterministic_tool_call_with_reuse(
                    &tool_name,
                    &payload,
                    "poll-again",
                    reuse,
                )
                .suppressed_failure
                .is_none(),
            "a live process can recover without changing the poll arguments"
        );
        assert!(collector.has_process_monitor());
        assert!(!collector.observed_successful_process_monitor());
    }

    #[test]
    fn silent_successful_write_stdin_does_not_refund_a_generation() {
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
        assert!(!collector.observed_successful_process_monitor());
    }

    #[test]
    fn only_the_current_failure_envelope_supplies_a_failure_signature() {
        assert_eq!(
            value_failure_signature(&json!(r#"{"failure_signature":"application-data"}"#)),
            None
        );
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
    fn yielded_write_stdin_with_new_output_refunds_a_generation() {
        for progress in [false, true] {
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
                Some(json!({"process_observation_progress": progress})),
                &successful_tool_response("poll-yielded", r#"{"session_id":7,"running":true}"#),
                false,
            );

            assert!(collector.has_process_monitor());
            assert_eq!(collector.observed_successful_process_monitor(), progress);
            assert_eq!(collector.observed_yielded_execution(), progress);
        }
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
    fn verified_evidence_failure_fingerprints_preserve_structured_values_and_invocations() {
        let fingerprint = |args: Value, result: Value| {
            code_mode_result_failure_fingerprint(
                &ToolName::plain("fixture"),
                &ToolPayload::Function {
                    arguments: args.to_string(),
                },
                &result,
            )
        };
        let base = serde_json::json!({"actual":7, "port":8080, "timeout":"7s", "error":"src/a.rs:12:3: failed"});
        let expected = fingerprint(serde_json::json!({"port":8080}), base.clone());
        for (key, value) in [
            ("actual", serde_json::json!(9)),
            ("port", serde_json::json!(8081)),
            ("timeout", serde_json::json!("9s")),
            ("error", serde_json::json!("timeout after 9s")),
        ] {
            let mut changed = base.clone();
            changed[key] = value;
            assert_ne!(
                expected,
                fingerprint(serde_json::json!({"port":8080}), changed),
                "{key}"
            );
        }
        assert_ne!(
            expected,
            fingerprint(serde_json::json!({"port":8081}), base.clone())
        );
        let mut moved = base;
        moved["error"] = serde_json::json!("src/a.rs:99:8: failed");
        assert_eq!(
            expected,
            fingerprint(serde_json::json!({"port":8080}), moved)
        );
        assert_ne!(
            fingerprint(
                serde_json::json!({}),
                serde_json::json!({"error":"timeout after 7s"})
            ),
            fingerprint(
                serde_json::json!({}),
                serde_json::json!({"error":"timeout after 9s"})
            )
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
        let first = direct_failure_collector(&control, &baselines, "io.locked")
            .deterministic_cycle_key()
            .expect("failure cycle");
        for (artifact, signature) in [
            ("artifact-1", "schema.invalid"),
            ("artifact-2", "io.locked"),
        ] {
            let changed =
                direct_failure_collector_for_artifact(&control, &baselines, artifact, signature)
                    .deterministic_cycle_key()
                    .expect("changed failure cycle");
            assert_ne!(first, changed, "{artifact}: {signature}");
        }
    }

    #[test]
    fn distinct_failure_strategies_allow_narrow_successful_recovery() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for (artifact_id, fingerprint) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ] {
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
            assert!(decision.directive.is_none());
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
    fn slow_distinct_failures_do_not_inject_recovery_directives() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);

        for (artifact_id, fingerprint) in [
            ("artifact-1", "io.locked"),
            ("artifact-2", "schema.invalid"),
        ] {
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
            assert!(decision.directive.is_none());
        }
    }

    #[test]
    fn successful_result_and_distinct_failures_leave_recovery_to_the_model() {
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
            SamplingConvergenceDecision::default()
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
        for _ in 0..=TURN_EFFICIENCY_TOOL_CALL_THRESHOLD {
            let collector = structured_tool_pass_collector(
                &control,
                &baselines,
                r#"{"command":"inspect"}"#,
                "same-evidence",
            );
            collector
                .record_child_runtime(TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL + 1);
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(
                decision.continuation,
                ContinuationDisposition::ModelRequired
            );
            assert!(control.turn_efficiency_guard.is_none());
            assert_eq!(control.turn_efficiency_tool_calls, 0);
            assert!(
                !decision
                    .directive
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("Turn-efficiency")
            );
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
        assert!(!decision.proven_loop_activated);

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
    fn blocked_wait_batched_with_plan_still_installs_its_owner_guard() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let collector = control.collector(&baselines);
        blocked_wait(&collector, "receipt-1");
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("update_plan"),
            &ToolPayload::Function {
                arguments: r#"{"plan":[]}"#.into(),
            },
            "plan",
        );
        collector.push(SamplingToolOutcome::plain(
            registration.ordinal,
            SamplingToolOutcomeKind::Success,
            Some(plan(&[StepStatus::InProgress])),
        ));
        control.settle(&baselines, &collector, &settled);
        let decision = control.evaluate_convergence(&baselines, &collector, &settled);
        assert!(matches!(
            decision.authoritative_wait,
            Some(AuthoritativeWaitResolution::Blocked(_))
        ));
        assert!(!decision.proven_loop_activated);
        let repeated = control.collector(&control.baselines(7));
        assert!(
            repeated
                .register_deterministic_tool_call(
                    &ToolName::plain("wait_agent"),
                    &ToolPayload::Function {
                        arguments: r#"{"cursor":"cursor-1"}"#.into()
                    },
                    "repeat",
                )
                .blocked_wait_guard
                .is_some()
        );
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
    fn failed_tool_signals_cannot_hide_failure() {
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
            // Native failure status wins over a misleading result payload.
            let payload = ToolPayload::Function {
                arguments: r#"{"cmd":"cat src/lib.rs"}"#.to_string(),
            };
            let registration =
                collector.register_deterministic_tool_call(&tool, &payload, "failed");
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
            assert_eq!(
                control
                    .evaluate_convergence(&baselines, &collector, &settled(0))
                    .continuation,
                ContinuationDisposition::ModelRequired,
                "{signal_name}",
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
            assert!(
                !collector_with(kind).snapshot()[0].is_failure_evidence(),
                "{kind:?}"
            );
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
        collector.record_code_mode_parent("cell-1", Some("exec-call"));
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
        // The dispatcher stamps a validation with the revision it executed at;
        // call order inside one request is not the freshness proof.
        let ordinal = collector
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                "validation-after-mutation",
            )
            .ordinal;
        let mut signal = test_execution_signal();
        signal["validation_mutation_revision"] = json!(1);
        collector.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal),
            &runner_tool_response("validation-after-mutation", "Ran 1 test in 0.001s\nOK"),
            false,
        );
        let mutation_settled = settled(1);
        validated_after_mutation.settle(&baselines, &collector, &mutation_settled);
        assert_eq!(
            validated_after_mutation.validated_mutation_revision,
            Some(1)
        );
        assert!(validated_after_mutation.completion_gaps(1).is_empty());
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
        assert_eq!(
            mutated_after_validation.validated_mutation_revision,
            Some(0)
        );
        assert!(
            mutated_after_validation
                .completion_gaps(1)
                .iter()
                .any(|gap| gap.contains("workspace changed"))
        );

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
        assert_eq!(
            observed_after_validation.validated_mutation_revision,
            Some(0)
        );
        assert!(observed_after_validation.completion_gaps(0).is_empty());
        assert_eq!(
            observed_after_validation
                .evaluate_convergence(&baselines, &collector, &settled_state)
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn restored_active_plan_does_not_create_completion_obligations() {
        let mut control = TurnExecutionControl::new().with_active_plan(Some(
            crate::plan_store::PlanExecutionSnapshot::new(
                &plan(&[StepStatus::Completed, StepStatus::Pending]),
                &crate::plan_store::PlanLineage::default(),
            ),
        ));
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        control.settle(&baselines, &collector, &settled(0));
        assert!(control.completion_gaps(0).is_empty());
    }







    #[test]
    fn completion_gaps_report_post_validation_changes_not_unfinished_plans() {
        let mut control = TurnExecutionControl::new();
        assert!(control.completion_gaps(0).is_empty());
        let baselines = control.baselines(0);
        let validation =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &validation, &settled(0));
        assert!(control.completion_gaps(0).is_empty());

        let baselines = control.baselines(0);
        let mutation = control.collector(&baselines);
        record_invocation_result(
            &mutation,
            ToolName::plain("apply_patch"),
            ToolPayload::Custom {
                input: "*** Begin Patch\n*** End Patch".to_string(),
            },
            "mutation-after-validation",
            ToolOutputOutcome::Success,
        );
        control.settle(&baselines, &mutation, &settled(1));
        assert_eq!(
            control.completion_gaps(1),
            vec!["The workspace changed after the last passing validation in this turn."]
        );

        let baselines = control.baselines(1);
        let revalidation =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &revalidation, &settled(1));
        assert!(control.completion_gaps(1).is_empty());

        settle_plan(
            &mut control,
            plan(&[StepStatus::Completed, StepStatus::Pending]),
        );
        assert!(control.completion_gaps(1).is_empty());
        settle_plan(
            &mut control,
            plan(&[StepStatus::Completed, StepStatus::Completed]),
        );
        assert!(control.completion_gaps(1).is_empty());
    }

    #[test]
    fn declared_paths_cannot_manufacture_validation_attribution() {
        let root = tempfile::tempdir().unwrap();
        let tool = ToolName::plain("exec_command");
        let payload = ToolPayload::Function {
            arguments: json!({
                "cmd": "opaque-check",
                "validation": {"covered_paths": ["unrelated.rs"]}
            })
            .to_string(),
        };
        let original = json!({"command_validation": {"proof": true, "tests": false}});
        for dependencies in [None, Some(BTreeSet::new())] {
            let signal = validation_scope_signal(
                &tool,
                &payload,
                Some(original.clone()),
                dependencies.as_ref(),
                root.path(),
                "local",
            )
            .unwrap();
            assert!(signal.get("validation_scope").is_none(), "{signal}");
        }
        let dependencies =
            BTreeSet::from([SourceDependencyV1::new(&root.path().join("src"), true)]);
        let signal = validation_scope_signal(
            &tool,
            &payload,
            Some(original),
            Some(&dependencies),
            root.path(),
            "local",
        )
        .unwrap();
        assert!(signal.get("validation_scope").is_none(), "{signal}");
    }


    #[test]
    fn validation_at_an_older_workspace_revision_cannot_finalize() {
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
        assert_ne!(
            control
                .evaluate_convergence(&baselines, &validation, &settled(1))
                .continuation,
            ContinuationDisposition::TerminalCompletionRequired,
        );
        assert_eq!(control.validated_mutation_revision, Some(0));
        assert!(
            control
                .completion_gaps(1)
                .iter()
                .any(|gap| gap.contains("workspace changed"))
        );
        assert_delivery_retains_validation_gap(&mut control, 1);

        let baselines = control.baselines(1);
        let validation =
            recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &validation, &settled(1));
        assert!(control.completion_gaps(1).is_empty());
    }


    #[test]
    fn failed_validation_explains_original_command_and_diagnostic() {
        for nested in [false, true] {
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            let collector = control.collector(&baseline);
            let tool = ToolName::plain("exec_command");
            let payload = ToolPayload::Function {
                arguments: json!({"cmd": "cargo test -p example", "workdir": "/repo"}).to_string(),
            };
            let signal = json!({"command_validation": {
                "validation": true, "proof": true, "tests": true,
                "execution_context": {"command": ["cargo", "test", "-p", "example"],
                    "cwd": "/repo", "environment_id": "local", "environment_fingerprint": "same"}
            }});
            let result = json!({"exit_code": 1, "stderr": "sccache startup timed out"});
            if nested {
                collector.record_code_mode_result_for_call(
                    Some("failed-check"),
                    CodeModeToolResult {
                        cell_id: "validation",
                        tool_name: &tool,
                        payload: &payload,
                        source_dependencies: None,
                        outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                        signal: Some(&signal),
                        result: &result,
                        canonical_artifact_required: false,
                    },
                );
            } else {
                let ordinal = collector
                    .register_deterministic_tool_call(&tool, &payload, "failed-check")
                    .ordinal;
                collector.record_response_result(
                    ordinal,
                    ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                    Some(signal),
                    &runner_tool_response("failed-check", &result.to_string()),
                    false,
                );
            }
            control.settle(&baseline, &collector, &settled(0));
            let work = control.completion_readiness(0).validation_work.join("\n");
            for detail in [
                "cargo",
                "example",
                "/repo",
                "failed-check",
                "sccache startup timed out",
            ] {
                assert!(work.contains(detail), "missing {detail}: {work}");
            }
            assert!(work.len() < 4096, "repair diagnostics must stay bounded");
        }
    }

    #[test]
    fn failed_background_validation_keeps_bounded_diagnostics_and_launch_owner() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let launch = control.collector(&baseline);
        let ordinal = launch
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                "background-check",
            )
            .ordinal;
        launch.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7})),
            &successful_tool_response("background-check", "running"),
            false,
        );
        control.settle(&baseline, &launch, &settled(0));
        let poll = control.collector(&baseline);
        let ordinal = poll
            .register_deterministic_tool_call(
                &ToolName::plain("write_stdin"),
                &ToolPayload::Function {
                    arguments: json!({"session_id": 7}).to_string(),
                },
                "poll",
            )
            .ordinal;
        poll.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({"observed_process_id": 7})),
            &runner_tool_response(
                "poll",
                &json!({"stderr": "cache δ timed out ".repeat(4096)}).to_string(),
            ),
            false,
        );
        control.settle(&baseline, &poll, &settled(0));
        let readiness = control.completion_readiness(0);
        assert!(readiness.validation_work.is_empty());
        let report = control.completion_assessment(0);
        assert_eq!(report.failed_checks.len(), 1);
        let diagnostic = &report.failed_checks[0];
        assert!(diagnostic.contains("background-check"));
        assert!(diagnostic.contains("python -m unittest -q"));
        assert!(diagnostic.contains("cache δ timed out"));
        assert!(diagnostic.len() < 4096);
        assert!(control.pending_validation_processes.is_empty());
        assert_eq!(control.failed_validation_checks.len(), 1);
    }

    #[test]
    fn validation_identity_uses_resolved_execution_not_launch_spelling() {
        for nested in [false, true] {
            for variant in ["same", "filter", "environment", "cwd"] {
                let mut control = TurnExecutionControl::new();
                let baseline = control.baselines(0);
                for passed in [false, true] {
                    let collector = control.collector(&baseline);
                    let tool = ToolName::plain("exec_command");
                    let payload = ToolPayload::Function {
                        arguments: if passed {
                            json!({"program": "sh", "args": ["-c", "python -m unittest tests.a"]})
                        } else {
                            json!({"cmd": "python -m unittest tests.a", "shell": "sh"})
                        }
                        .to_string(),
                    };
                    let mut signal = test_execution_signal();
                    signal["command_validation"] = json!({
                        "validation": true, "proof": true, "tests": true,
                        "execution_context": {
                            "command": ["sh", "-c", if passed && variant == "filter" {
                                "python -m unittest tests.b"
                            } else { "python -m unittest tests.a" }],
                            "cwd": if passed && variant == "cwd" { "/other" } else { "/repo" },
                            "environment_id": "local",
                            "environment_fingerprint": if passed && variant == "environment" { "changed" } else { "original" }
                        }
                    });
                    let outcome = if passed {
                        ToolOutputOutcome::Success
                    } else {
                        ToolOutputOutcome::Failure
                    };
                    if nested {
                        collector.record_code_mode_result(CodeModeToolResult {
                            cell_id: "checks",
                            tool_name: &tool,
                            payload: &payload,
                            source_dependencies: None,
                            outcome_context: ToolOutputOutcomeContext::new(outcome),
                            signal: Some(&signal),
                            result: &json!({"exit_code": if passed { 0 } else { 1 }}),
                            canonical_artifact_required: false,
                        });
                    } else {
                        let ordinal = collector
                            .register_deterministic_tool_call(&tool, &payload, "check")
                            .ordinal;
                        collector.record_response_result(
                            ordinal,
                            ToolOutputOutcomeContext::new(outcome),
                            Some(signal),
                            &runner_tool_response("check", "Ran 1 test"),
                            false,
                        );
                    }
                    control.settle(&baseline, &collector, &settled(0));
                }
                assert_eq!(
                    control.failed_validation_checks.is_empty(),
                    variant == "same",
                    "{nested} {variant}"
                );
            }
        }
    }

    fn assert_delivery_retains_validation_gap(control: &mut TurnExecutionControl, revision: u64) {
        let baselines = control.baselines(revision);
        let delivery = control.collector(&baselines);
        let ordinal = delivery
            .register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Custom {
                    input: "text('validation remains unverified')".into(),
                },
                "later-delivery",
            )
            .ordinal;
        delivery.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(json!({"explicit_completion_message": "validation remains unverified"})),
            &successful_tool_response("later-delivery", "validation remains unverified"),
            false,
        );
        let before = control.completion_gaps(revision);
        assert!(!before.is_empty());
        control.settle(&baselines, &delivery, &settled(revision));
        // A truthful report may be surfaced, but cannot erase the evidence gap
        // passed to the shared finalization/stop-hook policy.
        assert_eq!(
            control
                .evaluate_convergence(&baselines, &delivery, &settled(revision))
                .continuation,
            ContinuationDisposition::SurfaceExistingResult
        );
        assert_eq!(control.completion_gaps(revision), before);
    }


    #[test]
    fn failed_background_validation_supersedes_its_original_check() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let passed = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
        control.settle(&baseline, &passed, &settled(0));
        let launch = control.collector(&baseline);
        let ordinal = launch
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                "background-check",
            )
            .ordinal;
        launch.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("background-check", "running"),
            false,
        );
        control.settle(&baseline, &launch, &settled(0));
        let poll = control.collector(&baseline);
        let ordinal = poll
            .register_deterministic_tool_call(
                &ToolName::plain("write_stdin"),
                &ToolPayload::Function {
                    arguments: json!({"session_id": 7}).to_string(),
                },
                "poll",
            )
            .ordinal;
        poll.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({"observed_process_id": 7})),
            &successful_tool_response("poll", "FAILED"),
            false,
        );
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_processes.is_empty());
        assert_delivery_retains_validation_gap(&mut control, 0);
        let passed = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
        control.settle(&baseline, &passed, &settled(0));
        assert!(control.completion_gaps(0).is_empty());
    }

    #[test]
    fn uncertainty_old_background_pass_does_not_erase_newer_same_revision_failure() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let launch = control.collector(&baseline);
        let ordinal = launch
            .register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &validation_proof_payload(),
                "old-check",
            )
            .ordinal;
        launch.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("old-check", "running"),
            false,
        );
        control.settle(&baseline, &launch, &settled(0));

        let newer = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Failure);
        control.settle(&baseline, &newer, &settled(0));
        assert_eq!(control.failed_validation_checks.len(), 1);
        let poll = control.collector(&baseline);
        let ordinal = poll
            .register_deterministic_tool_call(
                &ToolName::plain("write_stdin"),
                &ToolPayload::Function {
                    arguments: json!({"session_id": 7}).to_string(),
                },
                "old-poll",
            )
            .ordinal;
        let mut signal = test_execution_signal();
        signal["observed_process_id"] = json!(7);
        poll.record_response_result(
            ordinal,
            ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal),
            &runner_tool_response("old-poll", "Ran 1 test"),
            false,
        );
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_processes.is_empty());
        assert_eq!(control.failed_validation_checks.len(), 1);
        assert!(control.validated_mutation_revision.is_none());

        let repair = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
        control.settle(&baseline, &repair, &settled(0));
        assert!(control.failed_validation_checks.is_empty());
    }




    #[test]
    fn verified_partial_changes_keep_known_path_gaps_and_report_unknown_changes() {
        let control = TurnExecutionControl::new();
        let paths = vec![("local".into(), std::path::PathBuf::from("src/changed.rs"))];
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&paths), true);
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("Untracked changes exist"))
        );
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("src") && gap.contains("changed.rs"))
        );
        let unknown = control.completion_gaps_with_changed_paths(0, None, true);
        assert!(
            unknown
                .iter()
                .any(|gap| gap.contains("Untracked changes exist"))
        );
    }










    #[test]
    fn rustc_error_sites_require_a_location_for_every_reported_error() {
        // Shape of the KDA check another session's missing import broke.
        let compile = "   Compiling kda v0.1.0 (C:\\Users\\kuh\\Desktop\\KDA)\nerror[E0433]: cannot find type `EdgeKind` in this scope\n    --> src\\body_ir.rs:1181:40\n     |\n1181 |                 if matches!(edge.kind, EdgeKind::True | EdgeKind::False) {\n     |                                        ^^^^^^^^ use of undeclared type `EdgeKind`\n     |\nhelp: consider importing one of these enums\n     |\n  13 + use crate::local_cfg::EdgeKind;\n     |\n\nerror[E0433]: cannot find type `ExecutionPhase` in this scope\n   --> src\\semantics\\semantic_summary_builder.rs:547:34\n    |\n547 |         if !matches!(fact.phase, ExecutionPhase::Deferred) {\n    |                                  ^^^^^^^^^^^^^^ use of undeclared type `ExecutionPhase`\n\nwarning: unused variable: `manifest_path`\n    --> src\\workspace.rs:4868:9\n     |\n4868 |         manifest_path: Option<&Path>,\n     |         ^^^^^^^^^^^^^\n\nSome errors have detailed explanations: E0433, E0599.\nFor more information about an error, try `rustc --explain E0433`.\nwarning: `kda` (lib) generated 1 warning\nerror: could not compile `kda` (lib) due to 2 previous errors; 1 warning emitted\nwarning: build failed, waiting for other jobs to finish...\n";
        let (primary, referenced) = rustc_error_sites(["", compile]).unwrap();
        let expected = BTreeSet::from([
            "src\\body_ir.rs".to_string(),
            "src\\semantics\\semantic_summary_builder.rs".to_string(),
        ]);
        assert_eq!(primary, expected, "warnings are not error sites");
        assert_eq!(referenced, expected);
        // `just` then reports the failed recipe; older versions capitalize it.
        for recipe in ["recipe", "Recipe"] {
            let wrapped = format!(
                "{compile}{{\"kind\": \"codex_validation_metrics_ref_v1\"}}\nerror: {recipe} `check-lane` failed on line 475 with exit code 101\n"
            );
            let sites = rustc_error_sites(["", wrapped.as_str()]).map(|(primary, _)| primary);
            assert_eq!(sites.as_ref(), Some(&expected), "{recipe}");
        }
        let noted ="error[E0308]: mismatched types\n --> C:\\repo\\src\\caller.rs:4:9\n  |\nnote: function defined here\n --> C:\\repo\\src\\callee.rs:1:8\n   ::: src\\lib.rs:2:1\n";
        let (primary, referenced) = rustc_error_sites([noted]).unwrap();
        assert_eq!(primary, BTreeSet::from(["C:\\repo\\src\\caller.rs".to_string()]));
        assert_eq!(referenced.len(), 3, "{referenced:?}");
        // Only a package that failed names the root its locations are under.
        let root = std::env::temp_dir().join("KDA");
        let progress = format!(
            "   Compiling kda v0.1.0 ({})\n    Checking vendored v0.1.0 ({})\n    Checking serde v1.0.0\nerror: could not compile `kda` (lib) due to 2 previous errors\n",
            root.display(),
            root.join("vendored").display()
        );
        assert_eq!(failed_package_dirs([progress.as_str()]), vec![root]);
        // Each of these reports an error no source location can attribute.
        for unlocated in [
            "error: linking with `link.exe` failed: exit code: 1181\n  |\n  = note: \"link.exe\" ...\n",
            "test tests::case ... FAILED\nerror: test failed, to rerun pass `--lib`\n",
            "error[E0433]: cannot find type `EdgeKind`\nwarning: unused import\n --> src\\lib.rs:1:5\n",
            "error: failed to run custom build command for `kda v0.1.0`\n",
            "warning: unused import\n --> src\\lib.rs:1:5\nerror: could not compile `kda` (lib)\n",
            "FAILED (failures=1)\n",
        ] {
            assert_eq!(rustc_error_sites([unlocated]), None, "{unlocated}");
        }
    }

    const OTHER_FILE_COMPILE_FAILURE: &str = "error[E0425]: cannot find value `edge` in this scope\n --> src/other.rs:3:5\n\nerror: could not compile `example` (lib) due to 1 previous error\n";

    /// A nested compile failure reporting `stderr`, settled against `tracker`
    /// as the turn loop does.
    fn settle_nested_compile_failure(
        control: &mut TurnExecutionControl,
        tracker: &crate::turn_diff_tracker::TurnDiffTracker,
        cwd: &std::path::Path,
        streams_complete: bool,
        stderr: &str,
    ) -> u64 {
        let tool = ToolName::plain("exec_command");
        let payload = ToolPayload::Function {
            arguments: json!({"cmd": "cargo check -p example"}).to_string(),
        };
        let signal = json!({"command_validation": {
            "validation": true, "proof": true, "tests": false,
            "execution_context": {
                "command": ["sh", "-c", "cargo check -p example"], "cwd": cwd,
                "environment_id": "local", "environment_fingerprint": "same"
            }
        }});
        let result = json!({
            "streams_complete": streams_complete, "stdout": "", "stderr": stderr, "exit_code": 101
        });
        let revision = tracker.current_mutation_revision();
        let baseline = control.baselines(revision);
        let collector = control.collector(&baseline);
        collector.record_code_mode_result_for_call(
            Some("check"),
            CodeModeToolResult {
                cell_id: "validation",
                tool_name: &tool,
                payload: &payload,
                source_dependencies: None,
                outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                signal: Some(&signal),
                result: &result,
                canonical_artifact_required: false,
            },
        );
        control.settle(&baseline, &collector, &settled(revision));
        revision
    }





    /// Audit 2026-10-22 #29: repair work names where the failed check's
    /// complete output is, rather than leaving its location to a guessed tool.
    #[test]
    fn failed_validation_work_names_its_retained_full_output() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let tool = ToolName::plain("exec_command");
        let payload = ToolPayload::Function {
            arguments: json!({"cmd": "cargo check -p example"}).to_string(),
        };
        let signal = json!({"command_validation": {
            "validation": true, "proof": true, "tests": false,
            "execution_context": {
                "command": ["sh", "-c", "cargo check -p example"], "cwd": "/repo",
                "environment_id": "local", "environment_fingerprint": "same"
            }
        }});
        collector.record_code_mode_result_for_call(
            Some("exec-cell/3-tool-1"),
            CodeModeToolResult {
                cell_id: "cell/3",
                tool_name: &tool,
                payload: &payload,
                source_dependencies: None,
                outcome_context: ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                signal: Some(&signal),
                result: &json!({
                    "stderr": "error: could not compile `example`", "exit_code": 101,
                    "raw_output_artifact_id": "out-7"
                }),
                canonical_artifact_required: false,
            },
        );
        control.settle(&baseline, &collector, &settled(0));
        let work = control.completion_readiness(0).validation_work;
        assert_eq!(work.len(), 1, "{work:?}");
        assert!(
            work[0].contains(
                "result: error: could not compile `example`; full output: read_tool_output artifact_id out-7"
            ),
            "{work:?}"
        );
        // Without other text the reference itself is the recorded evidence.
        assert_eq!(
            validation_failure_excerpt(&json!({"raw_output_artifact_id": "out-7"})).as_deref(),
            Some("full output: read_tool_output artifact_id out-7")
        );
    }


    #[test]
    fn documentation_is_listed_separately_from_validation_gaps() {
        let control = TurnExecutionControl::new();
        let paths = vec![
            ("local".into(), std::path::PathBuf::from("README.md")),
            ("local".into(), std::path::PathBuf::from("AGENTS.md")),
            (
                "local".into(),
                std::path::PathBuf::from("skills/example/SKILL.md"),
            ),
            (
                "local".into(),
                std::path::PathBuf::from("codex-rs/prompts/templates/compact/prompt.md"),
            ),
            ("local".into(), std::path::PathBuf::from("src/lib.rs")),
        ];
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&paths), false);
        let code = gaps
            .iter()
            .find(|gap| gap.starts_with("Changed paths"))
            .unwrap();
        assert!(!code.contains("README.md"));
        assert!(code.contains("AGENTS.md"));
        assert!(code.contains("SKILL.md"));
        assert!(code.contains("prompt.md"));
        assert!(gaps.iter().any(|gap| gap.starts_with("Documentation-only")
            && gap.contains("README.md")
            && !gap.contains("prompt.md")));
        assert!(
            !gaps
                .iter()
                .any(|gap| gap.contains("No recognized validation command passed"))
        );
    }

    #[test]
    fn completion_path_lists_are_bounded_and_all_documentation_needs_no_advisory() {
        let control = TurnExecutionControl::new();
        let docs = vec![
            ("local".into(), std::path::PathBuf::from("README.md")),
            ("local".into(), std::path::PathBuf::from("docs/guide.md")),
        ];
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&docs), false);
        assert!(
            !gaps.iter().any(
                |gap| gap.starts_with("Documentation-only") || gap.starts_with("Changed paths")
            ),
            "{gaps:?}"
        );
        let code = (0..12)
            .map(|index| {
                (
                    "local".to_string(),
                    std::path::PathBuf::from(format!("src/m{index}.rs")),
                )
            })
            .collect::<Vec<_>>();
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&code), false);
        let gap = gaps
            .iter()
            .find(|gap| gap.starts_with("Changed paths"))
            .unwrap();
        assert!(gap.contains("local:src/m9.rs"), "{gap}");
        assert!(!gap.contains("local:src/m10.rs"), "{gap}");
        assert!(gap.contains("and 2 more"), "{gap}");
    }

    #[test]
    fn missing_test_evidence_is_not_execution_authority() {
        let changed = vec![("local".into(), std::path::PathBuf::from("src/lib.rs"))];
        let mut control = TurnExecutionControl::new();
        control.accepted_user_input();
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&changed), false);
        assert!(
            gaps.iter().any(|gap| gap.starts_with("Changed paths")
                && gap.contains("does not require or authorize tests")),
            "{gaps:?}"
        );
        // Other gaps still report: an unavailable change set is not excused.
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&changed), true);
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("Untracked changes exist"))
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
            assert_eq!(control.validated_mutation_revision, Some(0), "{commands:?}");
            assert!(control.completion_gaps(0).is_empty(), "{commands:?}");
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
        collector.record_code_mode_parent("cell-1", Some("exec-call"));
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

        let outcomes = collector.snapshot();
        assert_eq!(outcomes.len(), 2);
        let nested = outcomes
            .iter()
            .find(|outcome| outcome.nested_in_code_mode)
            .unwrap();
        assert_eq!(nested.plan.as_ref(), Some(&nested_plan));
        assert_eq!(
            nested.source_evidence,
            Some(json!({"identity": "source-v1"}))
        );
        assert!(nested.canonical_artifact_required);
        assert_eq!(
            collector.generation_purpose(&baseline, &settled(0), false, false,),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
        control.settle(&baseline, &collector, &settled(0));
    }
}
#[cfg(test)]
mod decision_terminal_regressions {
    use super::*;



    #[test]
    fn blocked_plan_rows_do_not_repeat_while_independent_rows_remain() {
        let readiness = CompletionReadiness {
            plan_work: vec!["Blocked provider".into(), "Independent review".into()],
            plan_requirement_ids: vec!["provider".into(), "review".into()],
            ..Default::default()
        };
        assert_eq!(
            readiness.remaining(true, &["provider".into()]),
            ["Independent review"]
        );
        assert_eq!(readiness.remaining(false, &["provider".into()]).len(), 2);
        let mut unmatched = readiness;
        unmatched.plan_requirement_ids.pop();
        assert_eq!(
            unmatched.remaining(true, &["provider".into()]),
            ["Independent review"],
            "unidentified plan work must not inherit another row's blocker"
        );
    }

    #[test]
    fn explicit_goal_readiness_preserves_live_work_and_scoped_blockers() {
        let mut control = TurnExecutionControl::new();
        control
            .failed_validation_checks
            .insert("current-check".into());
        let current = control.completion_readiness(0);
        assert!(current.validation_work.is_empty());
        assert!(
            control
                .completion_readiness(0)
                .remaining(false, &[])
                .is_empty(),
            "historical failures are not current task authority"
        );
        let readiness = CompletionReadiness {
            plan_work: vec!["Review remains required".into()],
            plan_requirement_ids: vec!["review".into()],
            live_work: vec!["Consume an existing validation result".into()],
            ..Default::default()
        };
        assert_eq!(
            readiness.remaining(true, &["deploy".into()]).len(),
            1,
            "plan rows do not block delivery"
        );
        assert_eq!(
            readiness.remaining(true, &["review".into()]).len(),
            1,
            "a live required execution still needs its owned result"
        );
    }



    #[test]
    fn known_sibling_and_ignored_source_paths_remain_observations_not_test_authority() {
        let control = TurnExecutionControl::new();
        let paths = vec![
            (
                "local".into(),
                std::path::PathBuf::from("../shared/src/new_engine.rs"),
            ),
            (
                "local".into(),
                std::path::PathBuf::from("local-tools/preflight.py"),
            ),
        ];
        let assessment = control.completion_assessment_with_changed_paths(0, Some(&paths), false);
        assert!(
            assessment
                .verification_gaps
                .iter()
                .any(|gap| gap.contains("new_engine.rs")
                    && gap.contains("preflight.py")
                    && gap.contains("does not require or authorize tests"))
        );
    }
}

