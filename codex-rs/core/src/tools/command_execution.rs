use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::time::Duration;

use codex_agent_task_store::AttemptId;
use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::validation::ValidationFreshness;
use codex_protocol::validation::ValidationProofKey;
use codex_protocol::validation::ValidationResult;
use codex_protocol::validation::ValidationTerminalStatus;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::handlers::command_search::RgSearchBreadth;
use crate::tools::handlers::command_search::RgSearchNarrowing;
use crate::validation_admission::ValidationLaunchPlan;

const MAX_TRACKED_COMMANDS: usize = 128;
const MAX_COMPLETED_VALIDATION_PROOFS: usize = 128;
const COMMAND_EXECUTION_CACHE_SCHEMA_VERSION: u32 = 1;
static NEXT_COMMAND_EXECUTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct CommandExecutionId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionApplyResult {
    Applied,
    AlreadyApplied,
    Stale,
    Missing,
}

impl CompletionApplyResult {
    #[cfg(test)]
    pub(crate) fn accepted(self) -> bool {
        matches!(self, Self::Applied | Self::AlreadyApplied)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CommandAttemptKey {
    tool_name: String,
    environment_id: String,
    cwd: String,
    command: Vec<String>,
    search_narrowing: Option<SearchNarrowingAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchNarrowingAttempt {
    turn_id: String,
    environment_id: String,
    repository_identity: String,
    breadth: RgSearchBreadth,
    query_identity: String,
    search_identity: String,
    scope_identity: String,
    parent_scope_identity: Option<String>,
    can_record_miss: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct SearchMissCacheKey {
    environment_id: String,
    repository_identity: String,
    search_identity: String,
    execution_context: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchNarrowingScope {
    turn_id: String,
    environment_id: String,
    repository_identity: String,
    query_identity: String,
    scope_identity: String,
}

impl SearchNarrowingAttempt {
    fn parent_scope(&self) -> Option<SearchNarrowingScope> {
        Some(SearchNarrowingScope {
            turn_id: self.turn_id.clone(),
            environment_id: self.environment_id.clone(),
            repository_identity: self.repository_identity.clone(),
            query_identity: self.query_identity.clone(),
            scope_identity: self.parent_scope_identity.clone()?,
        })
    }
}

impl CommandAttemptKey {
    pub(crate) fn new(
        tool_name: &str,
        environment_id: &str,
        cwd: impl Into<String>,
        command: &[String],
    ) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            environment_id: environment_id.to_string(),
            cwd: cwd.into(),
            command: command.to_vec(),
            search_narrowing: None,
        }
    }

    pub(crate) fn with_executed_command(mut self, command: &[String]) -> Self {
        let context = self
            .command
            .iter()
            .filter(|argument| argument.starts_with('\0'))
            .cloned()
            .collect::<Vec<_>>();
        self.command = command.to_vec();
        self.command.extend(context);
        self
    }

    pub(crate) fn with_environment(self, environment: &HashMap<String, String>) -> Self {
        let mut entries = environment.iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|(left_key, left_value), (right_key, right_value)| {
            left_key
                .cmp(right_key)
                .then_with(|| left_value.cmp(right_value))
        });
        self.with_context_fingerprint("environment", &entries)
    }

    pub(crate) fn with_timeout_ms(self, timeout_ms: Option<u64>) -> Self {
        self.with_context_fingerprint("timeout_ms", &timeout_ms)
    }

    pub(crate) fn with_sandbox_context<T: Hash + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("sandbox", context)
    }

    pub(crate) fn with_permission_context<T: Hash + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("permission", context)
    }

    pub(crate) fn with_input_context<T: Hash + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("input", context)
    }

    pub(crate) fn with_runtime_context<T: Hash + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("runtime", context)
    }

    pub(crate) fn with_repository_epoch(self, epoch: u64) -> Self {
        self.with_context_fingerprint("repository_epoch", &epoch)
    }

    pub(crate) fn with_workspace_identity(self, identity: Option<&str>) -> Self {
        match identity {
            Some(identity) => self.with_context_fingerprint("workspace_identity", identity),
            None => self,
        }
    }

    pub(crate) fn with_search_narrowing(
        mut self,
        turn_id: &str,
        repository_identity: &str,
        search: Option<RgSearchNarrowing>,
    ) -> Self {
        self.search_narrowing = search.map(|search| SearchNarrowingAttempt {
            turn_id: turn_id.to_string(),
            environment_id: self.environment_id.clone(),
            repository_identity: repository_identity.to_string(),
            breadth: search.breadth,
            query_identity: search.query_identity,
            search_identity: search.search_identity,
            scope_identity: search.scope_identity,
            parent_scope_identity: search.parent_scope_identity,
            can_record_miss: search.can_record_miss,
        });
        self
    }

    fn search_miss_cache_key(&self) -> Option<SearchMissCacheKey> {
        let search = self.eligible_search_miss()?;
        Some(self.search_miss_cache_key_for(search))
    }

    fn eligible_search_miss(&self) -> Option<&SearchNarrowingAttempt> {
        self.search_narrowing
            .as_ref()
            .filter(|search| search.can_record_miss)
    }

    fn search_miss_cache_key_for(&self, search: &SearchNarrowingAttempt) -> SearchMissCacheKey {
        let has_workspace_identity = self
            .command
            .iter()
            .any(|argument| argument.starts_with("\0kd4-context:workspace_identity:"));
        SearchMissCacheKey {
            environment_id: search.environment_id.clone(),
            repository_identity: search.repository_identity.clone(),
            search_identity: search.search_identity.clone(),
            execution_context: self
                .command
                .iter()
                .filter(|argument| {
                    argument.starts_with('\0')
                        && !(has_workspace_identity
                            && argument.starts_with("\0kd4-context:repository_epoch:"))
                })
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn fingerprint(&self) -> String {
        format!("{:016x}", fingerprint_value(self))
    }

    fn with_context_fingerprint<T: Hash + ?Sized>(mut self, label: &str, value: &T) -> Self {
        let prefix = format!("\0kd4-context:{label}:");
        self.command
            .retain(|argument| !argument.starts_with(&prefix));
        self.command
            .push(format!("{prefix}{:016x}", fingerprint_value(value)));
        self
    }
}

fn fingerprint_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandAttemptBlocked {
    pub(crate) fingerprint: String,
    reason: CommandAttemptBlockedReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandAttemptBlockedReason {
    DeterministicFailure(DeterministicFailureRecord),
    SearchMiss,
}

impl CommandAttemptBlocked {
    pub(crate) fn render_for_model(&self) -> String {
        match &self.reason {
            CommandAttemptBlockedReason::DeterministicFailure(prior_failure) => format!(
                "Command failed: exact repeat of deterministic `{}` failure from the original attempt (fingerprint `{}`, exit code {}, evidence {:?}); execution was suppressed.",
                prior_failure.proof.outcome_class(),
                self.fingerprint,
                prior_failure.exit_code,
                prior_failure.evidence,
            ),
            CommandAttemptBlockedReason::SearchMiss => format!(
                "Search returned no matches: an equivalent search already produced a negative result under the unchanged repository and execution context (fingerprint `{}`); execution was suppressed. Change the query or scope, or use `force_fresh` when external state changed.",
                self.fingerprint,
            ),
        }
    }
}

/// Closed proof classes whose outcome is determined before a process starts or
/// mutable filesystem state is inspected. Keep this enum closed: ordinary
/// command failures and filesystem-dependent patch verification must remain
/// retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputStateDetermined {
    ApplyPatchImplicitInvocation,
    ApplyPatchEnvironmentIdMismatch,
}

impl InputStateDetermined {
    fn outcome_class(self) -> &'static str {
        match self {
            Self::ApplyPatchImplicitInvocation => "apply_patch implicit invocation",
            Self::ApplyPatchEnvironmentIdMismatch => "apply_patch environment mismatch",
        }
    }

    pub(crate) fn evidence_description(self) -> &'static str {
        match self {
            Self::ApplyPatchImplicitInvocation => {
                "input-determined apply_patch rejection: explicit invocation required"
            }
            Self::ApplyPatchEnvironmentIdMismatch => {
                "input-determined apply_patch rejection: selected environment mismatch"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeterministicFailureRecord {
    proof: InputStateDetermined,
    pub(crate) evidence: RawOutputArtifact,
    pub(crate) exit_code: i32,
}

impl DeterministicFailureRecord {
    fn from_input_state_determined(
        proof: InputStateDetermined,
        evidence: RawOutputArtifact,
        exit_code: i32,
    ) -> Self {
        Self {
            proof,
            evidence,
            exit_code,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AttemptEntry {
    attempts: u32,
    repairs: u32,
    consecutive_failures: u8,
    last_exit_code: Option<i32>,
    deterministic_failure: Option<DeterministicFailureRecord>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunningCommand {
    pub(crate) execution_id: CommandExecutionId,
    pub(crate) parent_tool_execution_id: ToolExecutionId,
    pub(crate) key: CommandAttemptKey,
    pub(crate) artifact: RawOutputArtifact,
    completed_exit_code: Option<i32>,
    validation_launch: Option<ValidationLaunchPlan>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
struct PendingCommandCompletion {
    process_id: u32,
    command: RunningCommand,
}

#[derive(Debug, Clone)]
struct CommandCompletionReceipt {
    parent_tool_execution_id: ToolExecutionId,
}

#[derive(Debug, Clone)]
struct CompletedValidationProof {
    result: Arc<ValidationResult>,
    artifact: RawOutputArtifact,
}

impl RunningCommand {
    pub(crate) fn completed_validation_skip_disposition(
        &self,
        output: &[u8],
        exit_code: i32,
    ) -> Option<codex_tools::ToolOutputSkipDisposition> {
        self.validation_launch
            .as_ref()
            .and_then(|launch| launch.structured_route.as_ref())
            .and_then(|route| completed_validation_skip_disposition(route, output, exit_code))
    }
}

#[derive(Debug, Clone)]
struct CommandExecutionPersistence {
    cache_path: PathBuf,
    shared_validation_cache_path: PathBuf,
    codex_home: PathBuf,
    thread_id: String,
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedValidationProof {
    result: ValidationResult,
    #[serde(default)]
    artifact_thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedValidationCacheDocument {
    schema_version: u32,
    completed_validations: Vec<PersistedValidationProof>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandExecutionCacheDocument {
    schema_version: u32,
    workspace_identity: crate::git_workspace::WorkspaceEvidenceIdentity,
    repository_epoch: u64,
    search_misses: Vec<SearchMissCacheKey>,
}

#[derive(Default)]
struct CommandProcessState {
    running: HashMap<u32, RunningCommand>,
    pending_by_execution_id: HashMap<CommandExecutionId, PendingCommandCompletion>,
    completion_receipts: HashMap<CommandExecutionId, CommandCompletionReceipt>,
    completion_receipt_order: VecDeque<CommandExecutionId>,
}

#[derive(Default)]
struct CommandRepositoryState {
    epoch: u64,
    observed_workspace_identity: Option<(u64, crate::git_workspace::WorkspaceEvidenceIdentity)>,
    observed_turn_mutation_revisions: HashMap<String, u64>,
    uncertain_command_baselines: HashMap<String, UncertainCommandBaseline>,
    typed_mutation_baselines: HashMap<String, PendingTypedMutationBaseline>,
}

struct UncertainCommandBaseline {
    turn_id: String,
    workspace_identity: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
}

pub(crate) struct TypedMutationBaseline {
    pub(crate) attempt_id: AttemptId,
    pub(crate) repo_root: PathBuf,
    pub(crate) paths: Vec<String>,
}

struct PendingTypedMutationBaseline {
    turn_id: String,
    baseline: TypedMutationBaseline,
}

#[derive(Default)]
struct CommandRetryState {
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
}

#[derive(Default)]
struct CommandSearchState {
    allowed_expansions: HashSet<SearchNarrowingScope>,
    misses: HashSet<SearchMissCacheKey>,
    miss_order: VecDeque<SearchMissCacheKey>,
}

#[derive(Default)]
struct CommandValidationState {
    completed: HashMap<ValidationProofKey, CompletedValidationProof>,
    completed_order: VecDeque<ValidationProofKey>,
    results_by_call: HashMap<String, Arc<ValidationResult>>,
    bound_plan_steps_by_call: HashMap<String, (String, u64)>,
    result_call_order: VecDeque<String>,
}

#[derive(Default)]
struct CommandExecutionState {
    retry: CommandRetryState,
    process: CommandProcessState,
    repository: CommandRepositoryState,
    validation: CommandValidationState,
    search: CommandSearchState,
}

pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    persistence: Option<CommandExecutionPersistence>,
}

impl Default for CommandExecutionLedger {
    fn default() -> Self {
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            persistence: None,
        }
    }
}

impl CommandExecutionLedger {
    pub(crate) fn allocate_execution_id(&self) -> CommandExecutionId {
        CommandExecutionId(NEXT_COMMAND_EXECUTION_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn load_or_new(codex_home: PathBuf, thread_id: String, cwd: &Path) -> Self {
        let Some(workspace_identity) =
            crate::git_workspace::capture_workspace_evidence_identity(cwd).await
        else {
            return Self::default();
        };
        let persistence = CommandExecutionPersistence {
            cache_path: codex_home
                .join("command-execution-cache")
                .join(format!("{thread_id}.json")),
            shared_validation_cache_path: shared_validation_cache_path(
                &codex_home,
                &workspace_identity,
                cwd,
            ),
            codex_home,
            thread_id,
            cwd: cwd.to_path_buf(),
        };
        let ledger = Self {
            state: Mutex::new(CommandExecutionState {
                repository: CommandRepositoryState {
                    observed_workspace_identity: Some((0, workspace_identity.clone())),
                    ..CommandRepositoryState::default()
                },
                ..CommandExecutionState::default()
            }),
            persistence: Some(persistence.clone()),
        };
        if let Ok(bytes) = tokio::fs::read(&persistence.cache_path).await
            && let Ok(document) = serde_json::from_slice::<CommandExecutionCacheDocument>(&bytes)
            && document.schema_version == COMMAND_EXECUTION_CACHE_SCHEMA_VERSION
            && document.workspace_identity == workspace_identity
        {
            let mut state = ledger.state.lock().await;
            state.repository.epoch = document.repository_epoch;
            state.repository.observed_workspace_identity =
                Some((document.repository_epoch, workspace_identity));
            for search_miss in document
                .search_misses
                .into_iter()
                .take(MAX_TRACKED_COMMANDS)
            {
                if state.search.misses.insert(search_miss.clone()) {
                    state.search.miss_order.push_back(search_miss);
                }
            }
        }
        ledger.refresh_shared_validation_cache().await;
        ledger
    }

    pub(crate) async fn admit_search_narrowing(
        &self,
        key: &CommandAttemptKey,
    ) -> Result<(), String> {
        let Some(search) = key.search_narrowing.as_ref() else {
            return Ok(());
        };
        if search.breadth == RgSearchBreadth::Narrow {
            return Ok(());
        }
        let scope = SearchNarrowingScope {
            turn_id: search.turn_id.clone(),
            environment_id: search.environment_id.clone(),
            repository_identity: search.repository_identity.clone(),
            query_identity: search.query_identity.clone(),
            scope_identity: search.scope_identity.clone(),
        };
        if self
            .state
            .lock()
            .await
            .search
            .allowed_expansions
            .contains(&scope)
        {
            return Ok(());
        }

        Err(
            "repository-wide `rg` search rejected: first search a narrower scope for the same query, then expand only after that search returns no matches"
                .to_string(),
        )
    }

    pub(crate) async fn record_uncertain_command_baseline(
        &self,
        call_id: &str,
        turn_id: &str,
        baseline: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) {
        self.state
            .lock()
            .await
            .repository
            .uncertain_command_baselines
            .insert(
                call_id.to_string(),
                UncertainCommandBaseline {
                    turn_id: turn_id.to_string(),
                    workspace_identity: baseline,
                },
            );
    }

    pub(crate) async fn take_uncertain_command_baseline(
        &self,
        call_id: &str,
    ) -> Option<Option<crate::git_workspace::WorkspaceEvidenceIdentity>> {
        self.state
            .lock()
            .await
            .repository
            .uncertain_command_baselines
            .remove(call_id)
            .map(|baseline| baseline.workspace_identity)
    }

    pub(crate) async fn record_typed_mutation_baseline(
        &self,
        call_id: &str,
        turn_id: &str,
        baseline: TypedMutationBaseline,
    ) {
        self.state
            .lock()
            .await
            .repository
            .typed_mutation_baselines
            .insert(
                call_id.to_string(),
                PendingTypedMutationBaseline {
                    turn_id: turn_id.to_string(),
                    baseline,
                },
            );
    }

    pub(crate) async fn has_typed_mutation_baseline(&self, call_id: &str) -> bool {
        self.state
            .lock()
            .await
            .repository
            .typed_mutation_baselines
            .contains_key(call_id)
    }

    pub(crate) async fn take_typed_mutation_baseline(
        &self,
        call_id: &str,
    ) -> Option<TypedMutationBaseline> {
        self.state
            .lock()
            .await
            .repository
            .typed_mutation_baselines
            .remove(call_id)
            .map(|pending| pending.baseline)
    }

    pub(crate) async fn reusable_validation(
        &self,
        key: &ValidationProofKey,
    ) -> Option<ValidationResult> {
        let mut proof = {
            let mut state = self.state.lock().await;
            supersede_validation_proofs_for_new_implementation(&mut state, key);
            state.validation.completed.get(key).cloned()
        };
        if proof.is_none() {
            self.refresh_shared_validation_cache().await;
            let mut state = self.state.lock().await;
            supersede_validation_proofs_for_new_implementation(&mut state, key);
            proof = state.validation.completed.get(key).cloned();
        }
        let proof = proof?;
        let Some((artifact_ref, artifact_sha256)) = proof.artifact.validation_integrity().await
        else {
            self.invalidate_validation_proof(key).await;
            return None;
        };
        if proof.result.raw_artifact_ref.as_deref() != Some(artifact_ref.as_str())
            || proof.result.raw_artifact_sha256.as_deref() != Some(artifact_sha256.as_str())
        {
            self.invalidate_validation_proof(key).await;
            return None;
        }
        let mut result = proof.result.as_ref().clone();
        result.freshness = ValidationFreshness::Reused;
        Some(result)
    }

    async fn invalidate_validation_proof(&self, key: &ValidationProofKey) {
        {
            let mut state = self.state.lock().await;
            state.validation.completed.remove(key);
            state
                .validation
                .completed_order
                .retain(|entry| entry != key);
        }
        self.remove_shared_validation(key).await;
    }

    async fn refresh_shared_validation_cache(&self) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let Ok(bytes) = tokio::fs::read(&persistence.shared_validation_cache_path).await else {
            return;
        };
        let Ok(document) = serde_json::from_slice::<PersistedValidationCacheDocument>(&bytes)
        else {
            return;
        };
        if document.schema_version != COMMAND_EXECUTION_CACHE_SCHEMA_VERSION {
            return;
        }
        let mut state = self.state.lock().await;
        restore_persisted_validations(
            &mut state,
            &persistence.codex_home,
            &persistence.thread_id,
            document.completed_validations,
        );
    }

    async fn persist_shared_validation(&self, proof: PersistedValidationProof) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let Ok(_permit) = shared_validation_cache_write_gate().acquire().await else {
            return;
        };
        let mut proofs = tokio::fs::read(&persistence.shared_validation_cache_path)
            .await
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<PersistedValidationCacheDocument>(&bytes).ok()
            })
            .filter(|document| document.schema_version == COMMAND_EXECUTION_CACHE_SCHEMA_VERSION)
            .map(|document| document.completed_validations)
            .unwrap_or_default();
        proofs.retain(|candidate| {
            candidate.result.status == ValidationTerminalStatus::Succeeded
                && candidate.result.proof_key != proof.result.proof_key
        });
        proofs.push(proof);
        let excess = proofs.len().saturating_sub(MAX_COMPLETED_VALIDATION_PROOFS);
        proofs.drain(..excess);
        let document = PersistedValidationCacheDocument {
            schema_version: COMMAND_EXECUTION_CACHE_SCHEMA_VERSION,
            completed_validations: proofs,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        write_cache_document(persistence.shared_validation_cache_path.clone(), bytes).await;
    }

    async fn remove_shared_validation(&self, key: &ValidationProofKey) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let Ok(_permit) = shared_validation_cache_write_gate().acquire().await else {
            return;
        };
        let Ok(bytes) = tokio::fs::read(&persistence.shared_validation_cache_path).await else {
            return;
        };
        let Ok(mut document) = serde_json::from_slice::<PersistedValidationCacheDocument>(&bytes)
        else {
            return;
        };
        if document.schema_version != COMMAND_EXECUTION_CACHE_SCHEMA_VERSION {
            return;
        }
        let previous_len = document.completed_validations.len();
        document
            .completed_validations
            .retain(|proof| proof.result.proof_key != *key);
        if document.completed_validations.len() == previous_len {
            return;
        }
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        write_cache_document(persistence.shared_validation_cache_path.clone(), bytes).await;
    }

    pub(crate) async fn validation_result_for_call(
        &self,
        call_id: &str,
    ) -> Option<ValidationResult> {
        self.state
            .lock()
            .await
            .validation
            .results_by_call
            .get(call_id)
            .map(|result| result.as_ref().clone())
    }

    pub(crate) async fn validation_result_with_plan_step_for_call(
        &self,
        call_id: &str,
    ) -> Option<(ValidationResult, Option<(String, u64)>)> {
        let state = self.state.lock().await;
        let result = state
            .validation
            .results_by_call
            .get(call_id)?
            .as_ref()
            .clone();
        let bound_plan_step = state
            .validation
            .bound_plan_steps_by_call
            .get(call_id)
            .cloned();
        Some((result, bound_plan_step))
    }

    pub(crate) async fn observe_repository_revision(
        &self,
        turn_id: &str,
        mutation_revision: u64,
    ) -> u64 {
        self.observe_repository_revision_with_identity(turn_id, mutation_revision, None)
            .await
    }

    pub(crate) async fn observe_repository_revision_with_identity(
        &self,
        turn_id: &str,
        mutation_revision: u64,
        observed_workspace_identity: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> u64 {
        let (repository_epoch, refresh_workspace_identity, expected_repository_root) = {
            let mut state = self.state.lock().await;
            let delta = {
                let observed_revision = state
                    .repository
                    .observed_turn_mutation_revisions
                    .entry(turn_id.to_string())
                    .or_default();
                let delta = mutation_revision.saturating_sub(*observed_revision);
                *observed_revision = (*observed_revision).max(mutation_revision);
                delta
            };
            state.repository.epoch = state.repository.epoch.saturating_add(delta);
            let repository_epoch = state.repository.epoch;
            let refresh_workspace_identity = delta > 0
                || state
                    .repository
                    .observed_workspace_identity
                    .as_ref()
                    .is_none_or(|(epoch, _)| *epoch != repository_epoch);
            let expected_repository_root = state
                .repository
                .observed_workspace_identity
                .as_ref()
                .and_then(|(_, identity)| identity.repository_root.clone());
            if refresh_workspace_identity {
                state.repository.observed_workspace_identity = None;
            }
            (
                repository_epoch,
                refresh_workspace_identity,
                expected_repository_root,
            )
        };

        if !refresh_workspace_identity {
            return repository_epoch;
        }
        let observed_workspace_identity = observed_workspace_identity.filter(|identity| {
            identity.repository_root.is_some()
                && identity.repository_root == expected_repository_root
        });
        let workspace_identity = match observed_workspace_identity {
            Some(workspace_identity) => workspace_identity,
            None => {
                let Some(persistence) = self.persistence.as_ref() else {
                    return repository_epoch;
                };
                let Some(workspace_identity) =
                    crate::git_workspace::capture_workspace_evidence_identity(&persistence.cwd)
                        .await
                else {
                    return repository_epoch;
                };
                workspace_identity
            }
        };
        let mut state = self.state.lock().await;
        if state.repository.epoch == repository_epoch
            && state.repository.observed_workspace_identity.is_none()
        {
            state.repository.observed_workspace_identity =
                Some((repository_epoch, workspace_identity));
        }
        state.repository.epoch
    }

    pub(crate) async fn current_workspace_identity_hash(
        &self,
        environment_id: &str,
        cwd: &Path,
    ) -> Option<String> {
        if environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID
            || self.persistence.as_ref()?.cwd != cwd
        {
            return None;
        }
        let state = self.state.lock().await;
        let (epoch, identity) = state.repository.observed_workspace_identity.as_ref()?;
        if *epoch != state.repository.epoch {
            return None;
        }
        let bytes = serde_json::to_vec(identity).ok()?;
        Some(format!("{:x}", Sha256::digest(bytes)))
    }

    #[cfg(test)]
    pub(crate) async fn begin_attempt(
        &self,
        key: &CommandAttemptKey,
        repaired: bool,
    ) -> Result<(), CommandAttemptBlocked> {
        self.begin_attempt_with_freshness(key, repaired, false)
            .await
    }

    pub(crate) async fn begin_attempt_with_freshness(
        &self,
        key: &CommandAttemptKey,
        repaired: bool,
        force_fresh: bool,
    ) -> Result<(), CommandAttemptBlocked> {
        let mut state = self.state.lock().await;
        if !repaired && !force_fresh {
            if let Some(prior_failure) = state
                .retry
                .attempts
                .get(key)
                .and_then(|entry| entry.deterministic_failure.clone())
            {
                return Err(CommandAttemptBlocked {
                    fingerprint: key.fingerprint(),
                    reason: CommandAttemptBlockedReason::DeterministicFailure(prior_failure),
                });
            }
            if let Some(search_miss_key) = key.search_miss_cache_key()
                && state.search.misses.contains(&search_miss_key)
            {
                return Err(CommandAttemptBlocked {
                    fingerprint: format!("{:016x}", fingerprint_value(&search_miss_key)),
                    reason: CommandAttemptBlockedReason::SearchMiss,
                });
            }
        }
        let entry = attempt_entry_locked(&mut state, key);
        entry.attempts = entry.attempts.saturating_add(1);
        if repaired {
            entry.repairs = entry.repairs.saturating_add(1);
        }
        Ok(())
    }

    pub(crate) async fn record_exit(&self, key: &CommandAttemptKey, exit_code: i32) {
        let mut state = self.state.lock().await;
        record_search_result_locked(&mut state, key, exit_code);
        record_exit_locked(&mut state, key, exit_code);
    }

    pub(crate) async fn record_input_state_determined_failure(
        &self,
        key: &CommandAttemptKey,
        proof: InputStateDetermined,
        evidence: RawOutputArtifact,
        exit_code: i32,
    ) {
        let mut state = self.state.lock().await;
        let entry = attempt_entry_locked(&mut state, key);
        entry.last_exit_code = Some(exit_code);
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.deterministic_failure = Some(
            DeterministicFailureRecord::from_input_state_determined(proof, evidence, exit_code),
        );
    }

    #[cfg(test)]
    pub(crate) async fn track_running_process(
        &self,
        process_id: u32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
    ) {
        let execution_id = self.allocate_execution_id();
        self.track_running_process_with_execution_id(
            execution_id,
            ToolExecutionId::default(),
            process_id,
            key,
            artifact,
            None,
            Instant::now(),
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) async fn track_running_process_with_validation_contract(
        &self,
        process_id: u32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
        validation_launch: Option<ValidationLaunchPlan>,
        started_at: Instant,
    ) {
        let execution_id = self.allocate_execution_id();
        self.track_running_process_with_execution_id(
            execution_id,
            ToolExecutionId::default(),
            process_id,
            key,
            artifact,
            validation_launch,
            started_at,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn track_running_process_with_execution_id(
        &self,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: ToolExecutionId,
        process_id: u32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
        validation_launch: Option<ValidationLaunchPlan>,
        started_at: Instant,
    ) {
        let mut state = self.state.lock().await;
        debug_assert_command_execution_invariants(&state);
        if state.process.running.contains_key(&process_id) {
            tracing::error!(process_id, "refusing to replace live command bookkeeping");
            return;
        }
        state.process.running.insert(
            process_id,
            RunningCommand {
                execution_id,
                parent_tool_execution_id,
                key,
                artifact,
                completed_exit_code: None,
                validation_launch,
                started_at,
            },
        );
        debug_assert_command_execution_invariants(&state);
    }

    pub(crate) async fn running_process(&self, process_id: u32) -> Option<RunningCommand> {
        self.state
            .lock()
            .await
            .process
            .running
            .get(&process_id)
            .cloned()
    }

    pub(crate) async fn process_execution_identity(
        &self,
        process_id: u32,
    ) -> Option<(CommandExecutionId, ToolExecutionId)> {
        let state = self.state.lock().await;
        state
            .process
            .running
            .get(&process_id)
            .map(|running| {
                (
                    running.execution_id,
                    running.parent_tool_execution_id.clone(),
                )
            })
            .or_else(|| {
                state
                    .process
                    .pending_by_execution_id
                    .iter()
                    .find(|(_, pending)| pending.process_id == process_id)
                    .map(|(execution_id, pending)| {
                        (
                            *execution_id,
                            pending.command.parent_tool_execution_id.clone(),
                        )
                    })
            })
    }

    pub(crate) async fn finish_turn(&self, turn_id: &str) {
        self.forget_turn_repository_revision(turn_id).await;
        self.state
            .lock()
            .await
            .repository
            .typed_mutation_baselines
            .retain(|_, pending| pending.turn_id != turn_id);
        self.persist_cache().await;
    }

    async fn persist_cache(&self) {
        let Some(persistence) = self.persistence.clone() else {
            return;
        };
        let (repository_epoch, observed_workspace_identity, search_misses) = {
            let state = self.state.lock().await;
            (
                state.repository.epoch,
                state.repository.observed_workspace_identity.clone(),
                state.search.miss_order.iter().cloned().collect::<Vec<_>>(),
            )
        };
        let Some((observed_epoch, observed_workspace_identity)) = observed_workspace_identity
        else {
            return;
        };
        if observed_epoch != repository_epoch {
            return;
        }
        let Some(workspace_identity) =
            crate::git_workspace::capture_workspace_evidence_identity(&persistence.cwd).await
        else {
            return;
        };
        if workspace_identity != observed_workspace_identity {
            return;
        }
        let document = CommandExecutionCacheDocument {
            schema_version: COMMAND_EXECUTION_CACHE_SCHEMA_VERSION,
            workspace_identity,
            repository_epoch,
            search_misses,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        write_cache_document(persistence.cache_path, bytes).await;
    }

    async fn forget_turn_repository_revision(&self, turn_id: &str) {
        let mut state = self.state.lock().await;
        state
            .repository
            .observed_turn_mutation_revisions
            .remove(turn_id);
        state
            .search
            .allowed_expansions
            .retain(|scope| scope.turn_id != turn_id);
        state
            .repository
            .uncertain_command_baselines
            .retain(|_, baseline| baseline.turn_id != turn_id);
    }

    pub(crate) async fn update_running_artifact(
        &self,
        process_id: u32,
        artifact: RawOutputArtifact,
    ) {
        {
            let mut state = self.state.lock().await;
            debug_assert_command_execution_invariants(&state);
            if let Some(running) = state.process.running.get_mut(&process_id) {
                running.artifact = artifact.clone();
            } else {
                if let Some(pending) = state
                    .process
                    .pending_by_execution_id
                    .values_mut()
                    .find(|pending| pending.process_id == process_id)
                {
                    pending.command.artifact = artifact;
                }
            }
            debug_assert_command_execution_invariants(&state);
        }
        self.publish_completed_validation_if_ready(process_id).await;
    }

    #[cfg(test)]
    pub(crate) async fn mark_running_process_completed(
        &self,
        process_id: u32,
        exit_code: i32,
    ) -> CompletionApplyResult {
        let identity = {
            let state = self.state.lock().await;
            state
                .process
                .running
                .get(&process_id)
                .map(|running| {
                    (
                        running.execution_id,
                        running.parent_tool_execution_id.clone(),
                    )
                })
                .or_else(|| {
                    state
                        .process
                        .pending_by_execution_id
                        .iter()
                        .find(|(_, pending)| pending.process_id == process_id)
                        .map(|(execution_id, pending)| {
                            (
                                *execution_id,
                                pending.command.parent_tool_execution_id.clone(),
                            )
                        })
                })
        };
        let Some((execution_id, parent_tool_execution_id)) = identity else {
            return CompletionApplyResult::Missing;
        };
        self.mark_process_exited(
            process_id,
            execution_id,
            &parent_tool_execution_id,
            exit_code,
        )
        .await
    }

    pub(crate) async fn mark_process_exited(
        &self,
        process_id: u32,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: &ToolExecutionId,
        exit_code: i32,
    ) -> CompletionApplyResult {
        {
            let mut state = self.state.lock().await;
            debug_assert_command_execution_invariants(&state);
            if state
                .process
                .running
                .get(&process_id)
                .is_some_and(|running| {
                    running.execution_id != execution_id
                        || &running.parent_tool_execution_id != parent_tool_execution_id
                })
            {
                return CompletionApplyResult::Stale;
            }
            if let Some(receipt) = state.process.completion_receipts.get(&execution_id) {
                return if &receipt.parent_tool_execution_id == parent_tool_execution_id {
                    CompletionApplyResult::AlreadyApplied
                } else {
                    CompletionApplyResult::Stale
                };
            }
            if let Some(pending) = state.process.pending_by_execution_id.get(&execution_id) {
                return if &pending.command.parent_tool_execution_id == parent_tool_execution_id {
                    CompletionApplyResult::AlreadyApplied
                } else {
                    CompletionApplyResult::Stale
                };
            }
            let Some(live) = state.process.running.get(&process_id) else {
                return if state
                    .process
                    .pending_by_execution_id
                    .values()
                    .any(|pending| pending.process_id == process_id)
                {
                    CompletionApplyResult::Stale
                } else {
                    CompletionApplyResult::Missing
                };
            };
            if live.execution_id != execution_id
                || &live.parent_tool_execution_id != parent_tool_execution_id
            {
                return CompletionApplyResult::Stale;
            }
            let Some(mut running) = state.process.running.remove(&process_id) else {
                return CompletionApplyResult::Missing;
            };
            running.completed_exit_code = Some(exit_code);
            record_running_exit_locked(&mut state, &running, exit_code);
            state.process.pending_by_execution_id.insert(
                execution_id,
                PendingCommandCompletion {
                    process_id,
                    command: running,
                },
            );
            debug_assert_command_execution_invariants(&state);
        }
        self.publish_completed_validation_if_ready(process_id).await;
        CompletionApplyResult::Applied
    }

    pub(crate) async fn retire_completed_process(
        &self,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: &ToolExecutionId,
    ) -> CompletionApplyResult {
        let mut state = self.state.lock().await;
        debug_assert_command_execution_invariants(&state);
        if let Some(receipt) = state.process.completion_receipts.get(&execution_id) {
            return if &receipt.parent_tool_execution_id == parent_tool_execution_id {
                CompletionApplyResult::AlreadyApplied
            } else {
                CompletionApplyResult::Stale
            };
        }
        let Some(pending) = state.process.pending_by_execution_id.get(&execution_id) else {
            return CompletionApplyResult::Missing;
        };
        if &pending.command.parent_tool_execution_id != parent_tool_execution_id {
            return CompletionApplyResult::Stale;
        }
        let Some(pending) = state.process.pending_by_execution_id.remove(&execution_id) else {
            return CompletionApplyResult::Missing;
        };
        insert_completion_receipt(
            &mut state,
            execution_id,
            pending.command.parent_tool_execution_id,
        );
        while state.retry.attempts.len() > MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(&mut state)
        {}
        debug_assert_command_execution_invariants(&state);
        CompletionApplyResult::Applied
    }

    async fn publish_completed_validation_if_ready(&self, process_id: u32) {
        let candidate = {
            let state = self.state.lock().await;
            let running = state.process.running.get(&process_id).or_else(|| {
                state
                    .process
                    .pending_by_execution_id
                    .values()
                    .find(|pending| pending.process_id == process_id)
                    .map(|pending| &pending.command)
            });
            let Some(running) = running else {
                return;
            };
            let Some(exit_code) = running.completed_exit_code else {
                return;
            };
            let Some(launch) = running.validation_launch.as_ref() else {
                return;
            };
            let (Some(proof_key), Some(route), Some(call_id)) = (
                launch.proof_key.clone(),
                launch.structured_route.clone(),
                launch.validation_call_id.clone(),
            ) else {
                return;
            };
            (
                proof_key,
                route,
                call_id,
                launch.bound_plan_step.clone(),
                running.artifact.clone(),
                running.started_at,
                exit_code,
                launch.turn_timing_state.clone(),
                launch.force_fresh,
            )
        };
        let (
            proof_key,
            route,
            call_id,
            bound_plan_step,
            artifact,
            started_at,
            exit_code,
            turn_timing_state,
            force_fresh,
        ) = candidate;
        self.publish_completed_validation_with_context(
            proof_key,
            route,
            call_id,
            bound_plan_step,
            artifact,
            started_at,
            exit_code,
            Some(process_id.to_string()),
            turn_timing_state,
            force_fresh,
        )
        .await;
    }

    pub(crate) async fn publish_inline_validation(
        &self,
        launch: &ValidationLaunchPlan,
        artifact: RawOutputArtifact,
        started_at: Instant,
        exit_code: i32,
    ) -> bool {
        let (Some(proof_key), Some(route), Some(call_id)) = (
            launch.proof_key.clone(),
            launch.structured_route.clone(),
            launch.validation_call_id.clone(),
        ) else {
            return false;
        };
        self.publish_completed_validation_with_context(
            proof_key,
            route,
            call_id,
            launch.bound_plan_step.clone(),
            artifact,
            started_at,
            exit_code,
            None,
            launch.turn_timing_state.clone(),
            launch.force_fresh,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn publish_completed_validation(
        &self,
        proof_key: ValidationProofKey,
        route: ValidationRoute,
        call_id: String,
        artifact: RawOutputArtifact,
        started_at: Instant,
        exit_code: i32,
        process_id: Option<String>,
    ) -> bool {
        self.publish_completed_validation_with_context(
            proof_key, route, call_id, None, artifact, started_at, exit_code, process_id, None,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_completed_validation_with_context(
        &self,
        proof_key: ValidationProofKey,
        route: ValidationRoute,
        call_id: String,
        bound_plan_step: Option<(String, u64)>,
        artifact: RawOutputArtifact,
        started_at: Instant,
        exit_code: i32,
        process_id: Option<String>,
        turn_timing_state: Option<std::sync::Arc<crate::turn_timing::TurnTimingState>>,
        force_fresh: bool,
    ) -> bool {
        let Some((artifact_ref, artifact_sha256, retained_output)) =
            artifact.validation_integrity_with_output().await
        else {
            return false;
        };
        let selected_test_count = validation_route_is_test(&route)
            .then(|| selected_test_count(&retained_output))
            .flatten();
        let missing_selected_tests =
            completed_validation_skip_disposition(&route, &retained_output, exit_code)
                == Some(codex_tools::ToolOutputSkipDisposition::NotApplicable);
        let succeeded = exit_code == 0 && !missing_selected_tests;
        let result = ValidationResult {
            proof_key: proof_key.clone(),
            route,
            call_id: call_id.clone(),
            process_id,
            status: if succeeded {
                ValidationTerminalStatus::Succeeded
            } else {
                ValidationTerminalStatus::Failed
            },
            duration_ms: u64::try_from(
                Instant::now()
                    .saturating_duration_since(started_at)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            summary: Some(if missing_selected_tests {
                "focused validation did not prove a nonzero selected-test count".to_string()
            } else if succeeded {
                selected_test_count.map_or_else(
                    || "focused validation succeeded".to_string(),
                    |count| format!("focused validation succeeded with {count} selected tests"),
                )
            } else {
                format!("focused validation exited with code {exit_code}")
            }),
            failure_excerpt: (!succeeded).then(|| {
                if missing_selected_tests {
                    "test validation exited successfully without a positive selected-test count; exact output is retained in the immutable artifact"
                        .to_string()
                } else {
                    format!(
                        "validation exited with code {exit_code}; exact output is retained in the immutable artifact"
                    )
                }
            }),
            raw_artifact_ref: Some(artifact_ref),
            raw_artifact_sha256: Some(artifact_sha256),
            freshness: ValidationFreshness::Executed,
        };
        let duration_ms = result.duration_ms;
        let artifact_thread_id = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.thread_id.clone())
            .unwrap_or_default();
        let persisted_success = succeeded.then(|| PersistedValidationProof {
            result: result.clone(),
            artifact_thread_id: artifact_thread_id.clone(),
        });
        let result = Arc::new(result);
        {
            let mut state = self.state.lock().await;
            if state.validation.results_by_call.contains_key(&call_id) {
                return true;
            }
            while state.validation.results_by_call.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
                let Some(oldest) = state.validation.result_call_order.pop_front() else {
                    break;
                };
                state.validation.results_by_call.remove(&oldest);
                state.validation.bound_plan_steps_by_call.remove(&oldest);
            }
            state
                .validation
                .result_call_order
                .push_back(call_id.clone());
            state
                .validation
                .results_by_call
                .insert(call_id.clone(), Arc::clone(&result));
            if let Some(bound_plan_step) = bound_plan_step {
                state
                    .validation
                    .bound_plan_steps_by_call
                    .insert(call_id.clone(), bound_plan_step);
            }
            if succeeded && !state.validation.completed.contains_key(&proof_key) {
                while state.validation.completed.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
                    let Some(oldest) = state.validation.completed_order.pop_front() else {
                        break;
                    };
                    state.validation.completed.remove(&oldest);
                }
                state
                    .validation
                    .completed_order
                    .push_back(proof_key.clone());
                state.validation.completed.insert(
                    proof_key.clone(),
                    CompletedValidationProof {
                        result: Arc::clone(&result),
                        artifact,
                    },
                );
            }
        }
        if let Some(persisted_success) = persisted_success {
            self.persist_shared_validation(persisted_success).await;
        }
        if let Some(turn_timing_state) = turn_timing_state {
            turn_timing_state.record_executed_validation(duration_ms, force_fresh);
        }
        tracing::info!(
            disposition = "executed",
            validation_call_id = %call_id,
            duration_ms,
            force_fresh,
            coverage_identity = %proof_key.coverage_identity,
            implementation_identity = %proof_key.implementation_identity,
            succeeded,
            "validation process completed"
        );
        true
    }

    #[cfg(test)]
    pub(crate) async fn finish_running_process(
        &self,
        process_id: u32,
        exit_code: Option<i32>,
    ) -> CompletionApplyResult {
        let identity = {
            let state = self.state.lock().await;
            state
                .process
                .running
                .get(&process_id)
                .map(|running| {
                    (
                        running.execution_id,
                        running.parent_tool_execution_id.clone(),
                    )
                })
                .or_else(|| {
                    state
                        .process
                        .pending_by_execution_id
                        .iter()
                        .find(|(_, pending)| pending.process_id == process_id)
                        .map(|(execution_id, pending)| {
                            (
                                *execution_id,
                                pending.command.parent_tool_execution_id.clone(),
                            )
                        })
                })
        };
        let Some((execution_id, parent_tool_execution_id)) = identity else {
            return CompletionApplyResult::Missing;
        };
        self.finish_running_process_with_execution_id(
            process_id,
            execution_id,
            &parent_tool_execution_id,
            exit_code,
        )
        .await
    }

    pub(crate) async fn finish_running_process_with_execution_id(
        &self,
        process_id: u32,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: &ToolExecutionId,
        exit_code: Option<i32>,
    ) -> CompletionApplyResult {
        let mut state = self.state.lock().await;
        debug_assert_command_execution_invariants(&state);
        if let Some(receipt) = state.process.completion_receipts.get(&execution_id) {
            return if &receipt.parent_tool_execution_id == parent_tool_execution_id {
                CompletionApplyResult::AlreadyApplied
            } else {
                CompletionApplyResult::Stale
            };
        }
        if state
            .process
            .running
            .get(&process_id)
            .is_some_and(|running| {
                running.execution_id != execution_id
                    || &running.parent_tool_execution_id != parent_tool_execution_id
            })
        {
            return CompletionApplyResult::Stale;
        }
        if let Some(mut running) = state.process.running.remove(&process_id) {
            if running.completed_exit_code.is_none()
                && let Some(exit_code) = exit_code
            {
                running.completed_exit_code = Some(exit_code);
                record_running_exit_locked(&mut state, &running, exit_code);
            }
            insert_completion_receipt(&mut state, execution_id, running.parent_tool_execution_id);
        } else if let Some(pending) = state.process.pending_by_execution_id.get(&execution_id) {
            if pending.process_id != process_id
                || &pending.command.parent_tool_execution_id != parent_tool_execution_id
            {
                return CompletionApplyResult::Stale;
            }
            let Some(pending) = state.process.pending_by_execution_id.remove(&execution_id) else {
                return CompletionApplyResult::Missing;
            };
            insert_completion_receipt(
                &mut state,
                execution_id,
                pending.command.parent_tool_execution_id,
            );
        } else {
            return CompletionApplyResult::Missing;
        }
        while state.retry.attempts.len() > MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(&mut state)
        {}
        debug_assert_command_execution_invariants(&state);
        CompletionApplyResult::Applied
    }

    #[cfg(test)]
    async fn snapshot(&self, key: &CommandAttemptKey) -> Option<AttemptEntry> {
        self.state.lock().await.retry.attempts.get(key).cloned()
    }

    #[cfg(test)]
    pub(crate) async fn consecutive_failures(&self, key: &CommandAttemptKey) -> u8 {
        self.snapshot(key)
            .await
            .map_or(0, |entry| entry.consecutive_failures)
    }
}

fn insert_completion_receipt(
    state: &mut CommandExecutionState,
    execution_id: CommandExecutionId,
    parent_tool_execution_id: ToolExecutionId,
) {
    if state
        .process
        .completion_receipts
        .contains_key(&execution_id)
    {
        return;
    }
    state.process.completion_receipts.insert(
        execution_id,
        CommandCompletionReceipt {
            parent_tool_execution_id,
        },
    );
    state
        .process
        .completion_receipt_order
        .push_back(execution_id);
    while state.process.completion_receipt_order.len() > MAX_TRACKED_COMMANDS {
        if let Some(oldest) = state.process.completion_receipt_order.pop_front() {
            state.process.completion_receipts.remove(&oldest);
        }
    }
}

fn debug_assert_command_execution_invariants(state: &CommandExecutionState) {
    debug_assert!(state.process.running.values().all(|running| {
        running.completed_exit_code.is_none()
            && !state
                .process
                .pending_by_execution_id
                .contains_key(&running.execution_id)
            && !state
                .process
                .completion_receipts
                .contains_key(&running.execution_id)
    }));
    debug_assert!(
        state
            .process
            .pending_by_execution_id
            .iter()
            .all(|(execution_id, pending)| {
                pending.command.execution_id == *execution_id
                    && pending.command.completed_exit_code.is_some()
                    && !state.process.completion_receipts.contains_key(execution_id)
                    && state
                        .process
                        .running
                        .values()
                        .all(|running| running.execution_id != *execution_id)
            })
    );
    debug_assert!(
        state
            .process
            .completion_receipt_order
            .iter()
            .all(|execution_id| {
                state.process.completion_receipts.contains_key(execution_id)
                    && !state
                        .process
                        .pending_by_execution_id
                        .contains_key(execution_id)
                    && state
                        .process
                        .running
                        .values()
                        .all(|running| running.execution_id != *execution_id)
            })
    );
}

fn record_running_exit_locked(
    state: &mut CommandExecutionState,
    running: &RunningCommand,
    exit_code: i32,
) {
    record_search_result_locked(state, &running.key, exit_code);
    record_exit_locked(state, &running.key, exit_code);
}

fn record_search_result_locked(
    state: &mut CommandExecutionState,
    key: &CommandAttemptKey,
    exit_code: i32,
) {
    let Some(search) = key.eligible_search_miss() else {
        return;
    };
    if exit_code == 1
        && let Some(parent_scope) = search.parent_scope()
    {
        state.search.allowed_expansions.insert(parent_scope);
    }
    let search_miss_key = key.search_miss_cache_key_for(search);
    if exit_code == 1 && state.search.misses.insert(search_miss_key.clone()) {
        state.search.miss_order.push_back(search_miss_key);
        while state.search.misses.len() > MAX_TRACKED_COMMANDS {
            if let Some(oldest) = state.search.miss_order.pop_front() {
                state.search.misses.remove(&oldest);
            }
        }
    } else if exit_code == 0 && state.search.misses.remove(&search_miss_key) {
        state
            .search
            .miss_order
            .retain(|cached| cached != &search_miss_key);
    }
}

fn record_exit_locked(state: &mut CommandExecutionState, key: &CommandAttemptKey, exit_code: i32) {
    let entry = attempt_entry_locked(state, key);
    entry.last_exit_code = Some(exit_code);
    if exit_code == 0 {
        entry.consecutive_failures = 0;
        entry.deterministic_failure = None;
    } else {
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    }
}

fn attempt_entry_locked<'a>(
    state: &'a mut CommandExecutionState,
    key: &CommandAttemptKey,
) -> &'a mut AttemptEntry {
    if !state.retry.attempts.contains_key(key) {
        while state.retry.attempts.len() >= MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(state)
        {}
        state.retry.insertion_order.push_back(key.clone());
    }
    state.retry.attempts.entry(key.clone()).or_default()
}

fn evict_oldest_inactive_attempt_locked(state: &mut CommandExecutionState) -> bool {
    if let Some(position) = state
        .retry
        .insertion_order
        .iter()
        .position(|key| !command_attempt_is_active(state, key))
        && let Some(oldest) = state.retry.insertion_order.remove(position)
    {
        state.retry.attempts.remove(&oldest);
        return true;
    }
    let Some(unordered_key) = state
        .retry
        .attempts
        .keys()
        .find(|key| !command_attempt_is_active(state, key))
        .cloned()
    else {
        return false;
    };
    state.retry.attempts.remove(&unordered_key);
    true
}

fn command_attempt_is_active(state: &CommandExecutionState, key: &CommandAttemptKey) -> bool {
    state
        .process
        .running
        .values()
        .any(|running| running.key == *key)
}

fn validation_route_is_test(route: &ValidationRoute) -> bool {
    route.leaves.as_slice().first().is_some_and(|leaf| {
        matches!(
            leaf.argv.as_slice(),
            [program, subcommand, ..] if program == "cargo" && subcommand == "test"
        ) || matches!(
            leaf.argv.as_slice(),
            [program, recipe, ..]
                if program == "just"
                    && matches!(
                        recipe.as_str(),
                        "test-fast" | "test-lane" | "test-lane-fast" | "test-lane-package"
                    )
        )
    })
}

pub(crate) fn completed_validation_skip_disposition(
    route: &ValidationRoute,
    output: &[u8],
    exit_code: i32,
) -> Option<codex_tools::ToolOutputSkipDisposition> {
    (exit_code == 0
        && validation_route_is_test(route)
        && selected_test_count(output).is_none_or(|count| count == 0))
    .then_some(codex_tools::ToolOutputSkipDisposition::NotApplicable)
}

fn shared_validation_cache_path(
    codex_home: &Path,
    workspace_identity: &crate::git_workspace::WorkspaceEvidenceIdentity,
    cwd: &Path,
) -> PathBuf {
    let repository = workspace_identity
        .repository_root
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
    let repository = repository.replace('\\', "/").to_ascii_lowercase();
    let identity = format!("{:x}", Sha256::digest(repository.as_bytes()));
    codex_home
        .join("command-execution-cache")
        .join(format!("validation-{identity}.json"))
}

fn restore_persisted_validations(
    state: &mut CommandExecutionState,
    codex_home: &Path,
    fallback_thread_id: &str,
    persisted_validations: Vec<PersistedValidationProof>,
) {
    let skip = persisted_validations
        .len()
        .saturating_sub(MAX_COMPLETED_VALIDATION_PROOFS);
    for persisted in persisted_validations.into_iter().skip(skip) {
        let result = Arc::new(persisted.result);
        if result.status != ValidationTerminalStatus::Succeeded
            || state.validation.completed.contains_key(&result.proof_key)
        {
            continue;
        }
        let Some(artifact_ref) = result.raw_artifact_ref.as_deref() else {
            continue;
        };
        let artifact_thread_id = if persisted.artifact_thread_id.is_empty() {
            fallback_thread_id.to_string()
        } else {
            persisted.artifact_thread_id
        };
        let Some(artifact) =
            RawOutputArtifact::restore_validation(codex_home, &artifact_thread_id, artifact_ref)
        else {
            continue;
        };
        while state.validation.completed.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
            let Some(oldest) = state.validation.completed_order.pop_front() else {
                break;
            };
            state.validation.completed.remove(&oldest);
        }
        let proof_key = result.proof_key.clone();
        state
            .validation
            .completed_order
            .push_back(proof_key.clone());
        state.validation.completed.insert(
            proof_key,
            CompletedValidationProof {
                result: Arc::clone(&result),
                artifact,
            },
        );
        if !state
            .validation
            .results_by_call
            .contains_key(&result.call_id)
        {
            while state.validation.results_by_call.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
                let Some(oldest) = state.validation.result_call_order.pop_front() else {
                    break;
                };
                state.validation.results_by_call.remove(&oldest);
                state.validation.bound_plan_steps_by_call.remove(&oldest);
            }
            state
                .validation
                .result_call_order
                .push_back(result.call_id.clone());
            state
                .validation
                .results_by_call
                .insert(result.call_id.clone(), Arc::clone(&result));
        }
    }
}

fn shared_validation_cache_write_gate() -> &'static Semaphore {
    static WRITE_GATE: std::sync::OnceLock<Semaphore> = std::sync::OnceLock::new();
    WRITE_GATE.get_or_init(|| Semaphore::new(1))
}

async fn write_cache_document(cache_path: PathBuf, bytes: Vec<u8>) {
    let _ = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let Some(parent) = cache_path.parent() else {
            return Err(std::io::Error::other("command cache path has no parent"));
        };
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&cache_path)
            .map_err(|error| error.error)?;
        Ok(())
    })
    .await;
}

fn supersede_validation_proofs_for_new_implementation(
    state: &mut CommandExecutionState,
    requested: &ValidationProofKey,
) {
    let superseded_keys = state
        .validation
        .completed
        .keys()
        .filter(|candidate| {
            candidate.repository == requested.repository
                && candidate.cwd == requested.cwd
                && candidate.canonical_route_hash == requested.canonical_route_hash
                && candidate.coverage_identity == requested.coverage_identity
                && candidate.environment_identity == requested.environment_identity
                && candidate.toolchain_identity == requested.toolchain_identity
                && candidate.configuration_identity == requested.configuration_identity
                && candidate.validation_contract_version == requested.validation_contract_version
                && candidate.implementation_identity != requested.implementation_identity
        })
        .cloned()
        .collect::<Vec<_>>();

    for superseded_key in superseded_keys {
        let Some(proof) = state.validation.completed.remove(&superseded_key) else {
            continue;
        };
        state
            .validation
            .completed_order
            .retain(|entry| entry != &superseded_key);
        let Some(result) = state
            .validation
            .results_by_call
            .get_mut(&proof.result.call_id)
        else {
            continue;
        };
        let result = Arc::make_mut(result);
        result.status = ValidationTerminalStatus::Superseded;
        result.freshness = ValidationFreshness::Superseded;
        result.summary = Some(format!(
            "focused validation was superseded by implementation identity {}",
            requested.implementation_identity
        ));
    }
}

fn selected_test_count(output: &[u8]) -> Option<u64> {
    let output = String::from_utf8_lossy(output);
    let cargo_count = output.lines().filter_map(|line| {
        line.trim()
            .strip_prefix("running ")
            .and_then(|summary| summary.split_whitespace().next())
            .and_then(|count| count.parse::<u64>().ok())
    });
    let nextest_count = output.lines().filter_map(|line| {
        let words = line.split_whitespace().collect::<Vec<_>>();
        words.windows(3).find_map(|window| {
            matches!(window[1], "test" | "tests")
                .then(|| window[0].parse::<u64>().ok())
                .flatten()
                .filter(|_| window[2].starts_with("run"))
        })
    });
    cargo_count.chain(nextest_count).max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn command_process_state_has_no_unused_order_mirror() {
        let source = include_str!("command_execution.rs");
        let obsolete_process_queue = ["running", "_order"].concat();

        assert!(
            !source.contains(&obsolete_process_queue),
            "unused running process ordering state must remain removed"
        );
    }

    fn key(command: &str) -> CommandAttemptKey {
        CommandAttemptKey::new("exec_command", "local", "C:/repo", &[command.to_string()])
    }

    fn validation_launch() -> ValidationLaunchPlan {
        ValidationLaunchPlan {
            invocation: crate::tools::handlers::command_shape::CommandInvocation::Argv {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
            },
            authorization_revision: 1,
            observation: None,
            proof_key: None,
            structured_route: None,
            bound_plan_step: None,
            validation_call_id: None,
            turn_timing_state: None,
            force_fresh: false,
        }
    }

    fn focused_cargo_route() -> ValidationRoute {
        ValidationRoute {
            leaves: vec![codex_protocol::plan_tool::ValidationRouteLeaf {
                argv: vec![
                    "cargo".to_string(),
                    "test".to_string(),
                    "-p".to_string(),
                    "codex-core".to_string(),
                    "focused_case".to_string(),
                ],
                uncertainty: "the focused case still passes".to_string(),
                covered_paths: vec!["core/src/tools/command_execution.rs".to_string()],
                covered_contracts: vec!["nonempty-cargo-proof".to_string()],
                timeout_ms: 30_000,
                semantic_timeout: false,
            }],
            ordering: Default::default(),
        }
    }

    fn validation_proof_key(identity: &str) -> ValidationProofKey {
        ValidationProofKey {
            repository: "C:/repo".to_string(),
            cwd: "C:/repo".to_string(),
            canonical_route_hash: format!("route-{identity}"),
            implementation_identity: identity.to_string(),
            coverage_identity: "focused-coverage".to_string(),
            environment_identity: "test-environment".to_string(),
            toolchain_identity: "test-toolchain".to_string(),
            configuration_identity: "test-configuration".to_string(),
            validation_contract_version: codex_protocol::validation::VALIDATION_CONTRACT_VERSION,
        }
    }

    fn initialize_git_repository(path: &Path) {
        std::fs::create_dir_all(path).expect("create repository");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .expect("launch git init");
        assert!(status.success(), "git init failed");
    }

    #[tokio::test]
    async fn workspace_identity_is_available_only_for_captured_local_cwd() {
        let temp = tempfile::tempdir().expect("workspace identity fixture");
        let repository = temp.path().join("repo");
        let alternate_repository = temp.path().join("alternate-repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        initialize_git_repository(&alternate_repository);
        let ledger = CommandExecutionLedger::load_or_new(
            codex_home,
            "workspace-scope".to_string(),
            &repository,
        )
        .await;

        assert!(
            ledger
                .current_workspace_identity_hash(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    &repository,
                )
                .await
                .is_some()
        );
        assert!(
            ledger
                .current_workspace_identity_hash(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    &alternate_repository,
                )
                .await
                .is_none()
        );
        assert!(
            ledger
                .current_workspace_identity_hash("remote-environment", &repository)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn supplied_workspace_identity_must_match_persisted_repository() {
        let temp = tempfile::tempdir().expect("workspace identity fixture");
        let repository = temp.path().join("repo");
        let other_repository = temp.path().join("other-repo");
        initialize_git_repository(&repository);
        initialize_git_repository(&other_repository);
        let ledger = CommandExecutionLedger::load_or_new(
            temp.path().join("codex-home"),
            "workspace-scope".to_string(),
            &repository,
        )
        .await;
        tokio::fs::write(repository.join("changed.txt"), b"changed")
            .await
            .expect("mutate persisted repository");
        let expected_identity =
            crate::git_workspace::capture_workspace_evidence_identity(&repository)
                .await
                .expect("expected repository identity");
        let other_identity =
            crate::git_workspace::capture_workspace_evidence_identity(&other_repository)
                .await
                .expect("other repository identity");

        ledger
            .observe_repository_revision_with_identity("turn-a", 1, Some(other_identity.clone()))
            .await;

        let expected_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&expected_identity).expect("serialize identity"))
        );
        let other_hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&other_identity).expect("serialize identity"))
        );
        let observed_hash = ledger
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repository)
            .await
            .expect("observed repository identity");
        assert_eq!(observed_hash, expected_hash);
        assert_ne!(observed_hash, other_hash);
    }

    #[tokio::test]
    async fn scoped_validation_reuse_survives_unrelated_workspace_change() {
        let temp = tempfile::tempdir().expect("cache fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let thread_id = "persisted-command-state";
        let ledger = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            thread_id.to_string(),
            &repository,
        )
        .await;
        assert_eq!(ledger.observe_repository_revision("turn-a", 1).await, 1);
        let search = RgSearchNarrowing {
            breadth: RgSearchBreadth::Narrow,
            query_identity: "missing".to_string(),
            search_identity: "missing:src".to_string(),
            scope_identity: "repo/src".to_string(),
            parent_scope_identity: Some("repo".to_string()),
            can_record_miss: true,
        };
        let missed = key("rg missing src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-a", "repo", Some(search.clone()));
        ledger.record_exit(&missed, 1).await;

        let proof_key = validation_proof_key("persisted");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            thread_id,
            b"running 1 test\ntest persisted_case ... ok\n",
        )
        .await;
        assert!(
            ledger
                .publish_completed_validation(
                    proof_key.clone(),
                    focused_cargo_route(),
                    "persisted-call".to_string(),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );
        ledger.finish_turn("turn-a").await;

        let reopened = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            thread_id.to_string(),
            &repository,
        )
        .await;
        let equivalent = key("rg missing ./src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-b", "repo", Some(search));
        assert!(
            reopened
                .begin_attempt_with_freshness(&equivalent, false, false)
                .await
                .is_err()
        );
        let reused = reopened
            .reusable_validation(&proof_key)
            .await
            .expect("persisted validation proof");
        assert_eq!(reused.freshness, ValidationFreshness::Reused);

        std::fs::write(repository.join("external-change.txt"), "changed")
            .expect("mutate repository");
        let changed_workspace =
            CommandExecutionLedger::load_or_new(codex_home, thread_id.to_string(), &repository)
                .await;
        assert!(
            changed_workspace
                .reusable_validation(&proof_key)
                .await
                .is_some(),
            "an unrelated workspace mutation must not invalidate a scoped proof"
        );
        assert!(
            changed_workspace
                .begin_attempt_with_freshness(&equivalent, false, false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn scoped_validation_reuse_is_immediately_available_to_later_thread() {
        let temp = tempfile::tempdir().expect("cache fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let producer = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "producer-thread".to_string(),
            &repository,
        )
        .await;
        let consumer = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "consumer-thread".to_string(),
            &repository,
        )
        .await;
        let proof_key = validation_proof_key("cross-thread");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            "producer-thread",
            b"running 1 test\ntest cross_thread_case ... ok\n",
        )
        .await;
        assert!(
            producer
                .publish_completed_validation(
                    proof_key.clone(),
                    focused_cargo_route(),
                    "producer-call".to_string(),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );

        std::fs::write(repository.join("unrelated.txt"), "changed")
            .expect("mutate an unrelated path");
        let reused = consumer
            .reusable_validation(&proof_key)
            .await
            .expect("later thread reuses the producer's completed validation");
        assert_eq!(reused.call_id, "producer-call");
        assert_eq!(reused.freshness, ValidationFreshness::Reused);
    }

    #[tokio::test]
    async fn scoped_validation_reuse_never_persists_failures() {
        let temp = tempfile::tempdir().expect("cache fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let producer = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "failed-producer".to_string(),
            &repository,
        )
        .await;
        let proof_key = validation_proof_key("failed-cross-thread");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            "failed-producer",
            b"running 1 test\ntest focused_case ... FAILED\n",
        )
        .await;
        assert!(
            producer
                .publish_completed_validation(
                    proof_key.clone(),
                    focused_cargo_route(),
                    "failed-call".to_string(),
                    artifact,
                    Instant::now(),
                    1,
                    None,
                )
                .await
        );

        let consumer = CommandExecutionLedger::load_or_new(
            codex_home,
            "failed-consumer".to_string(),
            &repository,
        )
        .await;
        assert!(consumer.reusable_validation(&proof_key).await.is_none());
    }

    #[tokio::test]
    async fn scoped_validation_reuse_rejects_cross_thread_artifact_tampering() {
        let temp = tempfile::tempdir().expect("cache fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let producer_thread = "tamper-producer";
        let producer = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            producer_thread.to_string(),
            &repository,
        )
        .await;
        let proof_key = validation_proof_key("tampered-cross-thread");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            producer_thread,
            b"running 1 test\ntest focused_case ... ok\n",
        )
        .await;
        assert!(
            producer
                .publish_completed_validation(
                    proof_key.clone(),
                    focused_cargo_route(),
                    "tamper-call".to_string(),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );
        let artifact_ref = producer
            .validation_result_for_call("tamper-call")
            .await
            .and_then(|result| result.raw_artifact_ref)
            .expect("retained artifact reference");
        let shared_validation_cache_path = producer
            .persistence
            .as_ref()
            .expect("persistent producer")
            .shared_validation_cache_path
            .clone();
        drop(producer);
        std::fs::write(
            codex_home
                .join("tool-output")
                .join(producer_thread)
                .join(format!("{artifact_ref}.log")),
            "tampered output",
        )
        .expect("tamper retained artifact");

        let consumer = CommandExecutionLedger::load_or_new(
            codex_home,
            "tamper-consumer".to_string(),
            &repository,
        )
        .await;
        assert!(consumer.reusable_validation(&proof_key).await.is_none());
        let persisted: PersistedValidationCacheDocument = serde_json::from_slice(
            &std::fs::read(shared_validation_cache_path).expect("read shared validation cache"),
        )
        .expect("parse shared validation cache");
        assert!(persisted.completed_validations.is_empty());
    }

    #[tokio::test]
    async fn superseded_validation_freshness_marks_outdated_implementation_proof() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let old_key = validation_proof_key("old-implementation");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "superseded-validation",
            b"running 1 test\ntest focused_case ... ok\n",
        )
        .await;
        assert!(
            ledger
                .publish_completed_validation(
                    old_key.clone(),
                    focused_cargo_route(),
                    "superseded-call".to_string(),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );

        let mut current_key = old_key.clone();
        current_key.implementation_identity = "current-implementation".to_string();
        assert!(ledger.reusable_validation(&current_key).await.is_none());

        let superseded = ledger
            .validation_result_for_call("superseded-call")
            .await
            .expect("superseded result remains addressable by its production call id");
        assert_eq!(superseded.status, ValidationTerminalStatus::Superseded);
        assert_eq!(superseded.freshness, ValidationFreshness::Superseded);
        assert!(
            superseded
                .summary
                .as_deref()
                .is_some_and(|summary| summary.contains("current-implementation"))
        );
        assert!(ledger.reusable_validation(&old_key).await.is_none());
    }

    #[tokio::test]
    async fn scoped_validation_reuse_persists_before_finish_turn() {
        let temp = tempfile::tempdir().expect("cache fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let thread_id = "mutation-before-persist";
        let ledger = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            thread_id.to_string(),
            &repository,
        )
        .await;
        assert_eq!(ledger.observe_repository_revision("turn-a", 1).await, 1);
        let search = RgSearchNarrowing {
            breadth: RgSearchBreadth::Narrow,
            query_identity: "missing".to_string(),
            search_identity: "missing:src".to_string(),
            scope_identity: "repo/src".to_string(),
            parent_scope_identity: Some("repo".to_string()),
            can_record_miss: true,
        };
        let missed = key("rg missing src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-a", "repo", Some(search.clone()));
        ledger.record_exit(&missed, 1).await;

        let proof_key = validation_proof_key("mutation-before-persist");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            &codex_home,
            thread_id,
            b"running 1 test\ntest persisted_case ... ok\n",
        )
        .await;
        assert!(
            ledger
                .publish_completed_validation(
                    proof_key.clone(),
                    focused_cargo_route(),
                    "persisted-call".to_string(),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );
        let per_thread_cache_path = ledger
            .persistence
            .as_ref()
            .expect("persistent ledger")
            .cache_path
            .clone();

        std::fs::write(repository.join("external-change.txt"), "changed")
            .expect("mutate repository before persistence");
        ledger.finish_turn("turn-a").await;
        if let Ok(bytes) = std::fs::read(per_thread_cache_path) {
            let per_thread_cache: serde_json::Value =
                serde_json::from_slice(&bytes).expect("parse per-thread command cache");
            assert!(per_thread_cache.get("completed_validations").is_none());
        }

        let reopened =
            CommandExecutionLedger::load_or_new(codex_home, thread_id.to_string(), &repository)
                .await;
        let equivalent = key("rg missing ./src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-b", "repo", Some(search));
        assert!(
            reopened
                .begin_attempt_with_freshness(&equivalent, false, false)
                .await
                .is_ok()
        );
        assert!(reopened.reusable_validation(&proof_key).await.is_some());
    }

    #[tokio::test]
    async fn zero_test_cargo_success_is_failed_and_not_reusable() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let route = focused_cargo_route();
        let zero_key = validation_proof_key("zero");
        let zero_artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "zero-test",
            b"running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n",
        )
        .await;
        assert_eq!(
            completed_validation_skip_disposition(
                &route,
                b"running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n",
                0,
            ),
            Some(codex_tools::ToolOutputSkipDisposition::NotApplicable)
        );
        assert!(
            ledger
                .publish_completed_validation(
                    zero_key.clone(),
                    route.clone(),
                    "zero-call".to_string(),
                    zero_artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );
        let zero_result = ledger
            .validation_result_for_call("zero-call")
            .await
            .expect("zero-test result");
        assert_eq!(zero_result.status, ValidationTerminalStatus::Failed);
        assert!(ledger.reusable_validation(&zero_key).await.is_none());

        let selected_key = validation_proof_key("selected");
        let selected_artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "selected-test",
            b"running 1 test\ntest focused_case ... ok\n",
        )
        .await;
        assert!(
            ledger
                .publish_completed_validation(
                    selected_key.clone(),
                    route,
                    "selected-call".to_string(),
                    selected_artifact,
                    Instant::now(),
                    0,
                    None,
                )
                .await
        );
        let selected_result = ledger
            .validation_result_for_call("selected-call")
            .await
            .expect("selected-test result");
        assert_eq!(selected_result.status, ValidationTerminalStatus::Succeeded);
        assert!(ledger.reusable_validation(&selected_key).await.is_some());
    }

    #[tokio::test]
    async fn validation_execution_telemetry_records_cost_and_force_fresh() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let timing = std::sync::Arc::new(crate::turn_timing::TurnTimingState::default());
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "telemetry-test",
            b"running 1 test\ntest focused_case ... ok\n",
        )
        .await;
        assert!(
            ledger
                .publish_completed_validation_with_context(
                    validation_proof_key("telemetry"),
                    focused_cargo_route(),
                    "telemetry-call".to_string(),
                    None,
                    artifact,
                    Instant::now() - Duration::from_millis(25),
                    0,
                    None,
                    Some(std::sync::Arc::clone(&timing)),
                    true,
                )
                .await
        );

        let counters = timing.complete_snapshot().protocol_timing().counters;
        assert_eq!(counters.executed_validation_count, 1);
        assert_eq!(counters.forced_fresh_validation_count, 1);
        assert!(counters.executed_validation_duration_ns >= 25_000_000);
        assert_eq!(counters.reused_validation_count, 0);
        assert_eq!(counters.duplicate_validation_count, 0);
    }

    #[tokio::test]
    async fn completed_validation_preserves_launch_bound_plan_step() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "bound-plan-step-test",
            b"running 1 test\ntest focused_case ... ok\n",
        )
        .await;

        assert!(
            ledger
                .publish_completed_validation_with_context(
                    validation_proof_key("bound-plan-step"),
                    focused_cargo_route(),
                    "bound-plan-step-call".to_string(),
                    Some(("implementation-step".to_string(), 7)),
                    artifact,
                    Instant::now(),
                    0,
                    None,
                    None,
                    false,
                )
                .await
        );

        let (result, bound_plan_step) = ledger
            .validation_result_with_plan_step_for_call("bound-plan-step-call")
            .await
            .expect("completed validation result");
        assert_eq!(result.status, ValidationTerminalStatus::Succeeded);
        assert_eq!(
            bound_plan_step,
            Some(("implementation-step".to_string(), 7))
        );
    }

    #[tokio::test]
    async fn broad_rg_requires_a_same_turn_narrow_search_miss() {
        let ledger = CommandExecutionLedger::default();
        let narrow = key("rg needle src").with_search_narrowing(
            "turn-a",
            "repo-a",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Narrow,
                query_identity: "needle".to_string(),
                search_identity: "needle:src".to_string(),
                scope_identity: "src".to_string(),
                parent_scope_identity: Some("repo".to_string()),
                can_record_miss: true,
            }),
        );
        let broad = key("rg needle .").with_search_narrowing(
            "turn-a",
            "repo-a",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Broad,
                query_identity: "needle".to_string(),
                search_identity: "needle:repo".to_string(),
                scope_identity: "repo".to_string(),
                parent_scope_identity: None,
                can_record_miss: true,
            }),
        );

        let blocked = ledger
            .admit_search_narrowing(&broad)
            .await
            .expect_err("broad search must start blocked");
        assert!(blocked.contains("first search a narrower scope"));
        ledger
            .admit_search_narrowing(&narrow)
            .await
            .expect("narrow search is admitted without prior state");
        ledger.record_exit(&narrow, 1).await;
        ledger
            .admit_search_narrowing(&broad)
            .await
            .expect("a narrow miss authorizes its immediate parent scope");

        ledger.finish_turn("turn-a").await;
        ledger
            .admit_search_narrowing(&broad)
            .await
            .expect_err("expansion authorization must expire with the turn");
    }

    #[tokio::test]
    async fn broad_rg_requires_a_miss_for_the_same_query() {
        let ledger = CommandExecutionLedger::default();
        let narrow = key("rg other src").with_search_narrowing(
            "turn-a",
            "repo-a",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Narrow,
                query_identity: "other".to_string(),
                search_identity: "other:src".to_string(),
                scope_identity: "src".to_string(),
                parent_scope_identity: Some("repo".to_string()),
                can_record_miss: true,
            }),
        );
        let broad = key("rg needle .").with_search_narrowing(
            "turn-a",
            "repo-a",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Broad,
                query_identity: "needle".to_string(),
                search_identity: "needle:repo".to_string(),
                scope_identity: "repo".to_string(),
                parent_scope_identity: None,
                can_record_miss: true,
            }),
        );

        ledger.record_exit(&narrow, 1).await;

        ledger
            .admit_search_narrowing(&broad)
            .await
            .expect_err("a miss for a different query must not authorize broad search");
    }

    #[tokio::test]
    async fn unattributable_search_does_not_record_miss_state() {
        let ledger = CommandExecutionLedger::default();
        let compound = key("rg needle src; echo finished").with_search_narrowing(
            "turn-a",
            "repo-a",
            Some(RgSearchNarrowing {
                breadth: RgSearchBreadth::Narrow,
                query_identity: "needle".to_string(),
                search_identity: "needle:src".to_string(),
                scope_identity: "src".to_string(),
                parent_scope_identity: Some("repo".to_string()),
                can_record_miss: false,
            }),
        );

        ledger.record_exit(&compound, 1).await;

        let state = ledger.state.lock().await;
        assert!(state.search.allowed_expansions.is_empty());
        assert!(state.search.misses.is_empty());
        assert!(state.search.miss_order.is_empty());
    }

    #[tokio::test]
    async fn equivalent_search_miss_is_cached_for_unchanged_workspace_identity() {
        let ledger = CommandExecutionLedger::default();
        let search = |search_identity: &str| RgSearchNarrowing {
            breadth: RgSearchBreadth::Narrow,
            query_identity: "needle".to_string(),
            search_identity: search_identity.to_string(),
            scope_identity: search_identity.to_string(),
            parent_scope_identity: Some("repo".to_string()),
            can_record_miss: true,
        };
        let first = key("rg needle src")
            .with_repository_epoch(1)
            .with_workspace_identity(Some("workspace-a"))
            .with_search_narrowing("turn-a", "repo-a", Some(search("needle:src")));
        let equivalent = key("rg needle ./src")
            .with_repository_epoch(2)
            .with_workspace_identity(Some("workspace-a"))
            .with_search_narrowing("turn-b", "repo-a", Some(search("needle:src")));
        let compound = key("rg needle ./src; echo finished")
            .with_repository_epoch(1)
            .with_search_narrowing(
                "turn-b",
                "repo-a",
                Some(RgSearchNarrowing {
                    can_record_miss: false,
                    ..search("needle:src")
                }),
            );

        ledger
            .begin_attempt(&first, false)
            .await
            .expect("first search");
        ledger.record_exit(&first, 1).await;
        let blocked = ledger
            .begin_attempt(&equivalent, false)
            .await
            .expect_err("equivalent miss should be reused");
        assert!(blocked.render_for_model().contains("equivalent search"));
        ledger
            .begin_attempt(&compound, false)
            .await
            .expect("an unattributable compound command must not be blocked by an rg miss");

        ledger
            .begin_attempt_with_freshness(&equivalent, false, true)
            .await
            .expect("force_fresh bypasses the negative cache");
        ledger.record_exit(&equivalent, 0).await;
        ledger
            .begin_attempt(&equivalent, false)
            .await
            .expect("a fresh successful search clears the cached miss");

        ledger
            .begin_attempt(
                &key("rg needle tests")
                    .with_repository_epoch(1)
                    .with_search_narrowing("turn-b", "repo-a", Some(search("needle:tests"))),
                false,
            )
            .await
            .expect("changed scope executes");
        ledger
            .begin_attempt(
                &key("rg needle src")
                    .with_repository_epoch(3)
                    .with_workspace_identity(Some("workspace-b"))
                    .with_search_narrowing("turn-b", "repo-a", Some(search("needle:src"))),
                false,
            )
            .await
            .expect("changed exact workspace identity executes");
    }

    #[tokio::test]
    async fn background_search_miss_populates_the_same_negative_cache() {
        let ledger = CommandExecutionLedger::default();
        let search = RgSearchNarrowing {
            breadth: RgSearchBreadth::Narrow,
            query_identity: "needle".to_string(),
            search_identity: "needle:src".to_string(),
            scope_identity: "repo/src".to_string(),
            parent_scope_identity: Some("repo".to_string()),
            can_record_miss: true,
        };
        let first = key("rg needle src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-a", "repo-a", Some(search.clone()));
        let equivalent = key("rg needle ./src")
            .with_repository_epoch(1)
            .with_search_narrowing("turn-b", "repo-a", Some(search));

        ledger
            .begin_attempt(&first, false)
            .await
            .expect("first search");
        ledger
            .track_running_process(
                42,
                first,
                RawOutputArtifact::unavailable("background search fixture"),
            )
            .await;
        assert!(
            ledger
                .mark_running_process_completed(42, 1)
                .await
                .accepted()
        );
        ledger
            .begin_attempt(&equivalent, false)
            .await
            .expect_err("background miss should be reused");
    }

    #[tokio::test]
    async fn stale_execution_completion_cannot_clear_reused_process_id() {
        let ledger = CommandExecutionLedger::default();
        let first_id = ledger.allocate_execution_id();
        let first_parent = ToolExecutionId("tool-execution-first".to_string());
        ledger
            .track_running_process_with_execution_id(
                first_id,
                first_parent.clone(),
                42,
                key("first"),
                RawOutputArtifact::unavailable("first"),
                None,
                Instant::now(),
            )
            .await;
        assert_eq!(
            ledger
                .mark_process_exited(42, first_id, &first_parent, 0)
                .await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger
                .retire_completed_process(first_id, &first_parent)
                .await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger
                .retire_completed_process(first_id, &ToolExecutionId("wrong-parent".to_string()),)
                .await,
            CompletionApplyResult::Stale
        );

        let second_id = ledger.allocate_execution_id();
        let second_parent = ToolExecutionId("tool-execution-second".to_string());
        ledger
            .track_running_process_with_execution_id(
                second_id,
                second_parent.clone(),
                42,
                key("second"),
                RawOutputArtifact::unavailable("second"),
                None,
                Instant::now(),
            )
            .await;
        assert_eq!(
            ledger
                .mark_process_exited(42, first_id, &first_parent, 0)
                .await,
            CompletionApplyResult::Stale
        );
        assert_eq!(
            ledger
                .mark_process_exited(42, second_id, &first_parent, 0)
                .await,
            CompletionApplyResult::Stale
        );
        assert_eq!(
            ledger
                .running_process(42)
                .await
                .map(|running| running.execution_id),
            Some(second_id)
        );
        assert_eq!(
            ledger
                .mark_process_exited(42, second_id, &second_parent, 0)
                .await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger
                .mark_process_exited(42, second_id, &second_parent, 0)
                .await,
            CompletionApplyResult::AlreadyApplied
        );
    }

    #[tokio::test]
    async fn exit_before_watcher_registration_is_observed_once() {
        let ledger = CommandExecutionLedger::default();
        let execution_id = ledger.allocate_execution_id();
        let parent = ToolExecutionId("tool-execution-sticky".to_string());
        ledger
            .track_running_process_with_execution_id(
                execution_id,
                parent.clone(),
                73,
                key("sticky-exit"),
                RawOutputArtifact::unavailable("sticky exit"),
                None,
                Instant::now(),
            )
            .await;
        let exit = CancellationToken::new();
        exit.cancel();

        // CancellationToken is sticky: registering after exit must still wake.
        exit.cancelled().await;
        assert_eq!(
            ledger
                .mark_process_exited(73, execution_id, &parent, 0)
                .await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger
                .mark_process_exited(73, execution_id, &parent, 0)
                .await,
            CompletionApplyResult::AlreadyApplied
        );
        assert!(ledger.running_process(73).await.is_none());
    }

    #[tokio::test]
    async fn input_state_determined_failure_blocks_exact_retry_but_freshness_bypasses() {
        let ledger = CommandExecutionLedger::default();
        let attempt_key = key("fails.exe").with_repository_epoch(1);

        ledger
            .begin_attempt(&attempt_key, false)
            .await
            .expect("first attempt");
        ledger
            .record_input_state_determined_failure(
                &attempt_key,
                InputStateDetermined::ApplyPatchImplicitInvocation,
                RawOutputArtifact::unavailable(
                    InputStateDetermined::ApplyPatchImplicitInvocation.evidence_description(),
                ),
                -1,
            )
            .await;

        ledger
            .begin_attempt(&attempt_key, false)
            .await
            .expect_err("the closed production proof blocks an exact retry");
        ledger
            .begin_attempt(&attempt_key, true)
            .await
            .expect("a repaired command bypasses the retained proof");
        ledger
            .begin_attempt_with_freshness(&attempt_key, false, true)
            .await
            .expect("force_fresh bypasses the retained proof");
        ledger
            .begin_attempt(&key("fails.exe --changed").with_repository_epoch(1), false)
            .await
            .expect("meaningful argument change executes");
        ledger
            .begin_attempt(&key("fails.exe").with_repository_epoch(2), false)
            .await
            .expect("repository revision change executes");
    }

    #[tokio::test]
    async fn unclassified_and_retryable_failures_are_never_suppressed() {
        let classes = [
            "unclassified-nonzero",
            "timeout",
            "lock",
            "network",
            "cancellation",
            "resource-exhaustion",
            "uncertain-crash",
            "flaky",
            "unknown",
        ];
        for class in classes {
            let ledger = CommandExecutionLedger::default();
            let key = key(class);
            ledger.begin_attempt(&key, false).await.expect("first run");
            ledger.record_exit(&key, 1).await;
            ledger.begin_attempt(&key, false).await.expect("retry runs");
            ledger.record_exit(&key, 1).await;
            ledger
                .begin_attempt(&key, false)
                .await
                .expect("additional retry still runs");
        }
    }

    #[tokio::test]
    async fn success_resets_consecutive_failure_guard_and_repairs_are_counted() {
        let ledger = CommandExecutionLedger::default();
        let key = key("rg.exe");

        ledger
            .begin_attempt(&key, true)
            .await
            .expect("repaired attempt");
        ledger.record_exit(&key, 2).await;
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("second attempt");
        ledger.record_exit(&key, 0).await;
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("success should reset guard");

        let snapshot = ledger.snapshot(&key).await.expect("tracked entry");
        assert_eq!(snapshot.attempts, 3);
        assert_eq!(snapshot.repairs, 1);
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.last_exit_code, Some(0));
    }

    #[tokio::test]
    async fn background_completion_and_poll_finalize_one_failure_only() {
        let ledger = CommandExecutionLedger::default();
        let key = key("background-failure.exe");
        ledger.begin_attempt(&key, false).await.expect("attempt");
        ledger
            .track_running_process(
                42,
                key.clone(),
                RawOutputArtifact::Failed {
                    id: None,
                    message: "fixture".to_string(),
                    owned_path: None,
                    bytes: 0,
                },
            )
            .await;

        assert!(
            ledger
                .mark_running_process_completed(42, 7)
                .await
                .accepted()
        );
        assert!(
            ledger
                .mark_running_process_completed(42, 7)
                .await
                .accepted()
        );
        assert!(ledger.finish_running_process(42, Some(7)).await.accepted());

        let snapshot = ledger.snapshot(&key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("one failure must not block the next attempt");
    }

    #[tokio::test]
    async fn kd4_latency_watcher_completion_retires_only_completed_metadata() {
        let ledger = CommandExecutionLedger::default();
        let completed_key = key("completed-background.exe");
        let live_key = key("live-background.exe");
        for (process_id, command_key) in [(41, &completed_key), (42, &live_key)] {
            ledger
                .begin_attempt(command_key, false)
                .await
                .expect("attempt");
            ledger
                .track_running_process(
                    process_id,
                    command_key.clone(),
                    RawOutputArtifact::unavailable("fixture"),
                )
                .await;
        }

        let completed = ledger
            .running_process(41)
            .await
            .expect("completed identity");
        assert!(
            ledger
                .mark_running_process_completed(41, 7)
                .await
                .accepted()
        );
        assert_eq!(
            ledger
                .retire_completed_process(
                    completed.execution_id,
                    &completed.parent_tool_execution_id,
                )
                .await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger
                .retire_completed_process(
                    completed.execution_id,
                    &completed.parent_tool_execution_id,
                )
                .await,
            CompletionApplyResult::AlreadyApplied
        );
        assert!(ledger.running_process(41).await.is_none());
        assert!(ledger.running_process(42).await.is_some());
        assert_eq!(ledger.consecutive_failures(&completed_key).await, 1);
        assert_eq!(ledger.consecutive_failures(&live_key).await, 0);
    }

    #[tokio::test]
    async fn tracked_validation_watcher_completion_records_once_and_refreshes_evidence() {
        let ledger = CommandExecutionLedger::default();
        let command_key = key("cargo test --test focused");
        let finalized_artifact = RawOutputArtifact::unavailable("finalized watcher artifact");
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("attempt");
        ledger
            .track_running_process_with_validation_contract(
                42,
                command_key.clone(),
                RawOutputArtifact::unavailable("initial watcher artifact"),
                Some(validation_launch()),
                Instant::now() - Duration::from_millis(25),
            )
            .await;

        assert!(
            ledger
                .mark_running_process_completed(42, 7)
                .await
                .accepted()
        );
        assert!(
            ledger
                .mark_running_process_completed(42, 7)
                .await
                .accepted()
        );
        ledger
            .update_running_artifact(42, finalized_artifact.clone())
            .await;
        assert!(ledger.finish_running_process(42, Some(7)).await.accepted());

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        assert_eq!(snapshot.deterministic_failure, None);
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("a nonzero validation without typed proof remains retryable");
    }

    #[tokio::test]
    async fn tracked_validation_handler_completion_records_once() {
        let ledger = CommandExecutionLedger::default();
        let command_key = key("cargo test --test direct");
        let finalized_artifact = RawOutputArtifact::unavailable("finalized handler artifact");
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("attempt");
        ledger
            .track_running_process_with_validation_contract(
                43,
                command_key.clone(),
                RawOutputArtifact::unavailable("initial handler artifact"),
                Some(validation_launch()),
                Instant::now() - Duration::from_millis(25),
            )
            .await;
        ledger
            .update_running_artifact(43, finalized_artifact.clone())
            .await;

        assert!(ledger.finish_running_process(43, Some(9)).await.accepted());
        assert!(!ledger.finish_running_process(43, Some(9)).await.accepted());

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        assert_eq!(snapshot.deterministic_failure, None);
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("handler-completed validation without typed proof remains retryable");
    }

    #[tokio::test]
    async fn tracked_validation_success_clears_prior_deterministic_failure() {
        let ledger = CommandExecutionLedger::default();
        let command_key = key("cargo test --test recovered");
        ledger
            .record_input_state_determined_failure(
                &command_key,
                InputStateDetermined::ApplyPatchEnvironmentIdMismatch,
                RawOutputArtifact::unavailable(
                    InputStateDetermined::ApplyPatchEnvironmentIdMismatch.evidence_description(),
                ),
                -1,
            )
            .await;
        ledger
            .track_running_process_with_validation_contract(
                44,
                command_key.clone(),
                RawOutputArtifact::unavailable("successful validation artifact"),
                Some(validation_launch()),
                Instant::now(),
            )
            .await;

        assert!(
            ledger
                .mark_running_process_completed(44, 0)
                .await
                .accepted()
        );
        assert!(ledger.finish_running_process(44, Some(0)).await.accepted());

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.last_exit_code, Some(0));
        assert_eq!(snapshot.deterministic_failure, None);
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("successful tracked validation permits another attempt");
    }

    #[test]
    fn retry_identity_tracks_executed_command_and_execution_context() {
        let original = vec!["rg".to_string(), "--ignorecase".to_string()];
        let repaired = vec!["rg".to_string(), "--ignore-case".to_string()];
        let mut environment = HashMap::from([
            ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ("RUST_BACKTRACE".to_string(), "1".to_string()),
        ]);
        let base = CommandAttemptKey::new("shell_command", "local", "C:/repo", &original)
            .with_executed_command(&repaired)
            .with_environment(&environment)
            .with_timeout_ms(Some(1_000))
            .with_sandbox_context(&"workspace-write")
            .with_runtime_context(&"classic")
            .with_repository_epoch(1);

        let mut changed_execution = base.clone();
        changed_execution.command.push("src".to_string());
        assert_ne!(base.fingerprint(), changed_execution.fingerprint());

        let direct_repaired =
            CommandAttemptKey::new("shell_command", "local", "C:/repo", &repaired)
                .with_environment(&environment)
                .with_timeout_ms(Some(1_000))
                .with_sandbox_context(&"workspace-write")
                .with_runtime_context(&"classic")
                .with_repository_epoch(1);
        assert_eq!(base.fingerprint(), direct_repaired.fingerprint());

        environment.insert("RUST_BACKTRACE".to_string(), "full".to_string());
        let changed_environment =
            CommandAttemptKey::new("shell_command", "local", "C:/repo", &original)
                .with_executed_command(&repaired)
                .with_environment(&environment)
                .with_timeout_ms(Some(1_000))
                .with_sandbox_context(&"workspace-write")
                .with_runtime_context(&"classic")
                .with_repository_epoch(1);
        assert_ne!(base.fingerprint(), changed_environment.fingerprint());

        assert_ne!(
            base.fingerprint(),
            base.with_repository_epoch(2).fingerprint()
        );
    }

    #[tokio::test]
    async fn repository_epoch_is_session_scoped_across_turns() {
        let ledger = CommandExecutionLedger::default();

        assert_eq!(ledger.observe_repository_revision("turn-1", 0).await, 0);
        assert_eq!(ledger.observe_repository_revision("turn-1", 1).await, 1);
        assert_eq!(ledger.observe_repository_revision("turn-2", 0).await, 1);
        assert_eq!(ledger.observe_repository_revision("turn-2", 2).await, 3);
        assert_eq!(ledger.observe_repository_revision("turn-1", 1).await, 3);
    }

    #[tokio::test]
    async fn duplicate_repository_reads_revision_reuses_supplied_workspace_identity() {
        let temp = tempfile::tempdir().expect("workspace identity fixture");
        let repository = temp.path().join("repo");
        initialize_git_repository(&repository);
        let ledger = CommandExecutionLedger::load_or_new(
            temp.path().join("codex-home"),
            "candidate-snapshot".to_string(),
            &repository,
        )
        .await;
        tokio::fs::write(repository.join("first.txt"), b"first")
            .await
            .expect("first mutation");
        let identity = crate::git_workspace::capture_workspace_evidence_identity(&repository)
            .await
            .expect("candidate identity");
        tokio::fs::write(repository.join("second.txt"), b"second")
            .await
            .expect("mutation after candidate capture");

        assert_eq!(
            ledger
                .observe_repository_revision_with_identity("turn", 1, Some(identity.clone()))
                .await,
            1
        );

        let state = ledger.state.lock().await;
        assert_eq!(
            state.repository.observed_workspace_identity,
            Some((1, identity))
        );
    }

    #[tokio::test]
    async fn terminal_turn_cleanup_forgets_its_observed_repository_revision() {
        let ledger = CommandExecutionLedger::default();
        let finished_attempt = AttemptId::new();
        let active_attempt = AttemptId::new();

        ledger.observe_repository_revision("finished-turn", 1).await;
        ledger.observe_repository_revision("active-turn", 2).await;
        ledger
            .record_uncertain_command_baseline("finished-call", "finished-turn", None)
            .await;
        ledger
            .record_uncertain_command_baseline("active-call", "active-turn", None)
            .await;
        ledger
            .record_typed_mutation_baseline(
                "finished-mutation",
                "finished-turn",
                TypedMutationBaseline {
                    attempt_id: finished_attempt,
                    repo_root: PathBuf::from("finished-repo"),
                    paths: vec!["finished.txt".to_string()],
                },
            )
            .await;
        ledger
            .record_typed_mutation_baseline(
                "active-mutation",
                "active-turn",
                TypedMutationBaseline {
                    attempt_id: active_attempt,
                    repo_root: PathBuf::from("active-repo"),
                    paths: vec!["active.txt".to_string()],
                },
            )
            .await;
        ledger.finish_turn("finished-turn").await;

        let state = ledger.state.lock().await;
        assert!(
            !state
                .repository
                .observed_turn_mutation_revisions
                .contains_key("finished-turn")
        );
        assert!(
            state
                .repository
                .observed_turn_mutation_revisions
                .contains_key("active-turn")
        );
        assert!(
            !state
                .repository
                .uncertain_command_baselines
                .contains_key("finished-call")
        );
        assert!(
            state
                .repository
                .uncertain_command_baselines
                .contains_key("active-call")
        );
        assert!(
            !state
                .repository
                .typed_mutation_baselines
                .contains_key("finished-mutation")
        );
        assert!(
            state
                .repository
                .typed_mutation_baselines
                .contains_key("active-mutation")
        );
    }

    #[tokio::test]
    async fn typed_mutation_baseline_survives_emitter_reconstruction_until_consumed() {
        let ledger = CommandExecutionLedger::default();
        let attempt_id = AttemptId::new();
        ledger
            .record_typed_mutation_baseline(
                "call",
                "turn",
                TypedMutationBaseline {
                    attempt_id,
                    repo_root: PathBuf::from("repo"),
                    paths: vec!["src/lib.rs".to_string()],
                },
            )
            .await;

        let baseline = ledger
            .take_typed_mutation_baseline("call")
            .await
            .expect("completion emitter should recover the launch baseline");

        assert_eq!(baseline.attempt_id, attempt_id);
        assert_eq!(baseline.repo_root, PathBuf::from("repo"));
        assert_eq!(baseline.paths, vec!["src/lib.rs"]);
        assert!(ledger.take_typed_mutation_baseline("call").await.is_none());
    }

    #[tokio::test]
    async fn handler_finalization_before_exit_watcher_records_one_failure() {
        let ledger = CommandExecutionLedger::default();
        let key = key("stored-process-failure.exe");
        ledger.begin_attempt(&key, false).await.expect("attempt");
        ledger
            .track_running_process(42, key.clone(), RawOutputArtifact::unavailable("fixture"))
            .await;

        assert!(ledger.finish_running_process(42, Some(-1)).await.accepted());
        assert!(
            !ledger
                .mark_running_process_completed(42, -1)
                .await
                .accepted()
        );
        assert_eq!(ledger.consecutive_failures(&key).await, 1);
    }

    #[tokio::test]
    async fn running_metadata_is_not_evicted_while_processes_are_live() {
        let ledger = CommandExecutionLedger::default();
        let keys = (0..=64)
            .map(|index| key(&format!("background-{index}.exe")))
            .collect::<Vec<_>>();

        for (process_id, key) in keys.iter().take(64).enumerate() {
            ledger.begin_attempt(key, false).await.expect("attempt");
            ledger
                .track_running_process(
                    process_id as u32,
                    key.clone(),
                    RawOutputArtifact::unavailable("fixture"),
                )
                .await;
        }
        let replacement_key = keys.last().expect("replacement key");
        ledger
            .begin_attempt(replacement_key, false)
            .await
            .expect("replacement attempt");
        ledger
            .track_running_process(
                64,
                replacement_key.clone(),
                RawOutputArtifact::unavailable("replacement fixture"),
            )
            .await;

        assert!(ledger.running_process(0).await.is_some());
        assert_eq!(ledger.consecutive_failures(&keys[0]).await, 0);
        assert!(ledger.mark_running_process_completed(0, 0).await.accepted());
        assert_eq!(ledger.consecutive_failures(&keys[0]).await, 0);
        assert!(ledger.running_process(64).await.is_some());
    }

    #[tokio::test]
    async fn late_exit_reinsertion_preserves_attempt_bound() {
        let ledger = CommandExecutionLedger::default();
        let keys = (0..=MAX_TRACKED_COMMANDS)
            .map(|index| key(&format!("command-{index}")))
            .collect::<Vec<_>>();

        for key in &keys {
            ledger.begin_attempt(key, false).await.expect("attempt");
        }
        assert_eq!(
            ledger.state.lock().await.retry.attempts.len(),
            MAX_TRACKED_COMMANDS
        );
        assert!(ledger.snapshot(&keys[0]).await.is_none());

        ledger.record_exit(&keys[0], 7).await;

        assert_eq!(
            ledger.state.lock().await.retry.attempts.len(),
            MAX_TRACKED_COMMANDS
        );
        assert!(ledger.snapshot(&keys[0]).await.is_some());
        assert!(ledger.snapshot(&keys[1]).await.is_none());
    }
}
