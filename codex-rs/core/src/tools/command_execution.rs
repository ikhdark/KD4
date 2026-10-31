use codex_protocol::protocol::ToolExecutionId;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;

use crate::digest_writer::DigestWriter;
use crate::tools::command_output_artifact::RawOutputArtifact;

const MAX_TRACKED_COMMANDS: usize = 128;
const COMMAND_EXECUTION_CACHE_SCHEMA_VERSION: u32 = 6;
const COMMAND_FINGERPRINT_VERSION: &str = "v3";
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
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct CommandAttemptKey {
    tool_name: String,
    environment_id: String,
    cwd: String,
    command: Vec<String>,
    execution_context: BTreeMap<String, String>,
    search_no_match_eligible: bool,
}

impl CommandAttemptKey {
    pub(crate) fn local_cwd(&self) -> Option<PathBuf> {
        if self.environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID {
            return None;
        }
        codex_utils_path_uri::PathUri::parse(&self.cwd)
            .ok()?
            .to_abs_path()
            .ok()
            .map(|path| path.to_path_buf())
    }

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
            search_no_match_eligible: false,
        }
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

    pub(crate) fn with_search_no_match_eligibility(mut self, eligible: bool) -> Self {
        self.search_no_match_eligible = eligible;
        self
    }

    pub(crate) fn with_search_environment(mut self, environment: &HashMap<String, String>) -> Self {
        if environment.iter().any(|(key, value)| {
            !value.is_empty()
                && ["RIPGREP_CONFIG_PATH", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"]
                    .iter().any(|name| key.eq_ignore_ascii_case(name))
        }) {
            self.search_no_match_eligible = false;
        }
        self
    }

    pub(crate) fn is_search_no_match(&self, exit_code: Option<i32>) -> bool {
        exit_code == Some(1) && self.search_no_match_eligible
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
    let mut hasher = Sha256::new();
    hasher.update(b"kd4-command-fingerprint\0");
    hasher.update(COMMAND_FINGERPRINT_VERSION.as_bytes());
    hasher.update(b"\0");
    if let Err(error) = serde_json::to_writer(DigestWriter(&mut hasher), value) {
        tracing::warn!(%error, "command fingerprint unavailable; disabling equivalent-attempt reuse");
        // A unique value fails open for execution without equating two
        // authorization contexts that could not be serialized.
        return format!(
            "{COMMAND_FINGERPRINT_VERSION}:unavailable:{}",
            uuid::Uuid::new_v4()
        );
    }
    format!(
        "{}:{}",
        COMMAND_FINGERPRINT_VERSION,
        hex::encode(hasher.finalize())
    )
}

fn workspace_identity_hash(identity: &crate::git_workspace::WorkspaceEvidenceIdentity) -> String {
    let bytes = match serde_json::to_vec(identity) {
        Ok(bytes) => bytes,
        Err(error) => unreachable!("workspace evidence identity must serialize: {error}"),
    };
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandAttemptBlocked {
    pub(crate) fingerprint: String,
    reason: CommandAttemptBlockedReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CommandAttemptBlockedReason {
    DeterministicFailure(DeterministicFailureRecord),
    PersistedInputFailure(PersistedInputFailure),
}

impl CommandAttemptBlocked {

    pub(crate) fn render_for_model(&self) -> String {
        match &self.reason {
            CommandAttemptBlockedReason::DeterministicFailure(prior_failure) => {
                let (artifact_id, _, _) = prior_failure.evidence.model_projection();
                let evidence = artifact_id.map_or_else(
                    || "original output artifact unavailable".to_string(),
                    |id| format!("read_tool_output artifact `{id}`"),
                );
                format!(
                    "Command failed: exact repeat of deterministic `{}` failure from the original attempt (fingerprint `{}`, exit code {}, {evidence}); execution was suppressed. Correct the invocation before retrying. For current filesystem evidence, use read_file or list_files when available; recovering the original artifact does not refresh it.",
                    prior_failure.proof.outcome_class(),
                    self.fingerprint,
                    prior_failure.exit_code,
                )
            }
            CommandAttemptBlockedReason::PersistedInputFailure(failure) => format!(
                "Command suppressed: this exact input and execution context previously produced an input-determined `{}` rejection (fingerprint `{}`, exit code {}). This is a persisted failure receipt, not a fresh execution or filesystem observation. Correct the invocation before retrying.",
                failure.proof.outcome_class(),
                self.fingerprint,
                failure.exit_code,
            ),
        }
    }
}

/// Closed proof classes whose outcome is determined before a process starts or
/// mutable filesystem state is inspected. Keep this enum closed: ordinary
/// command failures and filesystem-dependent patch verification must remain
/// retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Only closed, input-determined rejection classes survive restart. Arbitrary
/// command failures and their possibly expired artifact handles never do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedInputFailure {
    proof: InputStateDetermined,
    exit_code: i32,
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
    pub(crate) incarnation: uuid::Uuid,
    pub(crate) parent_tool_execution_id: ToolExecutionId,
    pub(crate) key: CommandAttemptKey,
    pub(crate) artifact: RawOutputArtifact,
    completed_exit_code: Option<i32>,
    exit_observed: bool,
    terminal_facts: Option<CommandTerminalFacts>,
}

#[derive(Debug, Clone)]
struct CommandTerminalFacts {
    process_exited: bool,
    output_drained: bool,
    success: Option<bool>,
    search_no_match: bool,
    failure_cause: Option<String>,
    sampling_signal: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct PendingCommandCompletion {
    process_id: u32,
    command: RunningCommand,
}

#[derive(Debug, Clone)]
struct CommandCompletionReceipt {
    parent_tool_execution_id: ToolExecutionId,
    process_id: u32,
    incarnation: uuid::Uuid,
    exit_code: Option<i32>,
    exit_observed: bool,
    local_cwd: Option<PathBuf>,
    artifact: RawOutputArtifact,
    terminal_facts: Option<CommandTerminalFacts>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    input_failures: BTreeMap<String, PersistedInputFailure>,
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
    workspace_identity_observation_turn: Option<String>,
    observed_workspace_identity: Option<(u64, crate::git_workspace::WorkspaceEvidenceIdentity)>,
    observed_workspace_identity_hash: Option<(u64, String)>,
    observed_turn_mutation_revisions: HashMap<String, u64>,

}


#[derive(Default)]
struct CommandRetryState {
    persisted_cache_loaded: bool,
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
    input_failures: BTreeMap<String, PersistedInputFailure>,
    revision: u64,
}

#[derive(Default)]
struct CommandExecutionState {
    persisted_revision: Option<(crate::git_workspace::WorkspaceEvidenceIdentity, u64)>,
    retry: CommandRetryState,
    process: CommandProcessState,
    repository: CommandRepositoryState,
}

pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    workspace_identity_refresh: tokio::sync::Semaphore,
    persistence: Option<CommandExecutionPersistence>,
    workspace_cache: Option<Arc<crate::git_workspace::GitWorkspaceCache>>,
    cache_persist: Arc<Mutex<()>>,
}


impl Default for CommandExecutionLedger {
    fn default() -> Self {
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            workspace_identity_refresh: tokio::sync::Semaphore::new(/*permits*/ 1),
            persistence: None,
            workspace_cache: None,
            cache_persist: Arc::new(Mutex::new(())),
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
        let ledger = Self {
            state: Mutex::new(CommandExecutionState::default()),
            workspace_identity_refresh: tokio::sync::Semaphore::new(/*permits*/ 1),
            persistence: Some(persistence),
            workspace_cache: Some(crate::git_workspace::GitWorkspaceCache::new()),
            cache_persist: Arc::new(Mutex::new(())),
        };
        ledger.refresh_workspace_identity(None, 0, None).await;
        ledger
    }

    pub(crate) fn with_workspace_cache(
        mut self,
        cache: Arc<crate::git_workspace::GitWorkspaceCache>,
    ) -> Self {
        self.workspace_cache = Some(cache);
        self
    }


    /// Reuse a snapshot captured under the dispatch workspace lease. The ledger
    /// belongs to its original local cwd; another workspace must not seed it.
    pub(crate) async fn observe_workspace_baseline(
        &self,
        turn_id: &str,
        mutation_revision: u64,
        cwd: &Path,
        identity: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) {
        let Some(identity) = identity.filter(|identity| !identity.unavailable) else { return; };
        let Some(persistence) = self.persistence.as_ref() else { return; };
        if self.state.lock().await.repository.observed_turn_mutation_revisions
            .get(turn_id).is_some_and(|previous| *previous >= mutation_revision)
        {
            return;
        }
        let (session_cwd, command_cwd) = (persistence.cwd.clone(), cwd.to_path_buf());
        self.invalidate_repository_revision(turn_id, mutation_revision).await;
        let epoch = self.state.lock().await.repository.epoch;
        let roots = tokio::task::spawn_blocking(move || {
            let root = |cwd: &Path| codex_git_utils::get_git_repo_root(cwd)
                .and_then(|root| dunce::canonicalize(root).ok());
            (root(&session_cwd), root(&command_cwd))
        }).await.ok();
        if roots.is_some_and(|(session, command)| session.is_some()
            && session == command
            && command.as_ref().and_then(|root| root.to_str()) == identity.repository_root.as_deref()) {
            self.refresh_workspace_identity(Some((turn_id, mutation_revision)), epoch, Some(identity))
                .await;
        }
    }


    /// Invalidate cached command observations without acquiring a workspace
    /// snapshot. The next command admission owns any required fresh capture.
    pub(crate) async fn invalidate_repository_revision(&self, turn_id: &str, mutation_revision: u64) {
        let mut state = self.state.lock().await;
        let previous = state.repository.observed_turn_mutation_revisions
            .entry(turn_id.to_owned()).or_default();
        let delta = mutation_revision.saturating_sub(*previous);
        *previous = (*previous).max(mutation_revision);
        if delta == 0 { return; }
        state.repository.epoch = state.repository.epoch.saturating_add(delta);
        state.repository.observed_workspace_identity = None;
        state.repository.observed_workspace_identity_hash = None;
        state.repository.workspace_identity_observation_epoch = None;
        state.repository.workspace_identity_observation_turn = None;
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
        self.invalidate_repository_revision(turn_id, mutation_revision).await;
        let epoch = self.state.lock().await.repository.epoch;
        self.refresh_workspace_identity(Some((turn_id, mutation_revision)), epoch, observed_workspace_identity).await
    }

    async fn refresh_workspace_identity(
        &self,
        turn: Option<(&str, u64)>,
        epoch: u64,
        observed: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> u64 {
        // The semaphore is never closed; if it were, skip this refresh.
        let Ok(_refresh) = self.workspace_identity_refresh.acquire().await else {
            return epoch;
        };
        {
            let state = self.state.lock().await;
            if state.repository.epoch != epoch || turn.is_some_and(|(id, revision)| {
                state.repository.observed_turn_mutation_revisions.get(id) != Some(&revision)
            }) {
                return state.repository.epoch;
            }
            if observed.is_none()
                && state.repository.workspace_identity_observation_epoch == Some(epoch)
                && state.retry.persisted_cache_loaded
            {
                return epoch;
            }
        }
        let identity = match observed.filter(|identity| !identity.unavailable) {
            Some(identity) => Some(identity),
            None => match (self.workspace_cache.as_ref(), self.persistence.as_ref()) {
                (Some(cache), Some(persistence)) => cache.command_receipt_workspace_identity(&persistence.cwd).await,
                _ => None,
            },
        };
        let Some(identity) = identity else { return epoch; };
        let identity_hash = workspace_identity_hash(&identity);
        // Refresh -> serialized cache owner -> state. Filesystem work never
        // holds state, and a retired turn cannot publish a late capture.
        let _commit = Arc::clone(&self.cache_persist).lock_owned().await;
        let load = !self.state.lock().await.retry.persisted_cache_loaded;
        let cached = if load {
            if let Some(persistence) = self.persistence.clone() {
                tokio::task::spawn_blocking(move || read_cache_document(&persistence.cache_path))
                    .await.ok().flatten()
            } else { None }
        } else { None };
        let mut state = self.state.lock().await;
        if state.repository.epoch != epoch || turn.is_some_and(|(id, revision)| {
            state.repository.observed_turn_mutation_revisions.get(id) != Some(&revision)
        }) {
            return state.repository.epoch;
        }
        if load {
            state.retry.persisted_cache_loaded = true;
            if let Some(document) = cached.filter(|document| document.workspace_identity == identity) {
                for (fingerprint, failure) in document.input_failures {
                    state.retry.input_failures.entry(fingerprint).or_insert(failure);
                }
                while state.retry.input_failures.len() > MAX_TRACKED_COMMANDS {
                    state.retry.input_failures.pop_first();
                }
            }
        }
        state.repository.observed_workspace_identity = Some((epoch, identity));
        state.repository.observed_workspace_identity_hash = Some((epoch, identity_hash));
        state.repository.workspace_identity_observation_epoch = Some(epoch);
        state.repository.workspace_identity_observation_turn = turn.map(|(id, _)| id.to_string());
        epoch
    }

    pub(crate) async fn current_workspace_identity_hash(
        &self,
        environment_id: &str,
        cwd: &Path,
    ) -> Option<String> {
        if environment_id != codex_exec_server::LOCAL_ENVIRONMENT_ID {
            return None;
        }
        let persisted_cwd = &self.persistence.as_ref()?.cwd;
        let command_root = if persisted_cwd != cwd {
            {
                let state = self.state.lock().await;
                let (epoch, _) = state.repository.observed_workspace_identity_hash.as_ref()?;
                if *epoch != state.repository.epoch
                    || state.repository.observed_workspace_identity.is_none()
                {
                    return None;
                }
            }
            let cwd = cwd.to_path_buf();
            Some(
                tokio::task::spawn_blocking(move || {
                    codex_git_utils::get_git_repo_root(&cwd)
                        .and_then(|root| dunce::canonicalize(root).ok())
                })
                .await
                .ok()??,
            )
        } else {
            None
        };
        let state = self.state.lock().await;
        if let Some(root) = command_root {
            let (_, identity) = state.repository.observed_workspace_identity.as_ref()?;
            if identity.repository_root.as_deref() != root.to_str() {
                return None;
            }
        }
        let (epoch, identity_hash) = state.repository.observed_workspace_identity_hash.as_ref()?;
        if *epoch != state.repository.epoch {
            return None;
        }
        Some(identity_hash.clone())
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
            let fingerprint = key.fingerprint();
            if let Some(failure) = state.retry.input_failures.get(&fingerprint) {
                return Err(CommandAttemptBlocked {
                    fingerprint,
                    reason: CommandAttemptBlockedReason::PersistedInputFailure(failure.clone()),
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
        record_exit_locked(&mut state, key, exit_code);
    }

    /// Enrich the existing identity even when an exit-only watcher got there
    /// first. A later failure must never be overwritten by a weaker success.
    pub(crate) async fn record_terminal_facts(
        &self,
        process_id: u32,
        parent: &str,
        incarnation: uuid::Uuid,
        exit_code: Option<i32>,
        failure_cause: Option<&str>,
        search_no_match: bool,
        process_exited: bool,
        output_drained: bool,
    ) {
        let mut facts = CommandTerminalFacts {
            process_exited,
            output_drained,
            success: (process_exited && (exit_code.is_some() || failure_cause.is_some()))
                .then_some(failure_cause.is_none() && (exit_code == Some(0) || search_no_match)),
            search_no_match,
            failure_cause: failure_cause.map(|cause| cause.chars().take(2048).collect()),
            sampling_signal: None,
        };
        let mut state = self.state.lock().await;
        let process = &mut state.process;
        let slot = if let Some(running) = process.running.get_mut(&process_id).filter(|command| {
            command.parent_tool_execution_id.0 == parent && command.incarnation == incarnation
        }) {
            Some(&mut running.terminal_facts)
        } else if let Some(pending) = process
            .pending_by_execution_id
            .values_mut()
            .find(|pending| {
                pending.process_id == process_id
                    && pending.command.parent_tool_execution_id.0 == parent
                    && pending.command.incarnation == incarnation
            })
        {
            Some(&mut pending.command.terminal_facts)
        } else {
            process
                .completion_receipts
                .values_mut()
                .find(|receipt| {
                    receipt.process_id == process_id
                        && receipt.parent_tool_execution_id.0 == parent
                        && receipt.incarnation == incarnation
                })
                .map(|receipt| &mut receipt.terminal_facts)
        };
        if let Some(slot) = slot {
            if let Some(previous) = slot.as_ref() {
                facts.process_exited |= previous.process_exited;
                facts.output_drained |= previous.output_drained;
                facts.sampling_signal = previous.sampling_signal.clone();
                // Exit-only and duplicate observers must not erase a known
                // late failure merely because they have no diagnostic of their own.
                facts.failure_cause = facts
                    .failure_cause
                    .or_else(|| previous.failure_cause.clone());
                facts.success = if previous.success == Some(false) {
                    Some(false)
                } else {
                    facts.success.or(previous.success)
                };
                facts.search_no_match |= previous.search_no_match;
                facts.search_no_match &= facts.success == Some(true);
            }
            *slot = Some(facts);
        }
    }

    pub(crate) async fn record_input_state_determined_failure(
        &self,
        key: &CommandAttemptKey,
        proof: InputStateDetermined,
        evidence: RawOutputArtifact,
        exit_code: i32,
    ) {
        {
            let mut state = self.state.lock().await;
            let entry = attempt_entry_locked(&mut state, key);
            entry.last_exit_code = Some(exit_code);
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.deterministic_failure = Some(
                DeterministicFailureRecord::from_input_state_determined(proof, evidence, exit_code),
            );
            state.retry.input_failures.insert(
                key.fingerprint(),
                PersistedInputFailure { proof, exit_code },
            );
            while state.retry.input_failures.len() > MAX_TRACKED_COMMANDS {
                state.retry.input_failures.pop_first();
            }
            state.retry.revision = state.retry.revision.wrapping_add(1);
        }
        self.persist_cache().await;
    }


    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn track_running_process_with_execution_id(
        &self,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: ToolExecutionId,
        process_id: u32,
        key: CommandAttemptKey,
        artifact: RawOutputArtifact,
        incarnation: uuid::Uuid,
    ) -> Result<(), String> {
        let mut state = self.state.lock().await;
        debug_assert_command_execution_invariants(&state);
        if state.process.running.contains_key(&process_id) {
            tracing::error!(process_id, "refusing to replace live command bookkeeping");
            return Err(format!(
                "process id {process_id} already has live command bookkeeping"
            ));
        }
        // A reused transport ID must not recover an earlier execution's result.
        state
            .process
            .completion_receipts
            .retain(|_, receipt| receipt.process_id != process_id);
        let retained = state
            .process
            .completion_receipts
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        state
            .process
            .completion_receipt_order
            .retain(|id| retained.contains(id));
        state.process.running.insert(
            process_id,
            RunningCommand {
                execution_id,
                incarnation,
                parent_tool_execution_id,
                key,
                artifact,
                completed_exit_code: None,
                exit_observed: false,
                terminal_facts: None,
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

    /// Diagnostic context survives watcher completion, but never an incarnation
    /// change or eviction of the existing bounded completion receipt.
    pub(crate) async fn process_local_cwd(
        &self,
        process_id: u32,
        incarnation: uuid::Uuid,
    ) -> Option<PathBuf> {
        let state = self.state.lock().await;
        if let Some(running) = state.process.running.get(&process_id) {
            return (running.incarnation == incarnation).then(|| running.key.local_cwd()).flatten();
        }
        if let Some(pending) = state.process.pending_by_execution_id.values()
            .find(|pending| pending.process_id == process_id)
        {
            return (pending.command.incarnation == incarnation)
                .then(|| pending.command.key.local_cwd()).flatten();
        }
        state.process.completion_receipt_order.iter().rev()
            .filter_map(|id| state.process.completion_receipts.get(id))
            .find(|receipt| receipt.process_id == process_id && receipt.incarnation == incarnation)
            .and_then(|receipt| receipt.local_cwd.clone())
    }

    /// Candidate handles only, never evidence that results or consumed inputs
    /// are interchangeable. The process owner must still confirm live identity.
    pub(crate) async fn matching_running_commands(
        &self,
        key: &CommandAttemptKey,
    ) -> Vec<(u32, uuid::Uuid)> {
        let Some(environment) = key.execution_context.get("environment") else {
            return Vec::new();
        };
        let state = self.state.lock().await;
        state.process.running.iter().filter_map(|(id, running)| {
            let other = &running.key;
            (key.environment_id == other.environment_id
                && key.cwd == other.cwd
                && key.command == other.command
                && other.execution_context.get("environment") == Some(environment))
                .then_some((*id, running.incarnation))
        }).collect()
    }

    pub(crate) async fn identical_running_process(
        &self,
        _key: &CommandAttemptKey,
    ) -> Option<(u32, CommandExecutionId)> {
        // Git-visible endpoint equality proves neither complete command inputs
        // (ignored/external files) nor what a live process consumed during an
        // A->B->A edit. No immutable/consumed-input witness is available here.
        // Keep existing handles alive, but never suppress a requested new run.
        None
    }

    pub(crate) async fn completed_process_result(
        &self,
        process_id: u32,
        incarnation: uuid::Uuid,
    ) -> Option<serde_json::Value> {
        let state = self.state.lock().await;
        if state.process.running.get(&process_id).is_some_and(|command| command.incarnation == incarnation) {
            return None;
        }
        let (execution_id, parent, exit_code, exit_observed, artifact, facts) = if let Some(pending) = state
            .process
            .pending_by_execution_id
            .values()
            .find(|pending| pending.process_id == process_id && pending.command.incarnation == incarnation)
        {
            (
                pending.command.execution_id,
                &pending.command.parent_tool_execution_id,
                pending.command.completed_exit_code,
                pending.command.exit_observed,
                &pending.command.artifact,
                pending.command.terminal_facts.as_ref(),
            )
        } else {
            let receipt = state
                .process
                .completion_receipt_order
                .iter()
                .rev()
                .filter_map(|id| state.process.completion_receipts.get(id).map(|receipt| (*id, receipt)))
                .find(|(_, receipt)| receipt.process_id == process_id && receipt.incarnation == incarnation)?;
            let (execution_id, receipt) = receipt;
            (
                execution_id,
                &receipt.parent_tool_execution_id,
                receipt.exit_code,
                receipt.exit_observed,
                &receipt.artifact,
                receipt.terminal_facts.as_ref(),
            )
        };
        let process_exited = exit_observed || facts.is_some_and(|facts| facts.process_exited);
        let (artifact_id, bytes, error) = artifact.model_projection();
        let mut result = serde_json::json!({
            "output_reference": RawOutputArtifact::output_reference(Some(artifact)),
            "polled_session_id": process_id, "incarnation": incarnation,
            "completion_provenance": {
                "source": "command_execution_ledger",
                "execution_id": execution_id.0,
                "parent_tool_execution_id": parent,
            },
            "execution_state": if process_exited { "exited" } else { "unknown" },
            "process_exited": process_exited,
            "output_drained": facts.map(|facts| facts.output_drained),
            "exit_code": exit_code,
            "output": "", "output_complete": false, "output_reduced": true,
            "raw_output_artifact_retention_limit_hit": artifact.retention_limit_hit(),
            "completion_recovered": true,
            "success": facts.and_then(|facts| facts.success),
            "search_no_match": facts.is_some_and(|facts| facts.search_no_match),
            "error": facts.and_then(|facts| facts.failure_cause.as_deref()).or_else(|| {
                facts.is_none().then_some("Terminal outcome details were not retained; do not infer successful execution from the exit code or replay the command.")
            }),
            "terminal_facts_available": facts.is_some(),
            "retained_artifact_complete": RawOutputArtifact::output_reference(Some(artifact))["capture_complete"],
            "notice": "Retained process receipt, not a live control handle. Retirement alone does not establish exit or complete output capture. Output is not replayed; use the retained output reference. Do not restart the command.",
        });
        if let Some(signal) = facts.and_then(|facts| facts.sampling_signal.as_ref()) {
            result["_sampling_request_signal"] = signal.clone();
        }
        if let Some(id) = artifact_id {
            result["raw_output_artifact_id"] = id.to_string().into();
        }
        if let Some(bytes) = bytes {
            result["raw_output_artifact_bytes"] = bytes.into();
        }
        if let Some(error) = error {
            result["raw_output_artifact_error"] = error.into();
        }
        if let (Some(id), Some(bytes)) = (artifact_id, bytes) {
            result["recovery_selector"] =
                serde_json::json!({"kind":"bytes", "start":0, "end":bytes.min(4096)});
            result["recovery"] = serde_json::json!({"tool":"read_tool_output", "arguments": {
                "artifact_id":id.to_string(), "selectors":[{"kind":"bytes", "start":0, "end":bytes.min(4096)}]
            }});
        }
        Some(result)
    }

    pub(crate) async fn retain_terminal_validation_signal(
        &self, process_id: u32, parent: &str, incarnation: uuid::Uuid,
        signal: serde_json::Value,
    ) {
        let mut state = self.state.lock().await;
        let process = &mut state.process;
        let slot = if let Some(command) = process.running.get_mut(&process_id)
            .filter(|command| command.incarnation == incarnation && command.parent_tool_execution_id.0 == parent) {
            command.terminal_facts.as_mut()
        } else if let Some(pending) = process.pending_by_execution_id.values_mut().find(|pending|
            pending.process_id == process_id && pending.command.incarnation == incarnation
                && pending.command.parent_tool_execution_id.0 == parent) {
            pending.command.terminal_facts.as_mut()
        } else {
            process.completion_receipts.values_mut().find(|receipt|
                receipt.process_id == process_id && receipt.incarnation == incarnation
                    && receipt.parent_tool_execution_id.0 == parent)
                .and_then(|receipt| receipt.terminal_facts.as_mut())
        };
        if let Some(facts) = slot { facts.sampling_signal = Some(signal); }
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
        self.finish_turn_before_terminal(turn_id).await;
        self.persist_cache_after_terminal().await;
    }

    pub(crate) async fn finish_turn_before_terminal(&self, turn_id: &str) {
        self.forget_turn_repository_revision(turn_id).await;
    }

    pub(crate) async fn persist_cache_after_terminal(&self) {
        self.persist_cache().await;
    }


    async fn persist_cache(&self) {
        let Some(persistence) = self.persistence.clone() else {
            return;
        };
        let commit_guard = Arc::clone(&self.cache_persist).lock_owned().await;
        let Some((
            workspace_identity,
            retry_revision,
            input_failures,
        )) = ({
            let state = self.state.lock().await;
            state
                .repository
                .observed_workspace_identity
                .as_ref()
                .filter(|(epoch, _)| *epoch == state.repository.epoch)
                .filter(|(_, identity)| {
                    state.persisted_revision.as_ref().is_none_or(
                        |(persisted, retry_revision)| {
                            persisted != identity
                                || *retry_revision != state.retry.revision
                        },
                    )
                })
                .map(|(_, workspace_identity)| {
                    (
                        workspace_identity.clone(),
                        state.retry.revision,
                        state.retry.input_failures.clone(),
                    )
                })
        })
        else {
            return;
        };
        let document = CommandExecutionCacheDocument {
            schema_version: COMMAND_EXECUTION_CACHE_SCHEMA_VERSION,
            workspace_identity: workspace_identity.clone(),
            input_failures,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&document) else {
            return;
        };
        match write_cache_document(persistence.cache_path, bytes, commit_guard).await {
            Ok(_commit_guard) => {
                self.state.lock().await.persisted_revision =
                    Some((workspace_identity, retry_revision));
            }
            Err(error) => tracing::warn!(%error, "failed to persist command execution cache"),
        }
    }

    async fn forget_turn_repository_revision(&self, turn_id: &str) {
        let mut state = self.state.lock().await;
        state
            .repository
            .observed_turn_mutation_revisions
            .remove(turn_id);

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
                } else if let Some(receipt) = state
                    .process
                    .completion_receipts
                    .values_mut()
                    .find(|receipt| receipt.process_id == process_id)
                {
                    receipt.artifact = artifact;
                }
            }
            debug_assert_command_execution_invariants(&state);
        }
    }


    pub(crate) async fn mark_process_exited(
        &self,
        process_id: u32,
        execution_id: CommandExecutionId,
        parent_tool_execution_id: &ToolExecutionId,
        exit_code: impl Into<Option<i32>>,
    ) -> CompletionApplyResult {
        let exit_code = exit_code.into();
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
            running.completed_exit_code = exit_code;
            running.exit_observed = true;
            if let Some(exit_code) = exit_code {
                record_running_exit_locked(&mut state, &running, exit_code);
            }
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
            pending.process_id,
            pending.command,
        );
        while state.retry.attempts.len() > MAX_TRACKED_COMMANDS
            && evict_oldest_inactive_attempt_locked(&mut state)
        {}
        debug_assert_command_execution_invariants(&state);
        CompletionApplyResult::Applied
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
                running.exit_observed = true;
                record_running_exit_locked(&mut state, &running, exit_code);
            }
            insert_completion_receipt(&mut state, execution_id, process_id, running);
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
                pending.process_id,
                pending.command,
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



}

fn insert_completion_receipt(
    state: &mut CommandExecutionState,
    execution_id: CommandExecutionId,
    process_id: u32,
    command: RunningCommand,
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
            local_cwd: command.key.local_cwd(),
            parent_tool_execution_id: command.parent_tool_execution_id,
            process_id,
            incarnation: command.incarnation,
            exit_code: command.completed_exit_code,
            exit_observed: command.exit_observed,
            artifact: command.artifact,
            terminal_facts: command.terminal_facts,
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
    record_exit_locked(state, &running.key, exit_code);
}

fn record_exit_locked(state: &mut CommandExecutionState, key: &CommandAttemptKey, exit_code: i32) {
    // A fresh execution supersedes a cached diagnosis, even if it failed for a
    // different reason. Never promote an ordinary failure into semantic memory.
    if state
        .retry
        .input_failures
        .remove(&key.fingerprint())
        .is_some()
    {
        state.retry.revision = state.retry.revision.wrapping_add(1);
    }
    let entry = attempt_entry_locked(state, key);
    entry.last_exit_code = Some(exit_code);
    entry.deterministic_failure = None;
    if exit_code == 0 {
        entry.consecutive_failures = 0;
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

async fn write_cache_document(
    cache_path: PathBuf,
    bytes: Vec<u8>,
    commit_guard: tokio::sync::OwnedMutexGuard<()>,
) -> io::Result<tokio::sync::OwnedMutexGuard<()>> {
    tokio::task::spawn_blocking(move || {
        // Cancellation must not let a later commit overtake a running disk write.
        write_cache_document_blocking(&cache_path, &bytes).map(|_| commit_guard)
    })
    .await
    .map_err(|error| io::Error::other(format!("command cache task failed: {error}")))?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DurableCacheCommit;

fn read_cache_document(cache_path: &Path) -> Option<CommandExecutionCacheDocument> {
    if !cache_path.is_file() { return None; }
    let parent = cache_path.parent()?;
    let _residency = if parent.file_name().is_some_and(|name| name == "command-execution-cache") {
        let home = parent.parent()?;
        let id = cache_path.file_stem()?.to_str()?;
        let owner = crate::tools::command_output_artifact::artifact_residency_file(home, id, false).ok()?;
        if home.join("tool-history").join(format!("{id}.retired")).try_exists().ok()? {
            return None;
        }
        Some(owner)
    } else { None };
    let document: CommandExecutionCacheDocument = serde_json::from_slice(&std::fs::read(cache_path).ok()?).ok()?;
    if document.schema_version != COMMAND_EXECUTION_CACHE_SCHEMA_VERSION
        || document.workspace_identity.unavailable
        || document.input_failures.len() > MAX_TRACKED_COMMANDS
        || document.input_failures.iter().any(|(key, failure)| {
            failure.exit_code == 0 || key.strip_prefix(&format!("{COMMAND_FINGERPRINT_VERSION}:"))
                .is_none_or(|digest| digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        })
    {
        return None;
    }
    Some(document)
}

fn write_cache_document_blocking(
    cache_path: &Path,
    bytes: &[u8],
) -> io::Result<DurableCacheCommit> {
    let Some(parent) = cache_path.parent() else {
        return Err(io::Error::other("command cache path has no parent"));
    };
    std::fs::create_dir_all(parent)?;
    let _residency = if parent
        .file_name()
        .is_some_and(|name| name == "command-execution-cache")
    {
        let home = parent
            .parent()
            .ok_or_else(|| io::Error::other("command cache has no home"))?;
        let id = cache_path
            .file_stem()
            .and_then(|id| id.to_str())
            .ok_or_else(|| io::Error::other("command cache has no thread identity"))?;
        let owner =
            crate::tools::command_output_artifact::artifact_residency_file(home, id, false)?;
        if home
            .join("tool-history")
            .join(format!("{id}.retired"))
            .try_exists()?
        {
            return Err(io::Error::other("thread storage was permanently retired"));
        }
        Some(owner)
    } else {
        None
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    persist_synced_file(temporary, cache_path, parent)?;
    Ok(DurableCacheCommit)
}

#[cfg(windows)]
pub(crate) fn persist_synced_file(
    temporary: tempfile::NamedTempFile,
    cache_path: &Path,
    _parent: &Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let temporary = temporary.into_temp_path();
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = cache_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // The contents were flushed above. Commit the same-directory rename with
    // write-through; flushing a read-only directory handle fails on Windows.
    // SAFETY: both paths are live NUL-terminated strings for this call.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn persist_synced_file(
    temporary: tempfile::NamedTempFile,
    cache_path: &Path,
    parent: &Path,
) -> io::Result<()> {
    temporary.persist(cache_path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()
}




