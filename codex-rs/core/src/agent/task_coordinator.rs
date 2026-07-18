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
use codex_agent_task_store::AttemptId;
use codex_agent_task_store::AttemptState;
use codex_agent_task_store::CriterionResult;
use codex_agent_task_store::CriterionStatus;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_agent_task_store::ObservationKind;
use codex_agent_task_store::ReceiptDraft;
use codex_agent_task_store::RuntimeObservation;
use codex_agent_task_store::StoreError;
use codex_agent_task_store::StoreResult;
use codex_agent_task_store::ValidationCall;
use codex_agent_task_store::WakeEventId;
use codex_agent_task_store::WakeRead;
use codex_protocol::AgentPath;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::OnceCell;

const MAX_RESTART_BINDINGS: usize = 256;

#[derive(Default)]
struct BindingIndex {
    by_agent_path: HashMap<String, AgentTaskBinding>,
    by_assignment: HashMap<AssignmentId, AgentTaskBinding>,
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
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    pub(crate) fn is_available(&self) -> bool {
        self.store.get().is_some()
    }

    pub(crate) fn store(&self) -> Option<Arc<dyn AgentTaskStore>> {
        self.store.get().cloned()
    }

    pub(crate) fn root_session_id(&self) -> Option<String> {
        self.root_session_id.get().cloned()
    }

    pub(crate) async fn create_assignment(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        self.required_store()?
            .create_assignment(repo_root, draft)
            .await
    }

    pub(crate) async fn bind_agent_task(
        &self,
        draft: AgentTaskBindingDraft,
    ) -> StoreResult<AgentTaskBinding> {
        let binding = self.required_store()?.bind_agent_task(draft).await?;
        self.remember_binding(binding.clone());
        Ok(binding)
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    pub(crate) async fn list_agent_task_bindings(
        &self,
        limit: Option<usize>,
    ) -> StoreResult<Vec<AgentTaskBinding>> {
        let bindings = self
            .required_store()?
            .list_agent_task_bindings(self.required_root_session_id()?, limit)
            .await?;
        for binding in &bindings {
            self.remember_binding(binding.clone());
        }
        Ok(bindings)
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

    pub(crate) async fn append_observation(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> StoreResult<RuntimeObservation> {
        self.required_store()?
            .append_observation(attempt_id, kind, summary, call_id)
            .await
    }

    pub(crate) async fn record_validation_call(&self, call: ValidationCall) -> StoreResult<()> {
        self.required_store()?.record_validation_call(call).await
    }

    pub(crate) async fn read_wake_events(
        &self,
        after_event_id: Option<&str>,
    ) -> StoreResult<WakeRead> {
        let after_event_id = after_event_id.map(WakeEventId::parse).transpose()?;
        self.required_store()?
            .read_wake_events(self.required_root_session_id()?, after_event_id)
            .await
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
        if task.receipt.is_some() || task.current_attempt.state != AttemptState::Active {
            return Ok(None);
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
        store
            .submit_agent_receipt(binding.attempt_id, receipt)
            .await
            .map(Some)
    }

    fn required_store(&self) -> StoreResult<Arc<dyn AgentTaskStore>> {
        self.store().ok_or_else(|| {
            StoreError::CorruptData(
                "typed agent task store is unavailable for this legacy or uninitialized session"
                    .to_string(),
            )
        })
    }

    fn required_root_session_id(&self) -> StoreResult<String> {
        self.root_session_id().ok_or_else(|| {
            StoreError::CorruptData(
                "typed agent root session is unavailable for this legacy or uninitialized session"
                    .to_string(),
            )
        })
    }

    fn remember_binding(&self, binding: AgentTaskBinding) {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        bindings
            .by_agent_path
            .insert(binding.agent_path.clone(), binding.clone());
        bindings
            .by_assignment
            .insert(binding.assignment_id, binding);
    }
}
