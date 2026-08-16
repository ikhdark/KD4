use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use codex_protocol::plan_tool::ValidationRoute;
use codex_protocol::validation::ValidationFreshness;
use codex_protocol::validation::ValidationProofKey;
use codex_protocol::validation::ValidationResult;
use codex_protocol::validation::ValidationTerminalStatus;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::tools::command_output_artifact::RawOutputArtifact;
use crate::validation_admission::ValidationClassification;
use crate::validation_admission::ValidationEcosystem;
use crate::validation_admission::ValidationLaunchPlan;
use crate::validation_admission::ValidationOperation;
use crate::validation_admission::classify_validation;

const MAX_TRACKED_COMMANDS: usize = 128;
const MAX_COMPLETED_VALIDATION_PROOFS: usize = 128;
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
            self.prior_failure.proof.outcome_class(),
            self.fingerprint,
            self.prior_failure.exit_code,
            self.prior_failure.evidence,
        )
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
struct CompletedCommandValidation {
    duration_ms: u64,
    artifact: RawOutputArtifact,
    selected_test_count: Option<u64>,
}

#[derive(Default)]
struct CommandExecutionState {
    attempts: HashMap<CommandAttemptKey, AttemptEntry>,
    insertion_order: VecDeque<CommandAttemptKey>,
    running: HashMap<i32, RunningCommand>,
    running_order: VecDeque<i32>,
    repository_epoch: u64,
    observed_turn_mutation_revisions: HashMap<String, u64>,
    completed_command_validations: HashMap<ValidationProofKey, CompletedCommandValidation>,
    completed_command_validation_order: VecDeque<ValidationProofKey>,
    completed_validations: HashMap<ValidationProofKey, CompletedValidationProof>,
    completed_validation_order: VecDeque<ValidationProofKey>,
    validation_results_by_call: HashMap<String, ValidationResult>,
    validation_result_call_order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct CommandExecutionLedger {
    state: Mutex<CommandExecutionState>,
    bound_auto_validations: Mutex<HashMap<String, BoundAutoValidationLeaf>>,
}

impl CommandExecutionLedger {
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

    pub(crate) async fn promote_reusable_command_validation(
        &self,
        command_key: &ValidationProofKey,
        proof_key: ValidationProofKey,
        route: ValidationRoute,
        call_id: String,
    ) -> Option<ValidationResult> {
        let completed = self
            .state
            .lock()
            .await
            .completed_command_validations
            .get(command_key)
            .cloned()?;
        let Some((artifact_ref, artifact_sha256)) = completed.artifact.validation_integrity().await
        else {
            let mut state = self.state.lock().await;
            state.completed_command_validations.remove(command_key);
            state
                .completed_command_validation_order
                .retain(|entry| entry != command_key);
            return None;
        };
        let result = ValidationResult {
            proof_key: proof_key.clone(),
            route,
            call_id: call_id.clone(),
            process_id: None,
            status: ValidationTerminalStatus::Succeeded,
            duration_ms: completed.duration_ms,
            summary: Some(
                "reused exact focused validation completed before route declaration".to_string(),
            ),
            failure_excerpt: None,
            failure_signature: None,
            selected_test_count: completed.selected_test_count,
            raw_artifact_ref: Some(artifact_ref),
            raw_artifact_sha256: Some(artifact_sha256),
            freshness: ValidationFreshness::Reused,
        };
        let mut state = self.state.lock().await;
        insert_validation_result_locked(&mut state, call_id, result.clone());
        insert_completed_validation_locked(
            &mut state,
            proof_key,
            CompletedValidationProof {
                result: result.clone(),
                artifact: completed.artifact,
            },
        );
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
        let entry = attempt_entry_locked(&mut state, key);
        if !repaired
            && !force_fresh
            && let Some(prior_failure) = entry.deterministic_failure.clone()
        {
            return Err(CommandAttemptBlocked {
                fingerprint: key.fingerprint(),
                prior_failure,
            });
        }
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
    }

    async fn forget_turn_repository_revision(&self, turn_id: &str) {
        self.state
            .lock()
            .await
            .observed_turn_mutation_revisions
            .remove(turn_id);
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
            let Some(launch) = running.validation_launch.clone() else {
                return;
            };
            (
                launch,
                running.artifact.clone(),
                running.started_at,
                exit_code,
            )
        };
        let (launch, artifact, started_at, exit_code) = candidate;
        self.publish_completed_validation(
            &launch,
            artifact,
            started_at,
            exit_code,
            Some(process_id.to_string()),
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
        self.publish_completed_validation(launch, artifact, started_at, exit_code, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_completed_validation(
        &self,
        launch: &ValidationLaunchPlan,
        artifact: RawOutputArtifact,
        started_at: Instant,
        exit_code: i32,
        process_id: Option<String>,
    ) -> bool {
        let Some((artifact_ref, artifact_sha256, output)) = artifact.validation_evidence().await
        else {
            return false;
        };
        let selected_test_count = rust_test_validation(&launch.invocation)
            .then(|| executed_rust_test_count(&output))
            .flatten();
        let zero_tests_selected = exit_code == 0 && selected_test_count == Some(0);
        let succeeded = exit_code == 0 && !zero_tests_selected;
        let failure_signature =
            normalized_validation_failure_signature(exit_code, zero_tests_selected, &output);
        let duration_ms = u64::try_from(
            Instant::now()
                .saturating_duration_since(started_at)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let mut state = self.state.lock().await;
        if succeeded && let Some(command_key) = launch.command_proof_key.clone() {
            insert_completed_command_validation_locked(
                &mut state,
                command_key,
                CompletedCommandValidation {
                    duration_ms,
                    artifact: artifact.clone(),
                    selected_test_count,
                },
            );
        }
        let (Some(proof_key), Some(route), Some(call_id)) = (
            launch.proof_key.clone(),
            launch.structured_route.clone(),
            launch.validation_call_id.clone(),
        ) else {
            return true;
        };
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
            duration_ms,
            summary: Some(if zero_tests_selected {
                "validation command exited successfully but selected zero Rust tests".to_string()
            } else if succeeded {
                "focused validation succeeded".to_string()
            } else {
                format!("focused validation exited with code {exit_code}")
            }),
            failure_excerpt: (!succeeded).then(|| {
                if zero_tests_selected {
                    "zero executed tests do not cover the declared behavioral contract".to_string()
                } else {
                    format!(
                    "validation exited with code {exit_code}; exact output is retained in the immutable artifact"
                    )
                }
            }),
            failure_signature,
            selected_test_count,
            raw_artifact_ref: Some(artifact_ref),
            raw_artifact_sha256: Some(artifact_sha256),
            freshness: ValidationFreshness::Executed,
        };
        if state.validation_results_by_call.contains_key(&call_id) {
            return true;
        }
        insert_validation_result_locked(&mut state, call_id, result.clone());
        if !succeeded || state.completed_validations.contains_key(&proof_key) {
            return true;
        }
        insert_completed_validation_locked(
            &mut state,
            proof_key,
            CompletedValidationProof { result, artifact },
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

fn insert_validation_result_locked(
    state: &mut CommandExecutionState,
    call_id: String,
    result: ValidationResult,
) {
    if state.validation_results_by_call.contains_key(&call_id) {
        state
            .validation_result_call_order
            .retain(|entry| entry != &call_id);
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
    state.validation_results_by_call.insert(call_id, result);
}

fn insert_completed_validation_locked(
    state: &mut CommandExecutionState,
    proof_key: ValidationProofKey,
    proof: CompletedValidationProof,
) {
    if state.completed_validations.contains_key(&proof_key) {
        state
            .completed_validation_order
            .retain(|entry| entry != &proof_key);
    }
    while state.completed_validations.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
        let Some(oldest) = state.completed_validation_order.pop_front() else {
            break;
        };
        state.completed_validations.remove(&oldest);
    }
    state
        .completed_validation_order
        .push_back(proof_key.clone());
    state.completed_validations.insert(proof_key, proof);
}

fn insert_completed_command_validation_locked(
    state: &mut CommandExecutionState,
    proof_key: ValidationProofKey,
    proof: CompletedCommandValidation,
) {
    if state.completed_command_validations.contains_key(&proof_key) {
        state
            .completed_command_validation_order
            .retain(|entry| entry != &proof_key);
    }
    while state.completed_command_validations.len() >= MAX_COMPLETED_VALIDATION_PROOFS {
        let Some(oldest) = state.completed_command_validation_order.pop_front() else {
            break;
        };
        state.completed_command_validations.remove(&oldest);
    }
    state
        .completed_command_validation_order
        .push_back(proof_key.clone());
    state.completed_command_validations.insert(proof_key, proof);
}

fn rust_test_validation(
    invocation: &crate::tools::handlers::command_shape::CommandInvocation,
) -> bool {
    matches!(
        classify_validation(invocation),
        ValidationClassification::Validation { leaves, .. }
            if leaves.iter().any(|leaf| {
                leaf.operation == ValidationOperation::Test
                    && leaf.ecosystem == ValidationEcosystem::Rust
            })
    )
}

fn executed_rust_test_count(output: &[u8]) -> Option<u64> {
    let output = String::from_utf8_lossy(output);
    let running_count = output
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()?.eq_ignore_ascii_case("running"))
                .then(|| words.next()?.parse::<u64>().ok())
                .flatten()
        })
        .fold(None, |total, count| {
            Some(total.unwrap_or(0_u64).saturating_add(count))
        });
    if running_count.is_some() {
        return running_count;
    }
    let mut count = 0_u64;
    let mut found_summary = false;
    for line in output.lines() {
        let normalized = line.to_ascii_lowercase();
        if !normalized.contains("test result:") {
            continue;
        }
        let words = normalized.split_whitespace().collect::<Vec<_>>();
        let Some(passed_index) = words.iter().position(|word| word.starts_with("passed")) else {
            continue;
        };
        let Some(value) = passed_index
            .checked_sub(1)
            .and_then(|index| words.get(index))
            .and_then(|value| {
                value
                    .trim_matches(|character: char| !character.is_ascii_digit())
                    .parse::<u64>()
                    .ok()
            })
        else {
            continue;
        };
        found_summary = true;
        count = count.saturating_add(value);
    }
    found_summary.then_some(count)
}

pub(crate) fn normalized_validation_failure_signature(
    exit_code: i32,
    zero_tests_selected: bool,
    output: &[u8],
) -> Option<String> {
    if exit_code == 0 && !zero_tests_selected {
        return None;
    }
    if zero_tests_selected {
        return Some("validation-failure-v1:zero-tests-selected".to_string());
    }
    let mut normalized = String::new();
    let mut in_digits = false;
    let mut in_whitespace = false;
    for character in String::from_utf8_lossy(output)
        .chars()
        .flat_map(char::to_lowercase)
    {
        let character = if character == '\\' { '/' } else { character };
        if character.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
            }
            in_digits = true;
            in_whitespace = false;
        } else if character.is_whitespace() {
            if !in_whitespace {
                normalized.push(' ');
            }
            in_digits = false;
            in_whitespace = true;
        } else {
            normalized.push(character);
            in_digits = false;
            in_whitespace = false;
        }
    }
    let bounded = normalized
        .char_indices()
        .rev()
        .nth(4096)
        .map_or(normalized.as_str(), |(index, _)| &normalized[index..]);
    let digest = format!("{:x}", Sha256::digest(bounded.as_bytes()));
    Some(format!(
        "validation-failure-v1:exit-{exit_code}:{}",
        &digest[..24]
    ))
}

fn record_running_exit_locked(
    state: &mut CommandExecutionState,
    running: &RunningCommand,
    exit_code: i32,
) {
    record_exit_locked(state, &running.key, exit_code);
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::plan_tool::ValidationRouteLeaf;
    use codex_protocol::plan_tool::ValidationRouteOrdering;

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
            command_proof_key: None,
            proof_key: None,
            structured_route: None,
            validation_call_id: None,
        }
    }

    fn validation_route() -> ValidationRoute {
        ValidationRoute {
            leaves: vec![ValidationRouteLeaf {
                argv: vec!["cargo".to_string(), "test".to_string()],
                covered_paths: vec!["core/src/tools/command_execution.rs".to_string()],
                covered_contracts: vec!["validation evidence reuse".to_string()],
                timeout_ms: 30_000,
                semantic_timeout: false,
            }],
            ordering: ValidationRouteOrdering::StopOnFailure,
        }
    }

    #[tokio::test]
    async fn validation_efficiency_promotes_exact_predeclared_proof_without_rerun() {
        let ledger = CommandExecutionLedger::default();
        let mut launch = validation_launch();
        let command_key = crate::validation_admission::validation_identity(
            b"C:/repo",
            "C:/repo",
            &launch.invocation,
            "env",
            "stable",
            7,
        );
        launch.command_proof_key = Some(command_key.clone());
        let tempdir = tempfile::tempdir().expect("tempdir");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            tempdir.path(),
            "thread",
            b"running 1 test\ntest result: ok. 1 passed; 0 failed; 0 ignored\n",
        )
        .await;
        assert!(
            ledger
                .publish_inline_validation(&launch, artifact, Instant::now(), 0)
                .await
        );

        let route = validation_route();
        let scoped_key = crate::validation_admission::validation_identity_with_scope(
            b"C:/repo",
            "C:/repo",
            &launch.invocation,
            "env",
            "stable",
            "features=[];semantic_timeout=nonsemantic",
            "implementation-v1",
            &route.leaves[0].covered_paths,
            &route.leaves[0].covered_contracts,
        );
        let promoted = ledger
            .promote_reusable_command_validation(
                &command_key,
                scoped_key.clone(),
                route,
                "validation-call".to_string(),
            )
            .await
            .expect("fresh exact command should be promoted");
        assert_eq!(promoted.status, ValidationTerminalStatus::Succeeded);
        assert_eq!(promoted.freshness, ValidationFreshness::Reused);
        assert_eq!(promoted.selected_test_count, Some(1));
        assert_eq!(promoted.failure_signature, None);
        assert!(ledger.reusable_validation(&scoped_key).await.is_some());
        assert!(
            ledger
                .validation_result_for_call("validation-call")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn evidence_contract_validation_reports_selected_tests_and_failure_signature() {
        let output = b"test result: ok. 2 passed; 0 failed; 1 ignored\n\
test result: ok. 0 passed; 0 failed; 0 ignored\n";
        assert_eq!(executed_rust_test_count(output), Some(2));
        assert_eq!(executed_rust_test_count(b"Finished test profile\n"), None);

        let ledger = CommandExecutionLedger::default();
        let mut launch = validation_launch();
        let command_key = crate::validation_admission::validation_identity(
            b"C:/repo",
            "C:/repo",
            &launch.invocation,
            "env",
            "stable",
            7,
        );
        launch.command_proof_key = Some(command_key.clone());
        launch.proof_key = Some(command_key.clone());
        launch.structured_route = Some(validation_route());
        launch.validation_call_id = Some("zero-test-call".to_string());
        let tempdir = tempfile::tempdir().expect("tempdir");
        let artifact = crate::tools::command_output_artifact::create_raw_output_artifact(
            tempdir.path(),
            "thread",
            b"running 0 tests\ntest result: ok. 0 passed; 0 failed; 3 ignored\n",
        )
        .await;
        assert!(
            ledger
                .publish_inline_validation(&launch, artifact, Instant::now(), 0)
                .await
        );

        let result = ledger
            .validation_result_for_call("zero-test-call")
            .await
            .expect("typed zero-test result");
        assert_eq!(result.status, ValidationTerminalStatus::Failed);
        assert_eq!(result.selected_test_count, Some(0));
        assert_eq!(
            result.failure_signature.as_deref(),
            Some("validation-failure-v1:zero-tests-selected")
        );
        assert!(ledger.reusable_validation(&command_key).await.is_none());
        assert!(
            ledger
                .promote_reusable_command_validation(
                    &command_key,
                    command_key.clone(),
                    validation_route(),
                    "later-call".to_string(),
                )
                .await
                .is_none()
        );

        let first = normalized_validation_failure_signature(
            101,
            false,
            b"error at C:\\repo\\src\\owner.rs:42 after 1.2s",
        );
        let repeated = normalized_validation_failure_signature(
            101,
            false,
            b"ERROR at C:/repo/src/owner.rs:99 after 8.7s",
        );
        assert_eq!(first, repeated);
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
