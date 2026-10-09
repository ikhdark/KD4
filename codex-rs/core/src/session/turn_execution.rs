use std::borrow::Cow;

#[cfg(test)]
#[path = "turn_execution_evidence_tests.rs"]
mod evidence_tests;
#[cfg(test)]
#[path = "turn_execution_cycle_tests.rs"]
mod cycle_tests;
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
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::tool_history::SourceDependencyV1;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::turn_timing::TurnTimingState;
use crate::validation::ValidationClassification;
use crate::validation::ValidationOperation;
use crate::validation::classify_validation;

const TURN_EFFICIENCY_TOOL_CALL_THRESHOLD: usize = 8;
const TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL: u64 = 500;
const LIGHTWEIGHT_HANDOFF_ADVISORY_GENERATIONS: u32 = 3;
/// A handoff whose model-visible output filled this much of an exec cell's
/// default budget could not have batched another comparable read, so it is
/// output-bound rather than orchestration overhead. Four bytes approximate one
/// token, as in the local input estimator.
const OUTPUT_BOUND_HANDOFF_BYTES: usize =
    codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL * 4 * 2 / 5;
/// Advisory-only policy. Production keeps these defaults; replay fixtures can
/// compare alternatives without changing completion, recovery or cancellation
/// invariants. This does not select models or alter reasoning effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HandoffEfficiencyPolicy {
    generations: u32,
    negligible_runtime_ms_per_call: u64,
}

impl Default for HandoffEfficiencyPolicy {
    fn default() -> Self {
        Self {
            generations: LIGHTWEIGHT_HANDOFF_ADVISORY_GENERATIONS,
            negligible_runtime_ms_per_call: TURN_EFFICIENCY_NEGLIGIBLE_CHILD_RUNTIME_MS_PER_CALL,
        }
    }
}

const LIGHTWEIGHT_HANDOFF_ADVISORY: &str = "Execution-efficiency advisory: several consecutive model handoffs each ran only one or two short tools. This is orchestration overhead, not evidence of a loop or task completion. Batch the next known independent calls and continue deterministic dependent steps in the same exec after checking prerequisites. Reuse existing report/inventory commands and retained results instead of regenerating scripts or bookkeeping. Emit decision-relevant summaries, not bulk records; recover required missing ranges within the same cell. Use direct delivery only when the requested answer is complete. Do not skip required reading, implementation, or validation; do not combine conflicting mutations or cancel progressing work. Keep tools available and preserve the user's scope.";

const SUCCESSFUL_REPLAY_GATE_LIMIT: usize = 32;
const RECENT_CYCLE_LIMIT: usize = 32;
const SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT: usize = 64 * 1024;
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
    attributed_mutation_revision: u64,
    plan_revision: u64,
    input_revision: u64,
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
        format!("{:x}", Sha256::digest(state.as_bytes()))
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
    tests_attributed: bool,
    runner_test_evidence: Option<RunnerTestEvidence>,
    process_observation_progress: bool,
    empty_output: bool,
    validation_scope: Option<ValidationScope>,
    validation_mutation_revision: Option<u64>,
    background_process_id: Option<u64>,
    observed_process_id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ValidationScope {
    environment_id: String,
    /// Input/path attribution is not evidence that these surfaces were exercised.
    paths: BTreeSet<SourceDependencyV1>,
    test_execution: bool,
    /// Only an owning validator can establish exercised surfaces. Package
    /// dependency expansion and caller-declared covered_paths cannot fill this.
    #[serde(default)]
    behavioral_paths: BTreeSet<SourceDependencyV1>,
    /// Complete owner-derived input graph, never caller-narrowed attribution.
    #[serde(default)]
    dependency_scope: Option<BTreeSet<SourceDependencyV1>>,
}

#[derive(Clone, Debug)]
struct PendingValidation {
    revision: u64,
    execution_order: (u64, u64),
    inherited: bool,
    check: String,
    scope: Option<ValidationScope>,
    test_execution: bool,
}

#[derive(Clone, Debug)]
struct RunnerTestEvidence {
    input_context: String,
    passed: BTreeSet<crate::validation::RunnerTestIdentity>,
    required: Option<BTreeSet<crate::validation::RunnerTestIdentity>>,
}

impl RunnerTestEvidence {
    fn from_signal(signal: &Value) -> Option<Self> {
        let receipt = signal.get("runner_execution_receipt")?;
        let context = &signal["command_validation"]["execution_context"];
        let configuration = receipt.get("execution_configuration").filter(|value| value.is_object())?;
        // Selection/command spelling may change; execution environment, runner
        // inputs and the settled source revision may not silently change.
        let input_context = serialized_evidence_identity(&(
            context["environment_id"].as_str()?, context["cwd"].as_str()?,
            context["environment_fingerprint"].as_str()?,
            receipt["runner_input_fingerprint"].as_str()?,
            receipt["dependency_manifest"]["execution_context_sha256"].as_str()?,
            configuration,
        ))?;
        Some(Self {
            input_context,
            passed: crate::validation::runner_test_identities(&receipt["executions"])?,
            required: if receipt["required_executions"].is_null() { None }
                else { Some(crate::validation::runner_test_identities(&receipt["required_executions"])?) },
        })
    }
}

#[derive(Clone, Debug)]
struct FailedTestValidation {
    execution_order: (u64, u64),
    evidence: RunnerTestEvidence,
    revision: u64,
    scope: Option<ValidationScope>,
}

/// Attach path attribution at the owning runtime boundary, where the actual
/// working directory and command dependencies are available. This is not line
/// coverage or proof that a model's declared paths were behaviorally exercised.
pub(crate) fn validation_scope_signal(
    tool_name: &ToolName,
    payload: &ToolPayload,
    signal: Option<Value>,
    dependencies: Option<&BTreeSet<SourceDependencyV1>>,
    default_cwd: &std::path::Path,
    environment_id: &str,
) -> Option<Value> {
    let arguments = canonical_tool_action(payload).value;
    let original_signal = signal;
    let mut signal = original_signal.clone().unwrap_or_else(|| serde_json::json!({}));
    let execution_context = signal.get("command_validation")
        .and_then(|validation| validation.get("execution_context")).filter(|value| value.is_object()).cloned();
    let execution_cwd = execution_context.as_ref()
        .and_then(|context| context.get("cwd")).and_then(Value::as_str).map(std::path::PathBuf::from);
    if execution_context.is_some() && execution_cwd.is_none() {
        signal["validation_scope_unavailable"] = serde_json::json!("Remote or non-native validation inputs are not proven by host filesystem evidence.");
        return Some(signal);
    }
    let environment_id = execution_context.as_ref()
        .and_then(|context| context["environment_id"].as_str()).unwrap_or(environment_id);
    let default_cwd = execution_cwd.as_deref().unwrap_or(default_cwd);
    if tool_name_matches(tool_name, "write_stdin") {
        if let Some(process_id) = arguments.get("session_id").and_then(Value::as_u64) {
            signal["observed_process_id"] = serde_json::json!(process_id);
        }
        if let Some(receipt) = signal.get("runner_execution_receipt") {
            let paths = crate::tool_history::runner_receipt_dependencies(receipt, default_cwd);
            if !paths.is_empty() {
                signal["validation_scope"] = serde_json::json!(ValidationScope {
                    behavioral_paths: BTreeSet::new(),
                    environment_id: environment_id.to_string(), dependency_scope: None, paths, test_execution: true,
                });
            }
        }
        return Some(signal);
    }
    let (_, mut proof, mut test_execution) = validation_status_from_arguments(tool_name, &arguments);
    if let Some(validation) = signal.get("command_validation") {
        proof = validation["proof"] == true;
        test_execution = validation["tests"] == true;
    }
    if !proof || arguments.get("environment_id").and_then(Value::as_str)
        .is_some_and(|selected| selected != environment_id)
    {
        return original_signal;
    }
    let cwd = if execution_context.is_some() { default_cwd.to_path_buf() } else { arguments.get("workdir").or_else(|| arguments.get("cwd"))
        .and_then(Value::as_str).map_or_else(
            || default_cwd.to_path_buf(),
            |path| default_cwd.join(path),
        ) };
    let mut paths = if execution_context.is_some() {
        // Classification may have happened against the primary environment.
        // Rebase from the execution owner's resolved cwd, without applying a
        // relative workdir twice or relabeling primary paths as secondary ones.
        let mut executed_arguments = arguments.clone();
        if let Some(arguments) = executed_arguments.as_object_mut() {
            arguments.remove("workdir");
            arguments.remove("cwd");
            arguments.remove("environment_id");
        }
        crate::tool_history::source_dependencies_for_tool_call_with_parsed_arguments(
            &tool_name.name, payload, Some(&executed_arguments), &cwd,
        )
    } else {
        dependencies.cloned().unwrap_or_default()
    };
    // Package/path attribution is not a complete execution-input contract:
    // build scripts, include files and test data can read outside that graph.
    // Keep cross-mutation reuse closed until an owner proves those inputs too.
    let dependency_scope = None;
    if let Some(receipt) = signal.get("runner_execution_receipt") {
        let graph = crate::tool_history::runner_receipt_dependencies(receipt, &cwd);
        paths.extend(graph);
    }
    if let Some(declared) = arguments.get("validation")
        .and_then(|validation| validation.get("covered_paths")).and_then(Value::as_array)
    {
        let root = codex_git_utils::get_git_repo_root(&cwd).unwrap_or(cwd);
        let declared = declared.iter().filter_map(Value::as_str)
            .map(|path| SourceDependencyV1::new(&root.join(path), true))
            .collect::<BTreeSet<_>>();
        // Caller assertions can narrow owner-derived attribution, but cannot
        // manufacture it when command dependencies are unknown. Trusted runner
        // receipts above can supply that missing scope.
        if !paths.is_empty() {
            paths = declared.iter().flat_map(|declared| paths.iter().filter_map(move |scope| {
                if source_scope_contains(scope, &declared.path) {
                    Some(declared.clone())
                } else if source_scope_contains(declared, &scope.path) {
                    Some(scope.clone())
                } else {
                    None
                }
            })).collect();
        }
    }
    if !paths.is_empty() || signal.get("background_process_id").is_some() {
        signal["validation_scope"] = serde_json::json!(ValidationScope {
            environment_id: environment_id.to_string(),
            behavioral_paths: BTreeSet::new(),
            paths,
            test_execution,
            dependency_scope,
        });
        return Some(signal);
    }
    original_signal
}

fn source_scope_contains(scope: &SourceDependencyV1, path: &str) -> bool {
    path == scope.path || scope.recursive
        && path.strip_prefix(&scope.path).is_some_and(|suffix|
            scope.path.ends_with('/') || suffix.starts_with('/'))
}

fn documentation_only_path(path: &std::path::Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let components = normalized.split('/').collect::<Vec<_>>();
    // This repository's prompt crate embeds templates into runtime requests.
    // Instruction files also control the harness; neither is a README.
    let runtime_input = components.windows(2).any(|pair| pair == ["prompts", "templates"])
        || components.last().is_some_and(|name| {
            ["AGENTS.md", "AGENTS.override.md", "SKILL.md"].iter()
                .any(|instruction| name.eq_ignore_ascii_case(instruction))
        });
    !runtime_input && path.extension().and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "md" | "rst"))
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
            tests_executed: signal
                .and_then(|signal| signal.get("runner_execution_receipt"))
                .is_some_and(|receipt| receipt["executed_tests"].as_u64().is_some_and(|count| count > 0)
                    && receipt["exit_code"].as_i64() == Some(0)),
            tests_attributed: signal.is_some_and(|signal|
                signal["validation_summary_tests"].as_u64().is_some_and(|count| count > 0)),
            runner_test_evidence: signal.and_then(RunnerTestEvidence::from_signal),
            process_observation_progress: signal
                .is_some_and(|signal| signal["process_observation_progress"] == true),
            empty_output: signal.is_some_and(|signal| signal["empty_output"] == true),
            validation_scope: signal.and_then(|signal| signal.get("validation_scope"))
                .and_then(|scope| serde_json::from_value(scope.clone()).ok()),
            validation_mutation_revision: signal
                .and_then(|signal| signal["validation_mutation_revision"].as_u64()),
            background_process_id: signal.and_then(|signal| signal["background_process_id"].as_u64()),
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
pub(crate) struct SuccessfulReplayGuard {
    response: ResponseInputItem,
    evidence: SuccessfulReplayEvidence,
}

#[derive(Clone, Debug)]
struct SuccessfulReplayEvidence {
    authorization_identity: Option<String>,
    read_payload: Option<ToolPayload>,
    read_output: Option<Value>,
    path_scoped: bool,
    mutation_revision: u64,
    workspace_revision: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    source_paths: Vec<crate::git_workspace::SourcePathChangeObservation>,
}

impl SuccessfulReplayGuard {
    pub(crate) fn authorization_payload<'a>(&'a self, requested: &'a ToolPayload) -> &'a ToolPayload {
        // The candidate has already proved equal native read scope. Recheck the
        // original invocation under today's permissions, not a broadened key.
        self.evidence.read_payload.as_ref().unwrap_or(requested)
    }

    pub(crate) fn matches_authorization(&self, identity: Option<&str>) -> bool {
        identity.is_some() && self.evidence.authorization_identity.as_deref() == identity
    }

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
        let same_workspace = if self.evidence.path_scoped {
            self.evidence.workspace_revision.as_ref().map(|revision| &revision.repository_root)
                == workspace_revision.map(|revision| &revision.repository_root)
        } else {
            self.evidence.workspace_revision.as_ref() == workspace_revision
        };
        same_workspace
            && self.evidence.workspace_revision.as_ref().is_none_or(|revision| !revision.unavailable)
            && workspace_revision.is_none_or(|revision| !revision.unavailable)
            && (self.evidence.path_scoped || self.evidence.mutation_revision == mutation_revision)
            && !self.evidence.source_paths.is_empty()
            && self
                .evidence
                .source_paths
                .iter()
                .all(|path| cache.source_path_change_observation_is_current(path))
    }

    pub(crate) fn matches_source_dependencies(&self, dependencies: &BTreeSet<SourceDependencyV1>) -> bool {
        !dependencies.is_empty()
            && self.evidence.source_paths.iter()
                .map(crate::git_workspace::SourcePathChangeObservation::source_dependency)
                .collect::<BTreeSet<_>>() == *dependencies
    }

    /// A restart loses watcher proof, not the authenticated source fingerprint.
    /// Only exact file reads may be revalidated; never directories or commands.
    pub(crate) fn cold_read_fingerprint(
        &self,
        cache: &crate::git_workspace::GitWorkspaceCache,
        workspace: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> Option<(String, u64)> {
        let prior = self.evidence.workspace_revision.as_ref()?;
        let workspace = workspace?;
        if !self.evidence.path_scoped || self.evidence.read_payload.is_none()
            || prior.unavailable || workspace.unavailable
            || prior.repository_root.is_none() || prior.repository_root != workspace.repository_root
            || self.evidence.source_paths.len() != 1
            || !cache.source_observation_is_from_prior_watcher(&self.evidence.source_paths[0])
        { return None; }
        let ResponseInputItem::FunctionCallOutput { output, .. } = &self.response else { return None };
        let codex_protocol::models::FunctionCallOutputBody::Text(text) = &output.body else { return None };
        let value: Value = serde_json::from_str(text).ok()?;
        let hash = value["source_sha256"].as_str()?;
        let bytes = value["canonical_bytes"].as_u64()?;
        (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| (hash.to_string(), bytes))
    }

    pub(crate) fn with_validated_source_paths(
        &self,
        paths: Vec<crate::git_workspace::SourcePathChangeObservation>,
        workspace: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
        revision: u64,
    ) -> Self {
        let mut guard = self.clone();
        guard.evidence.source_paths = paths;
        guard.evidence.workspace_revision = workspace;
        guard.evidence.mutation_revision = revision;
        guard
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
    successful_replay_gates: VecDeque<Arc<SuccessfulReplayGate>>,
    timing: Arc<TurnTimingState>,
}

impl DeterministicDispatchLedger {
    fn new(timing: Arc<TurnTimingState>) -> Self {
        Self {
            blocked_wait_gate: None,
            repeated_failure_gate: None,
            argument_syntax_failures: BTreeMap::new(),
            successful_replay_gates: VecDeque::new(),
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
    successful_replay_responses: BTreeMap<u64, ResponseInputItem>,
    successful_replay_evidence: BTreeMap<u64, SuccessfulReplayEvidence>,
    read_payloads: BTreeMap<u64, ToolPayload>,
    path_scoped_ordinals: BTreeSet<u64>,
    replayed_ordinals: BTreeSet<u64>,
    validation_ordinals: BTreeSet<u64>,
    validation_proof_ordinals: BTreeSet<u64>,
    validation_check_identities: BTreeMap<u64, String>,
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
            for key in ["force_fresh", "max_output_tokens", "yield_time_ms", "validation"] {
                arguments.remove(key);
            }
        }
        if let Some(identity) = serialized_evidence_identity(&(tool_name, arguments)) {
            self.validation_check_identities.insert(ordinal, identity);
        }
    }

    fn observe_command_validation(&mut self, ordinal: u64, signal: Option<&Value>) {
        let Some(validation) = signal.and_then(|signal| signal.get("command_validation")) else {
            return;
        };
        if let Some(context) = validation.get("execution_context").filter(|context|
            context["cwd"].is_string() && context["environment_id"].is_string()
                && context["environment_fingerprint"].is_string()
                && context["command"].as_array().is_some_and(|argv| !argv.is_empty()))
            && let Some(identity) = serialized_evidence_identity(context)
        {
            self.validation_check_identities.insert(ordinal, identity);
        }
        if validation["validation"] == true {
            self.saw_validation = true;
            self.validation_ordinals.insert(ordinal);
        }
        if validation["proof"] == true {
            self.validation_proof_ordinals.insert(ordinal);
        }
        if validation["tests"] == true {
            self.test_validation_ordinals.insert(ordinal);
        }
    }
    fn wrapper_ordinals(&self) -> BTreeSet<u64> {
        self.outcomes.iter()
            .filter_map(|outcome| outcome.code_mode_cell_id.as_ref())
            .filter_map(|cell| self.code_mode_cell_owners.get(cell))
            .copied()
            .filter(|ordinal| self.outcomes.iter().any(|outcome| {
                outcome.ordinal == *ordinal
                    && (matches!(outcome.kind, SamplingToolOutcomeKind::Success | SamplingToolOutcomeKind::Yielded)
                        || outcome.wraps_nested_terminal)
            }))
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

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ExecutedValidationSummary {
    pub(crate) count: u32,
    pub(crate) duration_ms: u64,
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
    pub(crate) replayed_success: Option<SuccessfulReplayGuard>,
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
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_semantics.insert(tool_name.clone(), semantics);
    }

    fn runtime_semantics(&self, tool_name: &ToolName) -> crate::tools::registry::ToolSemanticCapabilities {
        if let Some(semantics) = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime_semantics.get(tool_name).copied()
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
                coordination: matches!(tool_name.name.as_str(),
                    "spawn_agent" | "send_message" | "followup_task" | "wait_agent"),
                command: match tool_name.name.as_str() {
                    "exec_command" | "unified_exec" => Some(CommandArgumentFormat::Exec),
                    "shell_command" => Some(CommandArgumentFormat::Shell),
                    _ => None,
                },
            };
        }
        crate::tools::registry::ToolSemanticCapabilities::default()
    }

    fn validation_semantics(&self, tool_name: &ToolName, arguments: &Value) -> (bool, bool, bool, bool) {
        let Some(format) = self.runtime_semantics(tool_name).command else {
            return (false, false, false, false);
        };
        let wire_name = format.canonical_name();
        let (validation, proof, tests) = validation_status_from_arguments(&wire_name, arguments);
        (validation, proof, tests, final_diff_status_from_arguments(&wire_name, arguments))
    }

    pub(crate) fn completion_evidence_key(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let evidence = state
            .outcomes
            .iter()
            .filter(|outcome| !state.replayed_ordinals.contains(&outcome.ordinal))
            // Unrelated reads are not progress against a completion blocker.
            // The turn owner separately keys the actual candidate and revisions.
            .filter(|outcome| state.validation_proof_ordinals.contains(&outcome.ordinal)
                || outcome.validation_scope.is_some())
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
            || (live_process_poll && canonical.value.get("chars")
                .and_then(Value::as_str).is_none_or(str::is_empty));
        let action_identity = deterministic_action_identity(tool_name, &canonical);
        let structured_action =
            structured_action_identity_from_canonical(tool_name, payload, &canonical);
        let (validation, validation_proof, test_execution, final_verification) =
            self.validation_semantics(tool_name, &canonical.value);
        let semantics = self.runtime_semantics(tool_name);
        let mutation = semantics.mutation;
        let replayable_action = structured_action.as_ref().is_some_and(|action| {
            matches!(
                action.class,
                StructuredActionClass::BroadSource | StructuredActionClass::PreciseSource
            )
        }) || validation_proof
            || final_verification;
        let (blocked_wait_guard, suppressed_failure, candidates) = self
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
                let candidates = if replayable_action {
                    ledger.successful_replay_gates.iter().rev().cloned().collect::<Vec<_>>()
                } else { Vec::new() };
                (blocked_wait_guard, suppressed_failure, candidates)
            })
            .unwrap_or_default();
        // Only clone immutable references while holding the shared admission
        // lock. Source selection/serialization can be large and must not block
        // unrelated registrations. Dispatch still rechecks freshness and auth.
        let replayed_success = structured_action.as_ref().and_then(|action| {
            candidates.iter().find_map(|gate| {
                                // Path-scoped evidence is re-proven fresh by the
                                // dispatcher, so turn-local revisions do not gate it.
                                if gate.state_revision != self.request_state_revision
                                    && !gate.evidence.path_scoped { return None; }
                                let response = if gate.action_identity == action.identity {
                                    gate.response.clone()
                                } else if tool_name_matches(tool_name, "read_file") {
                                    let ToolPayload::Function { arguments: previous } = gate.evidence.read_payload.as_ref()? else { return None; };
                                    let ToolPayload::Function { arguments: requested } = payload else { return None; };
                                    let output = gate.evidence.read_output.as_ref()?;
                                    let output = crate::tools::handlers::reselect_read_file_output(previous, requested, output)?;
                                    let text = serde_json::to_string(&output).ok()?;
                                    if text.len() > SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT { return None; }
                                    ResponseInputItem::FunctionCallOutput {
                                        call_id: current_call_id.to_owned(),
                                        output: codex_protocol::models::FunctionCallOutputPayload::from_text(text),
                                    }
                                } else { return None; };
                                Some(SuccessfulReplayGuard { response, evidence: gate.evidence.clone() })
            })
        });

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.registered_count = state.registered_count.saturating_add(1);
        state.call_ordinals.insert(current_call_id.to_string(), ordinal);
        if tool_name_matches(tool_name, "read_file") && structured_action.is_some() {
            while state.read_payloads.len() >= SUCCESSFUL_REPLAY_GATE_LIMIT {
                state.read_payloads.pop_first();
            }
            state.read_payloads.insert(ordinal, payload.clone());
        }
        if tool_name_matches(tool_name, "read_file") || tool_name_matches(tool_name, "list_files") {
            state.path_scoped_ordinals.insert(ordinal);
        }
        if wait {
            state.wait_call_count = state.wait_call_count.saturating_add(1);
        }
        if live_process_poll {
            state.process_monitor_ordinals.insert(ordinal);
        }
        if direct_code_mode_exec {
            state.direct_code_mode_exec_count = state.direct_code_mode_exec_count.saturating_add(1);
            state.code_mode_call_ordinals.insert(current_call_id.to_string(), ordinal);
        } else if crate::tools::code_mode::is_orchestration_tool_name(tool_name)
            && let Some(cell_id) = canonical.value.get("cell_id").and_then(Value::as_str)
        {
            state.code_mode_cell_owners.insert(cell_id.to_string(), ordinal);
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
        state.child_runtime_by_call.insert(call_id.to_string(), runtime_ms);
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
        self.record_registered_owner_result(
            "multi_agent_v2", tool_name, payload, signal,
            canonical_authoritative_result(response).as_ref(), false,
        );
    }

    pub(crate) fn record_registered_owner_result(
        &self, adapter: &str, tool_name: &ToolName, payload: &ToolPayload,
        signal: Option<&Value>, result: Option<&Value>, nested: bool,
    ) {
        // A nested report/export completes its own operation, not the
        // enclosing cell's assignment. Only that cell may opt in to delivery
        // of its final result after all of its work has settled.
        if nested && matches!(adapter, "agent_job_report" | "agent_job_csv_export") {
            return;
        }
        let Some(observation) = authoritative_wait_observation(
            adapter,
            tool_name,
            payload,
            signal,
            result,
        ) else {
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
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ordinal) = parent_call_id.and_then(|parent| state.code_mode_call_ordinals.get(parent)).copied() {
            state.code_mode_cell_owners.insert(cell_id.to_string(), ordinal);
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
        let (validation, validation_proof, test_execution, final_verification) =
            self.validation_semantics(tool_name, &canonical.value);
        let semantics = self.runtime_semantics(tool_name);
        let mutation = semantics.mutation;
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
        if let Some(call_id) = call_id {
            state.call_ordinals.insert(call_id.to_string(), ordinal);
        }
        if is_wait_tool(tool_name) || (tool_name_matches(tool_name, "write_stdin")
            && canonical.value.get("chars").and_then(Value::as_str).is_none_or(str::is_empty))
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
        outcome.code_mode_cell_id = Some(cell_id.to_string());
        let canonical = payload.map(canonical_tool_action);
        let structured_action = payload
            .zip(canonical.as_ref())
            .and_then(|(payload, canonical)| {
                structured_action_identity_from_canonical(tool_name, payload, canonical)
            });
        let (validation, validation_proof, test_execution, final_verification) = canonical
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
    }

    fn authoritative_wait_observation(&self) -> Option<AuthoritativeWaitObservation> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let plan_calls = state.outcomes.iter().filter(|outcome|
            outcome.kind == SamplingToolOutcomeKind::Success && outcome.plan.is_some()
        ).count();
        let observation = state.authoritative_wait_observations.first()?;
        let owner_calls = state.authoritative_wait_observations.len();
        // Identical owner/revision/action/acceptance receipts can be joined
        // without another semantic decision. Different owners or partial
        // receipts require an explicit parent (for example an opt-in delivery
        // cell that awaits every dependency and constructs the final result).
        if !state.authoritative_wait_observations.iter().all(|other| other == observation) {
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
            && !matches!(observation.result.adapter.as_str(), "multi_agent_v2" | "code_mode_cell")
        {
            let expected = state.registered_count.saturating_add(state.code_mode_nested_tool_count);
            let observed = state.outcomes.iter().map(|outcome| outcome.ordinal).collect::<BTreeSet<_>>();
            if state.outcomes.len() != expected || observed.len() != expected
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
            "direct_tool.{:x}",
            Sha256::digest(crate::tools::context::normalize_tool_failure_text(failure).as_bytes())
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
        let path_scoped = state.path_scoped_ordinals.contains(&ordinal);
        let read_payload = state.read_payloads.get(&ordinal).cloned();
        state.successful_replay_evidence.insert(
            ordinal,
            SuccessfulReplayEvidence {
                authorization_identity: None,
                read_payload,
                read_output: None,
                path_scoped,
                mutation_revision,
                workspace_revision,
                source_paths,
            },
        );
    }

    /// Model projections can replace text with line counts. Only the native
    /// owner's exact, bounded result may seed selector-level reuse.
    pub(crate) fn record_read_replay_output(&self, ordinal: u64, output: Value) {
        if !serde_json::to_vec(&output).is_ok_and(|bytes| bytes.len() <= SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(evidence) = state.successful_replay_evidence.get_mut(&ordinal)
            && evidence.read_payload.is_some()
        {
            evidence.read_output = Some(output);
        }
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
        state.observe_command_validation(ordinal, signal.as_ref());
        if outcome.kind == SamplingToolOutcomeKind::Success
            && (state
                .code_mode_call_ordinals
                .values()
                .any(|value| *value == ordinal)
                || state.code_mode_cell_owners.values().any(|value| *value == ordinal))
            && let Some(message) = signal
                .as_ref()
                .and_then(|signal| signal.get("explicit_completion_message"))
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
        {
            if state.explicit_completion.as_ref().is_some_and(|(owner, _)| *owner != ordinal) {
                state.conflicting_explicit_completions = true;
            }
            state.explicit_completion = Some((ordinal, message.to_string()));
        }
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
        state.delivered_output_bytes = state
            .delivered_output_bytes
            .saturating_add(response_replay_text_size(response).unwrap_or(0));
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

    pub(crate) fn record_replay_authorization(&self, ordinal: u64, identity: Option<String>) {
        if let Some(evidence) = self.state.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .successful_replay_evidence.get_mut(&ordinal)
        {
            evidence.authorization_identity = identity;
        }
    }

    fn explicit_completion(&self) -> Option<AuthoritativeWaitOwnerResult> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (ordinal, message) = state.explicit_completion.as_ref()?;
        let admitted_cell = |outcome: &SamplingToolOutcome| outcome.nested_in_code_mode
            && outcome.code_mode_cell_id.as_ref().and_then(|cell| state.code_mode_cell_owners.get(cell)) == Some(ordinal);
        // A model-authored delivery intent is an explicit final response, not a
        // guess that an arbitrary successful tool completed the user's task.
        // Every registered sibling and nested call must be accounted for. A
        // yielded command is settled only by a later successful observation of
        // that same process; its original running receipt remains evidence.
        // A count alone would let duplicate outcomes hide pending work.
        let expected = state.registered_count.saturating_add(state.code_mode_nested_tool_count);
        let observed = state.outcomes.iter().map(|outcome| outcome.ordinal).collect::<BTreeSet<_>>();
        if state.conflicting_explicit_completions
            || state.outcomes.len() != expected
            || observed.len() != expected
            || state
                .outcomes
                .iter()
                .any(|outcome| {
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
        let expected_outcomes = state.registered_count + state.code_mode_nested_tool_count - wrappers.len();
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
            let repeated_action_identity = if failures.len() == 1
                && !failures[0].nested_in_code_mode
            {
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
            && outcomes.iter().all(|outcome| state.process_monitor_ordinals.contains(&outcome.ordinal))
        {
            return None;
        }
        if outcomes.len() != expected_outcomes
            || !outcomes.iter().all(|outcome| outcome.kind == SamplingToolOutcomeKind::Success)
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
        let calls = state.registered_count.saturating_sub(state.direct_code_mode_exec_count)
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
        let outcomes = state.outcomes.iter()
            .filter(|outcome| !wrappers.contains(&outcome.ordinal))
            .collect::<Vec<_>>();
        expected > 0 && outcomes.len() == expected && outcomes.iter().all(|outcome| {
            let eligible = if yielded_only {
                outcome.kind == SamplingToolOutcomeKind::Yielded
            } else {
                state.process_monitor_ordinals.contains(&outcome.ordinal) && matches!(
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

        let keyed_duration = state.child_runtime_by_call.iter()
            .filter(|(call_id, _)| state.call_ordinals.get(*call_id).is_some_and(|ordinal| {
                state.validation_ordinals.contains(ordinal)
                    && !state.replayed_ordinals.contains(ordinal)
                    && state.outcomes.iter().any(|outcome| outcome.ordinal == *ordinal
                        && outcome.kind != SamplingToolOutcomeKind::Skipped
                        && !outcome.failure_diagnosis_reused)
            }))
            .fold(0_u64, |total, (_, duration)| total.saturating_add(*duration));
        ExecutedValidationSummary {
            count,
            // Legacy unkeyed fixtures remain conservative. Production samples
            // are keyed, so unrelated reads cannot inflate validation duration.
            duration_ms: if !state.child_runtime_by_call.is_empty() {
                keyed_duration
            } else if duration_is_validation_only {
                state.child_runtime_ms
            } else {
                0
            },
        }
    }

    /// Whether the request's latest validation proof survived every later
    /// observation. Probes the live validation, final-verification and
    /// test-execution classification that gates successful replay.
    #[cfg(test)]
    fn fresh_successful_validation(&self) -> bool {
        let allocated_ordinal_count = self.next_ordinal.load(Ordering::Acquire);
        let Ok(allocated_count) = usize::try_from(allocated_ordinal_count) else {
            return false;
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(latest_validation_ordinal) = state.validation_proof_ordinals.last().copied()
        else {
            return false;
        };
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
        !(state.outcomes.len() != allocated_count
            || outcome_ordinals.len() != allocated_count
            || !(0..allocated_ordinal_count).all(|ordinal| outcome_ordinals.contains(&ordinal))
            || !terminal_observation_is_valid
            || state
                .outcomes
                .iter()
                .any(|outcome| outcome.kind != SamplingToolOutcomeKind::Success)
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
                .is_some())
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
            && state.wait_call_count == state.registered_count.saturating_sub(state.direct_code_mode_exec_count)
                .saturating_add(state.code_mode_nested_tool_count) {
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

    /// Identity of the actual ordered tool actions, independent of outcomes,
    /// request purpose, and provider call IDs. Missing identities are unknown,
    /// not proof that two generations chose the same actions.
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
        ToolPayload::Function { arguments } => match crate::tools::handlers::parsed_function_argument_value(arguments)
            .unwrap_or_else(|| serde_json::from_str::<Value>(arguments).map_err(|err| err.to_string())) {
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
    if canonical.identity_payload.is_none() {
        // Retain invalid bytes only for argument-error diagnosis. Do not give
        // malformed input a successful replay or wait identity.
        let identity = format!("{:x}", Sha256::digest(
            serde_json::to_vec(&(tool_name, "invalid_function_arguments", &canonical.value)).ok()?
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
    let normalized_payload = normalized_read.as_ref().map(serde_json::to_string).transpose().ok()?;
    let action =
        serde_json::to_string(&(tool_name, normalized_payload.as_deref()
            .or(canonical.identity_payload.as_deref())?)).ok()?;
    let identity = format!("{:x}", Sha256::digest(action.as_bytes()));
    let mut evidence_arguments = normalized_read.unwrap_or_else(|| canonical.value.clone());
    if ["exec_command", "shell_command", "write_stdin"].iter()
        .any(|name| tool_name_matches(tool_name, name))
        && let Some(arguments) = evidence_arguments.as_object_mut()
    {
        arguments.remove("max_output_tokens");
        arguments.remove("yield_time_ms");
    }
    let evidence_identity = format!("{:x}", Sha256::digest(
        serde_json::to_vec(&(tool_name, evidence_arguments)).ok()?
    ));
    Some(StructuredActionIdentity { identity, evidence_identity, class })
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
    format!("code_mode.nested_tool.{:x}", Sha256::digest(canonical.as_bytes()))
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
        "{}\n{}", serde_json::to_string(tool_name).ok()?,
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
            exit_code_is_authoritative && leaves.iter().all(|leaf| leaf.mode.can_prove_validation()),
            leaves
                .iter()
                .any(|leaf| leaf.operation == ValidationOperation::Test
                    && leaf.mode == codex_shell_command::validation::ValidationExecutionMode::Execution),
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

pub(crate) struct TurnExecutionControl {
    issued_directives: BTreeSet<String>,
    handoff_policy: HandoffEfficiencyPolicy,
    soft_convergence_issued: bool,
    lightweight_handoffs: u32,
    /// Completed generations since the last one that produced new evidence,
    /// a workspace mutation, or a plan/input change.
    continuations_without_progress: u32,
    plan: Option<crate::plan_store::PlanExecutionSnapshot>,
    /// An inherited completed checklist is not a final-answer hint for new input.
    plan_updated_since_input: bool,
    plan_revision: u64,
    input_revision: u64,
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
    evidence_fingerprint: OnceLock<String>,
    artifact_source_coverage: BTreeMap<String, BTreeSet<String>>,
    /// Mutation revision proven by this turn's last passing validation.
    validated_mutation_revision: Option<u64>,
    /// The last fresh passing execution's coverage entry, if it has one.
    /// Re-proving its dependencies does not rewrite the execution revision.
    validated_check: Option<String>,
    validation_coverage_revision: Option<u64>,
    /// Coverage is owned by the check that produced it, not an irreversible union.
    validation_coverage: BTreeMap<String, ValidationScope>,
    failed_validation_checks: BTreeSet<String>,
    inherited_validation_checks: BTreeSet<String>,
    failed_validation_tests: BTreeMap<String, FailedTestValidation>,
    /// Execution revisions, not arrival order, determine supersession.
    validation_check_revisions: BTreeMap<String, u64>,
    validation_check_orders: BTreeMap<String, (u64, u64)>,
    next_validation_request: AtomicU64,
    pending_validation_coverage: BTreeMap<u64, PendingValidation>,
    session_path_replays: Option<Arc<SessionPathReplays>>,
    session_validation_uncertainty: Option<Arc<Mutex<SessionValidationUncertainty>>>,
}

/// Unresolved execution accounting, not reusable validation proof. Live process
/// identities belong to this session; they must not be resurrected on restart.
#[derive(Default)]
pub(crate) struct SessionValidationUncertainty {
    failed_checks: BTreeSet<String>,
    failed_tests: BTreeMap<String, FailedTestValidation>,
    check_orders: BTreeMap<String, (u64, u64)>,
    next_request: u64,
    pending: BTreeMap<u64, PendingValidation>,
}

impl Drop for TurnExecutionControl {
    fn drop(&mut self) {
        let Some(owner) = &self.session_validation_uncertainty else { return; };
        let mut retained = owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.failed_checks = std::mem::take(&mut self.failed_validation_checks);
        retained.failed_tests = std::mem::take(&mut self.failed_validation_tests);
        retained.check_orders = std::mem::take(&mut self.validation_check_orders);
        retained.next_request = self.next_validation_request.load(Ordering::Relaxed);
        retained.pending = std::mem::take(&mut self.pending_validation_coverage);
        let pending_checks = retained.pending.values().map(|pending| pending.check.clone()).collect::<BTreeSet<_>>();
        let failed_checks = retained.failed_checks.clone();
        retained.check_orders.retain(|check, _| failed_checks.contains(check) || pending_checks.contains(check));
    }
}

/// Path-scoped successful read results retained across turns of a session.
/// Revisions are turn-local, so these match by action identity only; the
/// dispatcher re-proves repository identity and watcher freshness before use.
#[derive(Default)]
pub(crate) struct SessionPathReplays(Mutex<VecDeque<Arc<SuccessfulReplayGate>>>);

impl SessionPathReplays {
    /// Rebuild bounded candidates from the existing durable history, not a
    /// second persistence ledger. Watcher epochs are deliberately not reminted:
    /// a restart or a coverage gap still requires a fresh producer observation.
    pub(crate) fn rehydrate(
        &self,
        items: &[codex_protocol::models::ResponseItem],
        history: &crate::tool_history::ToolHistoryState,
    ) {
        use codex_protocol::models::ResponseItem;
        let mut gates = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !gates.is_empty() {
            return;
        }
        let recent = &items[items.len().saturating_sub(256)..];
        let calls = recent.iter().filter_map(|item| match item {
            ResponseItem::FunctionCall { name, namespace: None, arguments, call_id, .. }
                if matches!(name.as_str(), "read_file" | "list_files") =>
            {
                Some((call_id, (name, arguments)))
            }
            _ => None,
        }).collect::<BTreeMap<_, _>>();
        for item in recent {
            let ResponseItem::FunctionCallOutput { call_id, output, .. } = item else {
                continue;
            };
            if output.success == Some(false) {
                continue;
            }
            let Some((name, arguments)) = calls.get(call_id) else { continue };
            let payload = ToolPayload::Function { arguments: (*arguments).clone() };
            let Some(action) = structured_action_identity_from_canonical(
                &ToolName::plain((*name).clone()), &payload, &canonical_tool_action(&payload),
            ) else { continue };
            let Some((workspace_revision, source_paths, authorization_identity)) = history.read_replay_provenance(item)
            else { continue };
            let response = ResponseInputItem::FunctionCallOutput {
                call_id: call_id.clone(), output: output.clone(),
            };
            if !serde_json::to_vec(&response).ok()
                .is_some_and(|bytes| bytes.len() <= SUCCESSFUL_REPLAY_OUTPUT_BYTE_LIMIT)
            {
                continue;
            }
            gates.retain(|gate| gate.action_identity != action.identity);
            gates.push_back(Arc::new(SuccessfulReplayGate {
                state_revision: String::new(),
                action_identity: action.identity,
                response,
                evidence: SuccessfulReplayEvidence {
                    authorization_identity: Some(authorization_identity),
                    read_payload: (name.as_str() == "read_file").then_some(payload),
                    // Durable history may be a lossy model projection. It can
                    // restore exact-call replay, never remint missing raw bytes.
                    read_output: None,
                    path_scoped: true,
                    mutation_revision: 0,
                    workspace_revision,
                    source_paths,
                },
            }));
            while gates.len() > SUCCESSFUL_REPLAY_GATE_LIMIT {
                gates.pop_front();
            }
        }
    }

    fn snapshot(&self) -> VecDeque<Arc<SuccessfulReplayGate>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn retain(&self, gate: &Arc<SuccessfulReplayGate>) {
        let mut gates = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|existing| existing.action_identity != gate.action_identity);
        gates.push_back(gate.clone());
        while gates.len() > SUCCESSFUL_REPLAY_GATE_LIMIT {
            gates.pop_front();
        }
    }
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
            handoff_policy: HandoffEfficiencyPolicy::default(),
            soft_convergence_issued: false,
            lightweight_handoffs: 0,
            continuations_without_progress: 0,
            plan: None,
            plan_updated_since_input: false,
            plan_revision: 0,
            input_revision: 0,
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
            validated_check: None,
            validation_coverage_revision: None,
            validation_coverage: BTreeMap::new(),
            failed_validation_checks: BTreeSet::new(),
            inherited_validation_checks: BTreeSet::new(),
            failed_validation_tests: BTreeMap::new(),
            validation_check_revisions: BTreeMap::new(),
            validation_check_orders: BTreeMap::new(),
            next_validation_request: AtomicU64::new(0),
            pending_validation_coverage: BTreeMap::new(),
            session_path_replays: None,
            session_validation_uncertainty: None,
        }
    }

    pub(crate) fn with_active_plan(mut self, plan: Option<crate::plan_store::PlanExecutionSnapshot>) -> Self {
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

    /// Mechanical reasons this turn is not a verified completion: workspace
    /// changes after its last passing validation, or its plan left unfinished.
    /// The revision is the last settled request's. The turn owner separately
    /// reports the post-validation boundary of a mutating finalizer.
    fn completion_assessment(&self, settled_mutation_revision: u64) -> codex_protocol::protocol::TurnCompletionAssessment {
        let mut assessment = codex_protocol::protocol::TurnCompletionAssessment::default();
        let current_failures = self.failed_validation_checks.len().saturating_sub(self.inherited_validation_checks.len());
        if !self.inherited_validation_checks.is_empty() {
            assessment.advisories.push(format!(
                "{} earlier validation failure(s) remain unresolved; relevance to this request is not established. This is not a rerun request.",
                self.inherited_validation_checks.len(),
            ));
        }
        if current_failures != 0 {
            assessment.failed_checks.push(format!(
                "{} validation check(s) failed without a later passing execution or current per-test repairs.",
                current_failures,
            ));
            for (_, failed) in self.failed_validation_tests.iter().filter(|(check, _)| !self.inherited_validation_checks.contains(*check)) {
                if let Some(required) = &failed.evidence.required {
                    let unresolved = required.difference(&failed.evidence.passed)
                        .map(|test| format!("{} {:?} {}", test.binary, test.helpers, test.test))
                        .collect::<Vec<_>>();
                    assessment.failed_checks.push(format!("Unfulfilled test IDs: {}.", unresolved.join(", ")));
                }
            }
        }
        if self
            .validated_mutation_revision
            .is_some_and(|validated| validated != settled_mutation_revision)
            && !(self.validation_coverage_revision == Some(settled_mutation_revision)
                && self.validated_check.as_ref().is_some_and(|check|
                    self.validation_coverage.contains_key(check)))
        {
            assessment.verification_gaps.push(
                "The workspace changed after the last passing validation in this turn.".to_string(),
            );
        }
        if let Some(plan) = self.plan.as_ref().filter(|plan| !plan.obligations.unresolved.is_empty()) {
            let unfinished = plan.obligations.unresolved.len();
            assessment.advisories.push(format!(
                "The plan still has {unfinished} unresolved obligation(s)."
            ));
        }
        if !self.pending_validation_coverage.is_empty() {
            let current = self.pending_validation_coverage.values().filter(|pending| !pending.inherited).count();
            if current != 0 { assessment.verification_gaps.push(format!(
                "{} validation process(es) still await a consumed terminal result.",
                current,
            )); }
            let inherited = self.pending_validation_coverage.len() - current;
            if inherited != 0 { assessment.advisories.push(format!(
                "{inherited} earlier validation process(es) still lack a consumed terminal result; they are not proof for this turn."
            )); }
        }
        assessment
    }

    pub(crate) fn completion_assessment_with_changed_paths(
        &self,
        settled_mutation_revision: u64,
        changed_paths: Option<&[(String, std::path::PathBuf)]>,
        has_untracked_changes: bool,
    ) -> codex_protocol::protocol::TurnCompletionAssessment {
        let mut assessment = self.completion_assessment(settled_mutation_revision);
        if has_untracked_changes {
            assessment.verification_gaps.push("Untracked changes exist: part of the turn change set is unavailable; validation attribution is incomplete. Report this uncertainty rather than treating unavailable paths as unchanged.".to_string());
        }
        if let Some(changed_paths) = changed_paths {
            let documentation = documentation_only_path;
            let docs = changed_paths.iter().filter(|(_, path)| documentation(path))
                .map(|(environment, path)| format!("{environment}:{}", path.display()))
                .collect::<Vec<_>>();
            let uncovered = changed_paths.iter().filter(|(_, path)| !documentation(path))
                .filter(|(environment, path)| {
                let path = SourceDependencyV1::new(path, false).path;
                self.validation_coverage_revision != Some(settled_mutation_revision)
                    || !self.validation_coverage.values().any(|scope|
                        scope.environment_id == *environment
                            && scope.test_execution
                            && scope.behavioral_paths.iter().any(|scope| source_scope_contains(scope, &path))
                    )
            }).map(|(environment, path)| format!("{environment}:{}", path.display()))
                .collect::<Vec<_>>();
            if !uncovered.is_empty() {
                // With nothing attributed at all, the likeliest cause is a
                // repository runner the classifier was never told about.
                let unrecognized = if self.validation_coverage_revision
                    != Some(settled_mutation_revision)
                    || self.validation_coverage.is_empty()
                {
                    " No passing validation was attributed to these paths; a recognized command may have passed without an attributable test count or path scope."
                } else {
                    ""
                };
                assessment.verification_gaps.push(format!(
                    "Changed paths without passing behavioral validation attribution: {}.{unrecognized} Input dependencies and declared paths are not behavioral test coverage; behavior remains unverified. Inspection or intentionally omitted validation must be reported.",
                    uncovered.join(", ")
                ));
            }
            if self.validation_coverage_revision == Some(settled_mutation_revision)
                && self.validation_coverage.values().any(|scope| !scope.test_execution)
            {
                assessment.advisories.push("Non-test checks passed (for example formatting or static checks); behavior unverified by those checks.".to_string());
            }
            if !docs.is_empty() {
                assessment.advisories.push(format!("Documentation-only paths (inspection, not test attribution): {}.", docs.join(", ")));
            }
        }
        assessment
    }


    #[cfg(test)]
    fn completion_gaps(&self, revision: u64) -> Vec<String> {
        let report = self.completion_assessment(revision);
        report.failed_checks.into_iter().chain(report.verification_gaps).chain(report.advisories).collect()
    }

    #[cfg(test)]
    fn completion_gaps_with_changed_paths(&self, revision: u64, paths: Option<&[(String, std::path::PathBuf)]>, untracked: bool) -> Vec<String> {
        let report = self.completion_assessment_with_changed_paths(revision, paths, untracked);
        report.failed_checks.into_iter().chain(report.verification_gaps).chain(report.advisories).collect()
    }


    pub(crate) fn batching_advisory(&self, is_continuation: bool) -> Option<String> {
        (is_continuation
            // Measured execution, not the presence of a plan, establishes
            // fragmentation. Unplanned investigations incur the same handoffs.
            && self.lightweight_handoffs >= self.handoff_policy.generations
            && !self.issued_directives.contains(LIGHTWEIGHT_HANDOFF_ADVISORY))
        .then(|| LIGHTWEIGHT_HANDOFF_ADVISORY.to_string())
    }

    /// Owner-derived checklist accounting only, never proof of task completion.
    pub(crate) fn plan_completed(&self) -> bool {
        self.plan_updated_since_input && self.plan
            .as_ref()
            .is_some_and(|plan| plan.has_steps && plan.obligations.unresolved.is_empty())
    }

    pub(crate) fn take_soft_convergence_directive(
        &mut self,
        is_continuation: bool,
    ) -> Option<String> {
        if !is_continuation
            || self.soft_convergence_issued
            || self.continuations_without_progress < SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS
        {
            return None;
        }
        self.soft_convergence_issued = true;
        Some(SOFT_CONVERGENCE_DIRECTIVE.to_string())
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
        let (tool_calls, child_runtime_ms, runtime_samples) = signals.turn_efficiency_sample();
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
            if state.replayed_ordinals.contains(&outcome.ordinal)
                || outcome.failure_diagnosis_reused
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
            if let Some(novel) = outcome.source_evidence.as_ref()
                    .and_then(|evidence| self.observe_delivered_coverage(evidence, outcome.source_artifact_id.as_deref()))
            {
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
                    evidence.get("source").is_some() && evidence.get("scope").is_some()
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
        // Novel evidence still counts as progress. Fragmented execution is an
        // independent advisory, never authority to suppress a call or finish.
        // Require measured, completed, non-mutating work; do not confuse a
        // large batch, a budget-filling read, validation, or process
        // monitoring with serial overhead.
        let lightweight = (1..=2).contains(&tool_calls)
            && runtime_samples == tool_calls
            && child_runtime_ms
                <= tool_calls as u64 * self.handoff_policy.negligible_runtime_ms_per_call
            && state.delivered_output_bytes < OUTPUT_BOUND_HANDOFF_BYTES
            && state.wait_call_count == 0
            && state.process_monitor_ordinals.is_empty()
            && state.mutation_ordinals.is_empty()
            && state.validation_ordinals.is_empty()
            && state.replayed_ordinals.is_empty()
            && settled.mutation_revision == baselines.mutation_revision
            && settled.tool_exposure_revision == baselines.tool_exposure_revision
            && self.input_revision == baselines.input_revision
            && !state.outcomes.is_empty()
            && state.outcomes.iter().filter(|outcome| !wrappers.contains(&outcome.ordinal))
                .all(|outcome| outcome.kind == SamplingToolOutcomeKind::Success);
        self.lightweight_handoffs = if lightweight {
            self.lightweight_handoffs.saturating_add(1)
        } else {
            0
        };
        if !progress.is_empty() || process_progress {
            self.continuations_without_progress = 0;
        } else {
            self.continuations_without_progress =
                self.continuations_without_progress.saturating_add(1);
        }
        progress
    }

    /// Native file snapshots and their retained artifacts share delivered bytes,
    /// not freshness. A new source hash or path always starts independent coverage.
    fn observe_delivered_coverage(&mut self, evidence: &Value, artifact_id: Option<&str>) -> Option<bool> {
        let source = evidence["source"].as_str()?;
        if !matches!(source, "read_file" | "artifact") {
            return None;
        }
        let identity = evidence.get("identity")?;
        let sha256 = identity["sha256"].as_str()?;
        let scope = evidence.get("scope")?;
        let ranges = identity["ranges"].as_array()?.iter().map(|range| {
            let range = range.as_array()?;
            if range.len() != 2 {
                return None;
            }
            let start = range[0].as_u64()?;
            let end = range[1].as_u64()?;
            (start <= end).then_some((start, end))
        }).collect::<Option<Vec<_>>>()?;
        let values = identity["values"].as_array()?;
        let key = value_evidence_identity(&serde_json::json!([source, scope, sha256]))?;
        let delivered = DeliveredSourceCoverage {
            ranges,
            query_proofs: values.iter().filter_map(value_evidence_identity).collect(),
        };
        let sources = self.artifact_source_coverage.get(&key).cloned().unwrap_or_default();
        // Alias propagation can change the digest even without novel source bytes.
        self.evidence_fingerprint.take();
        // Other reads may have filled holes since this artifact was created.
        // Import their coverage before deciding whether recovery adds evidence.
        if source == "artifact" {
            for source in &sources {
                if let Some(coverage) = self.delivered_coverage.get(source).cloned() {
                    self.delivered_coverage.entry(key.clone()).or_default().merge(&coverage);
                }
            }
        }
        let novel = self.delivered_coverage.entry(key.clone()).or_default().merge(&delivered);
        if source == "read_file" {
            if let Some(id) = artifact_id {
                let artifact_key = value_evidence_identity(&serde_json::json!(["artifact", id, sha256]))?;
                let coverage = self.delivered_coverage.get(&key)?.clone();
                self.delivered_coverage.entry(artifact_key.clone()).or_default().merge(&coverage);
                self.artifact_source_coverage.entry(artifact_key).or_default().insert(key);
            }
        } else {
            // Recovery delivered the exact bytes of these authenticated snapshots.
            // It does not refresh their paths or authorize replay of stale files.
            for source in sources {
                self.delivered_coverage.entry(source).or_default().merge(&delivered);
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
        !self.observe_progress(baselines, signals, settled).is_empty()
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
            tool_exposure_revision,
            evidence_fingerprint: self.evidence_fingerprint.get_or_init(|| {
                format!(
                    "{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&(
                            &self.budget_progress_evidence,
                            &self.delivered_coverage,
                        ))
                        .expect("ordered evidence contains only JSON-serializable values"),
                    ),
                )
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
            failure_fingerprint: analysis.deterministic_cycle()
                .and_then(|cycle| cycle.failure_fingerprint.clone()),
            terminal_completion_only: false,
        }
    }

    pub(crate) fn accepted_user_input(&mut self) {
        self.plan_updated_since_input = false;
        self.issued_directives.clear();
        self.lightweight_handoffs = 0;
        self.input_revision = self.input_revision.saturating_add(1);
        self.continuations_without_progress = 0;
        self.soft_convergence_issued = false;
        self.reset_convergence();
        let mut ledger = self
            .dispatch_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let timing = Arc::clone(&ledger.timing);
        // Path-scoped reads stay reusable: the dispatcher re-proves every
        // source path unchanged before replaying one.
        let path_scoped_gates = std::mem::take(&mut ledger.successful_replay_gates)
            .into_iter()
            .filter(|gate| gate.evidence.path_scoped)
            .collect();
        let syntax_failures = std::mem::take(&mut ledger.argument_syntax_failures);
        *ledger = DeterministicDispatchLedger::new(timing);
        ledger.successful_replay_gates = path_scoped_gates;
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
                ResponseItem::FunctionCall { name, namespace: None, arguments, call_id, .. } => {
                    let payload = ToolPayload::Function { arguments: arguments.clone() };
                    let canonical = canonical_tool_action(&payload);
                    if canonical.identity_payload.is_none()
                        && let Some(action) = structured_action_identity_from_canonical(
                            &ToolName::plain(name.clone()), &payload, &canonical,
                        )
                    {
                        calls.insert(call_id, action.identity);
                    }
                }
                ResponseItem::FunctionCallOutput { call_id, output, .. }
                    if output.success == Some(false) =>
                {
                    if let Some(identity) = calls.get(call_id) {
                        diagnoses.insert(identity.clone(), format!("json-argument-syntax-v1:{identity}"));
                    }
                }
                _ => {}
            }
        }
        self.dispatch_ledger.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .argument_syntax_failures = diagnoses;
    }

    /// Transfer unresolved questions without importing prior-turn proof.
    pub(crate) fn with_session_validation_uncertainty(
        mut self, owner: Arc<Mutex<SessionValidationUncertainty>>,
    ) -> Self {
        {
            let mut retained = owner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.failed_validation_checks = std::mem::take(&mut retained.failed_checks);
            self.inherited_validation_checks = self.failed_validation_checks.clone();
            self.failed_validation_tests = std::mem::take(&mut retained.failed_tests);
            self.validation_check_orders = std::mem::take(&mut retained.check_orders);
            self.next_validation_request.store(retained.next_request, Ordering::Relaxed);
            self.pending_validation_coverage = std::mem::take(&mut retained.pending);
        }
        // Mutation revisions are turn-local. Preserve the question, not an old
        // pass or the fiction that revision zero in two turns means equal inputs.
        for failed in self.failed_validation_tests.values_mut() {
            failed.revision = 0;
            failed.evidence.passed.clear();
        }
        for pending in self.pending_validation_coverage.values_mut() {
            pending.revision = 0;
            pending.inherited = true;
        }
        self.validation_check_revisions.extend(self.validation_check_orders.keys().map(|check| (check.clone(), 0)));
        self.session_validation_uncertainty = Some(owner);
        self
    }

    /// Share path-scoped read results with later turns of this session.
    pub(crate) fn with_session_path_replays(mut self, replays: Arc<SessionPathReplays>) -> Self {
        {
            let mut ledger = self
                .dispatch_ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ledger.successful_replay_gates = replays.snapshot();
        }
        self.session_path_replays = Some(replays);
        self
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
        if self.pending_validation_coverage.values().any(|pending| !pending.inherited) {
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
            settled.attributed_mutation_revision, self.input_revision, settled.tool_exposure_revision,
        );
        // Validation proves only its observed execution. It cannot prove that
        // all user-requested changes, checks, or child lifecycle actions are done.
        if settled.attributed_mutation_revision != baselines.attributed_mutation_revision
            || self.input_revision != baselines.input_revision
            || settled.tool_exposure_revision != baselines.tool_exposure_revision
        {
            self.continuations_without_progress = 0;
            self.reset_convergence();
            self.last_state_revision = Some(settled_revision);
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
        let repeated_cycle = request_cycle.as_ref().is_some_and(|cycle| {
            self.recent_cycles.contains(&cycle.key)
        });
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
    /// and net-diff equality alone are not independence proofs.
    pub(crate) fn retain_validation_across_changes(
        &mut self,
        revision: u64,
        tracker: &crate::turn_diff_tracker::TurnDiffTracker,
    ) {
        let Some(before) = self.validation_coverage_revision else { return; };
        if before == revision || tracker.current_mutation_revision() != revision { return; }
        let Some(changes) = tracker.validation_changes_since(before) else { return; };
        self.validation_coverage.retain(|_, scope| {
            scope.dependency_scope.as_ref().is_some_and(|dependencies| {
                !dependencies.is_empty() && !changes.iter().any(|(environment, path)| {
                    if *environment != scope.environment_id { return false; }
                    let path = SourceDependencyV1::new(path, false);
                    dependencies.iter().any(|dependency| source_scope_contains(dependency, &path.path))
                })
            })
        });
        for failed in self.failed_validation_tests.values_mut() {
            let independent = failed.scope.as_ref().and_then(|scope|
                scope.dependency_scope.as_ref().map(|dependencies| (scope, dependencies)))
                .is_some_and(|(scope, dependencies)| !dependencies.is_empty()
                    && !changes.iter().any(|(environment, path)| *environment == scope.environment_id
                        && dependencies.iter().any(|dependency|
                            source_scope_contains(dependency, &SourceDependencyV1::new(path, false).path))));
            if !independent { failed.evidence.passed.clear(); }
            failed.revision = revision;
        }
        self.validation_coverage_revision = Some(revision);
    }

    fn reconcile_test_repairs(&mut self, check: &str, revision: u64,
        execution_order: (u64, u64), scope: Option<ValidationScope>, evidence: &RunnerTestEvidence, failed: bool)
    {
        if failed && evidence.required.as_ref().is_some_and(|required|
            !required.is_empty() && !required.is_subset(&evidence.passed))
        {
            self.failed_validation_tests.insert(check.to_string(), FailedTestValidation {
                evidence: evidence.clone(), revision, execution_order, scope,
            });
        }
        for previous in self.failed_validation_tests.values_mut()
            .filter(|previous| previous.revision == revision
                && previous.execution_order <= execution_order
                && previous.evidence.input_context == evidence.input_context)
        {
            if failed {
                if let Some(required) = &evidence.required {
                    previous.evidence.passed.retain(|test| !required.contains(test));
                } else {
                    previous.evidence.passed.clear();
                }
            }
            previous.evidence.passed.extend(evidence.passed.iter().cloned());
            previous.execution_order = execution_order;
        }
        if failed { return; }
        self.failed_validation_tests.retain(|check, previous| {
            let repaired = previous.revision == revision
                && previous.execution_order <= execution_order
                && previous.evidence.input_context == evidence.input_context
                && previous.evidence.required.as_ref().is_some_and(|required|
                    required.is_subset(&previous.evidence.passed));
            if repaired {
                self.failed_validation_checks.remove(check);
                self.inherited_validation_checks.remove(check);
            }
            !repaired
        });
    }

    pub(crate) fn settle(
        &mut self,
        baselines: &SamplingRequestBaselines,
        collector: &SamplingRequestSignalCollector,
        settled: &SamplingRequestSettledState,
    ) {
        if self.validation_coverage_revision != Some(settled.mutation_revision) {
            self.validation_coverage.clear();
            self.validation_coverage_revision = Some(settled.mutation_revision);
        }
        for failed in self.failed_validation_tests.values_mut() {
            if failed.revision != settled.mutation_revision {
                failed.evidence.passed.clear();
                failed.revision = settled.mutation_revision;
            }
        }
        {
            let state = collector.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut outcomes = state.outcomes.iter().collect::<Vec<_>>();
            outcomes.sort_by_key(|outcome| outcome.ordinal);
            for outcome in outcomes {
                if state.replayed_ordinals.contains(&outcome.ordinal) || outcome.failure_diagnosis_reused {
                    continue;
                }
                let pending = outcome.observed_process_id
                    .and_then(|id| self.pending_validation_coverage.get(&id).cloned());
                let scope = outcome.validation_scope.clone()
                    .or_else(|| pending.as_ref().and_then(|pending| pending.scope.clone()));
                let check = pending.as_ref().map(|pending| pending.check.clone())
                    .or_else(|| state.validation_check_identities.get(&outcome.ordinal).cloned());
                if let Some(check) = check.filter(|_| pending.is_some() || scope.is_some()
                    || state.validation_proof_ordinals.contains(&outcome.ordinal))
                {
                    // A poll's admitted revision describes observation, not
                    // when the background validation actually executed.
                    let revision = pending.as_ref().map(|pending| pending.revision)
                        .or(outcome.validation_mutation_revision)
                        .unwrap_or(collector.request_mutation_revision);
                    let test_execution = state.test_validation_ordinals.contains(&outcome.ordinal)
                        || scope.as_ref().is_some_and(|scope| scope.test_execution)
                        || pending.as_ref().is_some_and(|pending| pending.test_execution);
                    // Keep launch order across polls. A workspace revision alone
                    // cannot order contradictory executions against the same inputs.
                    let execution_order = pending.as_ref().map(|pending| pending.execution_order)
                        .unwrap_or((collector.request_ordinal, outcome.ordinal));
                    let inherited = pending.as_ref().is_some_and(|pending| pending.inherited);
                    if let Some(id) = outcome.background_process_id {
                        self.pending_validation_coverage.insert(id, PendingValidation {
                            revision, execution_order, inherited, check, scope, test_execution,
                        });
                        continue;
                    }
                    if self.validation_check_revisions.get(&check).is_some_and(|latest|
                        *latest > revision || *latest == revision
                            && self.validation_check_orders.get(&check).is_some_and(|order| *order > execution_order)) {
                        // The diagnostic remains in history; only its active
                        // proof bookkeeping is superseded by the newer run.
                        if let Some(id) = outcome.observed_process_id {
                            self.pending_validation_coverage.remove(&id);
                        }
                        continue;
                    }
                    let repair_scope = scope.clone();
                    if outcome.kind == SamplingToolOutcomeKind::Success
                        && (!test_execution || outcome.tests_executed || outcome.tests_attributed)
                    {
                        // Retain the revision actually tested even when settlement
                        // advanced without a mutation ordinal. A later delivery
                        // must still see that validation gap.
                        self.validation_check_revisions.insert(check.clone(), revision);
                        self.validation_check_orders.insert(check.clone(), execution_order);
                        if !inherited && self.validated_mutation_revision.is_none_or(|latest| revision >= latest) {
                            self.validated_mutation_revision = Some(revision);
                            self.validated_check = None;
                        }
                        if revision == settled.mutation_revision {
                            self.failed_validation_checks.remove(&check);
                            self.inherited_validation_checks.remove(&check);
                            self.failed_validation_tests.remove(&check);
                            self.validation_coverage.remove(&check);
                            if let Some(scope) = scope.filter(|_| !inherited) {
                                self.validated_check = Some(check.clone());
                                self.validation_coverage.insert(check.clone(), scope);
                            }
                        }
                    } else if matches!(outcome.kind, SamplingToolOutcomeKind::Failure
                        | SamplingToolOutcomeKind::Timeout | SamplingToolOutcomeKind::RecoverableCancellation)
                    {
                        self.validation_check_revisions.insert(check.clone(), revision);
                        self.validation_check_orders.insert(check.clone(), execution_order);
                        self.failed_validation_checks.insert(check.clone());
                        if inherited { self.inherited_validation_checks.insert(check.clone()); }
                        else { self.inherited_validation_checks.remove(&check); }
                        self.failed_validation_tests.remove(&check);
                        if inherited {
                            // Historical failure is not evidence against a
                            // fresh execution in this turn, even at revision 0.
                            if let Some(id) = outcome.observed_process_id {
                                self.pending_validation_coverage.remove(&id);
                            }
                            continue;
                        }
                        if test_execution && outcome.runner_test_evidence.is_none() {
                            // Without per-test outcomes a newer failure cannot
                            // leave older partial passes silently authoritative.
                            for previous in self.failed_validation_tests.values_mut() {
                                previous.evidence.passed.clear();
                            }
                        }
                        let previous = self.validation_coverage.remove(&check);
                        if let Some(failed) = scope.filter(|scope| !scope.paths.is_empty())
                            .or(previous.filter(|scope| !scope.paths.is_empty()))
                        {
                            for (_, coverage) in self.validation_coverage.iter_mut()
                                .filter(|(check, coverage)| coverage.environment_id == failed.environment_id
                                    && self.validation_check_revisions.get(*check).is_none_or(|latest| *latest <= revision))
                            {
                                coverage.paths.retain(|path| !failed.paths.iter().any(|failed|
                                    source_scope_contains(failed, &path.path)
                                        || source_scope_contains(path, &failed.path)));
                                coverage.behavioral_paths.retain(|path| !failed.paths.iter().any(|failed|
                                    source_scope_contains(failed, &path.path)
                                        || source_scope_contains(path, &failed.path)));
                            }
                            self.validation_coverage.retain(|_, coverage| !coverage.paths.is_empty());
                        } else {
                            // No dependency attribution can prove another scope
                            // independent of this failed execution.
                            self.validation_coverage.retain(|check, _|
                                self.validation_check_revisions.get(check).is_some_and(|latest| *latest > revision));
                        }
                    }
                    if !inherited && revision == settled.mutation_revision
                        && matches!(outcome.kind, SamplingToolOutcomeKind::Success | SamplingToolOutcomeKind::Failure)
                        && let Some(evidence) = &outcome.runner_test_evidence
                    {
                        self.reconcile_test_repairs(&check, revision, execution_order, repair_scope, evidence,
                            outcome.kind == SamplingToolOutcomeKind::Failure);
                    }
                }
                if outcome.kind != SamplingToolOutcomeKind::Yielded
                    && let Some(id) = outcome.observed_process_id
                {
                    self.pending_validation_coverage.remove(&id);
                }
            }
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
                    (gate.state_revision != state_revision && !gate.evidence.path_scoped)
                        || gate.action_identity != action_identity
                });
                let gate = Arc::new(SuccessfulReplayGate {
                    state_revision: state_revision.clone(),
                    action_identity,
                    response,
                    evidence,
                });
                if gate.evidence.path_scoped
                    && let Some(replays) = self.session_path_replays.as_ref()
                {
                    replays.retain(&gate);
                }
                ledger.successful_replay_gates.push_back(gate);
                while ledger.successful_replay_gates.len() > SUCCESSFUL_REPLAY_GATE_LIMIT {
                    ledger.successful_replay_gates.pop_front();
                }
            }
        }
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
    #[tokio::test]
    async fn verified10_cold_replay_requires_new_watch_and_rejects_mutation() {
        use crate::git_workspace::GitWorkspaceCache;
        use crate::git_workspace::WorkspaceEvidenceIdentity;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "retained").unwrap();
        let previous = GitWorkspaceCache::with_noop_watcher_for_tests();
        let old_paths = previous.begin_source_path_change_observations(
            directory.path(), &[(path.clone(), false)],
        ).await.unwrap();
        let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
        let workspace = WorkspaceEvidenceIdentity {
            unavailable: false, repository_root: Some(directory.path().to_string_lossy().into_owned()),
            head_identity: None, index_identity: None, worktree_identity: None, path_fingerprints: None,
        };
        let hash = crate::tool_history::sha256(b"retained");
        let guard = super::SuccessfulReplayGuard {
            response: codex_protocol::models::ResponseInputItem::FunctionCallOutput {
                call_id: "old-read".into(),
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(
                        serde_json::json!({"source_sha256":hash,"canonical_bytes":8}).to_string()),
                    success: Some(true),
                },
            },
            evidence: super::SuccessfulReplayEvidence {
                authorization_identity: Some("same-policy".into()),
                read_payload: Some(super::ToolPayload::Function {
                    arguments: serde_json::json!({"path":path}).to_string(),
                }),
                read_output: None, path_scoped: true, mutation_revision: 0,
                workspace_revision: Some(workspace.clone()), source_paths: old_paths,
            },
        };
        assert!(!guard.is_fresh(0, &cache, Some(&workspace)));
        assert_eq!(guard.cold_read_fingerprint(&cache, Some(&workspace)), Some((hash, 8)));
        let paths = cache.begin_source_path_change_observations(
            directory.path(), &[(path, false)],
        ).await.unwrap();
        let refreshed = guard.with_validated_source_paths(paths, Some(workspace.clone()), 0);
        assert!(refreshed.is_fresh(0, &cache, Some(&workspace)));
        assert!(refreshed.cold_read_fingerprint(&cache, Some(&workspace)).is_none());
        // A mutation after the bounded hash read but before installation rejects replay.
        cache.note_host_workspace_mutation();
        assert!(!refreshed.is_fresh(0, &cache, Some(&workspace)));
    }

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
        record_test_replay_dependencies(collector, registration.ordinal);
        collector.record_response_result(
            registration.ordinal,
            ToolOutputOutcomeContext::new(outcome),
            validation_invocation_status(&tool_name, &payload).2.then(test_execution_signal),
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
        json!({"runner_execution_receipt": {"executed_tests": 1, "exit_code": 0}})
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
            &plan, &crate::plan_store::PlanLineage::default(),
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
        settle_plan(&mut control, plan(&[StepStatus::InProgress, StepStatus::Pending]));
        let revision = control.plan_revision;
        settle_plan(&mut control, plan(&[StepStatus::Completed, StepStatus::Completed]));
        assert!(control.plan_completed());
        assert!(control.completion_gaps(0).is_empty());
        assert_eq!(control.plan_revision, revision + 1);
        settle_plan(&mut control, plan(&[StepStatus::Completed, StepStatus::Completed, StepStatus::Pending]));
        assert!(!control.plan_completed());
        assert_eq!(control.plan_revision, revision + 2);
    }

    #[test]
    fn plan_completion_hint_does_not_survive_new_input_or_turn_inheritance() {
        let completed = plan(&[StepStatus::Completed]);
        let snapshot = crate::plan_store::PlanExecutionSnapshot::new(&completed, &Default::default());
        let mut control = TurnExecutionControl::new().with_active_plan(Some(snapshot.clone()));
        assert!(!control.plan_completed());
        assert!(control.completion_gaps(0).is_empty(), "completed history is not new task debt");
        settle_plan(&mut control, plan(&[StepStatus::Pending]));
        settle_plan(&mut control, completed);
        assert!(control.plan_completed());
        control.accepted_user_input();
        control.refresh_plan(Some(snapshot));
        assert!(!control.plan_completed(), "unchanged old completion cannot qualify a new request");
    }

    fn lightweight_handoff_collector(
        control: &TurnExecutionControl,
        baselines: &SamplingRequestBaselines,
        generation: usize,
        calls: usize,
        runtime_ms: Option<u64>,
        output_bytes: usize,
    ) -> SamplingRequestSignalCollector {
        let collector = control.collector(baselines);
        for call in 0..calls {
            let id = format!("short-{generation}-{call}");
            let output = format!("{id}{}", " ".repeat(output_bytes));
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("exec_command"),
                &ToolPayload::Function {
                    arguments: json!({"cmd": format!("inspect-{generation}-{call}")}).to_string(),
                },
                &id,
            );
            collector.record_response_result(
                registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                Some(crate::tools::context::semantic_evidence_sampling_signal(json!(
                    crate::tools::context::semantic_evidence_for_command_output(id.as_bytes())
                ))),
                &successful_tool_response(&id, &output),
                false,
            );
            if let Some(runtime) = runtime_ms {
                collector.record_child_runtime(runtime);
            }
        }
        collector
    }

    #[test]
    fn lightweight_handoff_advisory_preserves_novel_progress_and_tool_access() {
        let mut measurements = Vec::new();
        for generations in [2, 3, 4] {
            for negligible_runtime_ms_per_call in [250, 500, 1_000] {
                let policy = HandoffEfficiencyPolicy { generations, negligible_runtime_ms_per_call };
                let mut control = TurnExecutionControl::new();
                control.handoff_policy = policy;
                // Replay the same producer evidence through the real control
                // path. Only advisory costs vary; novel work must stay possible.
                let (baselines, settled) = unchanged_state(&control);
                for generation in 1..=generations as usize {
                    let collector = lightweight_handoff_collector(
                        &control, &baselines, generation, 1, Some(100), 0,
                    );
                    assert_eq!(
                        control.observe_progress(&baselines, &collector, &settled),
                        vec![TurnTimingProgressKind::NewSourceEvidence],
                    );
                    assert_eq!(control.batching_advisory(true).is_some(),
                        generation == generations as usize);
                    assert!(control.batching_advisory(false).is_none());
                    let decision = control.evaluate_convergence(&baselines, &collector, &settled);
                    assert_eq!(decision.continuation, ContinuationDisposition::ModelRequired);
                    assert!(!decision.proven_loop_activated);
                    assert!(!control.continuation_generation_request(
                        &baselines, &collector, &settled, false,
                    ).terminal_completion_only);
                }
                let directive = control.batching_advisory(true).unwrap();
                assert!(directive.contains("Do not skip required reading"));
                assert!(control.admit_directive(&directive));
                assert!(control.batching_advisory(true).is_none());
                assert!(control.take_soft_convergence_directive(true).is_none());
                control.accepted_user_input();
                assert!(control.batching_advisory(true).is_none());
                let (baselines, settled) = unchanged_state(&control);
                let slow = lightweight_handoff_collector(
                    &control, &baselines, 99, 1, Some(negligible_runtime_ms_per_call + 1), 0,
                );
                control.observe_progress(&baselines, &slow, &settled);
                assert_eq!(control.lightweight_handoffs, 0);
                measurements.push(json!({
                    "baseline": policy == HandoffEfficiencyPolicy::default(),
                    "first_advisory_generation": generations,
                    "negligible_runtime_ms_per_call": negligible_runtime_ms_per_call,
                    "observed_child_runtime_before_advisory_ms": generations * 100,
                    "premature_terminal_decisions": 0,
                }));
            }
        }
        // Deterministic observations, not an estimate of model latency or a
        // production tuning recommendation. The gate pairs these with recovery
        // and cancellation regressions; no candidate changes safety budgets.
        eprintln!("{}", json!({"kind": "handoff_policy_replay_v1", "measurements": measurements}));
    }

    #[test]
    fn lightweight_handoff_window_resets_for_unmeasured_slow_batched_or_mutating_work() {
        for (calls, runtime, mutation_revision, output_bytes) in [
            (1, None, 0, 0),
            (1, Some(501), 0, 0),
            (3, Some(100), 0, 0),
            (1, Some(100), 1, 0),
            // A fast read that already filled the cell budget leaves nothing to batch.
            (1, Some(100), 0, OUTPUT_BOUND_HANDOFF_BYTES),
        ] {
            let mut control = TurnExecutionControl::new();
            settle_plan(&mut control, plan(&[StepStatus::InProgress]));
            let baselines = control.baselines(0);
            for generation in 0..2 {
                let collector = lightweight_handoff_collector(
                    &control, &baselines, generation, 1, Some(100), 0,
                );
                control.observe_progress(&baselines, &collector, &settled(0));
            }
            let boundary = lightweight_handoff_collector(
                &control, &baselines, 2, calls, runtime, output_bytes,
            );
            control.observe_progress(&baselines, &boundary, &settled(mutation_revision));
            assert!(control.batching_advisory(true).is_none());
            for generation in 3..6 {
                let collector = lightweight_handoff_collector(
                    &control, &baselines, generation, 1, Some(100), 0,
                );
                control.observe_progress(&baselines, &collector, &settled(0));
                assert_eq!(control.batching_advisory(true).is_some(), generation == 5);
            }
        }
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
        for _ in 0..SOFT_CONVERGENCE_NO_PROGRESS_GENERATIONS {
            observe_no_progress(&mut control);
        }
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
        assert!(control.take_soft_convergence_directive(true).is_none());
        assert!(
            !control
                .initial_generation_request(&control.baselines(0))
                .terminal_completion_only
        );
        let mut next_turn = TurnExecutionControl::new();
        assert!(next_turn.take_soft_convergence_directive(true).is_none());
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
            let mut pending = (index > 0).then_some(crate::turn_timing::ContinuationCause::ToolResult);
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
                Some(crate::tools::context::semantic_evidence_sampling_signal(json!(
                    crate::tools::context::semantic_evidence_for_command_output(text.as_bytes())
                ))),
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
    fn replayed_validation_is_not_new_execution_or_external_progress() {
        let mut control = TurnExecutionControl::new();
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
            !control
                .observe_progress(&baselines, &collector, &settled(0))
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
            Some(test_execution_signal()),
            &response,
            false,
        );
        assert_eq!(executed.executed_validation_summary().count, 1);
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
    fn nonexecuting_validation_modes_cannot_establish_collector_proof() {
        for command in ["cargo bench --no-run", "cargo test --no-run", "cargo fuzz list",
            "cargo fmt", "just --dry-run test", "cargo test -- --list"] {
            let collector = SamplingRequestSignalCollector::default();
            let payload = ToolPayload::Function { arguments:json!({"cmd":command}).to_string() };
            let status = validation_invocation_status(&ToolName::plain("exec_command"), &payload);
            assert_eq!(status, (true, false, false), "{command}");
            record_invocation_result(&collector, ToolName::plain("exec_command"), payload,
                "nonexecuting", ToolOutputOutcome::Success);
            let state = collector.state.lock().unwrap();
            assert!(state.validation_proof_ordinals.is_empty(), "{command}");
            assert!(state.test_validation_ordinals.is_empty(), "{command}");
        }
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
        mixed.record_child_runtime_for_call("read-call", 10);
        record_invocation_result(
            &mixed,
            ToolName::plain("exec_command"),
            ToolPayload::Function {
                arguments: r#"{"cmd":"cargo test -p codex-core focused"}"#.to_string(),
            },
            "mixed-validation",
            ToolOutputOutcome::Success,
        );
        mixed.record_child_runtime_for_call("mixed-validation", 100);
        mixed.record_child_runtime_for_call("mixed-validation", 100);
        assert_eq!(
            mixed.executed_validation_summary(),
            ExecutedValidationSummary {
                count: 1,
                duration_ms: 100,
            },
            "only the validation call contributes, even if its timing is observed twice"
        );
    }

    #[test]
    fn registered_semantics_classify_aliases_and_reject_name_spoofing() {
        use crate::tools::registry::CommandArgumentFormat;
        use crate::tools::registry::ToolSemanticCapabilities;
        let collector = SamplingRequestSignalCollector::default();
        let alias = ToolName::new(Some("local".to_string()), "check_project".to_string());
        collector.register_runtime_semantics(&alias, ToolSemanticCapabilities {
            command: Some(CommandArgumentFormat::Exec),
            ..Default::default()
        });
        record_invocation_result(
            &collector, alias,
            ToolPayload::Function { arguments: r#"{"cmd":"cargo check -p codex-core"}"#.into() },
            "alias-check", ToolOutputOutcome::Success,
        );
        collector.record_child_runtime_for_call("alias-check", 25);
        collector.register_runtime_semantics(
            &ToolName::plain("exec_command"), ToolSemanticCapabilities::default(),
        );
        record_invocation_result(
            &collector, ToolName::plain("exec_command"),
            ToolPayload::Function { arguments: r#"{"cmd":"cargo check -p codex-core"}"#.into() },
            "not-a-command-runtime", ToolOutputOutcome::Success,
        );
        assert_eq!(collector.executed_validation_summary(), ExecutedValidationSummary {
            count: 1, duration_ms: 25,
        });
        let edit = ToolName::plain("custom_editor");
        collector.register_runtime_semantics(&edit, ToolSemanticCapabilities {
            mutation: true, coordination: true, command: None,
        });
        record_invocation_result(
            &collector, edit, ToolPayload::Function { arguments: "{}".into() },
            "custom-edit", ToolOutputOutcome::Success,
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

        assert!(collector.fresh_successful_validation());
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
                json!({"cmd": "just test"}),
            ),
            (
                "exec_command",
                json!({"cmd": "npm run test:unit"}),
            ),
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
                collector.executed_validation_summary(),
                ExecutedValidationSummary {
                    count: 1,
                    duration_ms: 25
                },
                "{tool}: {arguments}"
            );
            assert!(collector.fresh_successful_validation());
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
                        !collector.fresh_successful_validation(),
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
            assert!(!collector.fresh_successful_validation(), "{command}");
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
                collector.fresh_successful_validation(),
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

            assert!(!collector.fresh_successful_validation());
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
        assert!(!collector.fresh_successful_validation());
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
                collector.fresh_successful_validation(),
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

            assert!(!collector.fresh_successful_validation());
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
        assert!(recovered.fresh_successful_validation());
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
        first.record_replay_authorization(registration.ordinal, Some("same-authority".into()));
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
        assert!(guard.matches_authorization(Some("same-authority")));
        assert!(!guard.matches_authorization(Some("different-authority")));
        assert!(!guard.matches_authorization(None));
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

        assert!(collector.fresh_successful_validation());
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
            assert!(collector.fresh_successful_validation(), "{scenario}");
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
        collector.state.lock().unwrap().authoritative_wait_observations[1].state_revision = "other".into();
        assert!(collector.authoritative_wait_observation().is_none());
    }

    #[test]
    fn native_receipt_requires_settled_success_not_plan_bookkeeping() {
        let mut control = TurnExecutionControl::new();
        let (baselines, settled) = unchanged_state(&control);
        let collector = authoritative_wait_collector(&control, &baselines, "native", false, Some("{}"));
        collector.state.lock().unwrap().authoritative_wait_observations[0].result.adapter =
            "agent_job_report".into();
        assert!(collector.authoritative_wait_observation().is_none(), "pending producer");
        collector.state.lock().unwrap().outcomes.push(
            SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None),
        );
        assert!(collector.authoritative_wait_observation().is_some());
        {
            let mut state = collector.state.lock().unwrap();
            state.outcomes.push(SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None));
            assert_eq!(state.outcomes.len(), 2);
        }
        assert!(collector.authoritative_wait_observation().is_none(), "duplicate is not another completion");
        {
            let mut state = collector.state.lock().unwrap();
            state.outcomes.pop();
            state.outcomes[0].kind = SamplingToolOutcomeKind::Failure;
        }
        assert!(collector.authoritative_wait_observation().is_none(), "failed producer");
        collector.state.lock().unwrap().outcomes[0].kind = SamplingToolOutcomeKind::Success;
        assert_eq!(control.evaluate_convergence(&baselines, &collector, &settled).continuation,
            ContinuationDisposition::SurfaceExistingResult);
        settle_plan(&mut control, plan(&[StepStatus::InProgress]));
        let baselines = control.baselines(settled.mutation_revision);
        assert_eq!(control.evaluate_convergence(&baselines, &collector, &settled).continuation,
            ContinuationDisposition::SurfaceExistingResult);
    }

    #[test]
    fn explicit_delivery_requires_consumed_process_and_complete_evidence() {
        for scenario in ["complete", "running", "failed", "unrelated", "missing",
                         "duplicate", "truncated", "out-of-order"] {
            let collector = SamplingRequestSignalCollector::default();
            {
                let mut state = collector.state.lock().unwrap();
                state.registered_count = 1;
                state.code_mode_nested_tool_count = 2;
                state.explicit_completion = Some((0, "verified answer".into()));
                let mut started = SamplingToolOutcome::plain(1, SamplingToolOutcomeKind::Yielded, None);
                started.background_process_id = Some(7);
                let mut observed = SamplingToolOutcome::plain(2, SamplingToolOutcomeKind::Success, None);
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
            assert_eq!(collector.explicit_completion().is_some(), scenario == "complete", "{scenario}");
        }
    }

    #[test]
    fn completion_boundary_retains_pending_validation_across_requests() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let launch = control.collector(&baseline);
        let ordinal = launch.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &validation_proof_payload(), "validation",
        ).ordinal;
        launch.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7})), &successful_tool_response("validation", "running"), false);
        control.settle(&baseline, &launch, &settled(0));
        assert_eq!(control.pending_validation_coverage.len(), 1);
        let delivery = control.collector(&baseline);
        {
            let mut state = delivery.state.lock().unwrap();
            state.registered_count = 1;
            state.explicit_completion = Some((0, "answer".into()));
            state.outcomes.push(SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None));
        }
        assert_ne!(control.evaluate_convergence(&baseline, &delivery, &settled(0)).continuation,
            ContinuationDisposition::SurfaceExistingResult);
        let poll = control.collector(&baseline);
        let ordinal = poll.register_deterministic_tool_call(&ToolName::plain("write_stdin"),
            &ToolPayload::Function { arguments: json!({"session_id":7}).to_string() }, "poll").ordinal;
        let mut signal = test_execution_signal();
        signal["observed_process_id"] = json!(7);
        poll.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal), &successful_tool_response("poll", "passed"), false);
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_coverage.is_empty());
        assert_eq!(control.evaluate_convergence(&baseline, &delivery, &settled(0)).continuation,
            ContinuationDisposition::SurfaceExistingResult);
    }

    #[test]
    fn completion_boundary_classifies_failures_uncertainty_and_bookkeeping() {
        let mut control = TurnExecutionControl::new();
        settle_plan(&mut control, plan(&[StepStatus::Pending]));
        let docs = vec![("local".into(), std::path::PathBuf::from("README.md"))];
        let report = control.completion_assessment_with_changed_paths(0, Some(&docs), false);
        assert!(report.failed_checks.is_empty());
        assert!(report.verification_gaps.is_empty());
        assert_eq!(report.advisories.len(), 2);
        let baseline = control.baselines(0);
        let failed = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Failure);
        control.settle(&baseline, &failed, &settled(0));
        let report = control.completion_assessment_with_changed_paths(0, Some(&docs), true);
        assert!(!report.failed_checks.is_empty());
        assert_eq!(report.verification_gaps.len(), 1);
        assert_eq!(report.advisories.len(), 2);
    }

    #[test]
    fn completion_boundary_unrelated_tool_output_is_not_hook_repair_evidence() {
        let collector = SamplingRequestSignalCollector::default();
        let ordinal = collector.register_deterministic_tool_call(&ToolName::plain("read_file"),
            &ToolPayload::Function { arguments: json!({"path":"unrelated.txt"}).to_string() }, "read").ordinal;
        collector.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            None, &successful_tool_response("read", "new but unrelated"), false);
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
                state.outcomes.push(SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None));
            }
            if new_input { control.input_revision += 1; }
            let decision = control.evaluate_convergence(&baselines, &collector, &settled);
            assert_eq!(decision.continuation == ContinuationDisposition::SurfaceExistingResult,
                expected, "{status:?}, new_input={new_input}");
            if !expected {
                assert_eq!(decision.continuation, ContinuationDisposition::ModelRequired);
            }
        }
    }

    #[test]
    fn completion_gaps_are_reported_without_automatic_repair_directives() {
        for mode in ["explicit", "native-text", "native-receipt"] {
            let mut control = TurnExecutionControl::new();
            let before = control.baselines(0);
            let validation = recorded_validation_collector(&control, &before, ToolOutputOutcome::Success);
            control.settle(&before, &validation, &settled(0));
            // Validation exists, but a later settled change invalidates it.
            let baselines = control.baselines(1);
            let receipt = if mode == "explicit" {
                let collector = control.collector(&baselines);
                {
                    let mut state = collector.state.lock().unwrap();
                    state.registered_count = 1;
                    state.explicit_completion = Some((0, "verified answer".into()));
                    state.outcomes.push(SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None));
                }
                collector
            } else {
                let message = (mode == "native-text").then_some("verified answer");
                let collector = authoritative_wait_collector(&control, &baselines, "native", false, message);
                {
                    let mut state = collector.state.lock().unwrap();
                    state.authoritative_wait_observations[0].result.adapter = "agent_job_report".into();
                    state.outcomes.push(SamplingToolOutcome::plain(0, SamplingToolOutcomeKind::Success, None));
                }
                collector
            };
            let stale = control.evaluate_convergence(&baselines, &receipt, &settled(1));
            let expected = if mode == "native-receipt" {
                ContinuationDisposition::TerminalCompletionRequired
            } else {
                ContinuationDisposition::SurfaceExistingResult
            };
            assert!(!control.completion_gaps(1).is_empty());
            assert_eq!(stale.continuation, expected);
            assert!(!stale.directive.as_deref().unwrap_or_default().contains("Completion preflight"));
            let failed = recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Failure);
            control.settle(&baselines, &failed, &settled(1));
            assert_eq!(control.evaluate_convergence(&baselines, &receipt, &settled(1)).continuation,
                expected, "failed validation remains a reportable gap, not an automatic continuation");
            assert!(!control.completion_gaps(1).is_empty());
            let passed = recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
            control.settle(&baselines, &passed, &settled(1));
            let expected = if mode == "native-receipt" {
                ContinuationDisposition::TerminalCompletionRequired
            } else {
                ContinuationDisposition::SurfaceExistingResult
            };
            assert_eq!(control.evaluate_convergence(&baselines, &receipt, &settled(1)).continuation,
                expected, "fresh evidence surfaces or synthesizes from the existing receipt");
            settle_plan(&mut control, plan(&[StepStatus::Pending]));
            let pending = control.baselines(1);
            assert_eq!(control.evaluate_convergence(&pending, &receipt, &settled(1)).continuation,
                expected, "unfinished plan bookkeeping must not override explicit delivery: {mode}");
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
    fn cosmetic_plan_revisions_do_not_reset_repeated_read_convergence() {
        let mut control = TurnExecutionControl::new();
        let mut checklist = plan(&[StepStatus::Pending, StepStatus::Pending]);
        for generation in 0..4 {
            let (baselines, settled) = unchanged_state(&control);
            checklist.plan.reverse();
            checklist.explanation = Some(format!("presentation {generation}"));
            control.refresh_plan(Some(crate::plan_store::PlanExecutionSnapshot::new(
                &checklist, &crate::plan_store::PlanLineage::default(),
            )));
            let (collector, _) = read_only_pass_collector(
                &control, &baselines, r#"{"artifact_id":"same-source"}"#, "same-evidence",
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
        lineage.requirements.insert("orphan".into(), crate::plan_store::PlanRequirement {
            text: "unmapped remaining obligation".into(), status: StepStatus::Pending,
            superseded_reason: None,
        });
        store.restore_with_lineage(Some(checklist.clone()), Some(lineage)).await;
        let snapshot = store.execution_snapshot().await.unwrap();
        for reverse in [false, true] {
            let mut control = TurnExecutionControl::new().with_active_plan(Some(snapshot.clone()));
            let (baselines, settled) = unchanged_state(&control);
            let collector = control.collector(&baselines);
            for ordinal in if reverse { [100, 0] } else { [0, 100] } {
                collector.push(SamplingToolOutcome::plain(
                    ordinal, SamplingToolOutcomeKind::Success, Some(checklist.clone()),
                ));
            }
            control.refresh_plan(store.execution_snapshot().await);
            control.settle(&baselines, &collector, &settled);
            assert!(!control.plan_completed(), "compaction must not infer completion from checklist alone");
            assert_eq!(control.plan.as_ref(), Some(&snapshot));
            assert_eq!(control.completion_gaps(0), vec!["The plan still has 1 unresolved obligation(s)."]);
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
        for (start, new_source, new_failure) in [(0, true, true), (10, true, false), (20, true, false), (10, false, false)] {
            let collector = control.collector(&baselines);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain("read_tool_output"),
                &ToolPayload::Function { arguments: json!({"artifact_id":"snapshot", "selectors":[
                    {"kind":"bytes", "start":start, "end":start + 10}]}).to_string() }, "page");
            collector.record_response_result(registration.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
                Some(json!({"outcome":"failure", "failure_signature":"same-selection-error",
                    "semantic_evidence":{"source":"artifact", "scope":"snapshot",
                        "identity":{"sha256":"source", "ranges":[[start, start + 10]], "values":[]}}})),
                &successful_tool_response("page", "partial recovery"), false);
            let progress = control.observe_progress(&baselines, &collector, &settled);
            assert_eq!(progress.contains(&TurnTimingProgressKind::NewSourceEvidence), new_source);
            assert_eq!(progress.contains(&TurnTimingProgressKind::FailureObservation), new_failure);
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
        let fingerprint = |args: Value, result: Value| code_mode_result_failure_fingerprint(
            &ToolName::plain("fixture"),
            &ToolPayload::Function { arguments: args.to_string() },
            &result,
        );
        let base = serde_json::json!({"actual":7, "port":8080, "timeout":"7s", "error":"src/a.rs:12:3: failed"});
        let expected = fingerprint(serde_json::json!({"port":8080}), base.clone());
        for (key, value) in [("actual", serde_json::json!(9)), ("port", serde_json::json!(8081)),
            ("timeout", serde_json::json!("9s")), ("error", serde_json::json!("timeout after 9s"))] {
            let mut changed = base.clone();
            changed[key] = value;
            assert_ne!(expected, fingerprint(serde_json::json!({"port":8080}), changed), "{key}");
        }
        assert_ne!(expected, fingerprint(serde_json::json!({"port":8081}), base.clone()));
        let mut moved = base.clone();
        moved["error"] = serde_json::json!("src/a.rs:99:8: failed");
        assert_eq!(expected, fingerprint(serde_json::json!({"port":8080}), moved));
        assert_ne!(fingerprint(serde_json::json!({}), serde_json::json!({"error":"timeout after 7s"})),
            fingerprint(serde_json::json!({}), serde_json::json!({"error":"timeout after 9s"})));
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

        for (_generation, (artifact_id, fingerprint)) in [
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

        for (_index, (artifact_id, fingerprint)) in [
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
            &ToolPayload::Function { arguments: r#"{"plan":[]}"#.into() },
            "plan",
        );
        collector.push(SamplingToolOutcome::plain(
            registration.ordinal, SamplingToolOutcomeKind::Success,
            Some(plan(&[StepStatus::InProgress])),
        ));
        control.settle(&baselines, &collector, &settled);
        let decision = control.evaluate_convergence(&baselines, &collector, &settled);
        assert!(matches!(decision.authoritative_wait, Some(AuthoritativeWaitResolution::Blocked(_))));
        assert!(!decision.proven_loop_activated);
        let repeated = control.collector(&control.baselines(7));
        assert!(repeated.register_deterministic_tool_call(
            &ToolName::plain("wait_agent"),
            &ToolPayload::Function { arguments: r#"{"cursor":"cursor-1"}"#.into() },
            "repeat",
        ).blocked_wait_guard.is_some());
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
        record_invocation_result(
            &collector,
            ToolName::plain("exec_command"),
            validation_proof_payload(),
            "validation-after-mutation",
            ToolOutputOutcome::Success,
        );
        let mutation_settled = settled(1);
        validated_after_mutation.settle(&baselines, &collector, &mutation_settled);
        assert!(collector.fresh_successful_validation());
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
        assert!(!collector.fresh_successful_validation());

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
        assert!(collector.fresh_successful_validation());
        assert_eq!(
            observed_after_validation
                .evaluate_convergence(&baselines, &collector, &settled_state)
                .continuation,
            ContinuationDisposition::ModelRequired
        );
    }

    #[test]
    fn restored_active_plan_preserves_completion_obligations_without_update() {
        let mut control = TurnExecutionControl::new().with_active_plan(Some(
            crate::plan_store::PlanExecutionSnapshot::new(
                &plan(&[StepStatus::Completed, StepStatus::Pending]),
                &crate::plan_store::PlanLineage::default(),
            ),
        ));
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        control.settle(&baselines, &collector, &settled(0));
        assert_eq!(control.completion_gaps(0), vec!["The plan still has 1 unresolved obligation(s)."]);
    }

    #[test]
    fn failed_only_runner_repairs_require_current_inputs_and_execution_identity() {
        for variant in ["unchanged", "changed", "helpers", "binary", "environment", "profile"] {
            let mut control = TurnExecutionControl::new();
            let record = |control: &mut TurnExecutionControl, revision, passed: bool, test: &str| {
                let baseline = control.baselines(revision);
                let collector = control.collector(&baseline);
                let payload = ToolPayload::Function { arguments: json!({
                    "cmd": format!("cargo test {test}")
                }).to_string() };
                let identity = |name: &str| json!({
                    "binary": if passed && variant == "binary" { "other" } else { "core" },
                    "helpers": if passed && variant == "helpers" { vec!["helper"] } else { vec![] },
                    "test": name,
                });
                let signal = json!({
                    "command_validation": {"validation": true, "proof": true, "tests": true,
                        "execution_context": {"cwd": "/repo", "environment_id": "local",
                            "environment_fingerprint": if passed && variant == "environment" { "new" } else { "original" }}},
                    "runner_execution_receipt": {
                        "executed_tests": 1, "exit_code": if passed { 0 } else { 2 },
                        "runner_input_fingerprint": "a".repeat(64),
                        "execution_configuration": {"cargo_profile": if passed && variant == "profile" { "release" } else { "test" }},
                        "dependency_manifest": {"execution_context_sha256": "b".repeat(64)},
                        "executions": [identity(if passed { test } else { "B" })],
                        "required_executions": if passed { json!([identity(test)]) } else { json!([identity("A"), identity("B")]) },
                    }
                });
                let ordinal = collector.register_deterministic_tool_call(&ToolName::plain("exec_command"), &payload, test).ordinal;
                collector.record_response_result(ordinal, ToolOutputOutcomeContext::new(if passed {
                    ToolOutputOutcome::Success
                } else { ToolOutputOutcome::Failure }), Some(signal), &runner_tool_response(test, "test result"), false);
                control.settle(&baseline, &collector, &settled(revision));
            };
            record(&mut control, 0, false, "A+B");
            assert_eq!(control.failed_validation_checks.len(), 1);
            let revision = u64::from(variant == "changed");
            record(&mut control, revision, true, "A");
            assert_eq!(control.failed_validation_checks.is_empty(), variant == "unchanged", "{variant}");
            if variant == "changed" {
                assert!(control.completion_gaps(revision).iter().any(|gap| gap.contains("Unfulfilled test IDs") && gap.contains('B')));
                record(&mut control, revision, true, "B");
                assert!(control.failed_validation_checks.is_empty());
            }
        }
    }

    #[test]
    fn formatting_and_dependency_inputs_do_not_establish_behavioral_coverage() {
        let root = tempfile::tempdir().unwrap();
        let changed = [("local".to_string(), root.path().join("unrelated.rs"))];
        for test_execution in [false, true] {
            let mut control = TurnExecutionControl::new();
            control.validation_coverage_revision = Some(0);
            control.validation_coverage.insert("check".into(), ValidationScope {
                environment_id: "local".into(), test_execution,
                paths: BTreeSet::from([SourceDependencyV1::new(root.path(), true)]),
                behavioral_paths: BTreeSet::new(), dependency_scope: None,
            });
            let report = control.completion_assessment_with_changed_paths(0, Some(&changed), false);
            assert!(report.verification_gaps.iter().any(|gap| gap.contains("unrelated.rs") && gap.contains("behavior remains unverified")));
            assert_eq!(report.advisories.iter().any(|gap| gap.contains("Non-test checks passed")), !test_execution);
            // An owner-proved surface is still insufficient for a non-test check.
            control.validation_coverage.get_mut("check").unwrap().behavioral_paths =
                BTreeSet::from([SourceDependencyV1::new(&changed[0].1, false)]);
            assert_eq!(control.completion_assessment_with_changed_paths(0, Some(&changed), false)
                .verification_gaps.is_empty(), test_execution);
        }
    }

    #[test]
    fn validation_path_attribution_does_not_promote_package_graph_to_complete_inputs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"),
            "[package]\nname = 'fixture'\nversion = '0.1.0'\nedition = '2021'\n[workspace]\n").unwrap();
        let dependencies = BTreeSet::from([SourceDependencyV1::new(root.path(), true)]);
        for tool in ["cargo_test", "exec_command", "write_stdin"] {
            let payload = ToolPayload::Function { arguments: json!({
                "package": "fixture", "program": "cargo", "args": ["test", "-p", "fixture"],
                "session_id": 1,
            }).to_string() };
            let signal = json!({
                "command_validation": {"proof": true, "tests": true},
                "runner_execution_receipt": {
                    "runner": "rust_test_runner", "selected_packages": ["fixture"],
                    "workspace_root": root.path(),
                },
            });
            let result = validation_scope_signal(&ToolName::plain(tool), &payload,
                Some(signal), Some(&dependencies), root.path(), "local").unwrap();
            let scope: ValidationScope = serde_json::from_value(result["validation_scope"].clone()).unwrap();
            assert!(!scope.paths.is_empty(), "attribution remains useful for {tool}");
            assert!(scope.behavioral_paths.is_empty(), "input graphs do not prove exercised surfaces");
            assert!(scope.dependency_scope.is_none(), "{tool} cannot prove arbitrary external inputs absent");
        }
    }

    #[test]
    fn validation_attribution_uses_execution_receipt_environment_and_cwd() {
        let root = tempfile::tempdir().unwrap();
        let secondary = root.path().join("secondary");
        std::fs::create_dir(&secondary).unwrap();
        let payload = ToolPayload::Function { arguments: serde_json::json!({
            "program": "pytest", "args": [], "environment_id": "secondary",
            "workdir": "secondary",
        }).to_string() };
        let dependencies = BTreeSet::from([SourceDependencyV1::new(root.path(), true)]);
        let signal = serde_json::json!({"command_validation": {
            "proof": true, "tests": false,
            "execution_context": {"environment_id": "secondary", "cwd": secondary},
        }});
        let attributed = validation_scope_signal(&ToolName::plain("exec_command"), &payload,
            Some(signal.clone()), Some(&dependencies), root.path(), "primary").unwrap();
        assert_eq!(attributed["validation_scope"]["environment_id"], "secondary");
        let scope: ValidationScope = serde_json::from_value(attributed["validation_scope"].clone()).unwrap();
        assert_eq!(scope.paths, BTreeSet::from([SourceDependencyV1::new(&secondary, true)]));
        let mut remote = signal;
        remote["command_validation"]["execution_context"]["cwd"] = Value::Null;
        let unavailable = validation_scope_signal(&ToolName::plain("exec_command"), &payload,
            Some(remote), Some(&dependencies), root.path(), "primary").unwrap();
        assert!(unavailable.get("validation_scope").is_none());
        assert!(unavailable.get("validation_scope_unavailable").is_some());
    }

    #[test]
    fn completion_gaps_report_post_validation_changes_and_unfinished_plans() {
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

        settle_plan(&mut control, plan(&[StepStatus::Completed, StepStatus::Pending]));
        assert_eq!(
            control.completion_gaps(1),
            vec!["The plan still has 1 unresolved obligation(s)."]
        );
        settle_plan(&mut control, plan(&[StepStatus::Completed, StepStatus::Completed]));
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
            }).to_string(),
        };
        let original = json!({"command_validation": {"proof": true, "tests": false}});
        for dependencies in [None, Some(BTreeSet::new())] {
            let signal = validation_scope_signal(
                &tool, &payload, Some(original.clone()), dependencies.as_ref(), root.path(), "local",
            ).unwrap();
            assert!(signal.get("validation_scope").is_none(), "{signal}");
        }
        let dependencies = BTreeSet::from([SourceDependencyV1::new(&root.path().join("src"), true)]);
        let signal = validation_scope_signal(
            &tool, &payload, Some(original), Some(&dependencies), root.path(), "local",
        ).unwrap();
        assert!(signal.get("validation_scope").is_none(), "{signal}");
    }

    #[test]
    fn validation_path_attribution_requires_exact_changes_and_passing_fresh_execution() {
        let root = tempfile::tempdir().unwrap();
        let changed = vec![
            ("local".to_string(), root.path().join("src/lib.rs")),
            ("local".to_string(), root.path().join("other/lib.rs")),
        ];
        let dependencies = BTreeSet::from([SourceDependencyV1::new(&root.path().join("src"), true)]);
        for nested in [false, true] {
            for scenario in ["passed", "summary", "failed", "skipped", "zero_tests", "stale", "background", "after_edit"] {
                let mut control = TurnExecutionControl::new();
                let baseline = control.baselines(0);
                let collector = control.collector(&baseline);
                let payload = validation_proof_payload();
                let tool = ToolName::plain("exec_command");
                let mut signal = validation_scope_signal(
                    &tool, &payload, None, Some(&dependencies), root.path(), "local",
                ).unwrap();
                if scenario == "after_edit" {
                    signal["validation_mutation_revision"] = json!(1);
                }
                let outcome = match scenario {
                    "failed" => ToolOutputOutcome::Failure,
                    "skipped" => ToolOutputOutcome::Skipped,
                    "background" => {
                        signal["background_process_id"] = json!(7);
                        ToolOutputOutcome::Yielded
                    }
                    _ => ToolOutputOutcome::Success,
                };
                let output = if scenario == "zero_tests" { "Ran 0 tests\nOK" }
                    else { "Ran 1 test in 0.001s\nOK" };
                if scenario == "summary" {
                    signal["validation_summary_tests"] = json!(98);
                } else if scenario != "zero_tests" && scenario != "background" {
                    signal["runner_execution_receipt"] =
                        test_execution_signal()["runner_execution_receipt"].clone();
                }
                if nested {
                    collector.record_code_mode_result(CodeModeToolResult {
                        cell_id: "coverage-cell", tool_name: &tool, payload: &payload,
                        source_dependencies: Some(dependencies.clone()),
                        outcome_context: ToolOutputOutcomeContext::new(outcome),
                        signal: Some(&signal), result: &json!({"output": output}),
                        canonical_artifact_required: false,
                    });
                } else {
                    let ordinal = collector.register_deterministic_tool_call(&tool, &payload, "coverage").ordinal;
                    collector.record_response_result(
                        ordinal, ToolOutputOutcomeContext::new(outcome), Some(signal),
                        &runner_tool_response("coverage", output), false,
                    );
                }
                let revision = u64::from(matches!(scenario, "stale" | "after_edit"));
                control.settle(&baseline, &collector, &settled(revision));
                if scenario == "background" {
                    let baseline = control.baselines(0);
                    let collector = control.collector(&baseline);
                    let tool = ToolName::plain("write_stdin");
                    let payload = ToolPayload::Function {
                        arguments: json!({"session_id": 7}).to_string(),
                    };
                    let mut signal = validation_scope_signal(
                        &tool, &payload, None, None, root.path(), "local",
                    ).unwrap_or_else(|| json!({}));
                    signal["runner_execution_receipt"] =
                        test_execution_signal()["runner_execution_receipt"].clone();
                    let ordinal = collector.register_deterministic_tool_call(&tool, &payload, "poll").ordinal;
                    collector.record_response_result(
                        ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success), Some(signal),
                        &runner_tool_response("poll", output), false,
                    );
                    control.settle(&baseline, &collector, &settled(0));
                }
                let revision_gaps = control.completion_gaps_with_changed_paths(revision, None, false);
                assert_eq!(revision_gaps.len(), usize::from(matches!(scenario, "failed" | "stale")),
                    "{nested} {scenario}: {revision_gaps:?}");
                let gaps = control.completion_gaps_with_changed_paths(revision, Some(&changed), false);
                assert_eq!(gaps.len(), 1 + revision_gaps.len(), "{nested} {scenario}: {gaps:?}");
                let path_gap = gaps.last().unwrap();
                assert!(path_gap.contains(&changed[1].1.display().to_string()));
                assert!(path_gap.contains(&changed[0].1.display().to_string()),
                    "input attribution never proves behavior: {nested} {scenario}: {gaps:?}");
                // Only a turn with no attribution at all points at runner declaration.
                assert_eq!(
                    path_gap.contains("No passing validation was attributed"),
                    !matches!(scenario, "passed" | "summary" | "background" | "after_edit"),
                    "{nested} {scenario}: {gaps:?}",
                );
            }
        }
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
        assert!(control.completion_gaps(1).iter().any(|gap| gap.contains("workspace changed")));
        assert_delivery_retains_validation_gap(&mut control, 1);

        let baselines = control.baselines(1);
        let validation = recorded_validation_collector(&control, &baselines, ToolOutputOutcome::Success);
        control.settle(&baselines, &validation, &settled(1));
        assert!(control.completion_gaps(1).is_empty());
    }

    #[test]
    fn late_validation_results_cannot_retract_newer_proof() {
        for outcome in [ToolOutputOutcome::Success, ToolOutputOutcome::Failure] {
            let mut control = TurnExecutionControl::new();
            let old_baseline = control.baselines(0);
            let old = recorded_validation_collector(&control, &old_baseline, outcome);
            let baseline = control.baselines(1);
            let fresh = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
            let scope = ValidationScope {
                environment_id: "local".into(),
                paths: BTreeSet::from([SourceDependencyV1::new(std::path::Path::new("src/lib.rs"), false)]),
                test_execution: true,
                behavioral_paths: BTreeSet::new(),
                dependency_scope: None,
            };
            fresh.state.lock().unwrap().outcomes[0].validation_scope = Some(scope);
            control.settle(&baseline, &fresh, &settled(1));
            let validated_check = control.validated_check.clone();
            assert!(validated_check.is_some());
            control.settle(&old_baseline, &old, &settled(1));
            assert_eq!(control.validated_mutation_revision, Some(1));
            assert_eq!(control.validated_check, validated_check);
            assert_eq!(control.validation_coverage.len(), 1);
            assert!(control.completion_gaps(1).is_empty());
        }
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
                    let payload = ToolPayload::Function { arguments: if passed {
                        json!({"program": "sh", "args": ["-c", "python -m unittest tests.a"]})
                    } else {
                        json!({"cmd": "python -m unittest tests.a", "shell": "sh"})
                    }.to_string() };
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
                    let outcome = if passed { ToolOutputOutcome::Success } else { ToolOutputOutcome::Failure };
                    if nested {
                        collector.record_code_mode_result(CodeModeToolResult {
                            cell_id: "checks", tool_name: &tool, payload: &payload,
                            source_dependencies: None,
                            outcome_context: ToolOutputOutcomeContext::new(outcome),
                            signal: Some(&signal), result: &json!({"exit_code": if passed { 0 } else { 1 }}),
                            canonical_artifact_required: false,
                        });
                    } else {
                        let ordinal = collector.register_deterministic_tool_call(&tool, &payload, "check").ordinal;
                        collector.record_response_result(ordinal, ToolOutputOutcomeContext::new(outcome),
                            Some(signal), &runner_tool_response("check", "Ran 1 test"), false);
                    }
                    control.settle(&baseline, &collector, &settled(0));
                }
                assert_eq!(control.failed_validation_checks.is_empty(), variant == "same", "{nested} {variant}");
            }
        }
    }

    fn assert_delivery_retains_validation_gap(control: &mut TurnExecutionControl, revision: u64) {
        let baselines = control.baselines(revision);
        let delivery = control.collector(&baselines);
        let ordinal = delivery.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Custom { input: "text('validation remains unverified')".into() },
            "later-delivery",
        ).ordinal;
        delivery.record_response_result(
            ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(json!({"explicit_completion_message": "validation remains unverified"})),
            &successful_tool_response("later-delivery", "validation remains unverified"), false,
        );
        let before = control.completion_gaps(revision);
        assert!(!before.is_empty());
        control.settle(&baselines, &delivery, &settled(revision));
        // A truthful report may be surfaced, but cannot erase the evidence gap
        // passed to the shared finalization/stop-hook policy.
        assert_eq!(control.evaluate_convergence(&baselines, &delivery, &settled(revision)).continuation,
            ContinuationDisposition::SurfaceExistingResult);
        assert_eq!(control.completion_gaps(revision), before);
    }

    #[test]
    fn later_failed_validation_retracts_only_affected_coverage_across_delivery() {
        let root = tempfile::tempdir().unwrap();
        let changed = ["a/lib.rs", "b/lib.rs", "shared/lib.rs"].map(|path|
            ("local".to_string(), root.path().join(path)));
        for nested in [false, true] {
            for separate_request in [false, true] {
                for failure_has_scope in [false, true] {
                    let mut control = TurnExecutionControl::new();
                    let baseline = control.baselines(0);
                    let collector = control.collector(&baseline);
                    let record = |collector: &SamplingRequestSignalCollector, check: &str, passed: bool| {
                        let tool = ToolName::plain("exec_command");
                        let mut arguments = json!({"cmd": format!("python -m unittest tests.{check}")});
                        if !passed {
                            arguments["force_fresh"] = json!(true);
                            arguments["max_output_tokens"] = json!(100);
                        }
                        let payload = ToolPayload::Function { arguments: arguments.to_string() };
                        let paths = BTreeSet::from([
                            SourceDependencyV1::new(&root.path().join(check), true),
                            SourceDependencyV1::new(&root.path().join("shared"), true),
                        ]);
                        let mut signal = test_execution_signal();
                        if passed || failure_has_scope {
                            signal["validation_scope"] = json!(ValidationScope {
                                behavioral_paths: paths.clone(),
                                environment_id: "local".into(), paths: paths.clone(), test_execution: true, dependency_scope: None,
                            });
                        }
                        let outcome = if passed { ToolOutputOutcome::Success } else { ToolOutputOutcome::Failure };
                        if nested {
                            collector.record_code_mode_result(CodeModeToolResult {
                                cell_id: "checks", tool_name: &tool, payload: &payload,
                                source_dependencies: Some(paths),
                                outcome_context: ToolOutputOutcomeContext::new(outcome),
                                signal: Some(&signal), result: &json!({"exit_code": if passed { 0 } else { 1 }}),
                                canonical_artifact_required: false,
                            });
                        } else {
                            let ordinal = collector.register_deterministic_tool_call(&tool, &payload, check).ordinal;
                            collector.record_response_result(ordinal, ToolOutputOutcomeContext::new(outcome),
                                Some(signal), &runner_tool_response(check, "Ran 1 test"), false);
                        }
                    };
                    record(&collector, "a", true);
                    record(&collector, "b", true);
                    let failure = if separate_request {
                        control.settle(&baseline, &collector, &settled(0));
                        assert!(control.completion_gaps_with_changed_paths(0, Some(&changed), false).is_empty());
                        control.collector(&baseline)
                    } else { collector };
                    record(&failure, "a", false);
                    control.settle(&baseline, &failure, &settled(0));
                    let gaps = control.completion_gaps_with_changed_paths(0, Some(&changed), false);
                    assert_eq!(gaps.len(), 2, "{nested} {separate_request} {failure_has_scope}: {gaps:?}");
                    assert!(gaps[0].contains("1 validation check(s) failed"));
                    assert!(gaps[1].contains(&changed[0].1.display().to_string()));
                    assert!(!gaps[1].contains(&changed[1].1.display().to_string()));
                    assert!(gaps[1].contains(&changed[2].1.display().to_string()));
                    assert_delivery_retains_validation_gap(&mut control, 0);

                    let unrelated = control.collector(&baseline);
                    record(&unrelated, "b", true);
                    control.settle(&baseline, &unrelated, &settled(0));
                    assert_eq!(control.failed_validation_checks.len(), 1);
                    let repair = control.collector(&baseline);
                    record(&repair, "a", true);
                    control.settle(&baseline, &repair, &settled(0));
                    assert!(control.completion_gaps_with_changed_paths(0, Some(&changed), false).is_empty());
                }
            }
        }
    }

    #[test]
    fn failed_background_validation_supersedes_its_original_check() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let passed = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
        control.settle(&baseline, &passed, &settled(0));
        let launch = control.collector(&baseline);
        let ordinal = launch.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &validation_proof_payload(), "background-check",
        ).ordinal;
        launch.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("background-check", "running"), false);
        control.settle(&baseline, &launch, &settled(0));
        let poll = control.collector(&baseline);
        let ordinal = poll.register_deterministic_tool_call(
            &ToolName::plain("write_stdin"),
            &ToolPayload::Function { arguments: json!({"session_id": 7}).to_string() }, "poll",
        ).ordinal;
        poll.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Failure),
            Some(json!({"observed_process_id": 7})), &successful_tool_response("poll", "FAILED"), false);
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_coverage.is_empty());
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
        let ordinal = launch.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &validation_proof_payload(), "old-check",
        ).ordinal;
        launch.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("old-check", "running"), false);
        control.settle(&baseline, &launch, &settled(0));

        let newer = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Failure);
        control.settle(&baseline, &newer, &settled(0));
        assert_eq!(control.failed_validation_checks.len(), 1);
        let poll = control.collector(&baseline);
        let ordinal = poll.register_deterministic_tool_call(
            &ToolName::plain("write_stdin"),
            &ToolPayload::Function { arguments: json!({"session_id": 7}).to_string() }, "old-poll",
        ).ordinal;
        let mut signal = test_execution_signal();
        signal["observed_process_id"] = json!(7);
        poll.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal), &runner_tool_response("old-poll", "Ran 1 test"), false);
        control.settle(&baseline, &poll, &settled(0));
        assert!(control.pending_validation_coverage.is_empty());
        assert_eq!(control.failed_validation_checks.len(), 1);
        assert!(control.validated_mutation_revision.is_none());

        let repair = recorded_validation_collector(&control, &baseline, ToolOutputOutcome::Success);
        control.settle(&baseline, &repair, &settled(0));
        assert!(control.failed_validation_checks.is_empty());
    }

    #[test]
    fn uncertainty_turn_boundary_retains_questions_not_passing_proof() {
        let owner = Arc::new(Mutex::new(SessionValidationUncertainty::default()));
        let mut first = TurnExecutionControl::new().with_session_validation_uncertainty(Arc::clone(&owner));
        let baseline = first.baselines(0);
        let failure = recorded_validation_collector(&first, &baseline, ToolOutputOutcome::Failure);
        first.settle(&baseline, &failure, &settled(0));
        let launch = first.collector(&baseline);
        let ordinal = launch.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &validation_proof_payload(), "carry-check",
        ).ordinal;
        launch.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("carry-check", "running"), false);
        first.settle(&baseline, &launch, &settled(0));
        drop(first); // Cancellation also takes this path.
        let mut next = TurnExecutionControl::new().with_session_validation_uncertainty(Arc::clone(&owner));
        assert_eq!(next.failed_validation_checks.len(), 1);
        assert_eq!(next.pending_validation_coverage.len(), 1);
        let assessment = next.completion_assessment(0);
        assert_eq!(assessment.advisories.len(), 2);
        assert!(assessment.failed_checks.is_empty());
        assert!(assessment.verification_gaps.is_empty());
        let baseline = next.baselines(0);
        let poll = next.collector(&baseline);
        let ordinal = poll.register_deterministic_tool_call(&ToolName::plain("write_stdin"),
            &ToolPayload::Function { arguments: json!({"session_id": 7}).to_string() }, "carry-poll").ordinal;
        let mut signal = test_execution_signal();
        signal["observed_process_id"] = json!(7);
        poll.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
            Some(signal), &runner_tool_response("carry-poll", "Ran 1 test"), false);
        next.settle(&baseline, &poll, &settled(0));
        assert!(next.pending_validation_coverage.is_empty());
        assert!(next.failed_validation_checks.is_empty());
        assert!(next.validated_mutation_revision.is_none());
        assert!(next.validation_coverage.is_empty());
        let repair = recorded_validation_collector(&next, &baseline, ToolOutputOutcome::Success);
        next.settle(&baseline, &repair, &settled(0));
        assert_eq!(next.validated_mutation_revision, Some(0));
        drop(next);
        let last = TurnExecutionControl::new().with_session_validation_uncertainty(owner);
        assert!(last.completion_assessment(0).advisories.is_empty());
        assert!(last.validated_mutation_revision.is_none());
    }

    #[test]
    fn background_validation_keeps_execution_revision_across_later_polls() {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let launch = control.collector(&baseline);
        let ordinal = launch.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &validation_proof_payload(), "background-check",
        ).ordinal;
        launch.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Yielded),
            Some(json!({"background_process_id": 7, "validation_mutation_revision": 0})),
            &successful_tool_response("background-check", "running"), false);
        control.settle(&baseline, &launch, &settled(0));
        for revision in [1, 2] {
            let baseline = control.baselines(revision);
            let poll = control.collector(&baseline);
            let ordinal = poll.register_deterministic_tool_call(
                &ToolName::plain("write_stdin"),
                &ToolPayload::Function { arguments: json!({"session_id": 7}).to_string() }, "poll",
            ).ordinal;
            let mut signal = test_execution_signal();
            signal["observed_process_id"] = json!(7);
            signal["validation_mutation_revision"] = json!(revision);
            let outcome = if revision == 1 {
                signal["background_process_id"] = json!(7);
                ToolOutputOutcome::Yielded
            } else { ToolOutputOutcome::Success };
            poll.record_response_result(ordinal, ToolOutputOutcomeContext::new(outcome),
                Some(signal), &runner_tool_response("poll", "Ran 1 test"), false);
            control.settle(&baseline, &poll, &settled(revision));
        }
        assert_eq!(control.validated_mutation_revision, Some(0));
        assert!(control.pending_validation_coverage.is_empty());
        assert_delivery_retains_validation_gap(&mut control, 2);
    }

    #[test]
    fn verified_partial_changes_keep_known_path_gaps_and_report_unknown_changes() {
        let control = TurnExecutionControl::new();
        let paths = vec![("local".into(), std::path::PathBuf::from("src/changed.rs"))];
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&paths), true);
        assert!(gaps.iter().any(|gap| gap.contains("Untracked changes exist")));
        assert!(gaps.iter().any(|gap| gap.contains("src") && gap.contains("changed.rs")));
        let unknown = control.completion_gaps_with_changed_paths(0, None, true);
        assert!(unknown.iter().any(|gap| gap.contains("Untracked changes exist")));
    }

    #[test]
    fn uncertainty_skill_instructions_are_not_documentation_only() {
        for path in ["skills/demo/SKILL.md", "skills/demo/skill.md", r"skills\demo\SKILL.md",
            "AGENTS.md", "AGENTS.override.md", "codex-rs/prompts/templates/compact/prompt.md"]
        {
            assert!(!documentation_only_path(std::path::Path::new(path)), "{path}");
        }
        assert!(documentation_only_path(std::path::Path::new("docs/README.md")));
        let control = TurnExecutionControl::new();
        let changed = vec![("local".into(), std::path::PathBuf::from("skills/demo/SKILL.md"))];
        let assessment = control.completion_assessment_with_changed_paths(0, Some(&changed), false);
        assert_eq!(assessment.verification_gaps.len(), 1);
        assert!(assessment.advisories.is_empty());
    }

    #[tokio::test]
    async fn validation_survives_only_exact_changes_outside_its_full_dependency_graph() {
        let root = tempfile::tempdir().unwrap();
        let cwd = codex_utils_path_uri::PathUri::from_host_native_path(root.path()).unwrap();
        let mut tracker = crate::turn_diff_tracker::TurnDiffTracker::new();
        let mut control = TurnExecutionControl::new();
        let record = |control: &mut TurnExecutionControl, check: &str, revision: u64| {
            let baseline = control.baselines(revision);
            let collector = control.collector(&baseline);
            let payload = ToolPayload::Function { arguments: json!({"cmd": format!("python -m unittest tests.{check}")}).to_string() };
            let ordinal = collector.register_deterministic_tool_call(&ToolName::plain("exec_command"), &payload, check).ordinal;
            let paths = BTreeSet::from([SourceDependencyV1::new(&root.path().join(check), true)]);
            let dependency_scope = Some(paths.iter().cloned().chain([
                SourceDependencyV1::new(&root.path().join("shared"), true),
                SourceDependencyV1::new(&root.path().join("Cargo.toml"), false),
                SourceDependencyV1::new(&root.path().join("generated"), true),
            ]).collect());
            let mut signal = test_execution_signal();
            signal["validation_mutation_revision"] = json!(revision);
            signal["validation_scope"] = json!(ValidationScope {
                behavioral_paths: paths.clone(),
                environment_id: "local".into(), paths, test_execution: true, dependency_scope,
            });
            collector.record_response_result(ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                Some(signal), &runner_tool_response(check, "Ran 1 test"), false);
            control.settle(&baseline, &collector, &settled(revision));
        };
        // Carry a completed check across a disjoint documentation edit without
        // pretending that the check executed at the later revision.
        for (path, survives) in [
            ("README.md", true), ("a/lib.rs", false),
            ("Cargo.toml", false), ("generated/input.rs", false),
        ] {
            let mut control = TurnExecutionControl::new();
            let mut tracker = crate::turn_diff_tracker::TurnDiffTracker::new();
            record(&mut control, "a", 0);
            let patch = format!("*** Begin Patch\n*** Add File: {path}\n+changed\n*** End Patch");
            let delta = codex_apply_patch::apply_patch(&patch, &cwd, &mut Vec::new(), &mut Vec::new(),
                codex_exec_server::LOCAL_FS.as_ref(), None).await.unwrap();
            tracker.track_delta("local", &delta);
            let revision = tracker.current_mutation_revision();
            control.retain_validation_across_changes(revision, &tracker);
            let baseline = control.baselines(revision);
            control.settle(&baseline, &control.collector(&baseline), &settled(revision));
            assert_eq!(control.validated_mutation_revision, Some(0));
            assert_eq!(control.completion_gaps(revision).is_empty(), survives, "{path}");
            assert_eq!(!control.validation_coverage.is_empty(), survives, "{path}");
        }
        record(&mut control, "a", 0);
        for (path, retains_a) in [("b/lib.rs", true), ("shared/lib.rs", false)] {
            let patch = format!("*** Begin Patch\n*** Add File: {path}\n+changed\n*** End Patch");
            let delta = codex_apply_patch::apply_patch(&patch, &cwd, &mut Vec::new(), &mut Vec::new(),
                codex_exec_server::LOCAL_FS.as_ref(), None).await.unwrap();
            tracker.track_delta("local", &delta);
            let revision = tracker.current_mutation_revision();
            control.retain_validation_across_changes(revision, &tracker);
            if retains_a { record(&mut control, "b", revision); }
            else {
                let baseline = control.baselines(revision);
                let collector = control.collector(&baseline);
                control.settle(&baseline, &collector, &settled(revision));
            }
            assert_eq!(control.validation_coverage.values().any(|scope|
                scope.paths.contains(&SourceDependencyV1::new(&root.path().join("a"), true))), retains_a);
            if !retains_a { assert!(control.validation_coverage.is_empty(), "shared dependency invalidates both checks"); }
        }
        record(&mut control, "a", tracker.current_mutation_revision());
        tracker.record_unknown_mutation();
        let revision = tracker.current_mutation_revision();
        control.retain_validation_across_changes(revision, &tracker);
        let baseline = control.baselines(revision);
        control.settle(&baseline, &control.collector(&baseline), &settled(revision));
        assert!(control.validation_coverage.is_empty());
    }

    #[test]
    fn documentation_is_listed_separately_from_validation_gaps() {
        let control = TurnExecutionControl::new();
        let paths = vec![("local".into(), std::path::PathBuf::from("README.md")),
            ("local".into(), std::path::PathBuf::from("AGENTS.md")),
            ("local".into(), std::path::PathBuf::from("skills/example/SKILL.md")),
            ("local".into(), std::path::PathBuf::from("codex-rs/prompts/templates/compact/prompt.md")),
            ("local".into(), std::path::PathBuf::from("src/lib.rs"))];
        let gaps = control.completion_gaps_with_changed_paths(0, Some(&paths), false);
        let code = gaps.iter().find(|gap| gap.starts_with("Changed paths")).unwrap();
        assert!(!code.contains("README.md"));
        assert!(code.contains("AGENTS.md"));
        assert!(code.contains("SKILL.md"));
        assert!(code.contains("prompt.md"));
        assert!(gaps.iter().any(|gap| gap.starts_with("Documentation-only") && gap.contains("README.md") && !gap.contains("prompt.md")));
        assert!(!gaps.iter().any(|gap| gap.contains("No recognized validation command passed")));
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
            assert!(collector.fresh_successful_validation(), "{commands:?}");
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

        assert_eq!(
            collector.generation_purpose(&baseline, &settled(0), false, false,),
            Some(TurnTimingGenerationPurpose::ArtifactContinuation)
        );
        control.settle(&baseline, &collector, &settled(0));
    }
}
