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

    #[cfg(test)]
    pub(crate) fn with_executed_command(mut self, command: &[String]) -> Self {
        self.command = command.to_vec();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_environment(self, environment: &HashMap<String, String>) -> Self {
        self.with_environment_fingerprint(&crate::tools::handlers::validation_environment_hash(
            environment,
        ))
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
    terminal_facts: Option<CommandTerminalFacts>,
}

#[derive(Debug, Clone)]
struct CommandTerminalFacts {
    success: bool,
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
    #[cfg(test)]
    cache_commit_count: AtomicU64,
    #[cfg(test)]
    workspace_identity_capture_count: AtomicU64,
    #[cfg(test)]
    cache_persist_test_gate: std::sync::Mutex<Option<CachePersistTestGate>>,
}

#[cfg(test)]
struct CachePersistTestGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

impl Default for CommandExecutionLedger {
    fn default() -> Self {
        Self {
            state: Mutex::new(CommandExecutionState::default()),
            workspace_identity_refresh: tokio::sync::Semaphore::new(/*permits*/ 1),
            persistence: None,
            workspace_cache: None,
            cache_persist: Arc::new(Mutex::new(())),
            #[cfg(test)]
            cache_commit_count: AtomicU64::new(0),
            #[cfg(test)]
            workspace_identity_capture_count: AtomicU64::new(0),
            #[cfg(test)]
            cache_persist_test_gate: std::sync::Mutex::new(None),
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
            workspace_cache: None,
            cache_persist: Arc::new(Mutex::new(())),
            #[cfg(test)]
            cache_commit_count: AtomicU64::new(0),
            #[cfg(test)]
            workspace_identity_capture_count: AtomicU64::new(0),
            #[cfg(test)]
            cache_persist_test_gate: std::sync::Mutex::new(None),
        }
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
        let (session_cwd, command_cwd) = (persistence.cwd.clone(), cwd.to_path_buf());
        let roots = tokio::task::spawn_blocking(move || {
            let root = |cwd: &Path| codex_git_utils::get_git_repo_root(cwd)
                .and_then(|root| dunce::canonicalize(root).ok());
            (root(&session_cwd), root(&command_cwd))
        }).await.ok();
        if roots.is_some_and(|(session, command)| session.is_some()
            && session == command
            && command.as_ref().and_then(|root| root.to_str()) == identity.repository_root.as_deref()) {
            self.observe_repository_revision_with_identity(turn_id, mutation_revision, Some(identity))
                .await;
        }
    }

    #[cfg(test)]
    pub(crate) fn workspace_identity_capture_count(&self) -> u64 {
        self.workspace_identity_capture_count
            .load(Ordering::Relaxed)
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
        _observed_workspace_identity: Option<crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> u64 {
        self.invalidate_repository_revision(turn_id, mutation_revision).await;
        self.state.lock().await.repository.epoch
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
    ) {
        let mut facts = CommandTerminalFacts {
            success: failure_cause.is_none() && (exit_code == Some(0) || search_no_match),
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
                facts.sampling_signal = previous.sampling_signal.clone();
                // Exit-only and duplicate observers must not erase a known
                // late failure merely because they have no diagnostic of their own.
                facts.failure_cause = facts
                    .failure_cause
                    .or_else(|| previous.failure_cause.clone());
                facts.success &= previous.success;
                facts.search_no_match &= facts.success;
            }
            if slot
                .as_ref()
                .is_none_or(|previous| previous.success || !facts.success)
            {
                *slot = Some(facts);
            }
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
            uuid::Uuid::new_v4(),
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
        if state.process.running.contains_key(&process_id) {
            return None;
        }
        let (exit_code, artifact, facts) = if let Some(pending) = state
            .process
            .pending_by_execution_id
            .values()
            .find(|pending| pending.process_id == process_id)
        {
            if pending.command.incarnation != incarnation {
                return None;
            }
            (
                pending.command.completed_exit_code,
                &pending.command.artifact,
                pending.command.terminal_facts.as_ref(),
            )
        } else {
            let receipt = state
                .process
                .completion_receipt_order
                .iter()
                .rev()
                .filter_map(|id| state.process.completion_receipts.get(id))
                .find(|receipt| receipt.process_id == process_id)?;
            if receipt.incarnation != incarnation {
                return None;
            }
            (
                receipt.exit_code,
                &receipt.artifact,
                receipt.terminal_facts.as_ref(),
            )
        };
        let (artifact_id, bytes, error) = artifact.model_projection();
        let mut result = serde_json::json!({
            "execution_state": "exited", "process_exited": true,
            "exit_code": exit_code,
            "output": "", "output_complete": false, "output_reduced": true,
            "raw_output_artifact_retention_limit_hit": artifact.retention_limit_hit(),
            "completion_recovered": true,
            "success": facts.is_some_and(|facts| facts.success),
            "search_no_match": facts.is_some_and(|facts| facts.search_no_match),
            "error": facts.and_then(|facts| facts.failure_cause.as_deref()).or_else(|| {
                facts.is_none().then_some("Terminal outcome details were not retained; do not infer successful execution from the exit code or replay the command.")
            }),
            "terminal_facts_available": facts.is_some(),
            "notice": "Process already completed and was cleaned up. Output is not replayed; recover the retained artifact when needed. Do not restart the command.",
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

    #[cfg(test)]
    pub(crate) fn block_next_cache_persist_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let previous = self
            .cache_persist_test_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(CachePersistTestGate {
                entered: entered_tx,
                release: release_rx,
            });
        assert!(
            previous.is_none(),
            "cache persistence test gate already installed"
        );
        (entered_rx, release_tx)
    }

    async fn persist_cache(&self) {
        #[cfg(test)]
        let test_gate = self
            .cache_persist_test_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        #[cfg(test)]
        if let Some(gate) = test_gate {
            let _ = gate.entered.send(());
            let _ = gate.release.await;
        }
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
                #[cfg(test)]
                self.cache_commit_count.fetch_add(1, Ordering::Relaxed);
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

    #[cfg(test)]
    pub(crate) async fn started_attempt_count(&self) -> u64 {
        self.state
            .lock()
            .await
            .retry
            .attempts
            .values()
            .map(|entry| u64::from(entry.attempts))
            .sum()
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
            parent_tool_execution_id: command.parent_tool_execution_id,
            process_id,
            incarnation: command.incarnation,
            exit_code: command.completed_exit_code,
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

#[cfg(test)]
#[path = "command_execution_outcome_tests.rs"]
mod outcome_tests;

#[cfg(test)]
#[path = "command_execution_recovery_identity_tests.rs"]
mod recovery_identity_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn key(command: &str) -> CommandAttemptKey {
        let cwd = if cfg!(windows) { "C:/repo" } else { "/repo" };
        CommandAttemptKey::new("exec_command", "local", cwd, &[command.to_string()])
    }

    #[tokio::test]
    async fn git_identity_alone_never_suppresses_a_requested_validation() {
        let ledger = CommandExecutionLedger::default();
        let original = key("cargo test -p app").with_workspace_identity(Some("revision-a"));
        ledger
            .track_running_process(
                42,
                original.clone(),
                RawOutputArtifact::unavailable("fixture"),
            )
            .await
            .unwrap();
        assert_eq!(
            ledger
                .identical_running_process(&original)
                .await
                .map(|(id, _)| id),
            None
        );
        assert!(
            ledger
                .identical_running_process(&key("cargo test -p app"))
                .await
                .is_none()
        );
        assert!(
            ledger
                .identical_running_process(
                    &key("cargo test -p app").with_workspace_identity(Some("revision-b"))
                )
                .await
                .is_none()
        );
        assert!(
            ledger
                .identical_running_process(
                    &original.clone().with_environment_fingerprint("changed")
                )
                .await
                .is_none()
        );
        assert!(ledger.finish_running_process(42, Some(0)).await.accepted());
        assert!(ledger.identical_running_process(&original).await.is_none());
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
                uuid::Uuid::new_v4(),
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
                uuid::Uuid::new_v4(),
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
                uuid::Uuid::new_v4(),
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
                uuid::Uuid::new_v4(),
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
    async fn input_state_determined_failure_blocks_exact_retry_but_freshness_bypasses() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        let home = temp.path().join("home");
        initialize_git_repository(&repository);
        let ledger =
            CommandExecutionLedger::load_or_new(home.clone(), "thread".into(), &repository).await;
        ledger.observe_repository_revision("turn-a", 0).await;
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

        let blocked = ledger
            .begin_attempt(&attempt_key, false)
            .await
            .expect_err("the closed production proof blocks an exact retry");
        let message = blocked.render_for_model();
        assert!(message.contains("original output artifact unavailable"));
        assert!(!message.contains("force_fresh"));
        assert!(message.contains("Correct the invocation"));
        assert!(message.contains("read_file or list_files"));
        assert!(!message.contains("Failed {"));
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

        let resumed =
            CommandExecutionLedger::load_or_new(home.clone(), "thread".into(), &repository).await;
        resumed.observe_repository_revision("turn-b", 0).await;
        let blocked = resumed
            .begin_attempt(&attempt_key, false)
            .await
            .expect_err("input-determined failure survives restart");
        assert!(
            blocked
                .render_for_model()
                .contains("persisted failure receipt")
        );
        resumed
            .begin_attempt(
                &attempt_key
                    .clone()
                    .with_environment_fingerprint("changed-environment"),
                false,
            )
            .await
            .expect("a different execution environment is not suppressed");
        resumed
            .begin_attempt_with_freshness(&attempt_key, false, true)
            .await
            .unwrap();
        resumed.record_exit(&attempt_key, 0).await;
        resumed.persist_cache_after_terminal().await;
        let after_success =
            CommandExecutionLedger::load_or_new(home, "thread".into(), &repository).await;
        after_success.observe_repository_revision("turn-c", 0).await;
        after_success
            .begin_attempt(&attempt_key, false)
            .await
            .expect("fresh execution supersedes the persisted diagnosis");
    }

    #[tokio::test]
    async fn suppressed_failure_exposes_only_the_opaque_evidence_handle() {
        let home = tempfile::TempDir::new().unwrap();
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            home.path(),
            "thread",
            b"Use apply_patch directly.",
        )
        .await;
        let artifact_id = artifact.model_projection().0.expect("stored artifact");
        let ledger = CommandExecutionLedger::default();
        let attempt_key = key("apply_patch patch");
        ledger.begin_attempt(&attempt_key, false).await.unwrap();
        ledger
            .record_input_state_determined_failure(
                &attempt_key,
                InputStateDetermined::ApplyPatchImplicitInvocation,
                artifact,
                -1,
            )
            .await;
        let blocked = ledger.begin_attempt(&attempt_key, false).await.unwrap_err();
        let message = blocked.render_for_model();
        assert!(message.contains(&format!("read_tool_output artifact `{artifact_id}`")));
        assert!(!message.contains("force_fresh"));
        assert!(message.contains("does not refresh it"));
        assert!(!message.contains(home.path().to_string_lossy().as_ref()));
        assert!(!message.contains("Stored {"));
    }

    #[tokio::test]
    async fn fresh_nonzero_exit_supersedes_an_input_determined_failure() {
        for background in [false, true] {
            let ledger = CommandExecutionLedger::default();
            let attempt = key("ordinary-command");
            ledger.begin_attempt(&attempt, false).await.unwrap();
            ledger
                .record_input_state_determined_failure(
                    &attempt,
                    InputStateDetermined::ApplyPatchImplicitInvocation,
                    RawOutputArtifact::unavailable("prior closed proof"),
                    -1,
                )
                .await;
            assert!(ledger.begin_attempt(&attempt, false).await.is_err());
            ledger
                .begin_attempt_with_freshness(&attempt, false, true)
                .await
                .unwrap();
            if background {
                ledger
                    .track_running_process(
                        42,
                        attempt.clone(),
                        RawOutputArtifact::unavailable("fresh execution"),
                    )
                    .await
                    .unwrap();
                assert!(ledger.finish_running_process(42, Some(7)).await.accepted());
            } else {
                ledger.record_exit(&attempt, 7).await;
            }
            // An observed ordinary failure is not the old pre-execution rejection.
            // The closed proof contract requires ordinary command failures to stay retryable.
            ledger
                .begin_attempt(&attempt, false)
                .await
                .expect("fresh ordinary failure supersedes the old diagnosis");
            let state = ledger.state.lock().await;
            assert!(
                !state
                    .retry
                    .input_failures
                    .contains_key(&attempt.fingerprint())
            );
            assert_eq!(state.retry.attempts[&attempt].last_exit_code, Some(7));
        }
    }

    #[tokio::test]
    async fn ordinary_nonzero_exit_codes_remain_retryable() {
        for exit_code in [-1, 1, 2, 124, 130, 137] {
            let ledger = CommandExecutionLedger::default();
            let key = key("ordinary-command");
            ledger.begin_attempt(&key, false).await.expect("first run");
            ledger.record_exit(&key, exit_code).await;
            ledger.begin_attempt(&key, false).await.expect("retry runs");
            ledger.record_exit(&key, exit_code).await;
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

        assert_eq!(
            ledger.mark_running_process_completed(42, 7).await,
            CompletionApplyResult::Applied
        );
        assert_eq!(
            ledger.mark_running_process_completed(42, 7).await,
            CompletionApplyResult::AlreadyApplied
        );
        assert!(ledger.running_process(42).await.is_none());
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
        assert!(
            ledger
                .completed_process_result(41, uuid::Uuid::new_v4())
                .await
                .is_none()
        );
        let recovered = ledger
            .completed_process_result(41, completed.incarnation)
            .await
            .expect("terminal receipt");
        assert_eq!(recovered["exit_code"], 7);
        assert_eq!(recovered["process_exited"], true);
        assert_eq!(recovered["output_complete"], false);
        assert!(
            ledger
                .completed_process_result(42, completed.incarnation)
                .await
                .is_none()
        );
        assert!(
            ledger
                .completed_process_result(99, completed.incarnation)
                .await
                .is_none()
        );
        assert_eq!(ledger.consecutive_failures(&completed_key).await, 1);
        assert_eq!(ledger.consecutive_failures(&live_key).await, 0);
        ledger
            .track_running_process(41, completed_key, RawOutputArtifact::unavailable("reused"))
            .await
            .unwrap();
        assert!(
            ledger
                .completed_process_result(41, completed.incarnation)
                .await
                .is_none()
        );
    }

    #[test]
    fn persisted_command_fingerprint_is_versioned_and_stable() {
        assert_eq!(
            fingerprint_value("fixed"),
            "v3:462675e7a390d1809f834632b7777ab89a039626355c114186b8f18f1550bc4e"
        );
    }

    #[test]
    fn unserializable_execution_contexts_do_not_collide_or_panic() {
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("fixture serialization failure"))
            }
        }
        let first = key("git status").with_permission_context(&Unserializable);
        let second = key("git status").with_permission_context(&Unserializable);
        assert_ne!(first.fingerprint(), second.fingerprint());
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
                .with_environment_fingerprint(&crate::tools::handlers::validation_environment_hash(
                    &environment,
                ))
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
        .with_search_no_match_eligibility(true);

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
            ("search_exit_classification", base.clone().with_search_no_match_eligibility(false)),
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
    async fn repository_epoch_is_session_scoped_across_turns() {
        let ledger = CommandExecutionLedger::default();

        assert_eq!(ledger.observe_repository_revision("turn-1", 0).await, 0);
        assert_eq!(ledger.observe_repository_revision("turn-1", 1).await, 1);
        assert_eq!(ledger.observe_repository_revision("turn-2", 0).await, 1);
        assert_eq!(ledger.observe_repository_revision("turn-2", 2).await, 3);
        assert_eq!(ledger.observe_repository_revision("turn-1", 1).await, 3);
    }

    #[tokio::test]
    async fn workspace_hash_follows_repository_identity_across_subdirectories() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        initialize_git_repository(&repository);
        let subdirectory = repository.join("crates/core");
        std::fs::create_dir_all(&subdirectory).unwrap();
        let ledger = CommandExecutionLedger::load_or_new(
            temp.path().join("home"),
            "thread".into(),
            &repository,
        )
        .await;
        ledger.observe_repository_revision("turn", 0).await;
        let root_hash = ledger
            .current_workspace_identity_hash(codex_exec_server::LOCAL_ENVIRONMENT_ID, &repository)
            .await
            .expect("observed repository");
        assert_eq!(
            ledger
                .current_workspace_identity_hash(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    &subdirectory,
                )
                .await,
            Some(root_hash)
        );
        let nested_repository = subdirectory.join("nested");
        initialize_git_repository(&nested_repository);
        assert_eq!(
            ledger
                .current_workspace_identity_hash(
                    codex_exec_server::LOCAL_ENVIRONMENT_ID,
                    &nested_repository,
                )
                .await,
            None,
            "a nested repository has independent evidence"
        );
        assert_eq!(
            ledger
                .current_workspace_identity_hash("remote", &subdirectory)
                .await,
            None
        );
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
    async fn repository_refresh_uses_shared_workspace_cache_after_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        initialize_git_repository(&repository);
        let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
        let ledger = CommandExecutionLedger::load_or_new(
            temp.path().join("home"),
            "shared-cache".into(),
            &repository,
        )
        .await
        .with_workspace_cache(Arc::clone(&cache));
        ledger.observe_repository_revision("turn", 0).await;
        let first = ledger
            .state
            .lock()
            .await
            .repository
            .observed_workspace_identity
            .clone();
        let captures = cache.workspace_evidence_capture_count();
        tokio::fs::write(repository.join("changed.txt"), b"changed")
            .await
            .unwrap();
        cache.note_host_workspace_mutation();
        ledger.observe_repository_revision("turn", 1).await;
        assert_eq!(cache.workspace_evidence_capture_count(), captures + 1);
        let second = ledger
            .state
            .lock()
            .await
            .repository
            .observed_workspace_identity
            .clone();
        assert!(first.is_some());
        assert!(second.is_some());
        assert_ne!(first.unwrap().1, second.unwrap().1);
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
        let keys = (0..=MAX_TRACKED_COMMANDS)
            .map(|index| key(&format!("background-{index}.exe")))
            .collect::<Vec<_>>();

        for (process_id, key) in keys.iter().take(MAX_TRACKED_COMMANDS).enumerate() {
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
                MAX_TRACKED_COMMANDS as u32,
                replacement_key.clone(),
                RawOutputArtifact::unavailable("replacement fixture"),
            )
            .await
            .expect("track replacement process");

        assert!(ledger.running_process(0).await.is_some());
        assert_eq!(ledger.consecutive_failures(&keys[0]).await, 0);
        assert!(ledger.mark_running_process_completed(0, 0).await.accepted());
        assert_eq!(ledger.consecutive_failures(&keys[0]).await, 0);
        assert!(
            ledger
                .running_process(MAX_TRACKED_COMMANDS as u32)
                .await
                .is_some()
        );
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

#[cfg(test)]
mod audit63_cache_retirement_tests {
    use super::*;

    #[test]
    fn permanently_deleted_thread_rejects_a_late_cache_commit() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("command-execution-cache/thread.json");
        assert!(write_cache_document_blocking(&path, b"original cache").is_ok());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(home.path().join("tool-history")).unwrap();
        std::fs::write(home.path().join("tool-history/thread.retired"), b"retired").unwrap();
        assert!(write_cache_document_blocking(&path, b"late detached commit").is_err());
        assert!(!path.exists());
    }
}
