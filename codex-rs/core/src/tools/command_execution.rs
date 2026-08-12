use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::WorkspaceMutationLease;
use codex_agent_task_store::WorkspaceMutationRequest;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::validation_admission::ValidationLaunchPlan;

const MAX_TRACKED_COMMANDS: usize = 128;
const WORKSPACE_MUTATION_RETRY_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(250);
const WORKSPACE_MUTATION_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CommandAttemptKey {
    tool_name: String,
    environment_id: String,
    cwd: String,
    command: Vec<String>,
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
    pub(crate) prior_failure: DeterministicFailureRecord,
}

impl CommandAttemptBlocked {
    pub(crate) fn render_for_model(&self) -> String {
        format!(
            "Command failed: exact repeat of deterministic `{}` failure from the original attempt (fingerprint `{}`, exit code {}, evidence {:?}); execution was suppressed.",
            self.prior_failure.outcome_class,
            self.fingerprint,
            self.prior_failure.exit_code,
            self.prior_failure.evidence,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeterministicFailureRecord {
    pub(crate) outcome_class: String,
    pub(crate) evidence: RawOutputArtifact,
    pub(crate) exit_code: i32,
    pub(crate) execution_started_at: SystemTime,
    pub(crate) execution_ended_at: SystemTime,
    pub(crate) execution_duration: Duration,
    pub(crate) termination_drain_duration: Option<Duration>,
}

impl DeterministicFailureRecord {
    pub(crate) fn from_trusted_classification(
        outcome_class: impl Into<String>,
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
            outcome_class: outcome_class.into(),
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
}

#[derive(Debug, Clone)]
pub(crate) struct RunningCommand {
    pub(crate) key: CommandAttemptKey,
    pub(crate) artifact: RawOutputArtifact,
    owner_turn_id: String,
    completed_exit_code: Option<i32>,
    workspace_mutation: Option<RunningWorkspaceMutation>,
    validation_launch: Option<ValidationLaunchPlan>,
    started_at: Instant,
}

#[derive(Clone)]
pub(crate) struct RunningWorkspaceMutation {
    inner: Arc<RunningWorkspaceMutationInner>,
}

pub(crate) struct WorkspaceMutationReservation {
    _guard: OwnedMutexGuard<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceMutationReservationAcquireError {
    Cancelled,
    TimedOut,
}

pub(crate) struct WorkspaceMutationGuard {
    store: Arc<dyn AgentTaskStore>,
    repo_root: PathBuf,
    lease: WorkspaceMutationLease,
    finalized: bool,
    _reservation: WorkspaceMutationReservation,
}

impl WorkspaceMutationGuard {
    pub(crate) fn new(
        store: Arc<dyn AgentTaskStore>,
        repo_root: PathBuf,
        lease: WorkspaceMutationLease,
        reservation: WorkspaceMutationReservation,
    ) -> Self {
        Self {
            store,
            repo_root,
            lease,
            finalized: false,
            _reservation: reservation,
        }
    }

    pub(crate) fn store(&self) -> Arc<dyn AgentTaskStore> {
        Arc::clone(&self.store)
    }

    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub(crate) fn lease(&self) -> &WorkspaceMutationLease {
        &self.lease
    }

    pub(crate) async fn finish(mut self) -> Result<(), StoreError> {
        self.store
            .finish_workspace_mutation(&self.repo_root, self.lease.clone())
            .await?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for WorkspaceMutationGuard {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let lease = self.lease.clone();
        let store = Arc::clone(&self.store);
        let repo_root = self.repo_root.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                lease_id = %lease.lease_id,
                "cannot finalize a dropped workspace mutation without a Tokio runtime"
            );
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = store.finish_workspace_mutation(&repo_root, lease).await {
                tracing::error!(
                    %error,
                    "failed to finalize a dropped workspace mutation"
                );
            }
        });
    }
}

#[derive(Debug)]
pub(crate) enum WorkspaceMutationAcquireError {
    Cancelled,
    TimedOut { details: Vec<String> },
    Store(StoreError),
}

pub(crate) async fn acquire_workspace_mutation_lease(
    store: &dyn AgentTaskStore,
    repo_root: &Path,
    request: &WorkspaceMutationRequest,
    cancellation: &CancellationToken,
) -> Result<WorkspaceMutationLease, WorkspaceMutationAcquireError> {
    acquire_workspace_mutation_lease_with_max_wait(
        store,
        repo_root,
        request,
        cancellation,
        WORKSPACE_MUTATION_MAX_WAIT,
    )
    .await
}

async fn acquire_workspace_mutation_lease_with_max_wait(
    store: &dyn AgentTaskStore,
    repo_root: &Path,
    request: &WorkspaceMutationRequest,
    cancellation: &CancellationToken,
    max_wait: std::time::Duration,
) -> Result<WorkspaceMutationLease, WorkspaceMutationAcquireError> {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if cancellation.is_cancelled() {
            return Err(WorkspaceMutationAcquireError::Cancelled);
        }

        match store
            .begin_workspace_mutation(repo_root, request.clone())
            .await
        {
            Ok(lease) => return Ok(lease),
            Err(StoreError::WorkspaceClaimConflict { details }) => {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        return Err(WorkspaceMutationAcquireError::Cancelled);
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        return Err(WorkspaceMutationAcquireError::TimedOut { details });
                    }
                    _ = tokio::time::sleep(WORKSPACE_MUTATION_RETRY_INTERVAL) => {}
                }
            }
            Err(error) => return Err(WorkspaceMutationAcquireError::Store(error)),
        }
    }
}

struct RunningWorkspaceMutationInner {
    store: Arc<dyn AgentTaskStore>,
    repo_root: PathBuf,
    lease: WorkspaceMutationLease,
    stop: CancellationToken,
    lease_lost: CancellationToken,
    finalized: Arc<Mutex<bool>>,
    finalization_complete: CancellationToken,
    reservation: Mutex<Option<WorkspaceMutationReservation>>,
    _heartbeat_task: AbortOnDropHandle<()>,
}

impl std::fmt::Debug for RunningWorkspaceMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunningWorkspaceMutation")
            .field("repo_root", &self.inner.repo_root)
            .field("lease_id", &self.inner.lease.lease_id)
            .finish()
    }
}

impl RunningWorkspaceMutation {
    pub(crate) fn new(
        store: Arc<dyn AgentTaskStore>,
        repo_root: PathBuf,
        lease: WorkspaceMutationLease,
        owner_cancelled: CancellationToken,
        reservation: WorkspaceMutationReservation,
    ) -> Self {
        let stop = CancellationToken::new();
        let heartbeat_stop = stop.clone();
        let lease_lost = CancellationToken::new();
        let heartbeat_lease_lost = lease_lost.clone();
        let heartbeat_store = store.clone();
        let heartbeat_root = repo_root.clone();
        let lease_id = lease.lease_id.clone();
        let actor_id = lease.actor_id.clone();
        let heartbeat_task = AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = heartbeat_stop.cancelled() => break,
                    _ = owner_cancelled.cancelled() => {
                        // A mutating process must not outlive the turn that owns its
                        // repository authority. The process manager observes lease_lost,
                        // terminates the process tree, and the exit watcher finalizes the
                        // lease only after termination is confirmed.
                        heartbeat_lease_lost.cancel();
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                        match heartbeat_store
                            .heartbeat_workspace_mutation(
                                &heartbeat_root,
                                lease_id.clone(),
                                actor_id.clone(),
                            )
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::warn!(
                                    %lease_id,
                                    "workspace mutation lease was lost"
                                );
                                heartbeat_lease_lost.cancel();
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    %lease_id,
                                    "unified exec workspace mutation heartbeat failed"
                                );
                                heartbeat_lease_lost.cancel();
                                break;
                            }
                        }
                    }
                }
            }
        }));
        Self {
            inner: Arc::new(RunningWorkspaceMutationInner {
                store,
                repo_root,
                lease,
                stop,
                lease_lost,
                finalized: Arc::new(Mutex::new(false)),
                finalization_complete: CancellationToken::new(),
                reservation: Mutex::new(Some(reservation)),
                _heartbeat_task: heartbeat_task,
            }),
        }
    }

    pub(crate) fn lease_lost_token(&self) -> CancellationToken {
        self.inner.lease_lost.clone()
    }

    pub(crate) fn cancel_owner(&self) {
        self.inner.stop.cancel();
        self.inner.lease_lost.cancel();
    }

    pub(crate) async fn finish(&self) -> Result<(), String> {
        let mut finalized = Arc::clone(&self.inner.finalized).lock_owned().await;
        if *finalized {
            return Ok(());
        }
        self.inner
            .store
            .finish_workspace_mutation(&self.inner.repo_root, self.inner.lease.clone())
            .await
            .map_err(|error| error.to_string())?;
        *finalized = true;
        self.inner.stop.cancel();
        self.inner.reservation.lock().await.take();
        self.inner.finalization_complete.cancel();
        Ok(())
    }

    async fn wait_until_finalized(&self) {
        self.inner.finalization_complete.cancelled().await;
    }
}

#[derive(Default)]
struct CommandExecutionState {
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
    running: HashMap<i32, RunningCommand>,
    running_order: VecDeque<i32>,
    repository_epoch: u64,
    observed_turn_mutation_revisions: HashMap<String, u64>,
    observed_turn_order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    workspace_mutation_gates: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl CommandExecutionLedger {
    async fn workspace_mutation_gate(&self, repo_root: &Path) -> Arc<Mutex<()>> {
        let mut gates = self.workspace_mutation_gates.lock().await;
        Arc::clone(
            gates
                .entry(repo_root.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    #[cfg(test)]
    pub(crate) async fn reserve_workspace_mutation(
        &self,
        repo_root: &Path,
    ) -> WorkspaceMutationReservation {
        let gate = self.workspace_mutation_gate(repo_root).await;
        WorkspaceMutationReservation {
            _guard: gate.lock_owned().await,
        }
    }

    pub(crate) async fn reserve_workspace_mutation_until_cancelled(
        &self,
        repo_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<WorkspaceMutationReservation, WorkspaceMutationReservationAcquireError> {
        self.reserve_workspace_mutation_with_max_wait(
            repo_root,
            cancellation,
            WORKSPACE_MUTATION_MAX_WAIT,
        )
        .await
    }

    async fn reserve_workspace_mutation_with_max_wait(
        &self,
        repo_root: &Path,
        cancellation: &CancellationToken,
        max_wait: Duration,
    ) -> Result<WorkspaceMutationReservation, WorkspaceMutationReservationAcquireError> {
        let gate = self.workspace_mutation_gate(repo_root).await;
        // A terminated tool call can leave its in-process work pending while the caller is gone.
        // Do not let that orphaned reservation prevent every later command from reaching the
        // persistent lease, whose wait is already bounded separately.
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                Err(WorkspaceMutationReservationAcquireError::Cancelled)
            }
            _ = tokio::time::sleep(max_wait) => {
                Err(WorkspaceMutationReservationAcquireError::TimedOut)
            }
            guard = gate.lock_owned() => {
                Ok(WorkspaceMutationReservation { _guard: guard })
            }
        }
    }

    pub(crate) async fn observe_repository_revision(
        &self,
        turn_id: &str,
        mutation_revision: u64,
    ) -> u64 {
        let mut state = self.state.lock().await;
        if !state.observed_turn_mutation_revisions.contains_key(turn_id) {
            while state.observed_turn_mutation_revisions.len() >= MAX_TRACKED_COMMANDS {
                let Some(oldest_turn) = state.observed_turn_order.pop_front() else {
                    break;
                };
                state.observed_turn_mutation_revisions.remove(&oldest_turn);
            }
            state.observed_turn_order.push_back(turn_id.to_string());
        }
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
        state.repository_epoch
    }

    pub(crate) async fn begin_attempt(
        &self,
        key: &CommandAttemptKey,
        repaired: bool,
    ) -> Result<(), CommandAttemptBlocked> {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.attempts.get(key)
            && let Some(prior_failure) = entry.deterministic_failure.clone()
        {
            return Err(CommandAttemptBlocked {
                fingerprint: key.fingerprint(),
                prior_failure,
            });
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
        owner_turn_id: String,
        workspace_mutation: Option<RunningWorkspaceMutation>,
    ) {
        self.track_running_process_with_validation_contract(
            process_id,
            key,
            artifact,
            owner_turn_id,
            workspace_mutation,
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
        owner_turn_id: String,
        workspace_mutation: Option<RunningWorkspaceMutation>,
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
                owner_turn_id,
                completed_exit_code: None,
                workspace_mutation,
                validation_launch,
                started_at,
            },
        );
    }

    pub(crate) async fn running_process(&self, process_id: i32) -> Option<RunningCommand> {
        self.state.lock().await.running.get(&process_id).cloned()
    }

    pub(crate) async fn cancel_mutations_for_turn(&self, turn_id: &str) {
        let mutations = {
            let state = self.state.lock().await;
            state
                .running
                .values()
                .filter(|running| running.owner_turn_id == turn_id)
                .filter_map(|running| running.workspace_mutation.clone())
                .collect::<Vec<_>>()
        };
        for mutation in &mutations {
            mutation.cancel_owner();
        }
        for mutation in mutations {
            mutation.wait_until_finalized().await;
        }
    }

    pub(crate) async fn cancel_mutations_for_turn_with_timeout(
        &self,
        turn_id: &str,
        timeout: Duration,
    ) -> bool {
        tokio::time::timeout(timeout, self.cancel_mutations_for_turn(turn_id))
            .await
            .is_ok()
    }

    pub(crate) async fn update_running_artifact(
        &self,
        process_id: i32,
        artifact: RawOutputArtifact,
    ) {
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

    pub(crate) async fn mark_running_process_completed(
        &self,
        process_id: i32,
        exit_code: i32,
    ) -> bool {
        let workspace_mutation = {
            let mut state = self.state.lock().await;
            let Some(running) = state.running.get_mut(&process_id) else {
                return false;
            };
            if running.completed_exit_code.is_some() {
                return true;
            }
            running.completed_exit_code = Some(exit_code);
            let running = running.clone();
            let workspace_mutation = running.workspace_mutation.clone();
            record_running_exit_locked(&mut state, &running, exit_code);
            workspace_mutation
        };
        if let Some(workspace_mutation) = workspace_mutation
            && let Err(error) = workspace_mutation.finish().await
        {
            // Keep the shared mutation handle on the running entry so a later poll can
            // retry finalization, but stop renewing authority for a process that has
            // already exited so an unpolled cleanup error cannot strand the repository.
            workspace_mutation.cancel_owner();
            tracing::warn!(
                %error,
                process_id,
                "workspace mutation finalization after process exit failed"
            );
        }
        true
    }

    pub(crate) async fn finish_running_process(
        &self,
        process_id: i32,
        exit_code: Option<i32>,
    ) -> bool {
        match self
            .finish_running_process_checked(process_id, exit_code)
            .await
        {
            Ok(finished) => finished,
            Err(error) => {
                tracing::warn!(%error, process_id, "workspace mutation finalization failed");
                true
            }
        }
    }

    pub(crate) async fn finish_running_process_checked(
        &self,
        process_id: i32,
        exit_code: Option<i32>,
    ) -> Result<bool, String> {
        let running = {
            let mut state = self.state.lock().await;
            let Some(mut running) = state.running.remove(&process_id) else {
                return Ok(false);
            };
            state.running_order.retain(|tracked| *tracked != process_id);
            if running.completed_exit_code.is_none()
                && let Some(exit_code) = exit_code
            {
                running.completed_exit_code = Some(exit_code);
                record_running_exit_locked(&mut state, &running, exit_code);
            }
            running
        };
        if let Some(workspace_mutation) = running.workspace_mutation.as_ref()
            && let Err(error) = workspace_mutation.finish().await
        {
            let mut state = self.state.lock().await;
            if !state.running.contains_key(&process_id) {
                state.running_order.push_back(process_id);
                state.running.insert(process_id, running);
            }
            return Err(error);
        }
        Ok(true)
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
    if exit_code != 0 && running.validation_launch.is_some() {
        let execution_ended_at = SystemTime::now();
        let execution_duration = Instant::now().saturating_duration_since(running.started_at);
        let entry = attempt_entry_locked(state, &running.key);
        entry.last_exit_code = Some(exit_code);
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.deterministic_failure =
            Some(DeterministicFailureRecord::from_trusted_classification(
                "focused-validation",
                running.artifact.clone(),
                exit_code,
                execution_ended_at,
                execution_duration,
                None,
            ));
    } else {
        record_exit_locked(state, &running.key, exit_code);
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
    if !state.attempts.contains_key(key) {
        while state.attempts.len() >= MAX_TRACKED_COMMANDS {
            if let Some(oldest) = state.insertion_order.pop_front() {
                state.attempts.remove(&oldest);
                continue;
            }

            let Some(unordered_key) = state.attempts.keys().next().cloned() else {
                break;
            };
            state.attempts.remove(&unordered_key);
        }
        state.insertion_order.push_back(key.clone());
    }
    state.attempts.entry(key.clone()).or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_agent_task_store::LocalAgentTaskStore;
    use codex_agent_task_store::REPOSITORY_WIDE_PATH;
    use codex_agent_task_store::WorkspaceActorKind;
    use codex_agent_task_store::WorkspaceMutationRequest;
    use codex_state::StateRuntime;
    use tempfile::TempDir;

    async fn running_workspace_mutation(
        owner_cancelled: CancellationToken,
    ) -> (
        TempDir,
        TempDir,
        Arc<LocalAgentTaskStore>,
        Arc<CommandExecutionLedger>,
        RunningWorkspaceMutation,
    ) {
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime initializes");
        let store = Arc::new(
            LocalAgentTaskStore::initialize(&state)
                .await
                .expect("task store initializes"),
        );
        let ledger = Arc::new(CommandExecutionLedger::default());
        let reservation = ledger.reserve_workspace_mutation(repo.path()).await;
        let lease = store
            .begin_workspace_mutation(
                repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: "owner-root".to_string(),
                    actor_id: "root:owner-root".to_string(),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                },
            )
            .await
            .expect("workspace mutation starts");
        let trait_store: Arc<dyn AgentTaskStore> = store.clone();
        let running = RunningWorkspaceMutation::new(
            trait_store,
            repo.path().to_path_buf(),
            lease,
            owner_cancelled,
            reservation,
        );
        (codex_home, repo, store, ledger, running)
    }

    fn key(command: &str) -> CommandAttemptKey {
        CommandAttemptKey::new("exec_command", "local", "C:/repo", &[command.to_string()])
    }

    fn deterministic_failure(class: &str, exit_code: i32) -> DeterministicFailureRecord {
        DeterministicFailureRecord::from_trusted_classification(
            class,
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
        }
    }

    #[tokio::test]
    async fn workspace_mutation_reservations_serialize_per_repository() {
        let ledger = Arc::new(CommandExecutionLedger::default());
        let first_repo = TempDir::new().expect("first repository tempdir");
        let second_repo = TempDir::new().expect("second repository tempdir");
        let first = ledger.reserve_workspace_mutation(first_repo.path()).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let waiting_ledger = Arc::clone(&ledger);
        let waiting_repo = first_repo.path().to_path_buf();
        let waiter = tokio::spawn(async move {
            started_tx.send(()).expect("waiter starts");
            waiting_ledger
                .reserve_workspace_mutation(&waiting_repo)
                .await
        });
        started_rx.await.expect("waiter reports startup");
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "a same-repository mutation reservation must wait"
        );

        let different_repo = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ledger.reserve_workspace_mutation(second_repo.path()),
        )
        .await
        .expect("a different repository remains independent");
        drop(different_repo);
        drop(first);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("same-repository waiter proceeds after release")
            .expect("same-repository waiter task succeeds");
    }

    #[tokio::test]
    async fn workspace_mutation_reservation_wait_is_cancellable() {
        let ledger = Arc::new(CommandExecutionLedger::default());
        let repo = TempDir::new().expect("repository tempdir");
        let first = ledger.reserve_workspace_mutation(repo.path()).await;
        let cancellation = CancellationToken::new();
        let waiting_ledger = Arc::clone(&ledger);
        let waiting_repo = repo.path().to_path_buf();
        let waiting_cancellation = cancellation.clone();
        let waiter = tokio::spawn(async move {
            waiting_ledger
                .reserve_workspace_mutation_until_cancelled(&waiting_repo, &waiting_cancellation)
                .await
        });

        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the reservation wait is exercised");
        cancellation.cancel();
        let reservation = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("cancellation promptly stops the reservation wait")
            .expect("reservation waiter task succeeds");
        assert!(
            matches!(
                reservation,
                Err(WorkspaceMutationReservationAcquireError::Cancelled)
            ),
            "cancelled wait acquires no reservation"
        );
        drop(first);
    }

    #[tokio::test]
    async fn workspace_mutation_reservation_wait_is_bounded() {
        let ledger = CommandExecutionLedger::default();
        let repo = TempDir::new().expect("repository tempdir");
        let _first = ledger.reserve_workspace_mutation(repo.path()).await;

        let result = ledger
            .reserve_workspace_mutation_with_max_wait(
                repo.path(),
                &CancellationToken::new(),
                std::time::Duration::from_millis(10),
            )
            .await;

        assert!(
            matches!(
                result,
                Err(WorkspaceMutationReservationAcquireError::TimedOut)
            ),
            "occupied reservation must time out"
        );
    }

    #[tokio::test]
    async fn workspace_mutation_lease_wait_is_serialized_and_cancellable() {
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime initializes");
        let store = Arc::new(
            LocalAgentTaskStore::initialize(&state)
                .await
                .expect("task store initializes"),
        );
        let owner_request = WorkspaceMutationRequest {
            root_session_id: "owner-root".to_string(),
            actor_id: "root:owner-root".to_string(),
            kind: WorkspaceActorKind::Root,
            attempt_id: None,
            paths: vec![REPOSITORY_WIDE_PATH.to_string()],
            contracts: Vec::new(),
            expected_manifest: Vec::new(),
        };
        let waiter_request = owner_request.clone();
        let owner_lease = store
            .begin_workspace_mutation(repo.path(), owner_request.clone())
            .await
            .expect("owner lease starts");
        let waiter_store: Arc<dyn AgentTaskStore> = store.clone();
        let waiter_repo = repo.path().to_path_buf();
        let waiter_cancellation = CancellationToken::new();
        let waiter = tokio::spawn({
            let waiter_cancellation = waiter_cancellation.clone();
            let waiter_request = waiter_request.clone();
            async move {
                acquire_workspace_mutation_lease(
                    waiter_store.as_ref(),
                    &waiter_repo,
                    &waiter_request,
                    &waiter_cancellation,
                )
                .await
            }
        });

        tokio::time::sleep(WORKSPACE_MUTATION_RETRY_INTERVAL * 2).await;
        assert!(
            !waiter.is_finished(),
            "a conflicting active lease must keep the waiter serialized"
        );
        store
            .finish_workspace_mutation(repo.path(), owner_lease)
            .await
            .expect("owner lease finishes");
        let waiter_lease = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter proceeds after the owner releases the lease")
            .expect("waiter task succeeds")
            .expect("waiter acquires the lease");
        store
            .finish_workspace_mutation(repo.path(), waiter_lease)
            .await
            .expect("waiter lease finishes");

        let owner_lease = store
            .begin_workspace_mutation(repo.path(), owner_request)
            .await
            .expect("replacement owner lease starts");
        let waiter_store: Arc<dyn AgentTaskStore> = store.clone();
        let waiter_repo = repo.path().to_path_buf();
        let cancellation = CancellationToken::new();
        let cancelled_waiter_request = waiter_request.clone();
        let cancelled_waiter = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                acquire_workspace_mutation_lease(
                    waiter_store.as_ref(),
                    &waiter_repo,
                    &cancelled_waiter_request,
                    &cancellation,
                )
                .await
            }
        });
        tokio::time::sleep(WORKSPACE_MUTATION_RETRY_INTERVAL * 2).await;
        assert!(
            !cancelled_waiter.is_finished(),
            "a conflicting lease must not be bypassed"
        );
        cancellation.cancel();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(2), cancelled_waiter)
            .await
            .expect("cancelled waiter exits promptly")
            .expect("cancelled waiter task succeeds")
            .expect_err("cancelled waiter reports cancellation");
        assert!(matches!(
            cancelled,
            WorkspaceMutationAcquireError::Cancelled
        ));

        let timed_out = acquire_workspace_mutation_lease_with_max_wait(
            store.as_ref(),
            repo.path(),
            &waiter_request,
            &CancellationToken::new(),
            WORKSPACE_MUTATION_RETRY_INTERVAL * 2,
        )
        .await
        .expect_err("a persistent conflicting lease reports a bounded wait");
        let WorkspaceMutationAcquireError::TimedOut { details } = timed_out else {
            panic!("persistent conflict should report a timeout, got {timed_out:?}");
        };
        assert!(
            !details.is_empty(),
            "the timeout must retain the conflict that blocked process launch"
        );

        store
            .finish_workspace_mutation(repo.path(), owner_lease)
            .await
            .expect("replacement owner lease finishes");
    }

    #[tokio::test]
    async fn deterministic_failure_suppresses_every_exact_repeat_until_identity_changes() {
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

        for _ in 0..3 {
            let blocked = ledger
                .begin_attempt(&attempt_key, false)
                .await
                .expect_err("every exact repeat should be suppressed");
            assert_eq!(blocked.prior_failure.exit_code, 7);
            assert_eq!(blocked.prior_failure.outcome_class, "focused-validation");
            assert_eq!(blocked.fingerprint, attempt_key.fingerprint());
        }

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
                "turn-1".to_string(),
                None,
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
                "turn-1".to_string(),
                None,
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
        let failure = snapshot
            .deterministic_failure
            .expect("trusted tracked validation is classified");
        assert_eq!(failure.outcome_class, "focused-validation");
        assert_eq!(failure.exit_code, 7);
        assert_eq!(failure.evidence, finalized_artifact);
        let blocked = ledger
            .begin_attempt(&command_key, false)
            .await
            .expect_err("the exact deterministic repeat is suppressed");
        assert_eq!(blocked.prior_failure.evidence, finalized_artifact);
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
                "turn-1".to_string(),
                None,
                Some(validation_launch()),
                Instant::now() - Duration::from_millis(25),
            )
            .await;
        ledger
            .update_running_artifact(43, finalized_artifact.clone())
            .await;

        assert_eq!(
            ledger.finish_running_process_checked(43, Some(9)).await,
            Ok(true)
        );
        assert_eq!(
            ledger.finish_running_process_checked(43, Some(9)).await,
            Ok(false)
        );

        let snapshot = ledger.snapshot(&command_key).await.expect("tracked entry");
        assert_eq!(snapshot.consecutive_failures, 1);
        let failure = snapshot
            .deterministic_failure
            .expect("trusted tracked validation is classified");
        assert_eq!(failure.outcome_class, "focused-validation");
        assert_eq!(failure.exit_code, 9);
        assert_eq!(failure.evidence, finalized_artifact);
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
                "turn-1".to_string(),
                None,
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

    #[tokio::test]
    async fn background_completion_releases_mutation_before_poll() {
        let (_codex_home, repo, store, ledger, mutation) =
            running_workspace_mutation(CancellationToken::new()).await;
        let command_key = key("background-success.exe");
        ledger
            .track_running_process(
                42,
                command_key,
                RawOutputArtifact::unavailable("fixture"),
                "turn-1".to_string(),
                Some(mutation),
            )
            .await;

        assert!(ledger.mark_running_process_completed(42, 0).await);
        assert!(
            ledger.running_process(42).await.is_some(),
            "completed metadata remains available for a later poll"
        );
        let _next_reservation = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ledger.reserve_workspace_mutation(repo.path()),
        )
        .await
        .expect("background completion releases the same-repository reservation");

        let replacement = store
            .begin_workspace_mutation(
                repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: "replacement-root".to_string(),
                    actor_id: "root:replacement-root".to_string(),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                },
            )
            .await
            .expect("confirmed background exit releases the repository lease");
        store
            .finish_workspace_mutation(repo.path(), replacement)
            .await
            .expect("replacement mutation finishes");
        assert!(ledger.finish_running_process(42, Some(0)).await);
    }

    #[tokio::test]
    async fn owner_cancellation_stops_heartbeat_and_signals_process_termination() {
        let owner_cancelled = CancellationToken::new();
        let (_codex_home, _repo, _store, _ledger, mutation) =
            running_workspace_mutation(owner_cancelled.clone()).await;
        let lease_lost = mutation.lease_lost_token();

        owner_cancelled.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), lease_lost.cancelled())
            .await
            .expect("owner cancellation signals mutation lease loss");
        mutation
            .finish()
            .await
            .expect("test mutation finalizes after cancellation");
    }

    #[tokio::test]
    async fn terminal_turn_revokes_only_its_mutating_background_process() {
        let (_codex_home, repo, store, ledger, mutation) =
            running_workspace_mutation(CancellationToken::new()).await;
        let lease_lost = mutation.lease_lost_token();
        ledger
            .track_running_process(
                42,
                key("turn-owned-background.exe"),
                RawOutputArtifact::unavailable("fixture"),
                "owner-turn".to_string(),
                Some(mutation),
            )
            .await;

        ledger.cancel_mutations_for_turn("other-turn").await;
        assert!(!lease_lost.is_cancelled());
        let cancellation = {
            let ledger = Arc::clone(&ledger);
            tokio::spawn(async move {
                ledger.cancel_mutations_for_turn("owner-turn").await;
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), lease_lost.cancelled())
            .await
            .expect("terminal owner turn signals process termination");
        assert!(
            !cancellation.is_finished(),
            "turn cancellation must wait for process exit and mutation finalization"
        );
        assert!(ledger.mark_running_process_completed(42, -1).await);
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation)
            .await
            .expect("turn cancellation finishes after process exit")
            .expect("turn cancellation task succeeds");

        let replacement_reservation = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ledger.reserve_workspace_mutation(repo.path()),
        )
        .await
        .expect("terminal completion releases the local mutation reservation");
        let replacement = store
            .begin_workspace_mutation(
                repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: "replacement-root".to_string(),
                    actor_id: "root:replacement-root".to_string(),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                },
            )
            .await
            .expect("terminal completion releases the persisted mutation lease");
        store
            .finish_workspace_mutation(repo.path(), replacement)
            .await
            .expect("replacement mutation finishes");
        drop(replacement_reservation);
    }

    #[tokio::test]
    async fn terminal_turn_mutation_cleanup_can_be_bounded() {
        let (_codex_home, _repo, _store, ledger, mutation) =
            running_workspace_mutation(CancellationToken::new()).await;
        let lease_lost = mutation.lease_lost_token();
        ledger
            .track_running_process(
                42,
                key("turn-owned-background.exe"),
                RawOutputArtifact::unavailable("fixture"),
                "owner-turn".to_string(),
                Some(mutation.clone()),
            )
            .await;

        assert!(
            !ledger
                .cancel_mutations_for_turn_with_timeout(
                    "owner-turn",
                    std::time::Duration::from_millis(1),
                )
                .await,
            "terminal cleanup must return after its deadline even if process exit is never observed"
        );
        assert!(
            lease_lost.is_cancelled(),
            "bounded cleanup must still revoke the process mutation authority"
        );

        mutation
            .finish()
            .await
            .expect("test mutation remains finalizable after bounded cancellation");
    }

    #[tokio::test]
    async fn dropped_workspace_mutation_guard_releases_lease_for_next_task() {
        let codex_home = TempDir::new().expect("codex home tempdir");
        let repo = TempDir::new().expect("repository tempdir");
        let state =
            StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
                .await
                .expect("state runtime initializes");
        let store = Arc::new(
            LocalAgentTaskStore::initialize(&state)
                .await
                .expect("task store initializes"),
        );
        let ledger = Arc::new(CommandExecutionLedger::default());
        let reservation = ledger.reserve_workspace_mutation(repo.path()).await;
        let lease = store
            .begin_workspace_mutation(
                repo.path(),
                WorkspaceMutationRequest {
                    root_session_id: "completed-review-root".to_string(),
                    actor_id: "root:completed-review-root".to_string(),
                    kind: WorkspaceActorKind::Root,
                    attempt_id: None,
                    paths: vec![REPOSITORY_WIDE_PATH.to_string()],
                    contracts: Vec::new(),
                    expected_manifest: Vec::new(),
                },
            )
            .await
            .expect("completed review acquires mutation lease");
        let trait_store: Arc<dyn AgentTaskStore> = store.clone();
        drop(WorkspaceMutationGuard::new(
            trait_store,
            repo.path().to_path_buf(),
            lease,
            reservation,
        ));

        let _replacement_reservation = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ledger.reserve_workspace_mutation(repo.path()),
        )
        .await
        .expect("dropped review releases its local mutation reservation");
        let replacement_request = WorkspaceMutationRequest {
            root_session_id: "bug-fix-root".to_string(),
            actor_id: "root:bug-fix-root".to_string(),
            kind: WorkspaceActorKind::Root,
            attempt_id: None,
            paths: vec![REPOSITORY_WIDE_PATH.to_string()],
            contracts: Vec::new(),
            expected_manifest: Vec::new(),
        };
        let replacement = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_workspace_mutation_lease(
                store.as_ref(),
                repo.path(),
                &replacement_request,
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("next task does not wait for the stale lease timeout")
        .expect("next task acquires repository-wide mutation ownership");
        store
            .finish_workspace_mutation(repo.path(), replacement)
            .await
            .expect("replacement mutation finishes");
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
    async fn handler_finalization_before_exit_watcher_records_one_failure() {
        let ledger = CommandExecutionLedger::default();
        let key = key("stored-process-failure.exe");
        ledger.begin_attempt(&key, false).await.expect("attempt");
        ledger
            .track_running_process(
                42,
                key.clone(),
                RawOutputArtifact::unavailable("fixture"),
                "turn-1".to_string(),
                None,
            )
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
                    "turn-1".to_string(),
                    None,
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
                "turn-1".to_string(),
                None,
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
