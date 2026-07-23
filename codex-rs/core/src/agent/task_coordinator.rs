use chrono::Utc;
use codex_agent_task_store::AgentReceipt;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::StoreResult;
use codex_agent_task_store::TaskActor;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::ValidationCallStatus;
use codex_otel::SessionTelemetry;
use codex_protocol::AgentPath;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use tokio::sync::OnceCell;
use tracing::warn;

use super::task_metrics::TaskMetricRuntime;
use super::task_metrics::terminal_metrics_ready;

const MAX_RESTART_BINDINGS: usize = 256;
#[derive(Default)]
struct BindingIndex {
    by_agent_path: HashMap<String, AgentTaskBinding>,
    by_assignment: HashMap<AssignmentId, AgentTaskBinding>,
}

#[derive(Default)]
struct TaskMetricIndex {
    runtimes: HashMap<AssignmentId, TaskMetricRuntime>,
    active: HashSet<AssignmentId>,
    configured_capacity: Option<u32>,
}

/// Shared typed-task persistence and identity index for one root agent tree.
///
/// The coordinator is cloned with [`super::AgentControl`], so every child resolves the same
/// assignment/attempt identity. Legacy agents simply have no binding and bypass this layer.
#[derive(Clone, Default)]
pub(crate) struct AgentTaskCoordinator {
    store: Arc<OnceCell<Arc<dyn AgentTaskStore>>>,
    root_session_id: Arc<OnceCell<String>>,
    bindings: Arc<RwLock<BindingIndex>>,
    metrics: Arc<Mutex<TaskMetricIndex>>,
}

impl AgentTaskCoordinator {
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
            .list_agent_task_bindings(
                initialized_root_session_id.clone(),
                Some(MAX_RESTART_BINDINGS),
            )
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

    pub(crate) async fn create_assignment(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        let (assignment, attempt) = self
            .required_store()?
            .create_assignment(repo_root, draft)
            .await?;
        self.start_task_metrics(&assignment);
        Ok((assignment, attempt))
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

    pub(crate) async fn record_validation_call_for_source(
        &self,
        session_source: &SessionSource,
        call_id: String,
        command_summary: String,
        status: ValidationCallStatus,
    ) -> StoreResult<bool> {
        let Some(binding) = self.binding_for_source(session_source) else {
            return Ok(false);
        };
        self.required_store()?
            .record_validation_call(ValidationCall {
                call_id,
                attempt_id: binding.attempt_id,
                command_summary,
                status,
                recorded_at: Utc::now(),
            })
            .await?;
        Ok(true)
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
            return Err(error);
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
