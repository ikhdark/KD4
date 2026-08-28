use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::time::Duration;

use codex_agent_task_store::AttemptId;
use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::validation::ValidationResult;
use codex_protocol::validation::ValidationTerminalStatus;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::tools::handlers::command_search::RgSearchBreadth;
use crate::tools::handlers::command_search::RgSearchNarrowing;
use crate::validation_admission::ValidationLaunchPlan;

const MAX_TRACKED_COMMANDS: usize = 128;
const COMMAND_EXECUTION_CACHE_SCHEMA_VERSION: u32 = 2;
const COMMAND_FINGERPRINT_VERSION: &str = "v2";
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct CommandAttemptKey {
    tool_name: String,
    environment_id: String,
    cwd: String,
    command: Vec<String>,
    execution_context: BTreeMap<String, String>,
    search_narrowing: Option<SearchNarrowingAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
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
    execution_context: BTreeMap<String, String>,
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
            execution_context: BTreeMap::new(),
            search_narrowing: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executed_command(mut self, command: &[String]) -> Self {
        self.command = command.to_vec();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_environment(self, environment: &HashMap<String, String>) -> Self {
        let mut entries = environment.iter().collect::<Vec<_>>();
        entries.sort_unstable_by(|(left_key, left_value), (right_key, right_value)| {
            left_key
                .cmp(right_key)
                .then_with(|| left_value.cmp(right_value))
        });
        self.with_context_fingerprint("environment", &entries)
    }

    pub(crate) fn with_environment_fingerprint(self, fingerprint: &str) -> Self {
        self.with_context_fingerprint("environment", fingerprint)
    }

    pub(crate) fn with_timeout_ms(self, timeout_ms: Option<u64>) -> Self {
        self.with_context_fingerprint("timeout_ms", &timeout_ms)
    }

    pub(crate) fn with_sandbox_context<T: Serialize + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("sandbox", context)
    }

    pub(crate) fn with_permission_context<T: Serialize + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("permission", context)
    }

    pub(crate) fn with_input_context<T: Serialize + ?Sized>(self, context: &T) -> Self {
        self.with_context_fingerprint("input", context)
    }

    pub(crate) fn with_runtime_context<T: Serialize + ?Sized>(self, context: &T) -> Self {
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
        let has_workspace_identity = self.execution_context.contains_key("workspace_identity");
        SearchMissCacheKey {
            environment_id: search.environment_id.clone(),
            repository_identity: search.repository_identity.clone(),
            search_identity: search.search_identity.clone(),
            execution_context: self
                .execution_context
                .iter()
                .filter(|(label, _)| !(has_workspace_identity && *label == "repository_epoch"))
                .map(|(label, fingerprint)| (label.clone(), fingerprint.clone()))
                .collect(),
        }
    }

    pub(crate) fn fingerprint(&self) -> String {
        fingerprint_value(self)
    }

    fn with_context_fingerprint<T: Serialize + ?Sized>(mut self, label: &str, value: &T) -> Self {
        self.execution_context
            .insert(label.to_string(), fingerprint_value(value));
        self
    }
}

fn fingerprint_value<T: Serialize + ?Sized>(value: &T) -> String {
    let encoded = match serde_json::to_vec(value) {
        Ok(encoded) => encoded,
        Err(error) => unreachable!("command fingerprint input must serialize: {error}"),
    };
    let mut hasher = Sha256::new();
    hasher.update(b"kd4-command-fingerprint\0");
    hasher.update(COMMAND_FINGERPRINT_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(encoded);
    format!("{}:{:x}", COMMAND_FINGERPRINT_VERSION, hasher.finalize())
}

fn workspace_identity_hash(identity: &crate::git_workspace::WorkspaceEvidenceIdentity) -> String {
    let bytes = match serde_json::to_vec(identity) {
        Ok(bytes) => bytes,
        Err(error) => unreachable!("workspace evidence identity must serialize: {error}"),
    };
    format!("{:x}", Sha256::digest(bytes))
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
struct CommandExecutionPersistence {
    cache_path: PathBuf,
    cwd: PathBuf,
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
    workspace_identity_observation_epoch: Option<u64>,
    observed_workspace_identity: Option<(u64, crate::git_workspace::WorkspaceEvidenceIdentity)>,
    observed_workspace_identity_hash: Option<(u64, String)>,
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
struct CommandExecutionState {
    retry: CommandRetryState,
    process: CommandProcessState,
    repository: CommandRepositoryState,
    search: CommandSearchState,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletedValidation {
    pub(crate) result: ValidationResult,
    pub(crate) bound_plan_step: Option<(String, u64)>,
    pub(crate) bound_work_unit: Option<(String, u64)>,
    pub(crate) focused_validation_token:
        Option<crate::agent::task_coordinator::FocusedValidationToken>,
}

pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    workspace_identity_refresh: tokio::sync::Semaphore,
    persistence: Option<CommandExecutionPersistence>,
}

impl Default for CommandExecutionLedger {
    fn default() -> Self {
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            workspace_identity_refresh: tokio::sync::Semaphore::new(/*permits*/ 1),
            persistence: None,
        }
    }
}

impl CommandExecutionLedger {
    pub(crate) fn allocate_execution_id(&self) -> CommandExecutionId {
        CommandExecutionId(NEXT_COMMAND_EXECUTION_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) async fn load_or_new(codex_home: PathBuf, thread_id: String, cwd: &Path) -> Self {
        let persistence = CommandExecutionPersistence {
            cache_path: codex_home
                .join("command-execution-cache")
                .join(format!("{thread_id}.json")),
            cwd: cwd.to_path_buf(),
        };
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            workspace_identity_refresh: tokio::sync::Semaphore::new(/*permits*/ 1),
            persistence: Some(persistence),
        }
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
        let preceded_by_narrow_miss = self
            .state
            .lock()
            .await
            .search
            .allowed_expansions
            .contains(&scope);
        tracing::debug!(
            preceded_by_narrow_miss,
            query_identity = %search.query_identity,
            "admitting broad rg search"
        );
        Ok(())
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
        let requested_repository_epoch = {
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
            state.repository.epoch
        };

        let Ok(_refresh_permit) = self.workspace_identity_refresh.acquire().await else {
            unreachable!("command-execution workspace refresh semaphore is never closed");
        };
        let (repository_epoch, expected_repository_root) = {
            let mut state = self.state.lock().await;
            let repository_epoch = state.repository.epoch;
            if state
                .repository
                .observed_workspace_identity
                .as_ref()
                .is_some_and(|(epoch, _)| *epoch == repository_epoch)
                || state.repository.workspace_identity_observation_epoch == Some(repository_epoch)
            {
                return repository_epoch;
            }
            let expected_repository_root = state
                .repository
                .observed_workspace_identity
                .as_ref()
                .and_then(|(_, identity)| identity.repository_root.clone());
            state.repository.observed_workspace_identity = None;
            state.repository.observed_workspace_identity_hash = None;
            (repository_epoch, expected_repository_root)
        };
        let observed_workspace_identity = observed_workspace_identity.filter(|identity| {
            identity.repository_root.is_some()
                && requested_repository_epoch == repository_epoch
                && expected_repository_root
                    .as_ref()
                    .is_none_or(|root| identity.repository_root.as_ref() == Some(root))
        });
        let workspace_identity = match observed_workspace_identity {
            Some(workspace_identity) => workspace_identity,
            None => {
                let Some(persistence) = self.persistence.as_ref() else {
                    self.finish_workspace_identity_observation_without_identity(repository_epoch)
                        .await;
                    return repository_epoch;
                };
                let Some(workspace_identity) =
                    crate::git_workspace::capture_workspace_evidence_identity(&persistence.cwd)
                        .await
                else {
                    self.finish_workspace_identity_observation_without_identity(repository_epoch)
                        .await;
                    return repository_epoch;
                };
                workspace_identity
            }
        };
        let cached_document = if repository_epoch == 0 {
            match self.persistence.as_ref() {
                Some(persistence) => tokio::fs::read(&persistence.cache_path)
                    .await
                    .ok()
                    .and_then(|bytes| {
                        serde_json::from_slice::<CommandExecutionCacheDocument>(&bytes).ok()
                    })
                    .filter(|document| {
                        document.schema_version == COMMAND_EXECUTION_CACHE_SCHEMA_VERSION
                            && document.workspace_identity == workspace_identity
                    }),
                None => None,
            }
        } else {
            None
        };
        let workspace_identity_hash = workspace_identity_hash(&workspace_identity);
        let mut state = self.state.lock().await;
        if state.repository.epoch == repository_epoch
            && state.repository.observed_workspace_identity.is_none()
        {
            let observed_epoch = cached_document
                .as_ref()
                .map_or(repository_epoch, |document| document.repository_epoch);
            state.repository.epoch = observed_epoch;
            state.repository.workspace_identity_observation_epoch = Some(observed_epoch);
            state.repository.observed_workspace_identity =
                Some((observed_epoch, workspace_identity));
            state.repository.observed_workspace_identity_hash =
                Some((observed_epoch, workspace_identity_hash));
            if let Some(document) = cached_document {
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
        }
        state.repository.epoch
    }

    async fn finish_workspace_identity_observation_without_identity(&self, repository_epoch: u64) {
        let mut state = self.state.lock().await;
        if state.repository.epoch == repository_epoch
            && state.repository.observed_workspace_identity.is_none()
        {
            state.repository.workspace_identity_observation_epoch = Some(repository_epoch);
        }
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
        let (epoch, identity_hash) = state.repository.observed_workspace_identity_hash.as_ref()?;
        if *epoch != state.repository.epoch {
            return None;
        }
        Some(identity_hash.clone())
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
                    fingerprint: fingerprint_value(&search_miss_key),
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
    ) -> Result<(), String> {
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
        .await
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
    ) -> Result<(), String> {
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
        .await
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
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        debug_assert_command_execution_invariants(&state);
        if state.process.running.contains_key(&process_id) {
            tracing::error!(process_id, "refusing to replace live command bookkeeping");
            return Err(format!(
                "process id {process_id} already has live command bookkeeping"
            ));
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
        Ok(())
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
        let document = CommandExecutionCacheDocument {
            schema_version: COMMAND_EXECUTION_CACHE_SCHEMA_VERSION,
            workspace_identity: observed_workspace_identity,
            repository_epoch,
            search_misses,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        if let Err(error) = write_cache_document(persistence.cache_path, bytes).await {
            tracing::warn!(%error, "failed to persist command execution cache");
        }
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

    pub(crate) async fn complete_running_validation(
        &self,
        process_id: u32,
        timed_out: bool,
    ) -> Option<CompletedValidation> {
        let (launch, artifact, started_at, exit_code) = {
            let mut state = self.state.lock().await;
            let running = if let Some(running) = state.process.running.get_mut(&process_id) {
                running
            } else {
                &mut state
                    .process
                    .pending_by_execution_id
                    .values_mut()
                    .find(|pending| pending.process_id == process_id)?
                    .command
            };
            let exit_code = running.completed_exit_code?;
            let launch = running.validation_launch.take()?;
            (
                launch,
                running.artifact.clone(),
                running.started_at,
                exit_code,
            )
        };
        self.complete_validation_from_launch(
            &launch,
            Some(artifact),
            started_at,
            Some(exit_code),
            timed_out,
            Some(process_id.to_string()),
        )
        .await
    }

    pub(crate) async fn complete_timed_out_running_validation(
        &self,
        process_id: u32,
    ) -> Option<CompletedValidation> {
        let (launch, artifact, started_at) = {
            let mut state = self.state.lock().await;
            let running = if let Some(running) = state.process.running.get_mut(&process_id) {
                running
            } else {
                &mut state
                    .process
                    .pending_by_execution_id
                    .values_mut()
                    .find(|pending| pending.process_id == process_id)?
                    .command
            };
            let launch = running.validation_launch.take()?;
            (launch, running.artifact.clone(), running.started_at)
        };
        self.complete_validation_from_launch(
            &launch,
            Some(artifact),
            started_at,
            /*exit_code*/ None,
            /*timed_out*/ true,
            Some(process_id.to_string()),
        )
        .await
    }

    pub(crate) async fn complete_inline_validation(
        &self,
        launch: &ValidationLaunchPlan,
        artifact: Option<RawOutputArtifact>,
        started_at: Instant,
        exit_code: Option<i32>,
        timed_out: bool,
        process_id: Option<String>,
    ) -> Option<CompletedValidation> {
        self.complete_validation_from_launch(
            launch, artifact, started_at, exit_code, timed_out, process_id,
        )
        .await
    }

    async fn complete_validation_from_launch(
        &self,
        launch: &ValidationLaunchPlan,
        artifact: Option<RawOutputArtifact>,
        started_at: Instant,
        exit_code: Option<i32>,
        timed_out: bool,
        process_id: Option<String>,
    ) -> Option<CompletedValidation> {
        let (Some(route), Some(call_id)) = (
            launch.structured_route.clone(),
            launch.validation_call_id.clone(),
        ) else {
            return None;
        };
        self.complete_validation(
            route,
            call_id,
            launch.bound_plan_step.clone(),
            launch.bound_work_unit.clone(),
            artifact,
            started_at,
            exit_code,
            timed_out,
            process_id,
            launch.turn_timing_state.clone(),
            launch.focused_validation_token.clone(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn complete_validation(
        &self,
        route: ValidationRoute,
        call_id: String,
        bound_plan_step: Option<(String, u64)>,
        bound_work_unit: Option<(String, u64)>,
        artifact: Option<RawOutputArtifact>,
        started_at: Instant,
        exit_code: Option<i32>,
        timed_out: bool,
        process_id: Option<String>,
        turn_timing_state: Option<std::sync::Arc<crate::turn_timing::TurnTimingState>>,
        focused_validation_token: Option<crate::agent::task_coordinator::FocusedValidationToken>,
    ) -> Option<CompletedValidation> {
        let [leaf] = route.leaves.as_slice() else {
            return None;
        };
        let (raw_artifact_ref, raw_artifact_sha256, retained_output) = match artifact.as_ref() {
            Some(artifact) => artifact
                .validation_integrity_with_output()
                .await
                .map_or((None, None, None), |(reference, sha256, output)| {
                    (Some(reference), Some(sha256), Some(output))
                }),
            None => (None, None, None),
        };
        let selected_test_evidence = retained_output.as_deref().map_or_else(
            || {
                if validation_route_is_test(&route) {
                    SelectedTestEvidence::Missing
                } else {
                    SelectedTestEvidence::NotATest
                }
            },
            |output| selected_test_evidence(&route, output),
        );
        let selected_test_count = match selected_test_evidence {
            SelectedTestEvidence::Exact(count) => Some(count),
            SelectedTestEvidence::NotATest
            | SelectedTestEvidence::Missing
            | SelectedTestEvidence::Ambiguous => None,
        };
        let missing_selected_tests = exit_code == Some(0)
            && !timed_out
            && validation_route_is_test(&route)
            && !matches!(selected_test_evidence, SelectedTestEvidence::Exact(1..));
        let succeeded = exit_code == Some(0) && !timed_out && !missing_selected_tests;
        let result = ValidationResult {
            argv: leaf.argv.clone(),
            covered_paths: leaf.covered_paths.clone(),
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
                "validation did not prove a nonzero selected-test count".to_string()
            } else {
                match (timed_out, exit_code, selected_test_count) {
                    (true, _, _) => "validation timed out".to_string(),
                    (false, Some(0), Some(count)) => {
                        format!("validation succeeded with {count} selected tests")
                    }
                    (false, Some(0), None) => "validation succeeded".to_string(),
                    (false, Some(exit_code), _) => {
                        format!("validation exited with code {exit_code}")
                    }
                    (false, None, _) => {
                        "validation terminated without an exit code".to_string()
                    }
                }
            }),
            failure_excerpt: (!succeeded).then(|| {
                if missing_selected_tests {
                    "test validation exited successfully without a positive selected-test count; exact output is retained in the immutable artifact"
                        .to_string()
                } else {
                    match (timed_out, exit_code) {
                        (true, _) => "validation timed out".to_string(),
                        (false, Some(exit_code)) => {
                            format!("validation exited with code {exit_code}")
                        }
                        (false, None) => {
                            "validation terminated without an exit code".to_string()
                        }
                    }
                }
            }),
            raw_artifact_ref,
            raw_artifact_sha256,
        };
        let duration_ms = result.duration_ms;
        if let Some(turn_timing_state) = turn_timing_state {
            turn_timing_state.record_executed_validation(duration_ms);
        }
        tracing::info!(
            disposition = "executed",
            validation_call_id = %call_id,
            duration_ms,
            succeeded,
            "validation process completed"
        );
        Some(CompletedValidation {
            result,
            bound_plan_step,
            bound_work_unit,
            focused_validation_token,
        })
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
    if running.validation_launch.is_some() {
        return;
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidationTestRunner {
    Cargo,
    Nextest,
    Wrapped,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedTestEvidence {
    NotATest,
    Exact(u64),
    Missing,
    Ambiguous,
}

fn validation_test_runner(route: &ValidationRoute) -> Option<ValidationTestRunner> {
    let leaf = route.leaves.as_slice().first()?;
    let [program, arguments @ ..] = leaf.argv.as_slice() else {
        return None;
    };
    let program = normalized_executable_name(program);
    match (program.as_str(), arguments) {
        ("cargo", [subcommand, ..]) if subcommand == "test" => Some(ValidationTestRunner::Cargo),
        ("cargo", [plugin, subcommand, ..]) if plugin == "nextest" && subcommand == "run" => {
            Some(ValidationTestRunner::Nextest)
        }
        ("cargo-nextest", [subcommand, ..]) if subcommand == "run" => {
            Some(ValidationTestRunner::Nextest)
        }
        ("just", [recipe, ..])
            if matches!(
                recipe.as_str(),
                "test-fast" | "test-lane" | "test-lane-fast" | "test-lane-package"
            ) =>
        {
            Some(ValidationTestRunner::Wrapped)
        }
        _ if leaf.argv.iter().any(|argument| test_intent_token(argument)) => {
            Some(ValidationTestRunner::Unverified)
        }
        _ => None,
    }
}

fn validation_route_is_test(route: &ValidationRoute) -> bool {
    validation_test_runner(route).is_some()
}

fn normalized_executable_name(program: &str) -> String {
    let file_name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let normalized = file_name.to_ascii_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_string()
}

fn test_intent_token(argument: &str) -> bool {
    let normalized = normalized_executable_name(argument);
    matches!(
        normalized.as_str(),
        "test" | "tests" | "pytest" | "unittest" | "nextest"
    ) || normalized.starts_with("test-")
        || normalized.ends_with("-test")
        || normalized.contains("-test-")
        || normalized.starts_with("--test")
}

fn selected_test_evidence(route: &ValidationRoute, output: &[u8]) -> SelectedTestEvidence {
    let Some(runner) = validation_test_runner(route) else {
        return SelectedTestEvidence::NotATest;
    };
    let cargo = cargo_selected_test_count(output);
    let nextest = nextest_selected_test_count(output);
    match runner {
        ValidationTestRunner::Cargo => {
            cargo.map_or(SelectedTestEvidence::Missing, SelectedTestEvidence::Exact)
        }
        ValidationTestRunner::Nextest => {
            nextest.map_or(SelectedTestEvidence::Missing, SelectedTestEvidence::Exact)
        }
        ValidationTestRunner::Wrapped => match (cargo, nextest) {
            (Some(count), None) | (None, Some(count)) => SelectedTestEvidence::Exact(count),
            (Some(_), Some(_)) => SelectedTestEvidence::Ambiguous,
            (None, None) => SelectedTestEvidence::Missing,
        },
        ValidationTestRunner::Unverified => SelectedTestEvidence::Missing,
    }
}

fn cargo_selected_test_count(output: &[u8]) -> Option<u64> {
    let output = String::from_utf8_lossy(output);
    let running = output
        .lines()
        .filter_map(parse_cargo_running_line)
        .collect::<Vec<_>>();
    let results = output
        .lines()
        .filter_map(parse_cargo_result_line)
        .collect::<Vec<_>>();
    (!running.is_empty()
        && running.len() == results.len()
        && running
            .iter()
            .zip(&results)
            .all(|(left, right)| left == right))
    .then(|| running.into_iter().sum())
}

fn parse_cargo_running_line(line: &str) -> Option<u64> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        ["running", count, "test" | "tests"] => count.parse().ok(),
        _ => None,
    }
}

fn parse_cargo_result_line(line: &str) -> Option<u64> {
    let summary = line.trim().strip_prefix("test result: ")?;
    let (status, fields) = summary.split_once(". ")?;
    if !matches!(status, "ok" | "FAILED") {
        return None;
    }
    let mut passed = None;
    let mut failed = None;
    let mut ignored = 0;
    let mut measured = 0;
    for field in fields.split(';') {
        let mut words = field.split_whitespace();
        let Some(count) = words.next().and_then(|count| count.parse::<u64>().ok()) else {
            continue;
        };
        match words.next()? {
            "passed" => passed = Some(count),
            "failed" => failed = Some(count),
            "ignored" => ignored = count,
            "measured" => measured = count,
            _ => {}
        }
    }
    Some(passed? + failed? + ignored + measured)
}

fn nextest_selected_test_count(output: &[u8]) -> Option<u64> {
    let output = String::from_utf8_lossy(output);
    let summaries = output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("Summary [") {
                return None;
            }
            let words = line.split_whitespace().collect::<Vec<_>>();
            words.windows(3).find_map(|window| {
                (matches!(window[1], "test" | "tests") && window[2] == "run:")
                    .then(|| window[0].parse::<u64>().ok())
                    .flatten()
            })
        })
        .collect::<Vec<_>>();
    match summaries.as_slice() {
        [count] => Some(*count),
        _ => None,
    }
}

async fn write_cache_document(cache_path: PathBuf, bytes: Vec<u8>) -> io::Result<()> {
    let _commit =
        tokio::task::spawn_blocking(move || write_cache_document_blocking(&cache_path, &bytes))
            .await
            .map_err(|error| io::Error::other(format!("command cache task failed: {error}")))??;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DurableCacheCommit;

fn write_cache_document_blocking(
    cache_path: &Path,
    bytes: &[u8],
) -> io::Result<DurableCacheCommit> {
    let Some(parent) = cache_path.parent() else {
        return Err(io::Error::other("command cache path has no parent"));
    };
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(cache_path).map_err(|error| error.error)?;
    sync_cache_parent(parent)?;
    Ok(DurableCacheCommit)
}

fn sync_cache_parent(parent: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)?
        .sync_all()
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

    fn initialize_git_repository(path: &Path) {
        std::fs::create_dir_all(path).expect("create repository");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .expect("launch git init");
        assert!(status.success(), "git init failed");
    }

    fn validation_launch() -> ValidationLaunchPlan {
        ValidationLaunchPlan {
            classification: crate::validation_admission::classify_validation(
                &crate::tools::handlers::command_shape::CommandInvocation::Argv {
                    program: "cargo".to_string(),
                    args: vec!["test".to_string()],
                },
            ),
            authorization_revision: 1,
            explicitly_tagged: false,
            structured_route: None,
            bound_plan_step: None,
            bound_work_unit: None,
            validation_call_id: None,
            turn_timing_state: None,
            focused_validation_token: None,
        }
    }

    fn receipt_validation_launch(call_id: &str) -> ValidationLaunchPlan {
        ValidationLaunchPlan {
            structured_route: Some(focused_cargo_route()),
            bound_plan_step: Some(("implementation-step".to_string(), 7)),
            validation_call_id: Some(call_id.to_string()),
            ..validation_launch()
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
                covered_paths: vec!["core/src/tools/command_execution.rs".to_string()],
                timeout_ms: 30_000,
            }],
            ordering: Default::default(),
        }
    }
    #[tokio::test]
    async fn zero_test_validation_fails_without_losing_launch_binding() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            temp.path(),
            "bound-plan-step-test",
            b"running 0 tests\ntest result: ok. 0 passed; 0 failed\n",
        )
        .await;

        let completed = ledger
            .complete_validation(
                focused_cargo_route(),
                "bound-plan-step-call".to_string(),
                Some(("implementation-step".to_string(), 7)),
                None,
                Some(artifact),
                Instant::now(),
                Some(0),
                false,
                None,
                None,
                None,
            )
            .await
            .expect("completed validation result");
        assert_eq!(completed.result.status, ValidationTerminalStatus::Failed);
        assert_eq!(
            completed.result.summary.as_deref(),
            Some("validation did not prove a nonzero selected-test count")
        );
        assert_eq!(
            completed.result.failure_excerpt.as_deref(),
            Some(
                "test validation exited successfully without a positive selected-test count; exact output is retained in the immutable artifact"
            )
        );
        assert_eq!(
            completed.bound_plan_step,
            Some(("implementation-step".to_string(), 7))
        );
        assert_eq!(completed.bound_work_unit, None);
    }

    #[tokio::test]
    async fn validation_result_uses_exit_status_without_parsing_output() {
        let temp = tempfile::tempdir().expect("artifact directory");
        let ledger = CommandExecutionLedger::default();
        let misleading_artifact =
            crate::tools::command_output_artifact::create_raw_output_artifact(
                temp.path(),
                "exit-status-oracle-test",
                b"running 1 test\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured\n",
            )
            .await;

        let succeeded = ledger
            .complete_validation(
                focused_cargo_route(),
                "exit-zero-call".to_string(),
                None,
                None,
                Some(misleading_artifact),
                Instant::now(),
                Some(0),
                false,
                None,
                None,
                None,
            )
            .await
            .expect("exit-zero result");
        let failed = ledger
            .complete_validation(
                focused_cargo_route(),
                "nonzero-call".to_string(),
                None,
                None,
                None,
                Instant::now(),
                Some(7),
                false,
                None,
                None,
                None,
            )
            .await
            .expect("nonzero result");
        let timed_out = ledger
            .complete_validation(
                focused_cargo_route(),
                "timeout-call".to_string(),
                None,
                None,
                None,
                Instant::now(),
                Some(0),
                true,
                None,
                None,
                None,
            )
            .await
            .expect("timeout result");

        assert_eq!(succeeded.result.status, ValidationTerminalStatus::Succeeded);
        assert_eq!(failed.result.status, ValidationTerminalStatus::Failed);
        assert_eq!(timed_out.result.status, ValidationTerminalStatus::Failed);
    }

    #[tokio::test]
    async fn token_efficiency_broad_rg_is_admitted_with_or_without_narrow_miss() {
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

        ledger
            .admit_search_narrowing(&broad)
            .await
            .expect("a justified broad search is advisory, not rejected");
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
            .expect("admission remains advisory after narrow-miss state expires");
    }

    #[tokio::test]
    async fn token_efficiency_different_query_miss_does_not_block_broad_rg() {
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
            .expect("broad search guidance is not a runtime rejection gate");
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
            .await
            .expect("track running process");
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
            .await
            .expect("track first running process");
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
            .await
            .expect("track second running process");
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
    async fn duplicate_running_process_id_is_rejected_without_replacing_owner() {
        let ledger = CommandExecutionLedger::default();
        let first_id = ledger.allocate_execution_id();
        let first_parent = ToolExecutionId("tool-execution-first".to_string());
        let first_key = key("first");
        ledger
            .track_running_process_with_execution_id(
                first_id,
                first_parent.clone(),
                42,
                first_key.clone(),
                RawOutputArtifact::unavailable("first"),
                None,
                Instant::now(),
            )
            .await
            .expect("track first running process");

        let second_id = ledger.allocate_execution_id();
        let error = ledger
            .track_running_process_with_execution_id(
                second_id,
                ToolExecutionId("tool-execution-second".to_string()),
                42,
                key("second"),
                RawOutputArtifact::unavailable("second"),
                None,
                Instant::now(),
            )
            .await
            .expect_err("a live process id must not be reassigned");

        assert!(error.contains("process id 42"));
        let running = ledger
            .running_process(42)
            .await
            .expect("first owner remains");
        assert_eq!(running.execution_id, first_id);
        assert_eq!(running.parent_tool_execution_id, first_parent);
        assert_eq!(running.key, first_key);
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
            .await
            .expect("track running process");
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
            .await
            .expect("track running process");

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
                .await
                .expect("track running process");
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
    async fn tracked_validation_watcher_completion_records_once_without_retry_state() {
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
                Some(receipt_validation_launch("watcher-validation-call")),
                Instant::now() - Duration::from_millis(25),
            )
            .await
            .expect("track validation process");

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
        let completed = ledger
            .complete_running_validation(42, false)
            .await
            .expect("watcher validation result");
        assert_eq!(completed.result.call_id, "watcher-validation-call");
        assert_eq!(completed.result.process_id.as_deref(), Some("42"));
        assert_eq!(completed.result.status, ValidationTerminalStatus::Failed);
        assert!(
            ledger
                .complete_running_validation(42, false)
                .await
                .is_none()
        );
        assert!(ledger.finish_running_process(42, Some(7)).await.accepted());

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.last_exit_code, None);
        assert_eq!(snapshot.deterministic_failure, None);
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("validation results do not enter the generic retry cache");
    }

    #[tokio::test]
    async fn tracked_validation_handler_completion_does_not_enter_retry_state() {
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
            .await
            .expect("track validation process");
        ledger
            .update_running_artifact(43, finalized_artifact.clone())
            .await;

        assert!(ledger.finish_running_process(43, Some(9)).await.accepted());
        assert!(!ledger.finish_running_process(43, Some(9)).await.accepted());

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 0);
        assert_eq!(snapshot.last_exit_code, None);
        assert_eq!(snapshot.deterministic_failure, None);
        ledger
            .begin_attempt(&command_key, false)
            .await
            .expect("handler-completed validation does not enter the generic retry cache");
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
            .await
            .expect("track validation process");

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
    fn persisted_command_fingerprint_is_versioned_and_stable() {
        assert_eq!(
            fingerprint_value("fixed"),
            "v2:5728fd88252f0fa0389c2564eb565e93671ef99ac9678ed4c24f0c8343786617"
        );
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
        assert!(
            base.command
                .iter()
                .all(|argument| !argument.starts_with('\0'))
        );
        assert_eq!(base.execution_context.len(), 5);
        let replaced_runtime = base.clone().with_runtime_context(&"unified");
        assert_eq!(replaced_runtime.execution_context.len(), 5);
        assert_ne!(base.fingerprint(), replaced_runtime.fingerprint());

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

    #[test]
    fn audit189_retry_identity_covers_every_semantic_dimension() {
        let environment = HashMap::from([("LANG".to_string(), "en-US".to_string())]);
        let search = RgSearchNarrowing {
            breadth: RgSearchBreadth::Narrow,
            query_identity: "needle".to_string(),
            search_identity: "needle:src".to_string(),
            scope_identity: "src".to_string(),
            parent_scope_identity: Some("repo".to_string()),
            can_record_miss: true,
        };
        let base = CommandAttemptKey::new(
            "exec_command",
            "local",
            "C:/repo",
            &["rg needle src".to_string()],
        )
        .with_executed_command(&["rg".to_string(), "needle".to_string(), "src".to_string()])
        .with_environment(&environment)
        .with_timeout_ms(Some(1_000))
        .with_sandbox_context(&"workspace-write")
        .with_permission_context(&"standard")
        .with_input_context(&"closed")
        .with_runtime_context(&"unified")
        .with_repository_epoch(7)
        .with_workspace_identity(Some("workspace-a"))
        .with_search_narrowing("turn-a", "repo-a", Some(search.clone()));

        let mut changed_tool = base.clone();
        changed_tool.tool_name = "shell_command".to_string();
        let mut changed_environment_id = base.clone();
        changed_environment_id.environment_id = "sandbox-a".to_string();
        let mut changed_cwd = base.clone();
        changed_cwd.cwd = "C:/repo/subdir".to_string();
        let variants = [
            ("tool", changed_tool),
            ("environment_id", changed_environment_id),
            ("cwd", changed_cwd),
            (
                "executed_command",
                base.clone().with_executed_command(&[
                    "rg".to_string(),
                    "other".to_string(),
                    "src".to_string(),
                ]),
            ),
            (
                "environment",
                base.clone()
                    .with_environment(&HashMap::from([("LANG".to_string(), "de-DE".to_string())])),
            ),
            ("timeout", base.clone().with_timeout_ms(Some(2_000))),
            ("sandbox", base.clone().with_sandbox_context(&"read-only")),
            (
                "permission",
                base.clone().with_permission_context(&"elevated"),
            ),
            ("input", base.clone().with_input_context(&"interactive")),
            ("runtime", base.clone().with_runtime_context(&"classic")),
            ("repository_epoch", base.clone().with_repository_epoch(8)),
            (
                "workspace_identity",
                base.clone().with_workspace_identity(Some("workspace-b")),
            ),
            (
                "search_identity",
                base.clone().with_search_narrowing(
                    "turn-a",
                    "repo-a",
                    Some(RgSearchNarrowing {
                        search_identity: "needle:tests".to_string(),
                        scope_identity: "tests".to_string(),
                        ..search
                    }),
                ),
            ),
        ];

        for (dimension, variant) in variants {
            assert_ne!(
                base.fingerprint(),
                variant.fingerprint(),
                "{dimension} must participate in retry identity"
            );
        }
    }
    #[tokio::test]
    async fn audit189_rg_classifier_drives_narrowing_ledger_for_real_commands() {
        use crate::shell::ShellType;
        use crate::tools::handlers::command_search::classify_rg_search_narrowing;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("codex-core is nested under the repository root");
        let argv = |args: &[&str]| args.iter().map(ToString::to_string).collect::<Vec<_>>();
        let narrow_command = argv(&["rg", "-n", "needle with space", "codex-rs/core/src/tools"]);
        let equivalent_command =
            argv(&["rg", "-n", "needle with space", "./codex-rs/core/src/tools"]);
        let broad_command = argv(&["rg", "-n", "needle with space", "."]);
        let classify = |command: &[String], shell| {
            classify_rg_search_narrowing(command, shell, root, root)
                .expect("rg command classifies")
                .expect("rg search is present")
        };
        let narrow_search = classify(&narrow_command, None);
        let equivalent_search = classify(&equivalent_command, None);
        assert_eq!(
            narrow_search.search_identity,
            equivalent_search.search_identity
        );
        let narrow = CommandAttemptKey::new(
            "exec_command",
            "local",
            root.to_string_lossy(),
            &narrow_command,
        )
        .with_repository_epoch(1)
        .with_workspace_identity(Some("workspace-a"))
        .with_search_narrowing("turn-a", "repo-a", Some(narrow_search));
        let equivalent = CommandAttemptKey::new(
            "exec_command",
            "local",
            root.to_string_lossy(),
            &equivalent_command,
        )
        .with_repository_epoch(2)
        .with_workspace_identity(Some("workspace-a"))
        .with_search_narrowing("turn-b", "repo-a", Some(equivalent_search.clone()));
        let changed_workspace = CommandAttemptKey::new(
            "exec_command",
            "local",
            root.to_string_lossy(),
            &equivalent_command,
        )
        .with_repository_epoch(2)
        .with_workspace_identity(Some("workspace-b"))
        .with_search_narrowing("turn-b", "repo-a", Some(equivalent_search));
        let ledger = CommandExecutionLedger::default();
        ledger.record_exit(&narrow, 1).await;
        ledger
            .begin_attempt(&equivalent, false)
            .await
            .expect_err("equivalent real rg miss is cached");
        ledger
            .begin_attempt(&changed_workspace, false)
            .await
            .expect("workspace identity mutation invalidates the real rg miss");

        let compound_command = argv(&[
            "pwsh",
            "-NoProfile",
            "-Command",
            "rg -n 'needle with space' codex-rs/core/src/tools | Measure-Object",
        ]);
        let compound_search = classify(&compound_command, Some(ShellType::PowerShell));
        assert!(!compound_search.can_record_miss);
        let compound = CommandAttemptKey::new(
            "exec_command",
            "local",
            root.to_string_lossy(),
            &compound_command,
        )
        .with_search_narrowing("turn-c", "repo-a", Some(compound_search));
        let broad = CommandAttemptKey::new(
            "exec_command",
            "local",
            root.to_string_lossy(),
            &broad_command,
        )
        .with_search_narrowing("turn-c", "repo-a", Some(classify(&broad_command, None)));
        let compound_ledger = CommandExecutionLedger::default();
        compound_ledger.record_exit(&compound, 1).await;
        compound_ledger
            .admit_search_narrowing(&broad)
            .await
            .expect("compound output does not turn advisory guidance into a rejection");
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
    async fn workspace_identity_and_persisted_search_misses_load_on_first_command() {
        let temp = tempfile::tempdir().expect("workspace identity fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let search = |turn_id: &str, repository_epoch: u64, workspace_identity: &str| {
            CommandAttemptKey::new(
                "exec_command",
                codex_exec_server::LOCAL_ENVIRONMENT_ID,
                repository.to_string_lossy(),
                &["rg".to_string(), "needle".to_string(), "src".to_string()],
            )
            .with_repository_epoch(repository_epoch)
            .with_workspace_identity(Some(workspace_identity))
            .with_search_narrowing(
                turn_id,
                "repository",
                Some(RgSearchNarrowing {
                    breadth: RgSearchBreadth::Narrow,
                    query_identity: "needle".to_string(),
                    search_identity: "needle:src".to_string(),
                    scope_identity: "src".to_string(),
                    parent_scope_identity: Some("repository".to_string()),
                    can_record_miss: true,
                }),
            )
        };

        let producer = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "thread".to_string(),
            &repository,
        )
        .await;
        let producer_epoch = producer.observe_repository_revision("turn-a", 0).await;
        let producer_identity = producer
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repository)
            .await
            .expect("first command observes the workspace");
        let first_search = search("turn-a", producer_epoch, &producer_identity);
        producer
            .begin_attempt(&first_search, false)
            .await
            .expect("first search runs");
        producer.record_exit(&first_search, 1).await;
        producer.finish_turn("turn-a").await;

        let unused = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "unused-thread".to_string(),
            &repository,
        )
        .await;
        unused.finish_turn("unused-turn").await;
        assert!(
            unused
                .state
                .lock()
                .await
                .repository
                .observed_workspace_identity
                .is_none(),
            "finishing a turn without a command must not inspect the workspace"
        );

        let matching = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "thread".to_string(),
            &repository,
        )
        .await;
        let supplied_identity =
            crate::git_workspace::capture_workspace_evidence_identity(&repository)
                .await
                .expect("matching supplied identity");
        let matching_epoch = matching
            .observe_repository_revision_with_identity("turn-match", 0, Some(supplied_identity))
            .await;
        let matching_identity = matching
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repository)
            .await
            .expect("supplied identity initializes the ledger");
        assert_eq!(matching_identity, producer_identity);
        assert!(
            matches!(
                matching
                    .begin_attempt(
                        &search("turn-match", matching_epoch, &matching_identity),
                        false,
                    )
                    .await,
                Err(CommandAttemptBlocked {
                    reason: CommandAttemptBlockedReason::SearchMiss,
                    ..
                })
            ),
            "the first supplied identity observation must load a matching persisted miss"
        );

        let consumer =
            CommandExecutionLedger::load_or_new(codex_home, "thread".to_string(), &repository)
                .await;
        {
            let state = consumer.state.lock().await;
            assert!(
                state.repository.observed_workspace_identity.is_none(),
                "ledger construction must not inspect the workspace"
            );
            assert!(state.search.misses.is_empty());
        }

        tokio::fs::write(
            repository.join("external-edit.txt"),
            b"changed before first command",
        )
        .await
        .expect("external workspace edit");
        let consumer_epoch = consumer.observe_repository_revision("turn-b", 0).await;
        let consumer_identity = consumer
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repository)
            .await
            .expect("first command observes the edited workspace");
        assert_ne!(producer_identity, consumer_identity);
        consumer
            .begin_attempt(&search("turn-b", consumer_epoch, &consumer_identity), false)
            .await
            .expect("a pre-command workspace edit invalidates the persisted search miss");
    }

    #[tokio::test]
    async fn finish_turn_persists_the_last_authoritative_workspace_identity_without_recapture() {
        let temp = tempfile::tempdir().expect("workspace identity fixture");
        let repository = temp.path().join("repo");
        let codex_home = temp.path().join("codex-home");
        initialize_git_repository(&repository);
        let mut ledger = CommandExecutionLedger::load_or_new(
            codex_home.clone(),
            "thread".to_string(),
            &repository,
        )
        .await;

        ledger.observe_repository_revision("turn-a", 0).await;
        let expected_identity = ledger
            .state
            .lock()
            .await
            .repository
            .observed_workspace_identity
            .clone()
            .expect("workspace identity was observed")
            .1;
        ledger.persistence.as_mut().expect("persistent ledger").cwd =
            repository.join("missing-after-observation");

        ledger.finish_turn("turn-a").await;

        let cache_path = codex_home
            .join("command-execution-cache")
            .join("thread.json");
        let document: CommandExecutionCacheDocument = serde_json::from_slice(
            &tokio::fs::read(cache_path)
                .await
                .expect("finish persists without another workspace capture"),
        )
        .expect("valid command cache document");
        assert_eq!(document.workspace_identity, expected_identity);
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

        let identity_hash = workspace_identity_hash(&identity);
        let state = ledger.state.lock().await;
        assert_eq!(
            state.repository.observed_workspace_identity,
            Some((1, identity))
        );
        assert_eq!(
            state.repository.observed_workspace_identity_hash,
            Some((1, identity_hash))
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
            .await
            .expect("track running process");

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
                .await
                .expect("track running process");
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
            .await
            .expect("track replacement process");

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
