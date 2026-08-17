use chrono::DateTime;
use chrono::Utc;
use codex_agent_task_store::AdmittedAssignment;
use codex_agent_task_store::AgentReceipt;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
#[cfg(test)]
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::ObservationKind;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::StoreResult;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_agent_task_store::ValidationEvidence;
use codex_agent_task_store::ValidationProofKind;
use codex_agent_task_store::WorkspaceActorKind;
use codex_agent_task_store::WorkspaceActorRegistration;
use codex_agent_task_store::WorkspaceStrategy;
use codex_otel::SessionTelemetry;
use codex_protocol::AgentPath;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use tokio::sync::Notify;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::task_metrics::TASK_FIRST_MEANINGFUL_PROGRESS_DURATION_METRIC;
use super::task_metrics::TaskMetricRuntime;
use super::task_metrics::terminal_metrics_ready;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FocusedValidationToken {
    assignment_id: AssignmentId,
    attempt_id: codex_agent_task_store::AttemptId,
    call_id: String,
    command_summary: String,
    resolved_executable: String,
    evidence: ValidationEvidence,
}

impl FocusedValidationToken {
    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn shared_from_call_id(&self) -> Option<&str> {
        self.evidence.shared_from_call_id.as_deref()
    }

    pub(crate) fn lease_expires_at(&self) -> Option<chrono::DateTime<Utc>> {
        self.evidence.lease_expires_at
    }
}

#[derive(Default)]
struct BindingIndex {
    by_agent_path: HashMap<String, AgentTaskBinding>,
    by_assignment: HashMap<AssignmentId, AgentTaskBinding>,
}

#[derive(Default)]
struct TaskMetricIndex {
    runtimes: HashMap<AssignmentId, TaskMetricRuntime>,
    active: HashSet<AssignmentId>,
    first_progress_attempts: HashSet<AttemptId>,
    hydrated_receipt_attempts: HashSet<AttemptId>,
    configured_capacity: Option<u32>,
}

const MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES: usize = 4_096;

/// Shared typed-task persistence and identity index for one root agent tree.
///
/// The coordinator is cloned with [`super::AgentControl`], so every child resolves the same
/// assignment/attempt identity. Legacy agents simply have no binding and bypass this layer.
#[derive(Clone, Default)]
pub(crate) struct AgentTaskCoordinator {
    store: Arc<OnceCell<Arc<dyn AgentTaskStore>>>,
    fallback_state_runtime: Arc<OnceCell<Arc<StateRuntime>>>,
    root_session_id: Arc<OnceCell<String>>,
    bindings: Arc<RwLock<BindingIndex>>,
    metrics: Arc<Mutex<TaskMetricIndex>>,
    validation_waiters: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl AgentTaskCoordinator {
    pub(crate) fn has_bindings(&self) -> bool {
        !self
            .bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_assignment
            .is_empty()
    }

    pub(crate) async fn initialize(
        &self,
        state_runtime: Arc<StateRuntime>,
        root_session_id: String,
    ) -> StoreResult<()> {
        let store = self
            .store
            .get_or_try_init(|| async move {
                let store = LocalAgentTaskStore::initialize(state_runtime.as_ref()).await?;
                Ok::<Arc<dyn AgentTaskStore>, StoreError>(Arc::new(store))
            })
            .await?
            .clone();
        let initialized_root_session_id = self
            .root_session_id
            .get_or_init(|| async { root_session_id.clone() })
            .await;
        if initialized_root_session_id != &root_session_id {
            return Err(StoreError::CorruptData(
                "agent task coordinator was initialized for a different root session".to_string(),
            ));
        }
        let persisted = store
            .list_agent_task_bindings(initialized_root_session_id.clone(), /*limit*/ None)
            .await?;
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for binding in persisted {
            bindings
                .by_agent_path
                .insert(binding.agent_path.clone(), binding.clone());
            bindings
                .by_assignment
                .insert(binding.assignment_id, binding);
        }
        Ok(())
    }

    pub(crate) async fn initialize_for_workspace_coordination(
        &self,
        state_runtime: Option<Arc<StateRuntime>>,
        sqlite_home: PathBuf,
        default_provider: String,
        root_session_id: String,
    ) -> StoreResult<()> {
        let state_runtime = match state_runtime {
            Some(state_runtime) => state_runtime,
            None => self
                .fallback_state_runtime
                .get_or_try_init(|| async move {
                    StateRuntime::init(sqlite_home, default_provider)
                        .await
                        .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))
                })
                .await?
                .clone(),
        };
        self.initialize(state_runtime, root_session_id).await
    }

    pub(crate) fn store(&self) -> Option<Arc<dyn AgentTaskStore>> {
        self.store.get().cloned()
    }

    pub(crate) fn root_session_id(&self) -> Option<String> {
        self.root_session_id.get().cloned()
    }

    pub(crate) fn initialize_metric_capacity(&self, max_threads: usize) {
        let capacity = u32::try_from(max_threads).unwrap_or(u32::MAX).max(1);
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        metrics.configured_capacity.get_or_insert(capacity);
    }

    pub(crate) fn record_first_meaningful_progress_once(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        task_started_at: &DateTime<Utc>,
        progress_created_at: &DateTime<Utc>,
        session_telemetry: &SessionTelemetry,
    ) -> bool {
        if !kind.is_meaningful_progress() {
            return false;
        }
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if metrics.first_progress_attempts.contains(&attempt_id)
            || metrics.first_progress_attempts.len() >= MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES
        {
            return false;
        }
        metrics.first_progress_attempts.insert(attempt_id);
        session_telemetry.counter(
            "codex.multi_agent.first_meaningful_progress",
            1,
            &[("kind", observation_metric_label(kind))],
        );
        session_telemetry.record_duration(
            TASK_FIRST_MEANINGFUL_PROGRESS_DURATION_METRIC,
            task_progress_duration(task_started_at, progress_created_at),
            &[("kind", observation_metric_label(kind))],
        );
        true
    }

    pub(crate) fn record_root_receipt_hydration_once(
        &self,
        attempt_id: AttemptId,
        session_telemetry: &SessionTelemetry,
    ) -> bool {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if metrics.hydrated_receipt_attempts.contains(&attempt_id)
            || metrics.hydrated_receipt_attempts.len() >= MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES
        {
            return false;
        }
        metrics.hydrated_receipt_attempts.insert(attempt_id);
        session_telemetry.counter(
            "codex.multi_agent.root_evidence_hydration",
            1,
            &[("evidence", "sealed_child_receipt")],
        );
        true
    }

    #[cfg(test)]
    pub(crate) async fn create_assignment(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        let store = self.required_store()?;
        store
            .register_workspace_actor(
                repo_root,
                WorkspaceActorRegistration {
                    root_session_id: draft.root_session_id.clone(),
                    actor_id: format!("root:{}", draft.root_session_id),
                    kind: WorkspaceActorKind::Root,
                    assignment_id: None,
                    attempt_id: None,
                    strategy: WorkspaceStrategy::Shared,
                },
            )
            .await?;
        let (assignment, attempt) = store.create_assignment(repo_root, draft).await?;
        self.start_task_metrics(&assignment);
        Ok((assignment, attempt))
    }

    pub(crate) async fn create_admitted_assignment(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
        isolated_integrator_available: bool,
    ) -> StoreResult<AdmittedAssignment> {
        let store = self.required_store()?;
        store
            .register_workspace_actor(
                repo_root,
                WorkspaceActorRegistration {
                    root_session_id: draft.root_session_id.clone(),
                    actor_id: format!("root:{}", draft.root_session_id),
                    kind: WorkspaceActorKind::Root,
                    assignment_id: None,
                    attempt_id: None,
                    strategy: WorkspaceStrategy::Shared,
                },
            )
            .await?;
        let admitted = store
            .create_admitted_assignment(repo_root, draft, isolated_integrator_available)
            .await?;
        self.start_task_metrics(&admitted.assignment);
        Ok(admitted)
    }

    pub(crate) async fn bind_agent_task(
        &self,
        draft: AgentTaskBindingDraft,
    ) -> StoreResult<AgentTaskBinding> {
        let binding = self.required_store()?.bind_agent_task(draft).await?;
        self.remember_binding(binding.clone());
        self.set_task_metric_active(binding.assignment_id, true);
        Ok(binding)
    }

    pub(crate) async fn remove_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<bool> {
        let removed = self
            .required_store()?
            .remove_agent_task_binding(TaskActor::Root, assignment_id)
            .await?;
        self.forget_binding(assignment_id);
        self.mark_task_inactive(assignment_id);
        Ok(removed)
    }

    pub(crate) fn binding_for_source(
        &self,
        session_source: &SessionSource,
    ) -> Option<AgentTaskBinding> {
        session_source
            .get_agent_path()
            .and_then(|path| self.binding_for_agent_path(&path))
    }

    pub(crate) async fn heartbeat_typed_actor_binding(
        &self,
        binding: &AgentTaskBinding,
    ) -> StoreResult<bool> {
        self.required_store()?
            .heartbeat_typed_workspace_actor(binding.clone())
            .await
    }

    pub(crate) fn binding_for_agent_path(
        &self,
        agent_path: &AgentPath,
    ) -> Option<AgentTaskBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_agent_path
            .get(agent_path.as_str())
            .cloned()
    }

    pub(crate) fn binding_for_assignment(
        &self,
        assignment_id: AssignmentId,
    ) -> Option<AgentTaskBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_assignment
            .get(&assignment_id)
            .cloned()
    }

    pub(crate) async fn get_agent_task(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> StoreResult<AgentTask> {
        self.required_store()?
            .get_agent_task(assignment_id, observation_limit)
            .await
    }

    pub(crate) async fn get_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<Option<AgentTaskBinding>> {
        if let Some(binding) = self.binding_for_assignment(assignment_id) {
            return Ok(Some(binding));
        }

        self.refresh_binding(assignment_id).await
    }

    pub(crate) async fn refresh_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<Option<AgentTaskBinding>> {
        let binding = self
            .required_store()?
            .get_agent_task_binding(assignment_id)
            .await?;
        if let Some(binding) = &binding {
            self.remember_binding(binding.clone());
        }
        Ok(binding)
    }

    #[cfg(test)]
    pub(crate) async fn begin_focused_validation_for_source(
        &self,
        session_source: &SessionSource,
        call_id: String,
        command_summary: String,
        resolved_executable: String,
    ) -> StoreResult<Option<FocusedValidationToken>> {
        self.begin_focused_validation_for_source_with_evidence(
            session_source,
            call_id,
            command_summary,
            resolved_executable,
            ValidationEvidence::default(),
        )
        .await
    }

    pub(crate) async fn begin_focused_validation_for_source_with_evidence(
        &self,
        session_source: &SessionSource,
        call_id: String,
        command_summary: String,
        resolved_executable: String,
        evidence: ValidationEvidence,
    ) -> StoreResult<Option<FocusedValidationToken>> {
        let Some(binding) = self.binding_for_source(session_source) else {
            return Ok(None);
        };
        let mut token = FocusedValidationToken {
            assignment_id: binding.assignment_id,
            attempt_id: binding.attempt_id,
            call_id,
            command_summary,
            resolved_executable,
            evidence,
        };
        let store = self.required_store()?;
        store
            .record_validation_call(ValidationCall {
                call_id: token.call_id.clone(),
                attempt_id: token.attempt_id,
                command_summary: token.command_summary.clone(),
                resolved_executable: Some(token.resolved_executable.clone()),
                proof_kind: ValidationProofKind::Focused,
                evidence: token.evidence.clone(),
                status: ValidationCallStatus::Running,
                recorded_at: Utc::now(),
            })
            .await?;
        let persisted = store
            .get_validation_call(token.call_id.clone())
            .await?
            .ok_or_else(|| {
                StoreError::CorruptData(format!(
                    "focused validation {} disappeared after persistence",
                    token.call_id
                ))
            })?;
        token.evidence = persisted.evidence;
        Ok(Some(token))
    }

    pub(crate) async fn get_validation_call(
        &self,
        call_id: String,
    ) -> StoreResult<Option<ValidationCall>> {
        self.required_store()?.get_validation_call(call_id).await
    }

    pub(crate) fn notify_validation_call(&self, call_id: &str) {
        let waiter = self
            .validation_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned();
        if let Some(waiter) = waiter {
            waiter.notify_waiters();
        }
    }

    /// Waits for a persisted validation call without model-visible polling.
    ///
    /// The notifier is installed before the first authoritative read so a
    /// terminal transition cannot be missed. Deadline wakeup returns the last
    /// authoritative store state to the caller for bounded recovery.
    pub(crate) async fn wait_for_validation_call_terminal(
        &self,
        call_id: &str,
        cancellation: &CancellationToken,
        deadline: chrono::DateTime<Utc>,
    ) -> StoreResult<Option<ValidationCall>> {
        let waiter = {
            let mut waiters = self
                .validation_waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            waiters
                .entry(call_id.to_string())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        loop {
            let current = self.get_validation_call(call_id.to_string()).await?;
            if current
                .as_ref()
                .is_some_and(|call| call.status.is_terminal())
            {
                self.validation_waiters
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(call_id);
                return Ok(current);
            }
            let Ok(remaining) = (deadline - Utc::now()).to_std() else {
                return Ok(current);
            };
            tokio::select! {
                _ = waiter.notified() => {}
                _ = cancellation.cancelled() => return Ok(None),
                _ = tokio::time::sleep(remaining) => {
                    return self.get_validation_call(call_id.to_string()).await;
                }
            }
        }
    }

    pub(crate) async fn heartbeat_validation_call(
        &self,
        call_id: String,
        lease_expires_at: chrono::DateTime<Utc>,
    ) -> StoreResult<bool> {
        self.required_store()?
            .heartbeat_validation_call(call_id, lease_expires_at)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn finish_focused_validation(
        &self,
        token: FocusedValidationToken,
        status: ValidationCallStatus,
    ) -> StoreResult<()> {
        self.finish_focused_validation_with_output(token, status, None, None)
            .await
    }

    pub(crate) async fn finish_focused_validation_with_output(
        &self,
        token: FocusedValidationToken,
        status: ValidationCallStatus,
        retained_output_ref: Option<String>,
        output_summary: Option<String>,
    ) -> StoreResult<()> {
        self.finish_focused_validation_with_result(
            token,
            status,
            retained_output_ref,
            output_summary,
            None,
        )
        .await
    }

    pub(crate) async fn finish_focused_validation_with_result(
        &self,
        token: FocusedValidationToken,
        status: ValidationCallStatus,
        retained_output_ref: Option<String>,
        output_summary: Option<String>,
        validation_result: Option<serde_json::Value>,
    ) -> StoreResult<()> {
        if !status.is_terminal() {
            return Err(StoreError::InvalidAssignment(
                "focused validation finish requires a terminal status".to_string(),
            ));
        }
        let store = self.required_store()?;
        let task = store.get_agent_task(token.assignment_id, Some(0)).await?;
        if task.current_attempt.attempt_id != token.attempt_id {
            return Err(StoreError::AttemptNotActive(token.attempt_id));
        }
        let mut evidence = token.evidence;
        evidence.retained_output_ref = retained_output_ref;
        evidence.output_summary = output_summary;
        evidence.validation_result = validation_result;
        let call_id = token.call_id.clone();
        store
            .record_validation_call(ValidationCall {
                call_id: token.call_id,
                attempt_id: token.attempt_id,
                command_summary: token.command_summary,
                resolved_executable: Some(token.resolved_executable),
                proof_kind: ValidationProofKind::Focused,
                evidence,
                status,
                recorded_at: Utc::now(),
            })
            .await?;
        self.notify_validation_call(&call_id);
        Ok(())
    }

    pub(crate) fn record_task_usage_for_source(
        &self,
        session_source: &SessionSource,
        tokens: u64,
        calls: u64,
    ) -> bool {
        let Some(binding) = self.binding_for_source(session_source) else {
            return false;
        };
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(runtime) = metrics.runtimes.get_mut(&binding.assignment_id) else {
            return false;
        };
        if let Err(error) = runtime.record_usage(tokens, calls) {
            warn!(
                assignment_id = %binding.assignment_id,
                ?error,
                "failed to record typed-task token usage"
            );
            return false;
        }
        true
    }

    pub(crate) fn mark_task_inactive(&self, assignment_id: AssignmentId) {
        self.set_task_metric_active(assignment_id, false);
    }

    pub(crate) async fn maybe_emit_terminal_metrics(
        &self,
        assignment_id: AssignmentId,
        session_telemetry: &SessionTelemetry,
    ) {
        let task = match self.get_agent_task(assignment_id, Some(0)).await {
            Ok(task) => task,
            Err(error) => {
                warn!(
                    %assignment_id,
                    ?error,
                    "failed to load typed task for terminal metrics"
                );
                return;
            }
        };
        if !terminal_metrics_ready(&task) {
            return;
        }

        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut runtime) = metrics.runtimes.remove(&assignment_id) else {
            return;
        };
        metrics.active.remove(&assignment_id);
        transition_metric_runtimes(&mut metrics);
        match runtime.finish_and_emit(&task, session_telemetry) {
            Ok(true) => {}
            Ok(false) => {
                warn!(%assignment_id, "typed-task metrics were already terminal");
            }
            Err(error) => {
                warn!(
                    %assignment_id,
                    ?error,
                    "failed to emit terminal typed-task metrics"
                );
                metrics.runtimes.insert(assignment_id, runtime);
            }
        }
    }

    pub(crate) async fn seal_missing_receipt(
        &self,
        agent_path: &AgentPath,
        summary: String,
    ) -> StoreResult<Option<AgentReceipt>> {
        let Some(binding) = self.binding_for_agent_path(agent_path) else {
            return Ok(None);
        };
        let store = self.required_store()?;
        let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
        if task.current_attempt.attempt_id != binding.attempt_id
            || task.receipt.is_some()
            || task.current_attempt.state != AttemptState::Active
        {
            return Ok(None);
        }
        if let Err(error) = store.finalize_pending_mutations(binding.attempt_id).await {
            if binding_no_longer_needs_receipt(store.as_ref(), &binding).await? {
                return Ok(None);
            }
            tracing::warn!(
                %error,
                attempt_id = %binding.attempt_id,
                "typed mutation evidence finalization was unavailable; sealing the missing receipt anyway"
            );
        }
        let receipt = ReceiptDraft {
            status: AgentStatusClaim::NeedsMain,
            summary,
            criterion_results: task
                .assignment
                .acceptance_criteria
                .iter()
                .map(|criterion| CriterionResult {
                    criterion_id: criterion.id.clone(),
                    status: CriterionStatus::NotRun,
                    evidence: None,
                })
                .collect(),
            declared_changes: Vec::new(),
            validation_call_ids: Vec::new(),
            blockers: vec!["typed agent finished without a valid receipt".to_string()],
            risks: Vec::new(),
            next_action: Some(
                "main agent must inspect the task and decide the outcome".to_string(),
            ),
            architecture_contract: None,
        };
        match store
            .submit_agent_receipt(binding.attempt_id, receipt)
            .await
        {
            Ok(receipt) => {
                self.mark_task_inactive(binding.assignment_id);
                Ok(Some(receipt))
            }
            Err(error) => {
                if binding_no_longer_needs_receipt(store.as_ref(), &binding).await? {
                    self.mark_task_inactive(binding.assignment_id);
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn required_store(&self) -> StoreResult<Arc<dyn AgentTaskStore>> {
        self.store().ok_or_else(|| {
            StoreError::CorruptData(
                "typed agent task store is unavailable for this legacy or uninitialized session"
                    .to_string(),
            )
        })
    }

    fn remember_binding(&self, binding: AgentTaskBinding) {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        bindings
            .by_agent_path
            .insert(binding.agent_path.clone(), binding.clone());
        bindings
            .by_assignment
            .insert(binding.assignment_id, binding);
    }

    fn forget_binding(&self, assignment_id: AssignmentId) {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(binding) = bindings.by_assignment.remove(&assignment_id) else {
            return;
        };
        if bindings
            .by_agent_path
            .get(&binding.agent_path)
            .is_some_and(|candidate| candidate.assignment_id == assignment_id)
        {
            bindings.by_agent_path.remove(&binding.agent_path);
        }
    }

    fn start_task_metrics(&self, assignment: &Assignment) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active_turns = saturating_active_turns(metrics.active.len());
        let capacity = metric_capacity(&metrics, active_turns);
        match TaskMetricRuntime::new(assignment, active_turns, capacity) {
            Ok(runtime) => {
                metrics.runtimes.insert(assignment.assignment_id, runtime);
            }
            Err(error) => {
                warn!(
                    assignment_id = %assignment.assignment_id,
                    ?error,
                    "failed to initialize typed-task metrics"
                );
            }
        }
    }

    fn set_task_metric_active(&self, assignment_id: AssignmentId, active: bool) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !metrics.runtimes.contains_key(&assignment_id) {
            return;
        }
        let changed = if active {
            metrics.active.insert(assignment_id)
        } else {
            metrics.active.remove(&assignment_id)
        };
        if changed {
            transition_metric_runtimes(&mut metrics);
        }
    }
}

fn transition_metric_runtimes(metrics: &mut TaskMetricIndex) {
    let active_turns = saturating_active_turns(metrics.active.len());
    let capacity = metric_capacity(metrics, active_turns);
    for (assignment_id, runtime) in &mut metrics.runtimes {
        if let Err(error) = runtime.transition_concurrency(active_turns, capacity) {
            warn!(
                %assignment_id,
                ?error,
                "failed to update typed-task concurrency metrics"
            );
        }
    }
}

fn saturating_active_turns(active: usize) -> u32 {
    u32::try_from(active).unwrap_or(u32::MAX)
}

fn metric_capacity(metrics: &TaskMetricIndex, active_turns: u32) -> u32 {
    metrics.configured_capacity.unwrap_or(1).max(active_turns)
}

fn task_progress_duration(
    task_started_at: &DateTime<Utc>,
    progress_created_at: &DateTime<Utc>,
) -> std::time::Duration {
    progress_created_at
        .signed_duration_since(task_started_at)
        .to_std()
        .unwrap_or_default()
}

const fn observation_metric_label(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::Accepted => "accepted",
        ObservationKind::Starting => "starting",
        ObservationKind::Reading => "reading",
        ObservationKind::Editing => "editing",
        ObservationKind::Reviewing => "reviewing",
        ObservationKind::Validating => "validating",
        ObservationKind::Blocked => "blocked",
        ObservationKind::ToolCall => "tool_call",
        ObservationKind::Mutation => "mutation",
        ObservationKind::GateChanged => "gate_changed",
        ObservationKind::ReceiptSealed => "receipt_sealed",
        ObservationKind::Completed => "completed",
        ObservationKind::NeedsMain => "needs_main",
        ObservationKind::Violated => "violated",
        ObservationKind::Abandoned => "abandoned",
    }
}

async fn binding_no_longer_needs_receipt(
    store: &dyn AgentTaskStore,
    binding: &AgentTaskBinding,
) -> StoreResult<bool> {
    let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
    Ok(task.current_attempt.attempt_id != binding.attempt_id
        || task.receipt.is_some()
        || task.current_attempt.state != AttemptState::Active)
}

#[cfg(test)]
#[path = "task_coordinator_tests.rs"]
mod tests;
