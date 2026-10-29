use chrono::DateTime;
use chrono::Utc;
use codex_agent_task_store::AdmittedAssignment;
use codex_agent_task_store::AgentReceipt;
use codex_agent_task_store::AgentStatusClaim;
use codex_agent_task_store::AgentTask;
use codex_agent_task_store::AgentTaskAuthorization;
use codex_agent_task_store::AgentTaskBinding;
use codex_agent_task_store::AgentTaskBindingDraft;
use codex_agent_task_store::Assignment;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AssignmentDraft;
use codex_agent_task_store::AssignmentId;
use codex_agent_task_store::Attempt;
use codex_agent_task_store::AttemptAmendment;
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
use codex_otel::SessionTelemetry;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use tokio::sync::OnceCell;
use tracing::warn;

use super::task_metrics::TASK_FIRST_MEANINGFUL_PROGRESS_DURATION_METRIC;
use super::task_metrics::TaskMetricRuntime;
use super::task_metrics::terminal_metrics_ready;

#[derive(Default)]
struct BindingIndex {
    by_agent_path: HashMap<String, AgentTaskBinding>,
    by_assignment: HashMap<AssignmentId, AgentTaskBinding>,
}

#[derive(Default)]
struct TaskMetricIndex {
    runtimes: HashMap<AssignmentId, TaskMetricRuntime>,
    active: HashSet<AssignmentId>,
    host_terminal_attempts: HashMap<AssignmentId, AttemptId>,
    first_progress_attempts: HashSet<AttemptId>,
    hydrated_receipt_attempts: HashSet<AttemptId>,
    configured_capacity: Option<u32>,
}

const MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES: usize = 4_096;

struct InitializedTaskStore {
    store: Arc<LocalAgentTaskStore>,
    root_session_id: String,
    state_runtime: Arc<StateRuntime>,
}

/// Per-call full-suite launch ownership shared within a validation task scope.
type SuiteBudgets =
    HashMap<(String, String), Arc<codex_command_analysis::command_preflight::FullSuiteBudget>>;

/// Last successful heartbeat renewal per assignment; each slot serializes one actor's renewals.
type HeartbeatReceipts =
    HashMap<AssignmentId, Arc<tokio::sync::Mutex<Option<TypedActorHeartbeatReceipt>>>>;

/// Trusted validation policy captured from the current root turn before child startup.
#[derive(Clone, Debug)]
pub(crate) struct ValidationTurnContext {
    pub(crate) scope: (String, String),
    pub(crate) mode: codex_protocol::validation::ValidationMode,
    /// Retains shared launch ownership while child initialization can suspend
    /// after its parent turn has stopped.
    pub(crate) budget: Arc<codex_command_analysis::command_preflight::FullSuiteBudget>,
}

/// Shared typed-task persistence and identity index for one root agent tree.
///
/// The coordinator is cloned with [`super::AgentControl`], so every child resolves the same
/// assignment/attempt identity. Legacy agents simply have no binding and bypass this layer.
#[derive(Clone, Default)]
pub(crate) struct AgentTaskCoordinator {
    store: Arc<OnceCell<InitializedTaskStore>>,
    fallback_state_runtime: Arc<OnceCell<Arc<StateRuntime>>>,
    binding_updates: Arc<tokio::sync::Mutex<()>>,
    bindings: Arc<RwLock<BindingIndex>>,
    metrics: Arc<Mutex<TaskMetricIndex>>,
    suite_budgets: Arc<Mutex<SuiteBudgets>>,
    unavailable_descendants: Arc<RwLock<HashMap<String, String>>>,
    /// Retained terminal outcomes this process already reconciled. The rows stay
    /// pending for their delivery owner; another pass would only rewrite the
    /// same idempotent evidence and bump the task revision.
    reconciled_terminal_outcomes: Arc<Mutex<HashSet<(AttemptId, String)>>>,
    heartbeat_receipts: Arc<Mutex<HeartbeatReceipts>>,
}

struct TypedActorHeartbeatReceipt {
    attempt_id: AttemptId,
    thread_id: Option<String>,
    renewed_at: tokio::time::Instant,
    progress: bool,
}

impl AgentTaskCoordinator {
    pub(crate) fn validation_budget_for_turn(
        &self,
        scope: &(String, String),
    ) -> Arc<codex_command_analysis::command_preflight::FullSuiteBudget> {
        let mut budgets = self.suite_budgets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep live root/child contexts, not every historical turn in a long session.
        budgets.retain(|key, budget| key == scope || Arc::strong_count(budget) > 1);
        Arc::clone(budgets.entry(scope.clone()).or_default())
    }

    pub(crate) fn record_unavailable_descendant(&self, thread: ThreadId, unavailable: bool) {
        let mut records = self.unavailable_descendants.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if unavailable {
            records.insert(thread.to_string(), "missing_or_archived_thread: retained graph obligation is unresolved; restore storage or explicitly resolve the obligation".into());
        } else {
            records.remove(&thread.to_string());
        }
    }

    pub(crate) fn unavailable_descendants(&self) -> Vec<(String, String)> {
        let mut records = self.unavailable_descendants.read().unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter().map(|(id, reason)| (id.clone(), reason.clone())).collect::<Vec<_>>();
        records.sort();
        records
    }

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
        let initialized = self
            .store
            .get_or_try_init(|| async {
                let store = LocalAgentTaskStore::initialize_shared(state_runtime.as_ref()).await?;
                let persisted = store
                    .list_agent_task_bindings(root_session_id.clone(), None)
                    .await?;
                // No store consumer can mutate bindings until this entire initializer is published.
                for binding in persisted {
                    self.remember_binding(binding);
                }
                Ok::<_, StoreError>(InitializedTaskStore {
                    store,
                    root_session_id: root_session_id.clone(),
                    state_runtime: Arc::clone(&state_runtime),
                })
            })
            .await?;
        if initialized.root_session_id != root_session_id {
            return Err(StoreError::RootSessionMismatch {
                expected: initialized.root_session_id.clone(),
                requested: root_session_id,
            });
        }
        self.reconcile_pending_terminal_outcomes().await?;
        Ok(())
    }

    pub(crate) async fn initialize_for_workspace_coordination(
        &self,
        state_runtime: Option<Arc<StateRuntime>>,
        sqlite_home: PathBuf,
        default_provider: String,
        root_session_id: String,
        ephemeral: bool,
    ) -> StoreResult<()> {
        // A missing session state runtime can mean intentionally nonpersistent,
        // not permission to create a separate disk-backed coordination owner.
        if ephemeral {
            return Err(StoreError::InvalidAssignment(
                "ephemeral sessions cannot initialize durable task coordination".to_string(),
            ));
        }
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

    pub(crate) fn store(&self) -> Option<Arc<LocalAgentTaskStore>> {
        self.store
            .get()
            .map(|initialized| initialized.store.clone())
    }

    pub(crate) fn root_session_id(&self) -> Option<String> {
        self.store
            .get()
            .map(|initialized| initialized.root_session_id.clone())
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


    pub(crate) async fn create_admitted_assignment(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
        isolated_integrator_available: bool,
        agent_path: String,
        task_name: String,
    ) -> StoreResult<AdmittedAssignment> {
        self.require_root_scope(&draft.root_session_id)?;
        let store = self.required_store()?;
        let admitted = store
            .create_admitted_assignment_for_agent(
                repo_root,
                draft,
                isolated_integrator_available,
                agent_path.clone(),
                task_name.clone(),
                self.store
                    .get()
                    .ok_or_else(|| StoreError::InvalidAssignment("task store unavailable".into()))?
                    .state_runtime
                    .agent_job_runner_instance_id()
                    .to_string(),
            )
            .await?;
        // No fallible await after commit: observer cancellation must leave the
        // same provisional identity available to launch rollback and maintenance.
        self.remember_binding(AgentTaskBinding {
            assignment_id: admitted.assignment.assignment_id,
            attempt_id: admitted.attempt.attempt_id,
            root_session_id: admitted.assignment.root_session_id.clone(),
            agent_path,
            task_name,
            thread_id: None,
            bound_at: admitted.attempt.created_at,
            updated_at: admitted.attempt.created_at,
        });
        self.start_task_metrics(&admitted.assignment);
        Ok(admitted)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize durable assignment recovery and binding cache invalidation"
    )]
    pub(crate) async fn recover_unstarted_assignments(&self) -> StoreResult<()> {
        self.reconcile_pending_terminal_outcomes().await?;
        let Some(initialized) = self.store.get() else {
            return Ok(());
        };
        let threshold = Utc::now()
            - chrono::Duration::seconds(codex_agent_task_store::DEFAULT_WORKSPACE_LEASE_SECONDS);
        for (assignment, owner) in initialized
            .store
            .unstarted_assignments(&initialized.root_session_id)
            .await?
        {
            if let Some(owner) = owner.as_deref()
                && initialized
                    .state_runtime
                    .agent_job_runner_instance_is_live(owner)
                    .await
                    .map_err(|error| StoreError::CorruptData(error.to_string()))?
            {
                continue;
            }
            let _update = self.binding_updates.lock().await;
            let recovered = initialized
                .store
                .recover_unstarted_assignment(assignment, threshold, owner)
                .await?;
            if matches!(recovered, codex_agent_task_store::NonproductiveRecovery::Recovered { .. }) {
                self.forget_binding(assignment);
            }
        }
        Ok(())
    }

    async fn reconcile_pending_terminal_outcomes(&self) -> StoreResult<()> {
        let Some(initialized) = self.store.get() else {
            return Ok(());
        };
        for pending in initialized
            .store
            .pending_terminal_outcomes(&initialized.root_session_id)
            .await?
        {
            let pending = match pending {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::warn!(%error, "invalid terminal record retained; continuing unrelated recovery");
                    continue;
                }
            };
            let key = (pending.binding.attempt_id, pending.turn_id.clone());
            if self
                .reconciled_terminal_outcomes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key)
            {
                continue;
            }
            let binding = pending.binding;
            let result: StoreResult<()> = async {
                let status: AgentStatus = serde_json::from_str(&pending.status_json)?;
                let event: codex_protocol::protocol::EventMsg = serde_json::from_str(&pending.event_json)?;
                let path = AgentPath::try_from(binding.agent_path.as_str())
                    .map_err(StoreError::InvalidAssignment)?;
                let thread = binding
                    .thread_id
                    .as_deref()
                    .and_then(|id| ThreadId::from_string(id).ok())
                    .ok_or_else(|| {
                        StoreError::CorruptData("pending terminal has no thread".into())
                    })?;
                let summary = match &event {
                    codex_protocol::protocol::EventMsg::TurnComplete(event) => event.timing.as_ref()
                        .and_then(|timing| timing.completion_assessment.as_ref())
                        .and_then(|assessment| assessment.bounded_summary(&event.turn_id))
                        .unwrap_or_else(|| crate::agent::status::NO_COMPLETION_DIAGNOSTICS.to_string()),
                    codex_protocol::protocol::EventMsg::TurnAborted(_) => format!(
                        "Host terminal boundary for turn {}: turn aborted; partial effects may remain.", pending.turn_id),
                    _ => return Err(StoreError::CorruptData("pending terminal event is not terminal".into())),
                };
                self.record_host_completion_and_seal(&binding, &path, thread, &pending.turn_id, summary, &status).await?;
                self.record_host_completion_observation(&binding, thread,
                    &format!("{}:parent-delivery", pending.turn_id),
                    format!("Completion retained across recovery; parent delivery is unacknowledged. Read get_agent_task for assignment {} attempt {}; do not replay the child.", binding.assignment_id, binding.attempt_id), None).await?;
                // Receipt recovery is not parent delivery. Retain the original
                // event until the delivery owner acknowledges it explicitly.
                Ok(())
            }
            .await;
            let settled = match result {
                Ok(()) => true,
                // A superseded attempt can never accept this evidence.
                Err(error @ StoreError::AttemptNotActive(_)) => {
                    tracing::warn!(attempt_id = %binding.attempt_id, %error,
                        "terminal outcome belongs to a superseded attempt; retained without further reconciliation");
                    true
                }
                Err(error) => {
                    tracing::warn!(attempt_id = %binding.attempt_id, %error,
                        "terminal reconciliation remains pending; continuing unrelated recovery");
                    false
                }
            };
            if settled {
                let mut reconciled = self
                    .reconciled_terminal_outcomes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if reconciled.len() < MAX_DIAGNOSTIC_ATTEMPT_IDENTITIES {
                    reconciled.insert(key);
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn retain_terminal_outcome(
        &self,
        binding: AgentTaskBinding,
        turn_id: String,
        status: &AgentStatus,
        event: &codex_protocol::protocol::EventMsg,
    ) -> StoreResult<bool> {
        self.require_root_scope(&binding.root_session_id)?;
        self.required_store()?
            .retain_terminal_outcome(
                binding,
                turn_id,
                serde_json::to_string(status)?,
                serde_json::to_string(event)?,
            )
            .await
    }

    pub(crate) async fn acknowledge_terminal_outcome(&self, attempt: AttemptId, turn_id: &str) -> StoreResult<()> {
        self.required_store()?
            .acknowledge_terminal_outcome(attempt, turn_id)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize durable binding writes and their cache publication"
    )]
    pub(crate) async fn bind_agent_task(
        &self,
        draft: AgentTaskBindingDraft,
    ) -> StoreResult<AgentTaskBinding> {
        let _update = self.binding_updates.lock().await;
        self.get_agent_task_authorization(draft.assignment_id)
            .await?;
        let binding = self.required_store()?.bind_agent_task(draft).await?;
        self.remember_binding(binding.clone());
        self.set_task_metric_active(binding.assignment_id, true);
        Ok(binding)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize amendment commit and binding cache publication"
    )]
    pub(crate) async fn amend_agent_task(
        &self,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> StoreResult<Attempt> {
        let _update = self.binding_updates.lock().await;
        let (attempt, binding) = self
            .required_store()?
            .amend_agent_task_with_binding(self.root_actor()?, assignment_id, amendment)
            .await?;
        if let Some(binding) = binding {
            self.remember_binding(binding);
        } else {
            self.forget_binding(assignment_id);
        }
        self.set_task_metric_active(assignment_id, true);
        Ok(attempt)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize durable binding removal and cache invalidation"
    )]
    pub(crate) async fn remove_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<bool> {
        let _update = self.binding_updates.lock().await;
        let terminal_attempt = self
            .binding_for_assignment(assignment_id)
            .map(|binding| binding.attempt_id);
        let removed = self
            .required_store()?
            .remove_agent_task_binding(self.root_actor()?, assignment_id)
            .await?;
        if let Some(attempt) = terminal_attempt {
            self.mark_host_terminal(assignment_id, attempt);
        }
        self.forget_binding(assignment_id);
        self.mark_task_inactive(assignment_id);
        Ok(removed)
    }

    /// Resolve authority against the attempt captured at turn entry, never a
    /// correction attempt rebound while this turn's calls are still in flight.
    pub(crate) fn binding_for_turn(
        &self,
        turn: &crate::session::turn_context::TurnContext,
    ) -> Option<AgentTaskBinding> {
        turn.agent_task_binding
            .get_or_init(|| self.binding_for_source(&turn.session_source))
            .clone()
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
        progress: bool,
    ) -> StoreResult<bool> {
        self.heartbeat_typed_actor_binding_inner(binding, progress, true).await
    }

    pub(crate) async fn heartbeat_typed_actor_binding_if_due(
        &self,
        binding: &AgentTaskBinding,
        progress: bool,
    ) -> StoreResult<bool> {
        self.heartbeat_typed_actor_binding_inner(binding, progress, false).await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Single-flight renewal per actor so concurrent callers share one receipt"
    )]
    async fn heartbeat_typed_actor_binding_inner(
        &self,
        binding: &AgentTaskBinding,
        progress: bool,
        force: bool,
    ) -> StoreResult<bool> {
        let slot = Arc::clone(self.heartbeat_receipts.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(binding.assignment_id).or_default());
        // Share successful renewals per exact actor, not a root-wide I/O lock.
        let mut receipt = slot.lock().await;
        if !force && receipt.as_ref().is_some_and(|prior|
            prior.attempt_id == binding.attempt_id && prior.thread_id == binding.thread_id
                && (!progress || prior.progress)
                && prior.renewed_at.elapsed() < std::time::Duration::from_secs(30))
        {
            return Ok(true);
        }
        let result = self.required_store()?
            .heartbeat_typed_workspace_actor(binding.clone(), progress).await;
        *receipt = matches!(&result, Ok(true)).then(|| TypedActorHeartbeatReceipt {
            attempt_id: binding.attempt_id,
            thread_id: binding.thread_id.clone(),
            renewed_at: tokio::time::Instant::now(),
            progress,
        });
        result
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

    pub(crate) fn bindings_for_recovery(&self) -> Vec<AgentTaskBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_assignment
            .values()
            .cloned()
            .collect()
    }

    pub(crate) async fn get_agent_task(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> StoreResult<AgentTask> {
        let task = self
            .required_store()?
            .get_agent_task(assignment_id, observation_limit)
            .await?;
        self.require_root_scope(&task.assignment.root_session_id)?;
        Ok(task)
    }

    pub(crate) async fn record_host_completion_and_seal(
        &self,
        binding: &AgentTaskBinding,
        agent_path: &AgentPath,
        thread_id: ThreadId,
        turn_id: &str,
        summary: String,
        status: &AgentStatus,
    ) -> StoreResult<()> {
        if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str()) {
            return Ok(());
        }
        let store = self.required_store()?;
        if matches!(status, AgentStatus::Completed(_) | AgentStatus::CompletedWithSurface { .. }) {
            let mut publication = store.record_host_completion_deferred(
                binding.attempt_id, format!("{thread_id}:{turn_id}"), summary,
            ).await?;
            let result = self.seal_missing_receipt_with_host_completion(
                agent_path, thread_id, status, binding.attempt_id, Some(&mut publication),
            ).await;
            // An existing receipt or unresolved seal still needs its actual
            // host evidence. This is idempotent after an atomic publication.
            let published = publication.publish().await;
            match result {
                Ok(_) => published,
                Err(error) => {
                    if let Err(publication_error) = published {
                        warn!(%publication_error, "host completion fallback remains owned");
                    }
                    Err(error)
                }
            }
        } else {
            // Failures, stops and cancellation are actionable immediately.
            self.record_host_completion_observation(binding, thread_id, turn_id, summary, Some(status)).await?;
            self.seal_missing_receipt_for_attempt(agent_path, thread_id, status, binding.attempt_id).await?;
            Ok(())
        }
    }

    pub(crate) async fn record_host_completion_observation(
        &self,
        binding: &AgentTaskBinding,
        thread_id: ThreadId,
        turn_id: &str,
        summary: String,
        status: Option<&AgentStatus>,
    ) -> StoreResult<()> {
        if binding.thread_id.as_deref() != Some(thread_id.to_string().as_str()) {
            return Ok(());
        }
        let cause = match status {
            Some(AgentStatus::Stopped { stop_cause, .. }) => serde_json::to_value(stop_cause)?
                .as_str()
                .map(str::to_owned),
            _ => None,
        };
        self.required_store()?
            .record_host_completion_with_stop_cause(
                binding.attempt_id,
                format!("{thread_id}:{turn_id}"),
                summary,
                cause,
            )
            .await
    }

    pub(crate) async fn get_agent_task_authorization(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<AgentTaskAuthorization> {
        let authorization = self
            .required_store()?
            .get_agent_task_authorization(assignment_id)
            .await?;
        self.require_root_scope(&authorization.root_session_id)?;
        Ok(authorization)
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize durable binding reads and cache publication against updates"
    )]
    pub(crate) async fn refresh_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<Option<AgentTaskBinding>> {
        let _update = self.binding_updates.lock().await;
        let binding = self
            .required_store()?
            .get_agent_task_binding(assignment_id)
            .await?;
        if let Some(binding) = &binding {
            self.require_root_scope(&binding.root_session_id)?;
            self.remember_binding(binding.clone());
        } else {
            self.forget_binding(assignment_id);
        }
        Ok(binding)
    }

    pub(crate) fn root_actor(&self) -> StoreResult<TaskActor> {
        let root = self.root_session_id().ok_or_else(|| {
            StoreError::InvalidAssignment("task coordinator is not initialized".to_string())
        })?;
        Ok(TaskActor::RootSession(root))
    }

    fn require_root_scope(&self, requested: &str) -> StoreResult<()> {
        let expected = self.root_session_id().ok_or_else(|| {
            StoreError::InvalidAssignment("task coordinator is not initialized".to_string())
        })?;
        if expected != requested {
            return Err(StoreError::RootSessionMismatch {
                expected,
                requested: requested.to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn record_task_usage_for_source(
        &self,
        session_source: &SessionSource,
        tokens: crate::agent::task_metrics::TokenClassTotals,
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

    pub(crate) fn mark_host_terminal(&self, assignment_id: AssignmentId, attempt_id: AttemptId) {
        if self
            .binding_for_assignment(assignment_id)
            .is_some_and(|binding| binding.attempt_id != attempt_id)
        {
            return;
        }
        self.mark_task_inactive(assignment_id);
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if metrics.runtimes.contains_key(&assignment_id) {
            metrics
                .host_terminal_attempts
                .insert(assignment_id, attempt_id);
        }
    }

    pub(crate) fn has_task_metric_runtime(&self, assignment_id: AssignmentId) -> bool {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtimes
            .contains_key(&assignment_id)
    }

    pub(crate) async fn maybe_emit_terminal_metrics(
        &self,
        assignment_id: AssignmentId,
        session_telemetry: &SessionTelemetry,
    ) {
        if !self.has_task_metric_runtime(assignment_id) {
            return;
        }
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
        self.emit_terminal_metrics_from_snapshot(&task, session_telemetry);
    }

    /// The caller may already own the sealed outcome used for parent delivery.
    /// Reuse that snapshot, not a second store read; attempt/readiness and
    /// exactly-once runtime retirement checks still apply below.
    pub(crate) fn emit_terminal_metrics_from_snapshot(
        &self,
        task: &AgentTask,
        session_telemetry: &SessionTelemetry,
    ) {
        let assignment_id = task.assignment.assignment_id;
        if !terminal_metrics_ready(task) {
            return;
        }

        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if metrics.host_terminal_attempts.get(&assignment_id)
            != Some(&task.current_attempt.attempt_id)
        {
            return;
        }
        let Some(mut runtime) = metrics.runtimes.remove(&assignment_id) else {
            return;
        };
        if task.current_attempt.ordinal < runtime.minimum_attempt_ordinal {
            metrics.runtimes.insert(assignment_id, runtime);
            return;
        }
        if metrics.active.remove(&assignment_id) {
            transition_metric_runtimes(&mut metrics);
        }
        match runtime.finish(task) {
            Ok(Some(finalized)) => {
                metrics.host_terminal_attempts.remove(&assignment_id);
                drop(metrics);
                finalized.emit(session_telemetry);
            }
            Ok(None) => {
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize follow-up renewal and durable binding cache publication"
    )]
    pub(crate) async fn prepare_legacy_agent_turn(
        &self,
        source: &SessionSource,
        thread_id: ThreadId,
    ) -> StoreResult<()> {
        // Root sources cannot have a binding. Avoid an unrelated sibling's
        // durable binding transaction for this immutable no-op case.
        if source.get_agent_path().is_none() {
            return Ok(());
        }
        let _update = self.binding_updates.lock().await;
        let Some(binding) = self
            .binding_for_source(source)
            .filter(|binding| binding.thread_id.as_deref() == Some(thread_id.to_string().as_str()))
        else {
            return Ok(());
        };
        let store = self.required_store()?;
        let authorization = store
            .get_agent_task_authorization(binding.assignment_id)
            .await?;
        if authorization.current_attempt.attempt_id == binding.attempt_id
            && authorization.current_attempt.state == AttemptState::Active
        {
            if !self.has_task_metric_runtime(binding.assignment_id) {
                let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
                self.activate_task_metric_attempt(&task, true);
            }
            return Ok(());
        }
        // Typed attempts cannot be renewed implicitly. Reject before sampling,
        // rather than spending a model request whose first tool must be blocked.
        if !matches!(
            authorization.admission_origin,
            AssignmentAdmissionOrigin::LegacyMessage { .. }
        ) {
            return Err(StoreError::InvalidAssignment(format!(
                "typed assignment {} has no active bound attempt; recover its retained receipt with get_agent_task, or explicitly admit new work before requesting another turn",
                binding.assignment_id
            )));
        }
        store.begin_legacy_agent_turn(binding.assignment_id).await?;
        let renewed = store
            .get_agent_task_binding(binding.assignment_id)
            .await?
            .ok_or_else(|| {
                StoreError::CorruptData(
                    "plain-message follow-up lost its durable binding".to_string(),
                )
            })?;
        self.remember_binding(renewed);
        let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
        self.activate_task_metric_attempt(&task, false);
        Ok(())
    }

    fn activate_task_metric_attempt(&self, task: &AgentTask, restored_active_attempt: bool) {
        let mut metrics = self
            .metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active_turns = saturating_active_turns(metrics.active.len());
        let capacity = metric_capacity(&metrics, active_turns);
        // Only new deltas enter a recreated recorder. Previous emitted attempts
        // are not replayed, including when this coordinator was restored.
        if let std::collections::hash_map::Entry::Vacant(entry) =
            metrics.runtimes.entry(task.assignment.assignment_id)
        {
            match TaskMetricRuntime::new(&task.assignment, active_turns, capacity) {
                Ok(runtime) => {
                    entry.insert(runtime);
                }
                Err(error) => {
                    warn!(?error, "failed to recreate task metrics");
                    return;
                }
            }
        }
        let Some(runtime) = metrics.runtimes.get_mut(&task.assignment.assignment_id) else {
            return;
        };
        runtime.minimum_attempt_ordinal = task.current_attempt.ordinal;
        runtime.usage_history_partial |= restored_active_attempt;
        if metrics.active.insert(task.assignment.assignment_id) {
            transition_metric_runtimes(&mut metrics);
        }
    }


    pub(crate) async fn seal_missing_receipt_for_attempt(
        &self,
        agent_path: &AgentPath,
        expected_thread_id: ThreadId,
        status: &AgentStatus,
        expected_attempt_id: AttemptId,
    ) -> StoreResult<Option<AgentReceipt>> {
        self.seal_missing_receipt_with_host_completion(
            agent_path, expected_thread_id, status, expected_attempt_id, None,
        ).await
    }

    async fn seal_missing_receipt_with_host_completion(
        &self,
        agent_path: &AgentPath,
        expected_thread_id: ThreadId,
        status: &AgentStatus,
        expected_attempt_id: AttemptId,
        mut host_completion: Option<&mut codex_agent_task_store::DeferredHostCompletionWake>,
    ) -> StoreResult<Option<AgentReceipt>> {
        // Retain the complete identity across awaits; a reused path must never select a new child.
        let Some(binding) = self.binding_for_agent_path(agent_path).filter(|binding| {
            binding.thread_id.as_deref() == Some(expected_thread_id.to_string().as_str())
                && binding.attempt_id == expected_attempt_id
        }) else {
            return Ok(None);
        };
        let store = self.required_store()?;
        let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
        if task.current_attempt.attempt_id != binding.attempt_id {
            return Ok(None);
        }
        if task.receipt.is_some() || task.current_attempt.state != AttemptState::Active {
            if let Some(publication) = host_completion.as_mut() {
                publication.publish().await?;
            }
            return Ok(None);
        }
        if super::status::is_final(status)
            && task
                .validation_calls
                .iter()
                .any(|call| !call.status.is_terminal())
        {
            // A running validation lease makes settlement non-immediate. Keep
            // the host boundary actionable without implying a receipt exists.
            if let Some(publication) = host_completion.as_mut() {
                publication.publish().await?;
            }
            // The terminal runtime no longer produces work. Reuse the store's
            // lease-fenced recovery: unexpired operations suspend settlement;
            // expired operations become cancelled, never successful evidence.
            if let codex_agent_task_store::NonproductiveRecovery::Recovered { receipt, .. } = store
                .recover_nonproductive_assignment(binding.assignment_id, Utc::now())
                .await?
            {
                self.mark_task_inactive(binding.assignment_id);
                return Ok(Some(*receipt));
            }
        }
        let validation_call_ids = task
            .validation_calls
            .iter()
            .filter(|call| call.attempt_id == binding.attempt_id && call.status.is_terminal())
            .map(|call| call.call_id.clone())
            .collect();
        let legacy_outcome = matches!(
            task.assignment.admission_origin,
            AssignmentAdmissionOrigin::LegacyMessage { .. }
        );
        let (outcome, summary) = match status {
            AgentStatus::Stopped {
                last_agent_message,
                stop_cause,
            } => (
                AgentStatusClaim::NeedsMain,
                format!(
                    "Execution stopped: {stop_cause:?}. {}",
                    last_agent_message
                        .as_deref()
                        .unwrap_or("No authored report was delivered.")
                ),
            ),
            AgentStatus::Completed(Some(message)) if !message.trim().is_empty() => {
                (AgentStatusClaim::Completed, message.clone())
            }
            AgentStatus::CompletedWithSurface {
                last_agent_message, ..
            } => (
                AgentStatusClaim::Completed,
                last_agent_message
                    .clone()
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or_else(|| "Agent returned a surfaced result.".to_string()),
            ),
            AgentStatus::Errored(message) => (AgentStatusClaim::Failed, message.clone()),
            AgentStatus::Shutdown => (
                AgentStatusClaim::Abandoned,
                "Agent shut down before returning a final response.".to_string(),
            ),
            _ => (
                AgentStatusClaim::NeedsMain,
                "Agent stopped without a final response.".to_string(),
            ),
        };
        let mut receipt = ReceiptDraft {
            status: AgentStatusClaim::NeedsMain,
            summary: format!(
                "typed agent {agent_path}, attempt {}, stopped with execution outcome {outcome:?} without submitting a receipt: {summary}", binding.attempt_id
            ),
            criterion_results: task
                .current_attempt
                .amendment
                .as_ref()
                .and_then(|amendment| amendment.acceptance_criteria.as_deref())
                .unwrap_or(&task.assignment.acceptance_criteria)
                .iter()
                .map(|criterion| CriterionResult {
                    evidence_ref: None,
                    criterion_id: criterion.id.clone(),
                    status: CriterionStatus::NotRun,
                    evidence: Some("No criterion report was submitted; acceptance is unverified. Recorded validation calls remain linked separately.".to_string()),
                })
                .collect(),
            declared_changes: Vec::new(),
            validation_call_ids,
            blockers: vec!["typed agent finished without a valid receipt".to_string()],
            risks: Vec::new(),
            next_action: Some(
                format!("Inspect assignment {} attempt {} and the linked validation calls; check evidence freshness and supply the missing criterion report before accepting the result.", binding.assignment_id, binding.attempt_id),
            ),
            architecture_contract: None,
        };
        let result = if let Some(publication) = host_completion {
            if legacy_outcome {
                receipt.status = outcome;
                receipt.summary = summary;
                // Match record_legacy_agent_outcome: a host final response is
                // lifecycle evidence, not a typed validation/acceptance report.
                receipt.validation_call_ids.clear();
                receipt.blockers.clear();
                receipt.next_action = None;
            }
            publication.seal_receipt(receipt, legacy_outcome).await
        } else if legacy_outcome {
            store
                .record_legacy_agent_outcome(binding.attempt_id, outcome, summary)
                .await
        } else {
            store
                .submit_agent_receipt(binding.attempt_id, receipt)
                .await
        };
        match result {
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

    fn required_store(&self) -> StoreResult<Arc<LocalAgentTaskStore>> {
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
        if let Some(previous) = bindings.by_assignment.remove(&binding.assignment_id) {
            bindings.by_agent_path.remove(&previous.agent_path);
        }
        if let Some(previous) = bindings.by_agent_path.remove(&binding.agent_path) {
            bindings.by_assignment.remove(&previous.assignment_id);
        }
        bindings
            .by_agent_path
            .insert(binding.agent_path.clone(), binding.clone());
        bindings
            .by_assignment
            .insert(binding.assignment_id, binding);
    }

    fn forget_binding(&self, assignment_id: AssignmentId) {
        self.heartbeat_receipts.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).remove(&assignment_id);
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
        ObservationKind::HostCompletion => "host_completion",
        ObservationKind::Completed => "completed",
        ObservationKind::NeedsMain => "needs_main",
        ObservationKind::Violated => "violated",
        ObservationKind::Abandoned => "abandoned",
    }
}

async fn binding_no_longer_needs_receipt(
    store: &LocalAgentTaskStore,
    binding: &AgentTaskBinding,
) -> StoreResult<bool> {
    let task = store.get_agent_task(binding.assignment_id, Some(0)).await?;
    Ok(task.current_attempt.attempt_id != binding.attempt_id
        || task.receipt.is_some()
        || task.current_attempt.state != AttemptState::Active)
}

