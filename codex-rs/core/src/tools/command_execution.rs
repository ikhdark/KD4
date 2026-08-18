use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::validation::ValidationFreshness;
use codex_protocol::validation::ValidationProofKey;
use codex_protocol::validation::ValidationResult;
use codex_protocol::validation::ValidationTerminalStatus;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::handlers::command_preflight::RgSearchBreadth;
use crate::tools::handlers::command_preflight::RgSearchNarrowing;
use crate::validation_admission::ValidationLaunchPlan;

const MAX_TRACKED_COMMANDS: usize = 128;
const MAX_COMPLETED_VALIDATION_PROOFS: usize = 128;
const COMMAND_EXECUTION_CACHE_SCHEMA_VERSION: u32 = 1;
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
        let search = self.search_narrowing.as_ref()?;
        if !search.can_record_miss {
            return None;
        }
        Some(SearchMissCacheKey {
            environment_id: search.environment_id.clone(),
            repository_identity: search.repository_identity.clone(),
            search_identity: search.search_identity.clone(),
            execution_context: self
                .command
                .iter()
                .filter(|argument| argument.starts_with('\0'))
                .cloned()
                .collect(),
        })
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

mod deterministic_failure_proof {
    /// Sealed proof that a failure outcome is determined by captured inputs
    /// and state. Production deliberately has no constructor until an
    /// authoritative classifier can define and capture its complete
    /// dependency identity.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct InputStateDetermined {
        outcome_class: String,
        _proof_identity: String,
    }

    impl InputStateDetermined {
        #[cfg(test)]
        pub(super) fn for_test(outcome_class: &str, proof_identity: &str) -> Self {
            Self {
                outcome_class: outcome_class.to_string(),
                _proof_identity: proof_identity.to_string(),
            }
        }

        pub(super) fn outcome_class(&self) -> &str {
            &self.outcome_class
        }

        #[cfg(test)]
        pub(super) fn proof_identity(&self) -> &str {
            &self._proof_identity
        }
    }
}

use deterministic_failure_proof::InputStateDetermined;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeterministicFailureRecord {
    proof: InputStateDetermined,
    pub(crate) evidence: RawOutputArtifact,
    pub(crate) exit_code: i32,
    pub(crate) execution_started_at: SystemTime,
    pub(crate) execution_ended_at: SystemTime,
    pub(crate) execution_duration: Duration,
    pub(crate) termination_drain_duration: Option<Duration>,
}

impl DeterministicFailureRecord {
    #[cfg(test)]
    fn from_input_state_determined(
        proof: InputStateDetermined,
        evidence: RawOutputArtifact,
        exit_code: i32,
        execution_ended_at: SystemTime,
        execution_duration: Duration,
        termination_drain_duration: Option<Duration>,
    ) -> Self {
        let execution_started_at = execution_ended_at
            .checked_sub(execution_duration)
            .unwrap_or(execution_ended_at);
        Self {
            proof,
            evidence,
            exit_code,
            execution_started_at,
            execution_ended_at,
            execution_duration,
            termination_drain_duration,
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
    last_diagnosis_identity: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunningCommand {
    pub(crate) key: CommandAttemptKey,
    pub(crate) artifact: RawOutputArtifact,
    completed_exit_code: Option<i32>,
    validation_launch: Option<ValidationLaunchPlan>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct BoundAutoValidationLeaf {
    pub(crate) step_id: String,
    pub(crate) step_revision: u64,
    pub(crate) implementation_revision: u64,
    pub(crate) implementation_identity: String,
    pub(crate) repository: PathBuf,
    pub(crate) route: ValidationRoute,
    pub(crate) leaf_index: usize,
}

impl BoundAutoValidationLeaf {
    pub(crate) fn leaf(&self) -> Option<&codex_protocol::plan_tool::ValidationRouteLeaf> {
        self.route.leaves.get(self.leaf_index)
    }

    pub(crate) fn leaf_route(&self) -> Option<ValidationRoute> {
        self.leaf().cloned().map(|leaf| ValidationRoute {
            leaves: vec![leaf],
            ordering: self.route.ordering,
        })
    }
}

#[derive(Debug, Clone)]
struct CompletedValidationProof {
    result: ValidationResult,
    artifact: RawOutputArtifact,
}

#[derive(Debug, Clone)]
struct CommandExecutionPersistence {
    cache_path: PathBuf,
    codex_home: PathBuf,
    thread_id: String,
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedValidationProof {
    result: ValidationResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandExecutionCacheDocument {
    schema_version: u32,
    workspace_identity: crate::git_workspace::WorkspaceEvidenceIdentity,
    repository_epoch: u64,
    search_misses: Vec<SearchMissCacheKey>,
    completed_validations: Vec<PersistedValidationProof>,
}

#[derive(Default)]
struct CommandExecutionState {
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
    running: HashMap<i32, RunningCommand>,
    running_order: VecDeque<i32>,
    repository_epoch: u64,
    observed_workspace_identity: Option<(u64, crate::git_workspace::WorkspaceEvidenceIdentity)>,
    observed_turn_mutation_revisions: HashMap<String, u64>,
    completed_validations: HashMap<ValidationProofKey, CompletedValidationProof>,
    completed_validation_order: VecDeque<ValidationProofKey>,
    validation_results_by_call: HashMap<String, ValidationResult>,
    validation_result_call_order: VecDeque<String>,
    allowed_search_expansions: HashSet<SearchNarrowingScope>,
    search_misses: HashSet<SearchMissCacheKey>,
    search_miss_order: VecDeque<SearchMissCacheKey>,
}

pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    bound_auto_validations: Mutex<HashMap<String, BoundAutoValidationLeaf>>,
    persistence: Option<CommandExecutionPersistence>,
}

impl Default for CommandExecutionLedger {
    fn default() -> Self {
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            bound_auto_validations: Mutex::new(HashMap::new()),
            persistence: None,
        }
    }
}

impl CommandExecutionLedger {
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
            codex_home,
            thread_id,
            cwd: cwd.to_path_buf(),
        };
        let mut ledger = Self {
            state: Mutex::new(CommandExecutionState {
                observed_workspace_identity: Some((0, workspace_identity.clone())),
                ..CommandExecutionState::default()
            }),
            bound_auto_validations: Mutex::new(HashMap::new()),
            persistence: Some(persistence.clone()),
        };
        let Ok(bytes) = tokio::fs::read(&persistence.cache_path).await else {
            return ledger;
        };
        let Ok(document) = serde_json::from_slice::<CommandExecutionCacheDocument>(&bytes) else {
            return ledger;
        };
        if document.schema_version != COMMAND_EXECUTION_CACHE_SCHEMA_VERSION
            || document.workspace_identity != workspace_identity
        {
            return ledger;
        }

        let mut state = CommandExecutionState {
            repository_epoch: document.repository_epoch,
            observed_workspace_identity: Some((document.repository_epoch, workspace_identity)),
            ..CommandExecutionState::default()
        };
        for search_miss in document
            .search_misses
            .into_iter()
            .take(MAX_TRACKED_COMMANDS)
        {
            if state.search_misses.insert(search_miss.clone()) {
                state.search_miss_order.push_back(search_miss);
            }
        }
        for persisted in document
            .completed_validations
            .into_iter()
            .take(MAX_COMPLETED_VALIDATION_PROOFS)
        {
            let result = persisted.result;
            if result.status != ValidationTerminalStatus::Succeeded {
                continue;
            }
            let Some(artifact_ref) = result.raw_artifact_ref.as_deref() else {
                continue;
            };
            let Some(artifact) = RawOutputArtifact::restore_validation(
                &persistence.codex_home,
                &persistence.thread_id,
                artifact_ref,
            ) else {
                continue;
            };
            let proof_key = result.proof_key.clone();
            state
                .completed_validation_order
                .push_back(proof_key.clone());
            state.completed_validations.insert(
                proof_key,
                CompletedValidationProof {
                    result: result.clone(),
                    artifact,
                },
            );
            state
                .validation_result_call_order
                .push_back(result.call_id.clone());
            state
                .validation_results_by_call
                .insert(result.call_id.clone(), result);
        }
        ledger.state = Mutex::new(state);
        ledger
    }

    pub(crate) async fn admit_search_narrowing(
        &self,
        _key: &CommandAttemptKey,
    ) -> Result<(), String> {
        Ok(())
    }

    pub(crate) async fn bind_auto_validation_leaf(
        &self,
        call_id: String,
        binding: BoundAutoValidationLeaf,
    ) -> bool {
        self.bound_auto_validations
            .lock()
            .await
            .insert(call_id, binding)
            .is_none()
    }

    pub(crate) async fn auto_validation_leaf(
        &self,
        call_id: &str,
    ) -> Option<BoundAutoValidationLeaf> {
        self.bound_auto_validations
            .lock()
            .await
            .get(call_id)
            .cloned()
    }

    pub(crate) async fn clear_auto_validation_leaf(&self, call_id: &str) {
        self.bound_auto_validations.lock().await.remove(call_id);
    }

    pub(crate) async fn reusable_validation(
        &self,
        key: &ValidationProofKey,
    ) -> Option<ValidationResult> {
        let proof = self
            .state
            .lock()
            .await
            .completed_validations
            .get(key)
            .cloned()?;
        let Some((artifact_ref, artifact_sha256)) = proof.artifact.validation_integrity().await
        else {
            let mut state = self.state.lock().await;
            state.completed_validations.remove(key);
            state
                .completed_validation_order
                .retain(|entry| entry != key);
            return None;
        };
        if proof.result.raw_artifact_ref.as_deref() != Some(artifact_ref.as_str())
            || proof.result.raw_artifact_sha256.as_deref() != Some(artifact_sha256.as_str())
        {
            let mut state = self.state.lock().await;
            state.completed_validations.remove(key);
            state
                .completed_validation_order
                .retain(|entry| entry != key);
            return None;
        }
        let mut result = proof.result;
        result.freshness = ValidationFreshness::Reused;
        Some(result)
    }

    pub(crate) async fn validation_result_for_call(
        &self,
        call_id: &str,
    ) -> Option<ValidationResult> {
        self.state
            .lock()
            .await
            .validation_results_by_call
            .get(call_id)
            .cloned()
    }

    pub(crate) async fn supersede_validation_result_for_call(
        &self,
        call_id: &str,
    ) -> Option<ValidationResult> {
        let mut state = self.state.lock().await;
        let proof_key = state
            .validation_results_by_call
            .get(call_id)?
            .proof_key
            .clone();
        state.completed_validations.remove(&proof_key);
        state
            .completed_validation_order
            .retain(|entry| entry != &proof_key);
        let result = state.validation_results_by_call.get_mut(call_id)?;
        result.status = ValidationTerminalStatus::Superseded;
        result.freshness = ValidationFreshness::Superseded;
        result.summary = Some(
            "focused validation was superseded by a newer relevant implementation".to_string(),
        );
        Some(result.clone())
    }

    pub(crate) async fn observe_repository_revision(
        &self,
        turn_id: &str,
        mutation_revision: u64,
    ) -> u64 {
        let (repository_epoch, refresh_workspace_identity) = {
            let mut state = self.state.lock().await;
            let delta = {
                let observed_revision = state
                    .observed_turn_mutation_revisions
                    .entry(turn_id.to_string())
                    .or_default();
                let delta = mutation_revision.saturating_sub(*observed_revision);
                *observed_revision = (*observed_revision).max(mutation_revision);
                delta
            };
            state.repository_epoch = state.repository_epoch.saturating_add(delta);
            let repository_epoch = state.repository_epoch;
            let refresh_workspace_identity = delta > 0
                || state
                    .observed_workspace_identity
                    .as_ref()
                    .is_none_or(|(epoch, _)| *epoch != repository_epoch);
            if refresh_workspace_identity {
                state.observed_workspace_identity = None;
            }
            (repository_epoch, refresh_workspace_identity)
        };

        if !refresh_workspace_identity {
            return repository_epoch;
        }
        let Some(persistence) = self.persistence.as_ref() else {
            return repository_epoch;
        };
        let Some(workspace_identity) =
            crate::git_workspace::capture_workspace_evidence_identity(&persistence.cwd).await
        else {
            return repository_epoch;
        };
        let mut state = self.state.lock().await;
        if state.repository_epoch == repository_epoch && state.observed_workspace_identity.is_none()
        {
            state.observed_workspace_identity = Some((repository_epoch, workspace_identity));
        }
        state.repository_epoch
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
                && state.search_misses.contains(&search_miss_key)
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

    /// Claims one diagnosis for an exact synthetically proven deterministic
    /// failure and selected hypothesis/recovery identity.
    #[cfg(test)]
    pub(crate) async fn claim_failure_diagnosis(
        &self,
        key: &CommandAttemptKey,
        selected_hypothesis_recovery_identity: &str,
    ) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.attempts.get_mut(key) else {
            return false;
        };
        let Some(failure) = entry.deterministic_failure.as_ref() else {
            return false;
        };
        let diagnosis_identity = format!(
            "{}:{}:{}:{}:{}",
            key.fingerprint(),
            failure.proof.outcome_class(),
            failure.proof.proof_identity(),
            failure.exit_code,
            selected_hypothesis_recovery_identity,
        );
        if entry.last_diagnosis_identity.as_deref() == Some(&diagnosis_identity) {
            return false;
        }
        entry.last_diagnosis_identity = Some(diagnosis_identity);
        true
    }

    pub(crate) async fn record_exit(&self, key: &CommandAttemptKey, exit_code: i32) {
        let mut state = self.state.lock().await;
        record_search_result_locked(&mut state, key, exit_code);
        record_exit_locked(&mut state, key, exit_code);
    }

    #[cfg(test)]
    pub(crate) async fn record_deterministic_failure(
        &self,
        key: &CommandAttemptKey,
        failure: DeterministicFailureRecord,
    ) {
        let mut state = self.state.lock().await;
        let entry = attempt_entry_locked(&mut state, key);
        entry.last_exit_code = Some(failure.exit_code);
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.deterministic_failure = Some(failure);
    }

    #[cfg(test)]
    pub(crate) async fn track_running_process(
        &self,
        process_id: i32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
    ) {
        self.track_running_process_with_validation_contract(
            process_id,
            key,
            artifact,
            None,
            Instant::now(),
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn track_running_process_with_validation_contract(
        &self,
        process_id: i32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
        validation_launch: Option<ValidationLaunchPlan>,
        started_at: Instant,
    ) {
        let mut state = self.state.lock().await;
        if state.running.contains_key(&process_id) {
            tracing::error!(process_id, "refusing to replace live command bookkeeping");
            return;
        }
        state.running_order.push_back(process_id);
        state.running.insert(
            process_id,
            RunningCommand {
                key,
                artifact,
                completed_exit_code: None,
                validation_launch,
                started_at,
            },
        );
    }

    pub(crate) async fn running_process(&self, process_id: i32) -> Option<RunningCommand> {
        self.state.lock().await.running.get(&process_id).cloned()
    }

    pub(crate) async fn finish_turn(&self, turn_id: &str) {
        self.forget_turn_repository_revision(turn_id).await;
        self.persist_cache().await;
    }

    async fn persist_cache(&self) {
        let Some(persistence) = self.persistence.clone() else {
            return;
        };
        let (repository_epoch, observed_workspace_identity, search_misses, completed_validations) = {
            let state = self.state.lock().await;
            (
                state.repository_epoch,
                state.observed_workspace_identity.clone(),
                state.search_miss_order.iter().cloned().collect::<Vec<_>>(),
                state
                    .completed_validation_order
                    .iter()
                    .filter_map(|key| state.completed_validations.get(key))
                    .map(|proof| PersistedValidationProof {
                        result: proof.result.clone(),
                    })
                    .collect::<Vec<_>>(),
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
            completed_validations,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        let cache_path = persistence.cache_path;
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

    async fn forget_turn_repository_revision(&self, turn_id: &str) {
        let mut state = self.state.lock().await;
        state.observed_turn_mutation_revisions.remove(turn_id);
        state
            .allowed_search_expansions
            .retain(|scope| scope.turn_id != turn_id);
    }

    pub(crate) async fn update_running_artifact(
        &self,
        process_id: i32,
        artifact: RawOutputArtifact,
    ) {
        {
            let mut state = self.state.lock().await;
            let deterministic_completion = state.running.get_mut(&process_id).and_then(|running| {
                running.artifact = artifact.clone();
                running
                    .completed_exit_code
                    .filter(|exit_code| *exit_code != 0 && running.validation_launch.is_some())
                    .map(|exit_code| (running.key.clone(), exit_code))
            });
            if let Some((key, exit_code)) = deterministic_completion
                && let Some(failure) = state
                    .attempts
                    .get_mut(&key)
                    .and_then(|entry| entry.deterministic_failure.as_mut())
                && failure.exit_code == exit_code
            {
                failure.evidence = artifact;
            }
        }
        self.publish_completed_validation_if_ready(process_id).await;
    }

    pub(crate) async fn mark_running_process_completed(
        &self,
        process_id: i32,
        exit_code: i32,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            let Some(running) = state.running.get_mut(&process_id) else {
                return false;
            };
            if running.completed_exit_code.is_some() {
                return true;
            }
            running.completed_exit_code = Some(exit_code);
            let running = running.clone();
            record_running_exit_locked(&mut state, &running, exit_code);
        }
        self.publish_completed_validation_if_ready(process_id).await;
        true
    }

    async fn publish_completed_validation_if_ready(&self, process_id: i32) {
        let candidate = {
            let state = self.state.lock().await;
            let Some(running) = state.running.get(&process_id) else {
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
            proof_key, route, call_id, artifact, started_at, exit_code, process_id, None, false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_completed_validation_with_context(
        &self,
        proof_key: ValidationProofKey,
        route: ValidationRoute,
        call_id: String,
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
        let selected_no_cargo_tests = exit_code == 0
            && validation_route_is_cargo_test(&route)
            && !cargo_test_output_selected_at_least_one(&retained_output);
        let succeeded = exit_code == 0 && !selected_no_cargo_tests;
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
            summary: Some(if selected_no_cargo_tests {
                "focused cargo validation selected zero tests".to_string()
            } else if succeeded {
                "focused validation succeeded".to_string()
            } else {
                format!("focused validation exited with code {exit_code}")
            }),
            failure_excerpt: (!succeeded).then(|| {
                if selected_no_cargo_tests {
                    "cargo validation exited successfully but selected zero tests; exact output is retained in the immutable artifact"
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
        {
            let mut state = self.state.lock().await;
            if state.validation_results_by_call.contains_key(&call_id) {
                return true;
            }
            while state.validation_results_by_call.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
                let Some(oldest) = state.validation_result_call_order.pop_front() else {
                    break;
                };
                state.validation_results_by_call.remove(&oldest);
            }
            state
                .validation_result_call_order
                .push_back(call_id.clone());
            state
                .validation_results_by_call
                .insert(call_id.clone(), result.clone());
            if succeeded && !state.completed_validations.contains_key(&proof_key) {
                while state.completed_validations.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
                    let Some(oldest) = state.completed_validation_order.pop_front() else {
                        break;
                    };
                    state.completed_validations.remove(&oldest);
                }
                state
                    .completed_validation_order
                    .push_back(proof_key.clone());
                state.completed_validations.insert(
                    proof_key.clone(),
                    CompletedValidationProof { result, artifact },
                );
            }
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

    pub(crate) async fn finish_running_process(
        &self,
        process_id: i32,
        exit_code: Option<i32>,
    ) -> bool {
        {
            let mut state = self.state.lock().await;
            let Some(mut running) = state.running.remove(&process_id) else {
                return false;
            };
            state.running_order.retain(|tracked| *tracked != process_id);
            if running.completed_exit_code.is_none()
                && let Some(exit_code) = exit_code
            {
                running.completed_exit_code = Some(exit_code);
                record_running_exit_locked(&mut state, &running, exit_code);
            }
        }
        let mut state = self.state.lock().await;
        while state.attempts.len() > MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(&mut state)
        {}
        true
    }

    #[cfg(test)]
    async fn snapshot(&self, key: &CommandAttemptKey) -> Option<AttemptEntry> {
        self.state.lock().await.attempts.get(key).cloned()
    }

    #[cfg(test)]
    pub(crate) async fn consecutive_failures(&self, key: &CommandAttemptKey) -> u8 {
        self.snapshot(key)
            .await
            .map_or(0, |entry| entry.consecutive_failures)
    }
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
    if exit_code == 1
        && let Some(search) = key.search_narrowing.as_ref()
        && search.can_record_miss
        && let Some(parent_scope) = search.parent_scope()
    {
        state.allowed_search_expansions.insert(parent_scope);
    }
    if key
        .search_narrowing
        .as_ref()
        .is_some_and(|search| search.can_record_miss)
        && let Some(search_miss_key) = key.search_miss_cache_key()
    {
        if exit_code == 1 && state.search_misses.insert(search_miss_key.clone()) {
            state.search_miss_order.push_back(search_miss_key);
            while state.search_misses.len() > MAX_TRACKED_COMMANDS {
                if let Some(oldest) = state.search_miss_order.pop_front() {
                    state.search_misses.remove(&oldest);
                }
            }
        } else if exit_code == 0 && state.search_misses.remove(&search_miss_key) {
            state
                .search_miss_order
                .retain(|cached| cached != &search_miss_key);
        }
    }
}

fn record_exit_locked(state: &mut CommandExecutionState, key: &CommandAttemptKey, exit_code: i32) {
    let entry = attempt_entry_locked(state, key);
    entry.last_exit_code = Some(exit_code);
    if exit_code == 0 {
        entry.consecutive_failures = 0;
        entry.deterministic_failure = None;
        entry.last_diagnosis_identity = None;
    } else {
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
    }
}

fn attempt_entry_locked<'a>(
    state: &'a mut CommandExecutionState,
    key: &CommandAttemptKey,
) -> &'a mut AttemptEntry {
    if !state.attempts.contains_key(key) {
        while state.attempts.len() >= MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(state)
        {}
        state.insertion_order.push_back(key.clone());
    }
    state.attempts.entry(key.clone()).or_default()
}

fn evict_oldest_inactive_attempt_locked(state: &mut CommandExecutionState) -> bool {
    if let Some(position) = state
        .insertion_order
        .iter()
        .position(|key| !command_attempt_is_active(state, key))
        && let Some(oldest) = state.insertion_order.remove(position)
    {
        state.attempts.remove(&oldest);
        return true;
    }
    let Some(unordered_key) = state
        .attempts
        .keys()
        .find(|key| !command_attempt_is_active(state, key))
        .cloned()
    else {
        return false;
    };
    state.attempts.remove(&unordered_key);
    true
}

fn command_attempt_is_active(state: &CommandExecutionState, key: &CommandAttemptKey) -> bool {
    state.running.values().any(|running| running.key == *key)
}

fn validation_route_is_cargo_test(route: &ValidationRoute) -> bool {
    route.leaves.as_slice().first().is_some_and(|leaf| {
        matches!(
            leaf.argv.as_slice(),
            [program, subcommand, ..] if program == "cargo" && subcommand == "test"
        )
    })
}

fn cargo_test_output_selected_at_least_one(output: &[u8]) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        line.trim()
            .strip_prefix("running ")
            .and_then(|summary| summary.split_whitespace().next())
            .and_then(|count| count.parse::<u64>().ok())
            .is_some_and(|count| count > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn key(command: &str) -> CommandAttemptKey {
        CommandAttemptKey::new("exec_command", "local", "C:/repo", &[command.to_string()])
    }

    fn deterministic_failure(class: &str, exit_code: i32) -> DeterministicFailureRecord {
        DeterministicFailureRecord::from_input_state_determined(
            InputStateDetermined::for_test(class, "synthetic-complete-identity-v1"),
            RawOutputArtifact::unavailable("original deterministic failure fixture"),
            exit_code,
            SystemTime::now(),
            Duration::from_millis(170),
            None,
        )
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
    async fn search_misses_and_successful_validations_survive_safe_reopen() {
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
        let stale =
            CommandExecutionLedger::load_or_new(codex_home, thread_id.to_string(), &repository)
                .await;
        assert!(stale.reusable_validation(&proof_key).await.is_none());
        assert!(
            stale
                .begin_attempt_with_freshness(&equivalent, false, false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn mutation_before_finish_turn_does_not_rebase_persisted_results() {
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

        std::fs::write(repository.join("external-change.txt"), "changed")
            .expect("mutate repository before persistence");
        ledger.finish_turn("turn-a").await;

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
        assert!(reopened.reusable_validation(&proof_key).await.is_none());
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
    async fn broad_rg_is_admitted_without_prior_search_state() {
        let ledger = CommandExecutionLedger::default();
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
        assert!(ledger.admit_search_narrowing(&broad).await.is_ok());
        ledger.finish_turn("turn-a").await;
        assert!(ledger.admit_search_narrowing(&broad).await.is_ok());
    }

    #[tokio::test]
    async fn equivalent_search_miss_is_cached_until_context_changes() {
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
            .with_search_narrowing("turn-a", "repo-a", Some(search("needle:src")));
        let equivalent = key("rg needle ./src")
            .with_repository_epoch(1)
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
                    .with_repository_epoch(2)
                    .with_search_narrowing("turn-b", "repo-a", Some(search("needle:src"))),
                false,
            )
            .await
            .expect("changed repository epoch executes");
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
        assert!(ledger.mark_running_process_completed(42, 1).await);
        ledger
            .begin_attempt(&equivalent, false)
            .await
            .expect_err("background miss should be reused");
    }

    #[tokio::test]
    async fn synthetic_input_state_proof_blocks_exact_retry_but_freshness_bypasses() {
        let ledger = CommandExecutionLedger::default();
        let attempt_key = key("fails.exe").with_repository_epoch(1);

        ledger
            .begin_attempt(&attempt_key, false)
            .await
            .expect("first attempt");
        ledger
            .record_deterministic_failure(
                &attempt_key,
                deterministic_failure("focused-validation", 7),
            )
            .await;

        assert!(
            ledger
                .claim_failure_diagnosis(&attempt_key, "hypothesis-a/recovery-a")
                .await
        );
        assert!(
            !ledger
                .claim_failure_diagnosis(&attempt_key, "hypothesis-a/recovery-a")
                .await
        );
        ledger
            .begin_attempt(&attempt_key, false)
            .await
            .expect_err("the synthetic closed proof blocks an exact retry");
        ledger
            .begin_attempt(&attempt_key, true)
            .await
            .expect("a repaired command bypasses the retained proof");
        ledger
            .begin_attempt_with_freshness(&attempt_key, false, true)
            .await
            .expect("force_fresh bypasses the retained proof");
        assert!(
            ledger
                .claim_failure_diagnosis(&attempt_key, "hypothesis-b/recovery-b")
                .await
        );

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

        assert!(ledger.mark_running_process_completed(42, 7).await);
        assert!(ledger.mark_running_process_completed(42, 7).await);
        assert!(ledger.finish_running_process(42, Some(7)).await);

        let snapshot = ledger.snapshot(&key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        ledger
            .begin_attempt(&key, false)
            .await
            .expect("one failure must not block the next attempt");
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

        assert!(ledger.mark_running_process_completed(42, 7).await);
        assert!(ledger.mark_running_process_completed(42, 7).await);
        ledger
            .update_running_artifact(42, finalized_artifact.clone())
            .await;
        assert!(ledger.finish_running_process(42, Some(7)).await);

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

        assert!(ledger.finish_running_process(43, Some(9)).await);
        assert!(!ledger.finish_running_process(43, Some(9)).await);

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
            .record_deterministic_failure(
                &command_key,
                deterministic_failure("focused-validation", 7),
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

        assert!(ledger.mark_running_process_completed(44, 0).await);
        assert!(ledger.finish_running_process(44, Some(0)).await);

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
    async fn terminal_turn_cleanup_forgets_its_observed_repository_revision() {
        let ledger = CommandExecutionLedger::default();

        ledger.observe_repository_revision("finished-turn", 1).await;
        ledger.observe_repository_revision("active-turn", 2).await;
        ledger.finish_turn("finished-turn").await;

        let state = ledger.state.lock().await;
        assert!(
            !state
                .observed_turn_mutation_revisions
                .contains_key("finished-turn")
        );
        assert!(
            state
                .observed_turn_mutation_revisions
                .contains_key("active-turn")
        );
    }

    #[tokio::test]
    async fn handler_finalization_before_exit_watcher_records_one_failure() {
        let ledger = CommandExecutionLedger::default();
        let key = key("stored-process-failure.exe");
        ledger.begin_attempt(&key, false).await.expect("attempt");
        ledger
            .track_running_process(42, key.clone(), RawOutputArtifact::unavailable("fixture"))
            .await;

        assert!(ledger.finish_running_process(42, Some(-1)).await);
        assert!(!ledger.mark_running_process_completed(42, -1).await);
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
                    process_id as i32,
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
        assert!(ledger.mark_running_process_completed(0, 0).await);
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
            ledger.state.lock().await.attempts.len(),
            MAX_TRACKED_COMMANDS
        );
        assert!(ledger.snapshot(&keys[0]).await.is_none());

        ledger.record_exit(&keys[0], 7).await;

        assert_eq!(
            ledger.state.lock().await.attempts.len(),
            MAX_TRACKED_COMMANDS
        );
        assert!(ledger.snapshot(&keys[0]).await.is_some());
        assert!(ledger.snapshot(&keys[1]).await.is_none());
    }
}
