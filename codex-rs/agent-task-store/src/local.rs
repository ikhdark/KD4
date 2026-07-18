use chrono::Utc;
use codex_state::StateRuntime;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::collections::HashSet;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::AgentGate;
use crate::AgentReceipt;
use crate::AgentRole;
use crate::AgentStatusClaim;
use crate::AgentTask;
use crate::AgentTaskBinding;
use crate::AgentTaskBindingDraft;
use crate::Assignment;
use crate::AssignmentDraft;
use crate::AssignmentId;
use crate::Attempt;
use crate::AttemptAmendment;
use crate::AttemptId;
use crate::AttemptState;
use crate::AttributionConfidence;
use crate::CriterionStatus;
use crate::DEFAULT_BINDING_LIMIT;
use crate::DEFAULT_MUTATION_EVIDENCE_LIMIT;
use crate::DEFAULT_SNAPSHOT_CHUNK_BYTES;
use crate::DependencyBlocker;
use crate::DependencyState;
use crate::GateKind;
use crate::GateStatus;
use crate::MAX_BINDING_LIMIT;
use crate::MAX_MUTATION_EVIDENCE_LIMIT;
use crate::MAX_OBSERVATION_LIMIT;
use crate::MAX_SNAPSHOT_CHUNK_BYTES;
use crate::MAX_WAKE_EVENTS_PER_READ;
use crate::MAX_WAKE_EVENTS_PER_ROOT;
use crate::MutationEventId;
use crate::MutationEvidence;
use crate::MutationSnapshotChunk;
use crate::MutationSnapshotVersion;
use crate::ObservationKind;
use crate::ReceiptDraft;
use crate::RepoScope;
use crate::RuntimeObservation;
use crate::StoreError;
use crate::StoreResult;
use crate::TaskActor;
use crate::TaskStoreFuture;
use crate::ValidationCall;
use crate::WakeEvent;
use crate::WakeEventId;
use crate::WakeRead;
use crate::WriteClaimConflict;
use crate::scope::RepositoryIdentity;
use crate::scope::absolute_repo_path;
use crate::scope::normalize_repo_path;
use crate::scope::repository_identity;

const COORDINATION_DIR: &str = "agent-task-coordination";
const DATABASE_FILENAME: &str = "agent_tasks.sqlite";
const NONEXISTENT_SENTINEL: &[u8] = b"CODEX_AGENT_TASK_STORE_NONEXISTENT\n";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct LocalAgentTaskStore {
    pool: SqlitePool,
    coordination_root: Arc<PathBuf>,
}

impl std::fmt::Debug for LocalAgentTaskStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalAgentTaskStore")
            .field("storage", &"private coordination storage")
            .finish_non_exhaustive()
    }
}

impl LocalAgentTaskStore {
    pub async fn initialize(state_runtime: &StateRuntime) -> StoreResult<Self> {
        let coordination_root = state_runtime.codex_home().join(COORDINATION_DIR);
        tokio::fs::create_dir_all(&coordination_root).await?;
        let database_path = coordination_root.join(DATABASE_FILENAME);
        let options = SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        let store = Self {
            pool,
            coordination_root: Arc::new(coordination_root),
        };
        store.rebuild_wake_streams().await?;
        Ok(store)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn validate_dependencies(
        &self,
        candidate_id: AssignmentId,
        dependencies: &[AssignmentId],
    ) -> StoreResult<()> {
        self.validate_dependencies_impl(candidate_id, None, dependencies)
            .await
    }

    async fn validate_dependencies_impl(
        &self,
        candidate_id: AssignmentId,
        repository_id: Option<&str>,
        dependencies: &[AssignmentId],
    ) -> StoreResult<()> {
        let mut transaction = self.pool.begin().await?;
        validate_dependencies_tx(&mut transaction, candidate_id, repository_id, dependencies)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn create_assignment_impl(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        let repository = repository_identity(repo_root)?;
        let assignment = draft.normalize(repo_root)?;
        if assignment.repository_id != repository.id {
            return Err(StoreError::InvalidScope(
                "repository root changed while the assignment was normalized".to_string(),
            ));
        }
        let attempt = Attempt {
            attempt_id: AttemptId::new(),
            assignment_id: assignment.assignment_id,
            ordinal: 0,
            amendment: None,
            state: AttemptState::Active,
            created_at: Utc::now(),
            sealed_at: None,
        };
        let mut transaction = self.pool.begin().await?;
        // The assignment insert acquires SQLite's writer lock before dependency and claim
        // validation. Any validation failure rolls the row back, so no dormant task is exposed.
        sqlx::query(
            "INSERT INTO assignments (assignment_id, root_session_id, body_json, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(assignment.assignment_id.to_string())
        .bind(&assignment.root_session_id)
        .bind(encode(&assignment)?)
        .bind(encode(&assignment.created_at)?)
        .execute(&mut *transaction)
        .await?;
        sqlx::query("INSERT INTO assignment_repositories (assignment_id, repository_id, canonical_root, bound_at) VALUES (?, ?, ?, ?)")
            .bind(assignment.assignment_id.to_string())
            .bind(&repository.id)
            .bind(&repository.canonical_path)
            .bind(encode(&assignment.created_at)?)
            .execute(&mut *transaction)
            .await?;
        validate_dependencies_tx(
            &mut transaction,
            assignment.assignment_id,
            Some(&assignment.repository_id),
            &assignment.dependencies,
        )
        .await?;
        let (supersedes, conflicts) =
            plan_write_claim_tx(&mut transaction, &assignment, None).await?;
        if !conflicts.is_empty() {
            return Err(StoreError::WriteClaimConflict { conflicts });
        }
        insert_attempt(&mut transaction, &attempt).await?;
        for superseded in &supersedes {
            sqlx::query("UPDATE write_claims SET active = 0, released_at = ?, superseded_by = ? WHERE assignment_id = ? AND active = 1")
                .bind(encode(&Utc::now())?)
                .bind(assignment.assignment_id.to_string())
                .bind(superseded.to_string())
                .execute(&mut *transaction)
                .await?;
        }
        if !assignment.write_scope.is_empty() {
            sqlx::query("INSERT INTO write_claims (assignment_id, attempt_id, scopes_json, supersedes_json, active, created_at) VALUES (?, ?, ?, ?, 1, ?)")
                .bind(assignment.assignment_id.to_string())
                .bind(attempt.attempt_id.to_string())
                .bind(encode(&assignment.write_scope)?)
                .bind(encode(&supersedes)?)
                .bind(encode(&attempt.created_at)?)
                .execute(&mut *transaction)
                .await?;
        }
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::Accepted,
            "typed assignment accepted".to_string(),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok((assignment, attempt))
    }

    async fn get_agent_task_impl(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> StoreResult<AgentTask> {
        let limit = observation_limit.unwrap_or(crate::DEFAULT_OBSERVATION_LIMIT);
        if limit > MAX_OBSERVATION_LIMIT {
            return Err(StoreError::InvalidObservationLimit(limit));
        }
        let mut transaction = self.pool.begin().await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let current_attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        let receipt =
            sqlx::query_scalar::<_, String>("SELECT body_json FROM receipts WHERE attempt_id = ?")
                .bind(current_attempt.attempt_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?
                .map(|value| decode(&value))
                .transpose()?;
        let gate_rows =
            sqlx::query("SELECT body_json FROM gates WHERE assignment_id = ? ORDER BY kind")
                .bind(assignment_id.to_string())
                .fetch_all(&mut *transaction)
                .await?;
        let gates = gate_rows
            .into_iter()
            .map(|row| decode(row.get::<String, _>("body_json").as_str()))
            .collect::<StoreResult<Vec<_>>>()?;
        let mut observations = if limit == 0 {
            Vec::new()
        } else {
            let rows = sqlx::query("SELECT body_json FROM observations WHERE assignment_id = ? ORDER BY sequence DESC LIMIT ?")
                .bind(assignment_id.to_string())
                .bind(limit as i64)
                .fetch_all(&mut *transaction)
                .await?;
            rows.into_iter()
                .map(|row| decode(row.get::<String, _>("body_json").as_str()))
                .collect::<StoreResult<Vec<_>>>()?
        };
        observations.reverse();
        transaction.commit().await?;
        Ok(AgentTask {
            assignment,
            current_attempt,
            gates,
            receipt,
            observations,
        })
    }

    async fn bind_agent_task_impl(
        &self,
        draft: AgentTaskBindingDraft,
    ) -> StoreResult<AgentTaskBinding> {
        if draft.agent_path.trim().is_empty() || draft.task_name.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "agent path and task name cannot be empty".to_string(),
            ));
        }
        if draft
            .thread_id
            .as_deref()
            .is_some_and(|thread_id| thread_id.trim().is_empty())
        {
            return Err(StoreError::InvalidAssignment(
                "thread id cannot be empty when present".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, draft.attempt_id).await?;
        let current = require_active_current_attempt_tx(&mut transaction, draft.attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, current.assignment_id).await?;
        if assignment.assignment_id != draft.assignment_id {
            return Err(StoreError::AttemptNotActive(draft.attempt_id));
        }

        let existing = sqlx::query("SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at FROM agent_task_bindings WHERE assignment_id = ?")
            .bind(draft.assignment_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| binding_from_row(&row))
            .transpose()?;
        if existing.as_ref().is_some_and(|binding| {
            binding.agent_path != draft.agent_path || binding.task_name != draft.task_name
        }) {
            return Err(StoreError::InvalidAssignment(
                "agent path and task name are immutable for a bound assignment".to_string(),
            ));
        }
        if existing.as_ref().is_some_and(|binding| {
            draft.thread_id.as_ref().is_some_and(|thread_id| {
                binding
                    .thread_id
                    .as_ref()
                    .is_some_and(|existing| existing != thread_id)
            })
        }) {
            return Err(StoreError::InvalidAssignment(
                "thread id is immutable once a task is bound to a thread".to_string(),
            ));
        }
        let conflict = sqlx::query_scalar::<_, String>("SELECT assignment_id FROM agent_task_bindings WHERE root_session_id = ? AND agent_path = ? AND assignment_id <> ?")
            .bind(&assignment.root_session_id)
            .bind(&draft.agent_path)
            .bind(draft.assignment_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
        if conflict.is_some() {
            return Err(StoreError::InvalidAssignment(
                "agent path is already bound in this root session".to_string(),
            ));
        }
        if let Some(thread_id) = &draft.thread_id {
            let conflict = sqlx::query_scalar::<_, String>("SELECT assignment_id FROM agent_task_bindings WHERE root_session_id = ? AND thread_id = ? AND assignment_id <> ?")
                .bind(&assignment.root_session_id)
                .bind(thread_id)
                .bind(draft.assignment_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?;
            if conflict.is_some() {
                return Err(StoreError::InvalidAssignment(
                    "thread id is already bound in this root session".to_string(),
                ));
            }
        }
        let now = Utc::now();
        let binding = AgentTaskBinding {
            assignment_id: draft.assignment_id,
            attempt_id: draft.attempt_id,
            root_session_id: assignment.root_session_id,
            agent_path: draft.agent_path,
            task_name: draft.task_name,
            thread_id: draft.thread_id.or_else(|| {
                existing
                    .as_ref()
                    .and_then(|binding| binding.thread_id.clone())
            }),
            bound_at: existing
                .as_ref()
                .map(|binding| binding.bound_at)
                .unwrap_or(now),
            updated_at: now,
        };
        sqlx::query("INSERT INTO agent_task_bindings (assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(assignment_id) DO UPDATE SET attempt_id = excluded.attempt_id, thread_id = excluded.thread_id, updated_at = excluded.updated_at")
            .bind(binding.assignment_id.to_string())
            .bind(binding.attempt_id.to_string())
            .bind(&binding.root_session_id)
            .bind(&binding.agent_path)
            .bind(&binding.task_name)
            .bind(&binding.thread_id)
            .bind(encode(&binding.bound_at)?)
            .bind(encode(&binding.updated_at)?)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(binding)
    }

    async fn get_agent_task_binding_impl(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<Option<AgentTaskBinding>> {
        let mut transaction = self.pool.begin().await?;
        load_assignment_tx(&mut transaction, assignment_id).await?;
        let binding = sqlx::query("SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at FROM agent_task_bindings WHERE assignment_id = ?")
            .bind(assignment_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| binding_from_row(&row))
            .transpose()?;
        transaction.commit().await?;
        Ok(binding)
    }

    async fn list_agent_task_bindings_impl(
        &self,
        root_session_id: String,
        limit: Option<usize>,
    ) -> StoreResult<Vec<AgentTaskBinding>> {
        if root_session_id.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "root session id cannot be empty".to_string(),
            ));
        }
        let limit = limit.unwrap_or(DEFAULT_BINDING_LIMIT);
        if limit > MAX_BINDING_LIMIT {
            return Err(StoreError::InvalidBindingLimit(limit));
        }
        let rows = sqlx::query("SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at FROM agent_task_bindings WHERE root_session_id = ? ORDER BY updated_at DESC, agent_path LIMIT ?")
            .bind(root_session_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(|row| binding_from_row(&row)).collect()
    }

    async fn append_observation_impl(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> StoreResult<RuntimeObservation> {
        if summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "observation summary cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        let observation = append_observation_tx(
            &mut transaction,
            &assignment,
            attempt_id,
            kind,
            summary,
            call_id,
        )
        .await?;
        transaction.commit().await?;
        Ok(observation)
    }

    async fn record_validation_call_impl(&self, call: ValidationCall) -> StoreResult<()> {
        if call.call_id.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "validation call id cannot be empty".to_string(),
            ));
        }
        if call.command_summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "validation command summary cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, call.attempt_id).await?;
        require_active_current_attempt_tx(&mut transaction, call.attempt_id).await?;
        if let Some(row) = sqlx::query(
            "SELECT attempt_id, body_json, status FROM validation_calls WHERE call_id = ?",
        )
        .bind(&call.call_id)
        .fetch_optional(&mut *transaction)
        .await?
        {
            if row.get::<String, _>("attempt_id") != call.attempt_id.to_string() {
                return Err(StoreError::ValidationCallOwnership {
                    call_ids: vec![call.call_id],
                });
            }
            let existing: ValidationCall = decode(row.get::<String, _>("body_json").as_str())?;
            let stored_status: crate::ValidationCallStatus =
                decode(row.get::<String, _>("status").as_str())?;
            if existing.attempt_id != call.attempt_id || existing.status != stored_status {
                return Err(StoreError::CorruptData(format!(
                    "validation call {} has inconsistent persisted identity or status",
                    call.call_id
                )));
            }
            if existing == call {
                transaction.commit().await?;
                return Ok(());
            }
            if existing.status.is_terminal()
                || !call.status.is_terminal()
                || existing.command_summary != call.command_summary
                || call.recorded_at < existing.recorded_at
            {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
            let result = sqlx::query("UPDATE validation_calls SET body_json = ?, status = ?, recorded_at = ? WHERE call_id = ? AND attempt_id = ? AND status = ?")
                .bind(encode(&call)?)
                .bind(encode(&call.status)?)
                .bind(encode(&call.recorded_at)?)
                .bind(&call.call_id)
                .bind(call.attempt_id.to_string())
                .bind(encode(&crate::ValidationCallStatus::Running)?)
                .execute(&mut *transaction)
                .await?;
            if result.rows_affected() != 1 {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
        } else {
            sqlx::query("INSERT INTO validation_calls (call_id, attempt_id, body_json, status, recorded_at) VALUES (?, ?, ?, ?, ?)")
                .bind(&call.call_id)
                .bind(call.attempt_id.to_string())
                .bind(encode(&call)?)
                .bind(encode(&call.status)?)
                .bind(encode(&call.recorded_at)?)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn submit_agent_receipt_impl(
        &self,
        attempt_id: AttemptId,
        draft: ReceiptDraft,
    ) -> StoreResult<AgentReceipt> {
        if draft.summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "receipt summary cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = load_attempt_tx(&mut transaction, attempt_id).await?;
        if attempt.state.is_terminal() || attempt.sealed_at.is_some() {
            return Err(StoreError::AttemptSealed(attempt_id));
        }
        let current = load_current_attempt_tx(&mut transaction, attempt.assignment_id).await?;
        if current.attempt_id != attempt_id {
            return Err(StoreError::AttemptNotActive(attempt_id));
        }
        if sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM receipts WHERE attempt_id = ?")
            .bind(attempt_id.to_string())
            .fetch_one(&mut *transaction)
            .await?
            != 0
        {
            return Err(StoreError::ReceiptAlreadySealed(attempt_id));
        }
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        validate_criterion_results(&assignment, attempt.amendment.as_ref(), &draft)?;
        let mut invalid_calls = Vec::new();
        let mut invalid_statuses = Vec::new();
        let mut seen_calls = HashSet::new();
        for call_id in &draft.validation_call_ids {
            if !seen_calls.insert(call_id.as_str()) {
                invalid_calls.push(call_id.clone());
                continue;
            }
            let call_row = sqlx::query(
                "SELECT attempt_id, body_json, status FROM validation_calls WHERE call_id = ?",
            )
            .bind(call_id)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(call_row) = call_row else {
                invalid_calls.push(call_id.clone());
                continue;
            };
            if call_row.get::<String, _>("attempt_id") != attempt_id.to_string() {
                invalid_calls.push(call_id.clone());
                continue;
            }
            let call: ValidationCall = decode(call_row.get::<String, _>("body_json").as_str())?;
            let stored_status: crate::ValidationCallStatus =
                decode(call_row.get::<String, _>("status").as_str())?;
            if call.attempt_id != attempt_id || call.status != stored_status {
                return Err(StoreError::CorruptData(format!(
                    "validation call {call_id} has inconsistent persisted identity or status"
                )));
            }
            if !call.status.is_terminal()
                || draft.status == AgentStatusClaim::Completed && !call.status.is_success()
            {
                invalid_statuses.push(call_id.clone());
            }
        }
        if !invalid_calls.is_empty() {
            return Err(StoreError::ValidationCallOwnership {
                call_ids: invalid_calls,
            });
        }
        if !invalid_statuses.is_empty() {
            return Err(StoreError::ValidationCallStatusInvalid {
                call_ids: invalid_statuses,
            });
        }
        let receipt = AgentReceipt {
            assignment_id: attempt.assignment_id,
            attempt_id,
            status: draft.status,
            summary: draft.summary,
            criterion_results: draft.criterion_results,
            declared_changes: draft.declared_changes,
            validation_call_ids: draft.validation_call_ids,
            blockers: draft.blockers,
            risks: draft.risks,
            next_action: draft.next_action,
            sealed_at: Utc::now(),
        };
        let state = receipt.status.attempt_state();
        sqlx::query("INSERT INTO receipts (attempt_id, assignment_id, status, body_json, sealed_at) VALUES (?, ?, ?, ?, ?)")
            .bind(attempt_id.to_string())
            .bind(attempt.assignment_id.to_string())
            .bind(encode(&receipt.status)?)
            .bind(encode(&receipt)?)
            .bind(encode(&receipt.sealed_at)?)
            .execute(&mut *transaction)
            .await?;
        let updated = sqlx::query(
            "UPDATE attempts SET state = ?, sealed_at = ? WHERE attempt_id = ? AND state = ?",
        )
        .bind(encode(&state)?)
        .bind(encode(&receipt.sealed_at)?)
        .bind(attempt_id.to_string())
        .bind(encode(&AttemptState::Active)?)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::AttemptSealed(attempt_id));
        }
        if !receipt.status.is_success()
            || pending_gate_count(&mut transaction, attempt.assignment_id).await? == 0
        {
            release_claim(&mut transaction, attempt.assignment_id, None).await?;
        }
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt_id,
            receipt_observation_kind(receipt.status),
            "agent receipt sealed".to_string(),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(receipt)
    }

    async fn amend_agent_task_impl(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> StoreResult<Attempt> {
        actor.require_root()?;
        amendment.validate()?;
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        if assignment.role != AgentRole::Worker {
            return Err(StoreError::WorkerCorrectionRequired(assignment_id));
        }
        let current = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if current.ordinal != 0 {
            return Err(StoreError::AmendmentLimitReached(assignment_id));
        }
        if !current.state.is_terminal() {
            return Err(StoreError::InvalidAssignment(
                "the original attempt must be sealed before amendment".to_string(),
            ));
        }
        let changes_requested = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM gates WHERE assignment_id = ? AND kind = ? AND status = ?",
        )
        .bind(assignment_id.to_string())
        .bind(encode(&GateKind::Review)?)
        .bind(encode(&GateStatus::ChangesRequested)?)
        .fetch_one(&mut *transaction)
        .await?
            != 0;
        if !changes_requested {
            return Err(StoreError::InvalidAssignment(
                "a correction attempt requires a changes_requested review gate".to_string(),
            ));
        }
        let next = Attempt {
            attempt_id: AttemptId::new(),
            assignment_id,
            ordinal: 1,
            amendment: Some(amendment),
            state: AttemptState::Active,
            created_at: Utc::now(),
            sealed_at: None,
        };
        if !assignment.write_scope.is_empty() {
            let claim =
                sqlx::query("SELECT attempt_id, active FROM write_claims WHERE assignment_id = ?")
                    .bind(assignment_id.to_string())
                    .fetch_optional(&mut *transaction)
                    .await?;
            if claim.as_ref().is_none_or(|claim| {
                claim.get::<String, _>("attempt_id") != current.attempt_id.to_string()
            }) {
                return Err(StoreError::WriteClaimInactive(assignment_id));
            }
            let (_, conflicts) =
                plan_write_claim_tx(&mut transaction, &assignment, Some(assignment_id)).await?;
            if !conflicts.is_empty() {
                return Err(StoreError::WriteClaimConflict { conflicts });
            }
        }
        insert_attempt(&mut transaction, &next).await?;
        let binding_updated = sqlx::query(
            "UPDATE agent_task_bindings SET attempt_id = ?, updated_at = ? WHERE assignment_id = ? AND attempt_id = ?",
        )
        .bind(next.attempt_id.to_string())
        .bind(encode(&next.created_at)?)
        .bind(assignment_id.to_string())
        .bind(current.attempt_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if binding_updated.rows_affected() != 1 {
            return Err(StoreError::CorruptData(format!(
                "assignment {assignment_id} has no binding for current attempt {}",
                current.attempt_id
            )));
        }
        if !assignment.write_scope.is_empty() {
            let updated = sqlx::query("UPDATE write_claims SET attempt_id = ?, active = 1, released_at = NULL, superseded_by = NULL WHERE assignment_id = ? AND attempt_id = ?")
                .bind(next.attempt_id.to_string())
                .bind(assignment_id.to_string())
                .bind(current.attempt_id.to_string())
                .execute(&mut *transaction)
                .await?;
            if updated.rows_affected() != 1 {
                return Err(StoreError::WriteClaimInactive(assignment_id));
            }
        }
        append_observation_tx(
            &mut transaction,
            &assignment,
            next.attempt_id,
            ObservationKind::Accepted,
            "single correction attempt accepted".to_string(),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(next)
    }

    async fn abandon_agent_task_impl(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        reason: String,
    ) -> StoreResult<AgentReceipt> {
        actor.require_root()?;
        if reason.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "abandonment reason cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.state.is_terminal() || attempt.sealed_at.is_some() {
            return Err(StoreError::AttemptSealed(attempt.attempt_id));
        }
        let criterion_results = effective_criteria(&assignment, attempt.amendment.as_ref())
            .iter()
            .map(|criterion| crate::CriterionResult {
                criterion_id: criterion.id.clone(),
                status: CriterionStatus::NotRun,
                evidence: None,
            })
            .collect();
        let receipt = AgentReceipt {
            assignment_id,
            attempt_id: attempt.attempt_id,
            status: AgentStatusClaim::Abandoned,
            summary: reason,
            criterion_results,
            declared_changes: Vec::new(),
            validation_call_ids: Vec::new(),
            blockers: Vec::new(),
            risks: Vec::new(),
            next_action: None,
            sealed_at: Utc::now(),
        };
        sqlx::query("INSERT INTO receipts (attempt_id, assignment_id, status, body_json, sealed_at) VALUES (?, ?, ?, ?, ?)")
            .bind(attempt.attempt_id.to_string())
            .bind(assignment_id.to_string())
            .bind(encode(&receipt.status)?)
            .bind(encode(&receipt)?)
            .bind(encode(&receipt.sealed_at)?)
            .execute(&mut *transaction)
            .await?;
        let updated = sqlx::query(
            "UPDATE attempts SET state = ?, sealed_at = ? WHERE attempt_id = ? AND state = ?",
        )
        .bind(encode(&AttemptState::Abandoned)?)
        .bind(encode(&receipt.sealed_at)?)
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&AttemptState::Active)?)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::AttemptSealed(attempt.attempt_id));
        }
        release_claim(&mut transaction, assignment_id, None).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::Abandoned,
            "agent task abandoned by root".to_string(),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(receipt)
    }

    async fn set_agent_gate_impl(
        &self,
        assignment_id: AssignmentId,
        kind: GateKind,
        status: GateStatus,
        reason: String,
    ) -> StoreResult<AgentGate> {
        if reason.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "gate reason cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        if let Some(existing_json) = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
        )
        .bind(assignment_id.to_string())
        .bind(encode(&kind)?)
        .fetch_optional(&mut *transaction)
        .await?
        {
            let existing: AgentGate = decode(&existing_json)?;
            if existing.status.is_sealed() {
                return Err(StoreError::GateAlreadySealed {
                    gate: kind.to_string(),
                });
            }
        }
        let now = Utc::now();
        let gate = AgentGate {
            assignment_id,
            kind,
            status,
            reason,
            waiver_reason: None,
            updated_at: now,
            sealed_at: status.is_sealed().then_some(now),
        };
        sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(assignment_id, kind) DO UPDATE SET status = excluded.status, body_json = excluded.body_json, updated_at = excluded.updated_at, sealed_at = excluded.sealed_at")
            .bind(assignment_id.to_string())
            .bind(encode(&kind)?)
            .bind(encode(&status)?)
            .bind(encode(&gate)?)
            .bind(encode(&now)?)
            .bind(gate.sealed_at.map(|value| encode(&value)).transpose()?)
            .execute(&mut *transaction)
            .await?;
        release_successful_claim_if_unblocked_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::GateChanged,
            format!("{kind} gate is {status:?}"),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(gate)
    }

    async fn waive_agent_gate_impl(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        reason: String,
    ) -> StoreResult<AgentGate> {
        actor.require_root()?;
        if !kind.is_waivable() {
            return Err(StoreError::GateNotWaivable {
                gate: kind.to_string(),
            });
        }
        if reason.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "waiver reason cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let now = Utc::now();
        let gate = AgentGate {
            assignment_id,
            kind,
            status: GateStatus::Waived,
            reason: "root waived soft gate".to_string(),
            waiver_reason: Some(reason),
            updated_at: now,
            sealed_at: Some(now),
        };
        sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(assignment_id, kind) DO UPDATE SET status = excluded.status, body_json = excluded.body_json, updated_at = excluded.updated_at, sealed_at = excluded.sealed_at")
            .bind(assignment_id.to_string())
            .bind(encode(&kind)?)
            .bind(encode(&GateStatus::Waived)?)
            .bind(encode(&gate)?)
            .bind(encode(&now)?)
            .bind(encode(&now)?)
            .execute(&mut *transaction)
            .await?;
        release_successful_claim_if_unblocked_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::GateChanged,
            format!("{kind} gate is waived"),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(gate)
    }

    async fn read_wake_events_impl(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> StoreResult<WakeRead> {
        if root_session_id.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "root session id cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let Some(stream) = sqlx::query("SELECT next_sequence, retained_from_sequence, latest_event_id FROM wake_streams WHERE root_session_id = ?")
            .bind(&root_session_id)
            .fetch_optional(&mut *transaction)
            .await?
        else {
            return Ok(WakeRead {
                reason: None,
                updated_agents: Vec::new(),
                latest_event_id: None,
                truncated_count: 0,
                timed_out: true,
            });
        };
        let retained_from = stream.get::<i64, _>("retained_from_sequence");
        let latest_sequence = stream.get::<i64, _>("next_sequence") - 1;
        let after_sequence = if let Some(event_id) = after_event_id {
            let owner_and_sequence = sqlx::query(
                "SELECT root_session_id, wake_sequence FROM observations WHERE wake_event_id = ?",
            )
            .bind(event_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(row) = owner_and_sequence else {
                return Err(StoreError::InvalidWakeWatermark(event_id.to_string()));
            };
            if row.get::<String, _>("root_session_id") != root_session_id {
                return Err(StoreError::InvalidWakeWatermark(event_id.to_string()));
            }
            row.get::<i64, _>("wake_sequence")
        } else {
            0
        };
        let start_sequence = (after_sequence + 1).max(retained_from);
        let rows = sqlx::query("SELECT body_json FROM wake_events WHERE root_session_id = ? AND wake_sequence >= ? ORDER BY wake_sequence LIMIT ?")
            .bind(&root_session_id)
            .bind(start_sequence)
            .bind(MAX_WAKE_EVENTS_PER_READ as i64)
            .fetch_all(&mut *transaction)
            .await?;
        let updated_agents = rows
            .into_iter()
            .map(|row| decode(row.get::<String, _>("body_json").as_str()))
            .collect::<StoreResult<Vec<WakeEvent>>>()?;
        let lost_to_retention = (retained_from - after_sequence - 1).max(0) as u64;
        let available = (latest_sequence - start_sequence + 1).max(0) as u64;
        let not_returned = available.saturating_sub(updated_agents.len() as u64);
        let reason = updated_agents.last().map(|event| event.reason);
        let latest_event_id = updated_agents
            .last()
            .map(|event| event.event_id)
            .or(after_event_id);
        transaction.commit().await?;
        Ok(WakeRead {
            reason,
            timed_out: updated_agents.is_empty(),
            updated_agents,
            latest_event_id,
            truncated_count: lost_to_retention + not_returned,
        })
    }

    async fn begin_mutation_impl(
        &self,
        attempt_id: AttemptId,
        repo_root: &Path,
        path: String,
        confidence: AttributionConfidence,
    ) -> StoreResult<MutationEventId> {
        let normalized = normalize_repo_path(repo_root, &path)?;
        let repository = repository_identity(repo_root)?;
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        require_repository_identity_tx(&mut transaction, &assignment, &repository).await?;
        require_active_claim_tx(&mut transaction, &assignment, attempt_id, &normalized).await?;
        let existing = sqlx::query(
            "SELECT finalized_at FROM mutation_files WHERE attempt_id = ? AND path = ?",
        )
        .bind(attempt_id.to_string())
        .bind(&normalized)
        .fetch_optional(&mut *transaction)
        .await?;
        if existing
            .as_ref()
            .is_some_and(|row| row.get::<Option<String>, _>("finalized_at").is_some())
        {
            return Err(StoreError::MutationAlreadyFinalized {
                attempt_id,
                path: normalized,
            });
        }
        if existing.is_none() {
            let absolute = absolute_repo_path(&repository.canonical_root, &normalized);
            let pre_write = match tokio::fs::read(&absolute).await {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let snapshot_name = snapshot_name(
                assignment.assignment_id,
                attempt_id,
                &normalized,
                MutationSnapshotVersion::PreWrite,
                pre_write.is_some(),
            );
            let snapshot_name = snapshot_name.to_string_lossy().into_owned();
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            if let Some(parent) = snapshot_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(
                &snapshot_path,
                pre_write.as_deref().unwrap_or(NONEXISTENT_SENTINEL),
            )
            .await?;
            sqlx::query("INSERT INTO mutation_files (attempt_id, assignment_id, path, pre_write_hash, pre_write_existed, attribution_confidence, snapshot_name, snapshot_retained, first_observed_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?)")
                .bind(attempt_id.to_string())
                .bind(assignment.assignment_id.to_string())
                .bind(&normalized)
                .bind(pre_write.as_deref().map(hash_bytes))
                .bind(i64::from(pre_write.is_some()))
                .bind(encode(&confidence)?)
                .bind(snapshot_name)
                .bind(encode(&Utc::now())?)
                .execute(&mut *transaction)
                .await?;
        } else if confidence == AttributionConfidence::Definitive {
            sqlx::query("UPDATE mutation_files SET attribution_confidence = ? WHERE attempt_id = ? AND path = ?")
                .bind(encode(&confidence)?)
                .bind(attempt_id.to_string())
                .bind(&normalized)
                .execute(&mut *transaction)
                .await?;
        }
        let event_id = MutationEventId::new();
        sqlx::query("INSERT INTO mutation_events (event_id, attempt_id, path, created_at) VALUES (?, ?, ?, ?)")
            .bind(event_id.to_string())
            .bind(attempt_id.to_string())
            .bind(&normalized)
            .bind(encode(&Utc::now())?)
            .execute(&mut *transaction)
            .await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt_id,
            ObservationKind::Mutation,
            format!("mutation attributed to {normalized}"),
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(event_id)
    }

    async fn finalize_mutation_impl(
        &self,
        attempt_id: AttemptId,
        repo_root: &Path,
        path: String,
    ) -> StoreResult<MutationEvidence> {
        let normalized = normalize_repo_path(repo_root, &path)?;
        let repository = repository_identity(repo_root)?;
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        require_repository_identity_tx(&mut transaction, &assignment, &repository).await?;
        require_active_claim_tx(&mut transaction, &assignment, attempt_id, &normalized).await?;
        let existing = sqlx::query(
            "SELECT finalized_at FROM mutation_files WHERE attempt_id = ? AND path = ?",
        )
        .bind(attempt_id.to_string())
        .bind(&normalized)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(existing) = existing else {
            return Err(StoreError::MutationNotStarted {
                attempt_id,
                path: normalized,
            });
        };
        if existing.get::<Option<String>, _>("finalized_at").is_some() {
            return Err(StoreError::MutationAlreadyFinalized {
                attempt_id,
                path: normalized,
            });
        }
        let absolute = absolute_repo_path(&repository.canonical_root, &normalized);
        let final_write = match tokio::fs::read(&absolute).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let final_hash: Option<String> = final_write.as_deref().map(hash_bytes);
        let final_snapshot_name = snapshot_name(
            assignment.assignment_id,
            attempt_id,
            &normalized,
            MutationSnapshotVersion::Final,
            final_write.is_some(),
        );
        let final_snapshot_name = final_snapshot_name.to_string_lossy().into_owned();
        let snapshot_path = private_snapshot_path(&self.coordination_root, &final_snapshot_name)?;
        if let Some(parent) = snapshot_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &snapshot_path,
            final_write.as_deref().unwrap_or(NONEXISTENT_SENTINEL),
        )
        .await?;
        let finalized_at = Utc::now();
        let updated = sqlx::query("UPDATE mutation_files SET final_hash = ?, final_write_existed = ?, final_snapshot_name = ?, finalized_at = ? WHERE attempt_id = ? AND path = ? AND finalized_at IS NULL")
            .bind(&final_hash)
            .bind(i64::from(final_write.is_some()))
            .bind(final_snapshot_name)
            .bind(encode(&finalized_at)?)
            .bind(attempt_id.to_string())
            .bind(&normalized)
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::MutationAlreadyFinalized {
                attempt_id,
                path: normalized,
            });
        }
        let evidence = load_mutation_evidence_tx(&mut transaction, attempt_id, &normalized).await?;
        transaction.commit().await?;
        Ok(evidence)
    }

    async fn list_mutation_evidence_impl(
        &self,
        attempt_id: AttemptId,
        limit: Option<usize>,
    ) -> StoreResult<Vec<MutationEvidence>> {
        let limit = limit.unwrap_or(DEFAULT_MUTATION_EVIDENCE_LIMIT);
        if limit > MAX_MUTATION_EVIDENCE_LIMIT {
            return Err(StoreError::InvalidMutationEvidenceLimit(limit));
        }
        let mut transaction = self.pool.begin().await?;
        load_attempt_tx(&mut transaction, attempt_id).await?;
        if limit == 0 {
            transaction.commit().await?;
            return Ok(Vec::new());
        }
        let mut rows = sqlx::query("SELECT path FROM mutation_files WHERE attempt_id = ? ORDER BY first_observed_at DESC, path DESC LIMIT ?")
            .bind(attempt_id.to_string())
            .bind(limit as i64)
            .fetch_all(&mut *transaction)
            .await?;
        rows.reverse();
        let mut evidence = Vec::with_capacity(rows.len());
        for row in rows {
            evidence.push(
                load_mutation_evidence_tx(
                    &mut transaction,
                    attempt_id,
                    row.get::<String, _>("path").as_str(),
                )
                .await?,
            );
        }
        transaction.commit().await?;
        Ok(evidence)
    }

    async fn read_mutation_snapshot_impl(
        &self,
        attempt_id: AttemptId,
        path: String,
        version: MutationSnapshotVersion,
        offset: u64,
        max_bytes: Option<usize>,
    ) -> StoreResult<MutationSnapshotChunk> {
        let max_bytes = max_bytes.unwrap_or(DEFAULT_SNAPSHOT_CHUNK_BYTES);
        if max_bytes == 0 || max_bytes > MAX_SNAPSHOT_CHUNK_BYTES {
            return Err(StoreError::InvalidSnapshotChunkSize(max_bytes));
        }
        let mut transaction = self.pool.begin().await?;
        load_attempt_tx(&mut transaction, attempt_id).await?;
        let row = sqlx::query("SELECT assignment_id, pre_write_existed, final_hash, final_write_existed, snapshot_name, final_snapshot_name, snapshot_retained, finalized_at FROM mutation_files WHERE attempt_id = ? AND path = ?")
            .bind(attempt_id.to_string())
            .bind(&path)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| StoreError::MutationNotStarted {
                attempt_id,
                path: path.clone(),
            })?;
        let assignment_id = AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?;
        if row.get::<i64, _>("snapshot_retained") == 0 {
            return Err(StoreError::SnapshotUnavailable { attempt_id, path });
        }
        let (existed, snapshot_name) = match version {
            MutationSnapshotVersion::PreWrite => (
                row.get::<i64, _>("pre_write_existed") != 0,
                row.get::<String, _>("snapshot_name"),
            ),
            MutationSnapshotVersion::Final => {
                if row.get::<Option<String>, _>("finalized_at").is_none() {
                    return Err(StoreError::MutationNotFinalized { attempt_id, path });
                }
                let existed = row
                    .get::<Option<i64>, _>("final_write_existed")
                    .map(|value| value != 0)
                    .unwrap_or_else(|| row.get::<Option<String>, _>("final_hash").is_some());
                let snapshot_name = row
                    .get::<Option<String>, _>("final_snapshot_name")
                    .ok_or_else(|| StoreError::SnapshotUnavailable {
                        attempt_id,
                        path: path.clone(),
                    })?;
                (existed, snapshot_name)
            }
        };
        transaction.commit().await?;

        let (total_bytes, bytes) = if existed {
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            let mut file = std::fs::File::open(snapshot_path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    StoreError::SnapshotUnavailable {
                        attempt_id,
                        path: path.clone(),
                    }
                } else {
                    error.into()
                }
            })?;
            let total_bytes = file.metadata()?.len();
            if offset > total_bytes {
                return Err(StoreError::InvalidSnapshotOffset {
                    offset,
                    total_bytes,
                });
            }
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::with_capacity(max_bytes.min((total_bytes - offset) as usize));
            file.take(max_bytes as u64).read_to_end(&mut bytes)?;
            (total_bytes, bytes)
        } else {
            if offset != 0 {
                return Err(StoreError::InvalidSnapshotOffset {
                    offset,
                    total_bytes: 0,
                });
            }
            (0, Vec::new())
        };
        let returned_through = offset.saturating_add(bytes.len() as u64);
        Ok(MutationSnapshotChunk {
            assignment_id,
            attempt_id,
            path,
            version,
            existed,
            offset,
            total_bytes,
            bytes,
            next_offset: (returned_through < total_bytes).then_some(returned_through),
        })
    }

    pub async fn garbage_collect_snapshots(
        &self,
        assignment_id: AssignmentId,
        retention_allows: bool,
    ) -> StoreResult<usize> {
        if !retention_allows {
            return Err(StoreError::SnapshotRetentionRequired);
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let attempts =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM attempts WHERE assignment_id = ?")
                .bind(assignment_id.to_string())
                .fetch_one(&mut *transaction)
                .await?;
        let sealed_attempts = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM attempts WHERE assignment_id = ? AND sealed_at IS NOT NULL",
        )
        .bind(assignment_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        let receipts =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM receipts WHERE assignment_id = ?")
                .bind(assignment_id.to_string())
                .fetch_one(&mut *transaction)
                .await?;
        let pending_gates = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM gates WHERE assignment_id = ? AND sealed_at IS NULL",
        )
        .bind(assignment_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        if attempts == 0
            || attempts != sealed_attempts
            || attempts != receipts
            || pending_gates != 0
        {
            return Err(StoreError::SnapshotRetentionRequired);
        }
        let rows = sqlx::query("SELECT attempt_id, path, snapshot_name, final_snapshot_name FROM mutation_files WHERE assignment_id = ? AND snapshot_retained = 1")
            .bind(assignment_id.to_string())
            .fetch_all(&mut *transaction)
            .await?;
        let mut removed = 0;
        for row in rows {
            let snapshot_names = [
                Some(row.get::<String, _>("snapshot_name")),
                row.get::<Option<String>, _>("final_snapshot_name"),
            ];
            for snapshot_name in snapshot_names.into_iter().flatten() {
                let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
                match tokio::fs::remove_file(snapshot_path).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            sqlx::query(
                "UPDATE mutation_files SET snapshot_retained = 0 WHERE attempt_id = ? AND path = ?",
            )
            .bind(row.get::<String, _>("attempt_id"))
            .bind(row.get::<String, _>("path"))
            .execute(&mut *transaction)
            .await?;
            removed += 1;
        }
        transaction.commit().await?;
        Ok(removed)
    }

    async fn rebuild_wake_streams(&self) -> StoreResult<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM wake_events")
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM wake_streams")
            .execute(&mut *transaction)
            .await?;
        let roots = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT root_session_id FROM observations ORDER BY root_session_id",
        )
        .fetch_all(&mut *transaction)
        .await?;
        for root in roots {
            let mut rows = sqlx::query("SELECT wake_sequence, body_json FROM observations WHERE root_session_id = ? ORDER BY wake_sequence DESC LIMIT ?")
                .bind(&root)
                .bind(MAX_WAKE_EVENTS_PER_ROOT as i64)
                .fetch_all(&mut *transaction)
                .await?;
            rows.reverse();
            let mut retained_from = 1;
            let mut next_sequence = 1;
            let mut latest_event_id = None;
            for row in rows {
                let wake_sequence = row.get::<i64, _>("wake_sequence");
                let observation: RuntimeObservation =
                    decode(row.get::<String, _>("body_json").as_str())?;
                let event = WakeEvent {
                    event_id: observation.wake_event_id,
                    assignment_id: observation.assignment_id,
                    attempt_id: observation.attempt_id,
                    reason: observation.kind,
                    summary: observation.summary,
                    created_at: observation.created_at,
                };
                if latest_event_id.is_none() {
                    retained_from = wake_sequence;
                }
                latest_event_id = Some(event.event_id);
                next_sequence = wake_sequence + 1;
                sqlx::query("INSERT INTO wake_events (root_session_id, wake_sequence, event_id, assignment_id, attempt_id, reason, body_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(&root)
                    .bind(wake_sequence)
                    .bind(event.event_id.to_string())
                    .bind(event.assignment_id.to_string())
                    .bind(event.attempt_id.to_string())
                    .bind(encode(&event.reason)?)
                    .bind(encode(&event)?)
                    .bind(encode(&event.created_at)?)
                    .execute(&mut *transaction)
                    .await?;
            }
            sqlx::query("INSERT INTO wake_streams (root_session_id, next_sequence, retained_from_sequence, latest_event_id) VALUES (?, ?, ?, ?)")
                .bind(root)
                .bind(next_sequence)
                .bind(retained_from)
                .bind(latest_event_id.map(|id| id.to_string()))
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

impl crate::AgentTaskStore for LocalAgentTaskStore {
    fn create_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
    ) -> TaskStoreFuture<'a, (Assignment, Attempt)> {
        Box::pin(async move { self.create_assignment_impl(repo_root, draft).await })
    }

    fn get_agent_task(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> TaskStoreFuture<'_, AgentTask> {
        Box::pin(async move {
            self.get_agent_task_impl(assignment_id, observation_limit)
                .await
        })
    }

    fn bind_agent_task(
        &self,
        binding: AgentTaskBindingDraft,
    ) -> TaskStoreFuture<'_, AgentTaskBinding> {
        Box::pin(async move { self.bind_agent_task_impl(binding).await })
    }

    fn get_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, Option<AgentTaskBinding>> {
        Box::pin(async move { self.get_agent_task_binding_impl(assignment_id).await })
    }

    fn list_agent_task_bindings(
        &self,
        root_session_id: String,
        limit: Option<usize>,
    ) -> TaskStoreFuture<'_, Vec<AgentTaskBinding>> {
        Box::pin(async move {
            self.list_agent_task_bindings_impl(root_session_id, limit)
                .await
        })
    }

    fn append_observation(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> TaskStoreFuture<'_, RuntimeObservation> {
        Box::pin(async move {
            self.append_observation_impl(attempt_id, kind, summary, call_id)
                .await
        })
    }

    fn record_validation_call(&self, call: ValidationCall) -> TaskStoreFuture<'_, ()> {
        Box::pin(async move { self.record_validation_call_impl(call).await })
    }

    fn submit_agent_receipt(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move { self.submit_agent_receipt_impl(attempt_id, receipt).await })
    }

    fn amend_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> TaskStoreFuture<'_, Attempt> {
        Box::pin(async move {
            self.amend_agent_task_impl(actor, assignment_id, amendment)
                .await
        })
    }

    fn abandon_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            self.abandon_agent_task_impl(actor, assignment_id, reason)
                .await
        })
    }

    fn set_agent_gate(
        &self,
        assignment_id: AssignmentId,
        kind: GateKind,
        status: GateStatus,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate> {
        Box::pin(async move {
            self.set_agent_gate_impl(assignment_id, kind, status, reason)
                .await
        })
    }

    fn waive_agent_gate(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate> {
        Box::pin(async move {
            self.waive_agent_gate_impl(actor, assignment_id, kind, reason)
                .await
        })
    }

    fn read_wake_events(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> TaskStoreFuture<'_, WakeRead> {
        Box::pin(async move {
            self.read_wake_events_impl(root_session_id, after_event_id)
                .await
        })
    }

    fn begin_mutation<'a>(
        &'a self,
        attempt_id: AttemptId,
        repo_root: &'a Path,
        path: String,
        confidence: AttributionConfidence,
    ) -> TaskStoreFuture<'a, MutationEventId> {
        Box::pin(async move {
            self.begin_mutation_impl(attempt_id, repo_root, path, confidence)
                .await
        })
    }

    fn finalize_mutation<'a>(
        &'a self,
        attempt_id: AttemptId,
        repo_root: &'a Path,
        path: String,
    ) -> TaskStoreFuture<'a, MutationEvidence> {
        Box::pin(async move {
            self.finalize_mutation_impl(attempt_id, repo_root, path)
                .await
        })
    }

    fn list_mutation_evidence(
        &self,
        attempt_id: AttemptId,
        limit: Option<usize>,
    ) -> TaskStoreFuture<'_, Vec<MutationEvidence>> {
        Box::pin(async move { self.list_mutation_evidence_impl(attempt_id, limit).await })
    }

    fn read_mutation_snapshot(
        &self,
        attempt_id: AttemptId,
        path: String,
        version: MutationSnapshotVersion,
        offset: u64,
        max_bytes: Option<usize>,
    ) -> TaskStoreFuture<'_, MutationSnapshotChunk> {
        Box::pin(async move {
            self.read_mutation_snapshot_impl(attempt_id, path, version, offset, max_bytes)
                .await
        })
    }
}

async fn lock_attempt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
) -> StoreResult<()> {
    let result = sqlx::query("UPDATE attempts SET state = state WHERE attempt_id = ?")
        .bind(attempt_id.to_string())
        .execute(&mut **transaction)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StoreError::AttemptNotFound(attempt_id));
    }
    Ok(())
}

async fn lock_assignment_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<()> {
    let result = sqlx::query("UPDATE attempts SET state = state WHERE assignment_id = ?")
        .bind(assignment_id.to_string())
        .execute(&mut **transaction)
        .await?;
    if result.rows_affected() == 0 {
        return Err(StoreError::AssignmentNotFound(assignment_id));
    }
    Ok(())
}

async fn load_assignment_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<Assignment> {
    let row = sqlx::query("SELECT a.root_session_id, a.body_json, ar.repository_id FROM assignments a LEFT JOIN assignment_repositories ar ON ar.assignment_id = a.assignment_id WHERE a.assignment_id = ?")
        .bind(assignment_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(StoreError::AssignmentNotFound(assignment_id))?;
    let mut assignment: Assignment = decode(row.get::<String, _>("body_json").as_str())?;
    if assignment.assignment_id != assignment_id {
        return Err(StoreError::CorruptData(format!(
            "assignment body identity does not match {assignment_id}"
        )));
    }
    if assignment.root_session_id != row.get::<String, _>("root_session_id") {
        return Err(StoreError::CorruptData(format!(
            "assignment root session does not match {assignment_id}"
        )));
    }
    if let Some(bound_repository_id) = row.get::<Option<String>, _>("repository_id") {
        if assignment.repository_id.is_empty() {
            assignment.repository_id = bound_repository_id;
        } else if assignment.repository_id != bound_repository_id {
            return Err(StoreError::CorruptData(format!(
                "assignment repository identity does not match {assignment_id}"
            )));
        }
    }
    Ok(assignment)
}

async fn load_attempt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
) -> StoreResult<Attempt> {
    let row = sqlx::query("SELECT assignment_id, ordinal, amendment_json, state, created_at, sealed_at FROM attempts WHERE attempt_id = ?")
        .bind(attempt_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(StoreError::AttemptNotFound(attempt_id))?;
    attempt_from_row(attempt_id, &row)
}

async fn load_current_attempt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<Attempt> {
    let row = sqlx::query("SELECT attempt_id, assignment_id, ordinal, amendment_json, state, created_at, sealed_at FROM attempts WHERE assignment_id = ? ORDER BY ordinal DESC LIMIT 1")
        .bind(assignment_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(StoreError::AssignmentNotFound(assignment_id))?;
    let attempt_id = AttemptId::parse(row.get::<String, _>("attempt_id").as_str())?;
    attempt_from_row(attempt_id, &row)
}

async fn require_active_current_attempt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
) -> StoreResult<Attempt> {
    let attempt = load_attempt_tx(transaction, attempt_id).await?;
    let current = load_current_attempt_tx(transaction, attempt.assignment_id).await?;
    if current.attempt_id != attempt_id
        || attempt.state != AttemptState::Active
        || attempt.sealed_at.is_some()
    {
        return Err(StoreError::AttemptNotActive(attempt_id));
    }
    Ok(attempt)
}

async fn dependency_reaches_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    start: AssignmentId,
    target: AssignmentId,
) -> StoreResult<bool> {
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    while let Some(next) = pending.pop() {
        if !seen.insert(next) {
            continue;
        }
        let json = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM assignments WHERE assignment_id = ?",
        )
        .bind(next.to_string())
        .fetch_optional(&mut **transaction)
        .await?;
        let Some(json) = json else {
            continue;
        };
        let assignment: Assignment = decode(&json)?;
        if assignment.dependencies.contains(&target) {
            return Ok(true);
        }
        pending.extend(assignment.dependencies);
    }
    Ok(false)
}

async fn validate_dependencies_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    candidate_id: AssignmentId,
    repository_id: Option<&str>,
    dependencies: &[AssignmentId],
) -> StoreResult<()> {
    let mut blockers = Vec::new();
    for dependency in dependencies {
        if *dependency == candidate_id {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::SelfReference,
                detail: "an assignment cannot depend on itself".to_string(),
            });
            continue;
        }
        let exists = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM assignments WHERE assignment_id = ?",
        )
        .bind(dependency.to_string())
        .fetch_one(&mut **transaction)
        .await?
            != 0;
        if !exists {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Unknown,
                detail: "dependency does not exist".to_string(),
            });
            continue;
        }
        let dependency_assignment = load_assignment_tx(transaction, *dependency).await?;
        if let Some(repository_id) = repository_id {
            let bound_repository_id = sqlx::query_scalar::<_, String>(
                "SELECT repository_id FROM assignment_repositories WHERE assignment_id = ?",
            )
            .bind(dependency.to_string())
            .fetch_optional(&mut **transaction)
            .await?;
            if bound_repository_id.as_deref() != Some(repository_id)
                || dependency_assignment.repository_id != repository_id
            {
                blockers.push(DependencyBlocker {
                    assignment_id: *dependency,
                    state: DependencyState::Unknown,
                    detail: "dependency belongs to a different or legacy-unbound repository"
                        .to_string(),
                });
                continue;
            }
        }
        if dependency_reaches_tx(transaction, *dependency, candidate_id).await? {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Cyclic,
                detail: "dependency would create a cycle".to_string(),
            });
            continue;
        }
        let receipt_json = sqlx::query_scalar::<_, Option<String>>(
            "SELECT r.body_json FROM attempts t LEFT JOIN receipts r ON r.attempt_id = t.attempt_id WHERE t.assignment_id = ? ORDER BY t.ordinal DESC LIMIT 1",
        )
        .bind(dependency.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .flatten();
        let Some(receipt_json) = receipt_json else {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Incomplete,
                detail: "dependency has no sealed receipt".to_string(),
            });
            continue;
        };
        let receipt: AgentReceipt = decode(&receipt_json)?;
        if !receipt.status.is_success() {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: dependency_state(receipt.status),
                detail: format!("dependency receipt is {:?}", receipt.status),
            });
        }
    }
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(StoreError::DependencyBlocked { blockers })
    }
}

async fn plan_write_claim_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    exclude_assignment_id: Option<AssignmentId>,
) -> StoreResult<(Vec<AssignmentId>, Vec<WriteClaimConflict>)> {
    if assignment.write_scope.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let bound_repository_id = sqlx::query_scalar::<_, String>(
        "SELECT repository_id FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(
        assignment.assignment_id,
    ))?;
    if assignment.repository_id.is_empty() || assignment.repository_id != bound_repository_id {
        return Err(StoreError::CorruptData(format!(
            "assignment repository identity does not match {}",
            assignment.assignment_id
        )));
    }
    let integrator_targets: HashSet<_> = if assignment.role == AgentRole::Integrator {
        assignment
            .relation
            .as_ref()
            .map(|relation| relation.target_assignment_ids.iter().copied().collect())
            .unwrap_or_default()
    } else {
        HashSet::new()
    };
    let rows = if let Some(excluded) = exclude_assignment_id {
        sqlx::query("SELECT wc.assignment_id, wc.scopes_json, ar.repository_id FROM write_claims wc LEFT JOIN assignment_repositories ar ON ar.assignment_id = wc.assignment_id WHERE wc.active = 1 AND (ar.repository_id = ? OR ar.repository_id IS NULL) AND wc.assignment_id <> ?")
            .bind(&bound_repository_id)
            .bind(excluded.to_string())
            .fetch_all(&mut **transaction)
            .await?
    } else {
        sqlx::query("SELECT wc.assignment_id, wc.scopes_json, ar.repository_id FROM write_claims wc LEFT JOIN assignment_repositories ar ON ar.assignment_id = wc.assignment_id WHERE wc.active = 1 AND (ar.repository_id = ? OR ar.repository_id IS NULL)")
            .bind(&bound_repository_id)
            .fetch_all(&mut **transaction)
            .await?
    };
    let mut supersedes = HashSet::new();
    let mut conflicts = Vec::new();
    for row in rows {
        let existing_id = AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?;
        let existing_repository_id = row.get::<Option<String>, _>("repository_id");
        let scopes: Vec<RepoScope> = decode(row.get::<String, _>("scopes_json").as_str())?;
        for existing_scope in &scopes {
            for requested_scope in &assignment.write_scope {
                if !existing_scope.overlaps(requested_scope) {
                    continue;
                }
                if existing_repository_id.is_some() && integrator_targets.contains(&existing_id) {
                    supersedes.insert(existing_id);
                } else {
                    conflicts.push(WriteClaimConflict {
                        assignment_id: existing_id,
                        existing_scope: existing_scope.clone(),
                        requested_scope: requested_scope.clone(),
                    });
                }
            }
        }
    }
    let mut supersedes: Vec<_> = supersedes.into_iter().collect();
    supersedes.sort();
    conflicts.sort_by(|left, right| {
        left.assignment_id
            .cmp(&right.assignment_id)
            .then_with(|| left.existing_scope.path.cmp(&right.existing_scope.path))
            .then_with(|| left.requested_scope.path.cmp(&right.requested_scope.path))
    });
    Ok((supersedes, conflicts))
}

async fn require_repository_identity_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    repository: &RepositoryIdentity,
) -> StoreResult<()> {
    let row = sqlx::query(
        "SELECT repository_id, canonical_root FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(
        assignment.assignment_id,
    ))?;
    let bound_id = row.get::<String, _>("repository_id");
    let bound_root = row.get::<String, _>("canonical_root");
    let root_matches = if cfg!(windows) {
        bound_root.to_lowercase() == repository.canonical_path.to_lowercase()
    } else {
        bound_root == repository.canonical_path
    };
    if assignment.repository_id.is_empty()
        || assignment.repository_id != bound_id
        || repository.id != bound_id
        || !root_matches
    {
        return Err(StoreError::RepositoryMismatch(assignment.assignment_id));
    }
    Ok(())
}

async fn require_active_claim_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    path: &str,
) -> StoreResult<()> {
    let row = sqlx::query(
        "SELECT attempt_id, scopes_json, active FROM write_claims WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Err(StoreError::MutationOutsideClaim(path.to_string()));
    };
    let scopes: Vec<RepoScope> = decode(row.get::<String, _>("scopes_json").as_str())?;
    if row.get::<i64, _>("active") == 0
        || row.get::<String, _>("attempt_id") != attempt_id.to_string()
        || !scopes.iter().any(|scope| scope.covers_path(path))
    {
        return Err(StoreError::MutationOutsideClaim(path.to_string()));
    }
    Ok(())
}

async fn load_mutation_evidence_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
    path: &str,
) -> StoreResult<MutationEvidence> {
    let row = sqlx::query("SELECT assignment_id, pre_write_hash, pre_write_existed, final_hash, final_write_existed, attribution_confidence, snapshot_retained, first_observed_at, finalized_at FROM mutation_files WHERE attempt_id = ? AND path = ?")
        .bind(attempt_id.to_string())
        .bind(path)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| StoreError::MutationNotStarted {
            attempt_id,
            path: path.to_string(),
        })?;
    let event_rows = sqlx::query("SELECT event_id FROM mutation_events WHERE attempt_id = ? AND path = ? ORDER BY created_at, event_id")
        .bind(attempt_id.to_string())
        .bind(path)
        .fetch_all(&mut **transaction)
        .await?;
    let mutation_event_ids = event_rows
        .into_iter()
        .map(|event| MutationEventId::parse(event.get::<String, _>("event_id").as_str()))
        .collect::<StoreResult<Vec<_>>>()?;
    let final_hash: Option<String> = row.get("final_hash");
    let finalized_at = row
        .get::<Option<String>, _>("finalized_at")
        .map(|value| decode(&value))
        .transpose()?;
    let final_write_existed = finalized_at.as_ref().map(|_| {
        row.get::<Option<i64>, _>("final_write_existed")
            .map(|value| value != 0)
            .unwrap_or_else(|| final_hash.is_some())
    });
    Ok(MutationEvidence {
        assignment_id: AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?,
        attempt_id,
        path: path.to_string(),
        pre_write_hash: row.get("pre_write_hash"),
        pre_write_existed: row.get::<i64, _>("pre_write_existed") != 0,
        final_hash,
        final_write_existed,
        mutation_event_ids,
        attribution_confidence: decode(row.get::<String, _>("attribution_confidence").as_str())?,
        snapshot_retained: row.get::<i64, _>("snapshot_retained") != 0,
        first_observed_at: decode(row.get::<String, _>("first_observed_at").as_str())?,
        finalized_at,
    })
}

async fn release_successful_claim_if_unblocked_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<()> {
    let current = load_current_attempt_tx(transaction, assignment_id).await?;
    let successful = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM receipts WHERE attempt_id = ? AND status = ?",
    )
    .bind(current.attempt_id.to_string())
    .bind(encode(&AgentStatusClaim::Completed)?)
    .fetch_one(&mut **transaction)
    .await?
        != 0;
    if successful && pending_gate_count(transaction, assignment_id).await? == 0 {
        release_claim(transaction, assignment_id, None).await?;
    }
    Ok(())
}

fn binding_from_row(row: &sqlx::sqlite::SqliteRow) -> StoreResult<AgentTaskBinding> {
    Ok(AgentTaskBinding {
        assignment_id: AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?,
        attempt_id: AttemptId::parse(row.get::<String, _>("attempt_id").as_str())?,
        root_session_id: row.get("root_session_id"),
        agent_path: row.get("agent_path"),
        task_name: row.get("task_name"),
        thread_id: row.get("thread_id"),
        bound_at: decode(row.get::<String, _>("bound_at").as_str())?,
        updated_at: decode(row.get::<String, _>("updated_at").as_str())?,
    })
}

fn private_snapshot_path(coordination_root: &Path, snapshot_name: &str) -> StoreResult<PathBuf> {
    let relative = Path::new(snapshot_name);
    if relative.is_absolute() {
        return Err(StoreError::CorruptData(
            "private snapshot path is absolute".to_string(),
        ));
    }
    let mut has_component = false;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(_) => has_component = true,
            std::path::Component::CurDir
            | std::path::Component::ParentDir
            | std::path::Component::Prefix(_)
            | std::path::Component::RootDir => {
                return Err(StoreError::CorruptData(
                    "private snapshot path contains unsafe components".to_string(),
                ));
            }
        }
    }
    if !has_component {
        return Err(StoreError::CorruptData(
            "private snapshot path is empty".to_string(),
        ));
    }
    Ok(coordination_root.join(relative))
}

async fn append_observation_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    kind: ObservationKind,
    summary: String,
    call_id: Option<String>,
) -> StoreResult<RuntimeObservation> {
    let observation = RuntimeObservation {
        event_id: MutationEventId::new(),
        wake_event_id: WakeEventId::new(),
        assignment_id: assignment.assignment_id,
        attempt_id,
        kind,
        summary,
        call_id,
        created_at: Utc::now(),
    };
    let wake_event = WakeEvent {
        event_id: observation.wake_event_id,
        assignment_id: observation.assignment_id,
        attempt_id,
        reason: kind,
        summary: observation.summary.clone(),
        created_at: observation.created_at,
    };
    sqlx::query("INSERT OR IGNORE INTO wake_streams (root_session_id, next_sequence, retained_from_sequence) VALUES (?, 1, 1)")
        .bind(&assignment.root_session_id)
        .execute(&mut **transaction)
        .await?;
    let wake_sequence = sqlx::query_scalar::<_, i64>(
        "SELECT next_sequence FROM wake_streams WHERE root_session_id = ?",
    )
    .bind(&assignment.root_session_id)
    .fetch_one(&mut **transaction)
    .await?;
    let retained_from = (wake_sequence - MAX_WAKE_EVENTS_PER_ROOT as i64 + 1).max(1);
    sqlx::query("UPDATE wake_streams SET next_sequence = ?, retained_from_sequence = ?, latest_event_id = ? WHERE root_session_id = ?")
        .bind(wake_sequence + 1)
        .bind(retained_from)
        .bind(observation.wake_event_id.to_string())
        .bind(&assignment.root_session_id)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("INSERT INTO observations (event_id, wake_event_id, root_session_id, wake_sequence, assignment_id, attempt_id, kind, body_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(observation.event_id.to_string())
        .bind(observation.wake_event_id.to_string())
        .bind(&assignment.root_session_id)
        .bind(wake_sequence)
        .bind(observation.assignment_id.to_string())
        .bind(attempt_id.to_string())
        .bind(encode(&kind)?)
        .bind(encode(&observation)?)
        .bind(encode(&observation.created_at)?)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("INSERT INTO wake_events (root_session_id, wake_sequence, event_id, assignment_id, attempt_id, reason, body_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&assignment.root_session_id)
        .bind(wake_sequence)
        .bind(wake_event.event_id.to_string())
        .bind(wake_event.assignment_id.to_string())
        .bind(wake_event.attempt_id.to_string())
        .bind(encode(&kind)?)
        .bind(encode(&wake_event)?)
        .bind(encode(&wake_event.created_at)?)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("DELETE FROM wake_events WHERE root_session_id = ? AND wake_sequence < ?")
        .bind(&assignment.root_session_id)
        .bind(retained_from)
        .execute(&mut **transaction)
        .await?;
    Ok(observation)
}

async fn insert_attempt(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
) -> StoreResult<()> {
    sqlx::query("INSERT INTO attempts (attempt_id, assignment_id, ordinal, amendment_json, state, created_at, sealed_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(attempt.attempt_id.to_string())
        .bind(attempt.assignment_id.to_string())
        .bind(i64::from(attempt.ordinal))
        .bind(attempt.amendment.as_ref().map(encode).transpose()?)
        .bind(encode(&attempt.state)?)
        .bind(encode(&attempt.created_at)?)
        .bind(attempt.sealed_at.map(|value| encode(&value)).transpose()?)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn attempt_from_row(attempt_id: AttemptId, row: &sqlx::sqlite::SqliteRow) -> StoreResult<Attempt> {
    Ok(Attempt {
        attempt_id,
        assignment_id: AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?,
        ordinal: u8::try_from(row.get::<i64, _>("ordinal"))
            .map_err(|_| StoreError::CorruptData("attempt ordinal is out of range".to_string()))?,
        amendment: row
            .get::<Option<String>, _>("amendment_json")
            .map(|value| decode(&value))
            .transpose()?,
        state: decode(row.get::<String, _>("state").as_str())?,
        created_at: decode(row.get::<String, _>("created_at").as_str())?,
        sealed_at: row
            .get::<Option<String>, _>("sealed_at")
            .map(|value| decode(&value))
            .transpose()?,
    })
}

async fn pending_gate_count(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<i64> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM gates WHERE assignment_id = ? AND status = ?",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateStatus::Pending)?)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn release_claim(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
    superseded_by: Option<AssignmentId>,
) -> StoreResult<()> {
    sqlx::query("UPDATE write_claims SET active = 0, released_at = ?, superseded_by = COALESCE(?, superseded_by) WHERE assignment_id = ? AND active = 1")
        .bind(encode(&Utc::now())?)
        .bind(superseded_by.map(|id| id.to_string()))
        .bind(assignment_id.to_string())
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn validate_criterion_results(
    assignment: &Assignment,
    amendment: Option<&AttemptAmendment>,
    receipt: &ReceiptDraft,
) -> StoreResult<()> {
    let criteria = effective_criteria(assignment, amendment);
    let expected: HashSet<_> = criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect();
    let mut actual = HashSet::new();
    for result in &receipt.criterion_results {
        if !actual.insert(result.criterion_id.as_str()) {
            return Err(StoreError::CriterionResultsInvalid(format!(
                "duplicate result for {}",
                result.criterion_id
            )));
        }
    }
    if actual != expected {
        return Err(StoreError::CriterionResultsInvalid(
            "every criterion must appear exactly once".to_string(),
        ));
    }
    if receipt.status == AgentStatusClaim::Completed
        && receipt
            .criterion_results
            .iter()
            .any(|result| result.status != CriterionStatus::Passed)
    {
        return Err(StoreError::CriterionResultsInvalid(
            "completed receipts require every criterion to pass".to_string(),
        ));
    }
    Ok(())
}

fn effective_criteria<'a>(
    assignment: &'a Assignment,
    amendment: Option<&'a AttemptAmendment>,
) -> &'a [crate::AcceptanceCriterion] {
    amendment
        .and_then(|value| value.acceptance_criteria.as_deref())
        .unwrap_or(&assignment.acceptance_criteria)
}

fn dependency_state(status: AgentStatusClaim) -> DependencyState {
    match status {
        AgentStatusClaim::Blocked | AgentStatusClaim::NeedsMain => DependencyState::Blocked,
        AgentStatusClaim::Failed => DependencyState::Failed,
        AgentStatusClaim::Violated => DependencyState::Violated,
        AgentStatusClaim::Abandoned => DependencyState::Abandoned,
        AgentStatusClaim::Completed => DependencyState::Incomplete,
    }
}

fn receipt_observation_kind(status: AgentStatusClaim) -> ObservationKind {
    match status {
        AgentStatusClaim::Completed => ObservationKind::Completed,
        AgentStatusClaim::NeedsMain | AgentStatusClaim::Blocked | AgentStatusClaim::Failed => {
            ObservationKind::NeedsMain
        }
        AgentStatusClaim::Violated => ObservationKind::Violated,
        AgentStatusClaim::Abandoned => ObservationKind::Abandoned,
    }
}

fn snapshot_name(
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    path: &str,
    version: MutationSnapshotVersion,
    existed: bool,
) -> PathBuf {
    let extension = match (version, existed) {
        (MutationSnapshotVersion::PreWrite, true) => "pre",
        (MutationSnapshotVersion::PreWrite, false) => "pre-missing",
        (MutationSnapshotVersion::Final, true) => "final",
        (MutationSnapshotVersion::Final, false) => "final-missing",
    };
    PathBuf::from("snapshots")
        .join(assignment_id.to_string())
        .join(attempt_id.to_string())
        .join(format!("{}.{}", hash_bytes(path.as_bytes()), extension))
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn encode<T: Serialize>(value: &T) -> StoreResult<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    Ok(serde_json::from_str(value)?)
}
