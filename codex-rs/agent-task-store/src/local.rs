use chrono::Duration;
use chrono::Utc;
use codex_state::StateRuntime;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Acquire;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use crate::ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION;
use crate::AdmissionOverlapSummary;
use crate::AdmissionRejectionReason;
use crate::AdmittedAssignment;
use crate::AgentGate;
use crate::AgentReceipt;
use crate::AgentRole;
use crate::AgentStatusClaim;
use crate::AgentTask;
use crate::AgentTaskBinding;
use crate::AgentTaskBindingDraft;
use crate::ArchitectureContractV1;
use crate::Assignment;
use crate::AssignmentDraft;
use crate::AssignmentId;
use crate::Attempt;
use crate::AttemptAmendment;
use crate::AttemptId;
use crate::AttemptState;
use crate::AttributionConfidence;
use crate::CONCURRENT_DRIFT_REASON;
use crate::CriterionStatus;
use crate::DEFAULT_MUTATION_EVIDENCE_LIMIT;
use crate::DEFAULT_SNAPSHOT_CHUNK_BYTES;
use crate::DependencyBlocker;
use crate::DependencyState;
use crate::GateKind;
use crate::GateStatus;
use crate::IntegrationPlan;
use crate::IsolationHandoff;
use crate::IsolationHandoffState;
use crate::MAX_BINDING_LIMIT;
use crate::MAX_MUTATION_EVIDENCE_LIMIT;
use crate::MAX_MUTATION_SNAPSHOT_BYTES;
use crate::MAX_OBSERVATION_LIMIT;
use crate::MAX_SNAPSHOT_CHUNK_BYTES;
use crate::MAX_VALIDATION_CALLS_PER_TASK;
use crate::MAX_WAKE_EVENTS_PER_READ;
use crate::MAX_WAKE_EVENTS_PER_ROOT;
use crate::MissingEvidenceObligation;
use crate::MutationEventId;
use crate::MutationEvidence;
use crate::MutationSnapshotChunk;
use crate::MutationSnapshotVersion;
use crate::NonproductiveRecovery;
use crate::ObservationKind;
use crate::ProductivitySummary;
use crate::ReceiptDraft;
use crate::RelationKind;
use crate::RepoScope;
use crate::RuntimeObservation;
use crate::SealedArchitectureContractV1;
use crate::StoreError;
use crate::StoreResult;
use crate::TaskActor;
use crate::TaskCapsuleV1;
use crate::TaskStoreFuture;
use crate::ValidationCall;
use crate::ValidationCallStatus;
use crate::ValidationEvidence;
use crate::WakeEvent;
use crate::WakeEventId;
use crate::WakeRead;
use crate::WorkspaceActorRegistration;
use crate::WorkspaceRevision;
use crate::WorkspaceStrategy;
use crate::WorkspaceTaskStatus;
use crate::scope::RepositoryIdentity;
use crate::scope::absolute_repo_path;
use crate::scope::normalize_repo_path;
use crate::scope::normalize_repo_scopes;
use crate::scope::repository_identity;

const COORDINATION_DIR: &str = "agent-task-coordination";
const COLD_REVIEW_REASON_PREFIX: &str = "cold review required: ";
// Snapshot capture can retain SQLite's single writer while a bounded file copy is
// flushed. Give short-lived lease and provenance writes enough time to wait out
// that contention instead of failing an otherwise read-only source inspection.
const DATABASE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DATABASE_FILENAME: &str = "agent_tasks.sqlite";

fn sqlite_contention_code(error: &sqlx::Error) -> Option<&str> {
    let sqlx::Error::Database(error) = error else {
        return None;
    };
    match error.code().as_deref() {
        Some("5") => Some("busy"),
        Some("6") => Some("locked"),
        _ => None,
    }
}

fn record_coordination_timing(
    operation: &'static str,
    phase: &'static str,
    started_at: Instant,
    error: Option<&sqlx::Error>,
) {
    let elapsed_ms = started_at.elapsed().as_secs_f64() * 1_000.0;
    let contention = error.and_then(sqlite_contention_code);
    tracing::info!(
        target: "codex_agent_task_store::coordination",
        operation,
        phase,
        elapsed_ms,
        sqlite_contention = contention,
        "agent-task coordination timing"
    );
    if let Some(sqlite_contention) = contention {
        tracing::warn!(
            target: "codex_agent_task_store::coordination",
            operation,
            phase,
            elapsed_ms,
            sqlite_contention,
            "agent-task SQLite contention"
        );
    }
}
const NONEXISTENT_SENTINEL: &[u8] = b"CODEX_AGENT_TASK_STORE_NONEXISTENT\n";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[cfg(test)]
tokio::task_local! {
    static TEST_COMPARISON_NOW: chrono::DateTime<Utc>;
}

fn comparison_now() -> chrono::DateTime<Utc> {
    #[cfg(test)]
    if let Ok(now) = TEST_COMPARISON_NOW.try_with(ToOwned::to_owned) {
        return now;
    }
    Utc::now()
}

#[cfg(test)]
pub(crate) async fn with_test_comparison_now<T>(
    now: chrono::DateTime<Utc>,
    future: impl std::future::Future<Output = T>,
) -> T {
    TEST_COMPARISON_NOW.scope(now, future).await
}

enum ReceiptHandoffAction {
    Publish(IsolationHandoff),
    Integrate(Vec<AssignmentId>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MissingEvidenceFingerprint {
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    binding_hash: String,
    receipt_hash: String,
    obligation_set_hash: String,
    validation_evidence_revision: u64,
    workspace_epoch: u64,
}

#[derive(Clone, Debug)]
struct MissingEvidenceRejection {
    fingerprint: MissingEvidenceFingerprint,
    obligations: Vec<MissingEvidenceObligation>,
    total_obligation_count: usize,
}

#[derive(Clone)]
pub struct LocalAgentTaskStore {
    pool: SqlitePool,
    coordination_root: Arc<PathBuf>,
    missing_evidence_rejections: Arc<Mutex<HashMap<AttemptId, MissingEvidenceRejection>>>,
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
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(DATABASE_BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        upgrade_legacy_repository_bindings(&pool).await?;
        let store = Self {
            pool,
            coordination_root: Arc::new(coordination_root),
            missing_evidence_rejections: Arc::new(Mutex::new(HashMap::new())),
        };
        store
            .drain_snapshot_gc_queue_best_effort("store initialization")
            .await;
        store.queue_eligible_retained_snapshot_candidates().await?;
        store
            .drain_snapshot_gc_queue_best_effort("store initialization")
            .await;
        store.reconcile_snapshot_files().await?;
        store.rebuild_wake_streams().await?;
        Ok(store)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    fn cached_missing_evidence_rejection(
        &self,
        attempt_id: AttemptId,
    ) -> Option<MissingEvidenceRejection> {
        self.missing_evidence_rejections
            .lock()
            .ok()
            .and_then(|entries| entries.get(&attempt_id).cloned())
    }

    fn cache_missing_evidence_rejection(
        &self,
        attempt_id: AttemptId,
        rejection: MissingEvidenceRejection,
    ) {
        if let Ok(mut entries) = self.missing_evidence_rejections.lock() {
            entries.insert(attempt_id, rejection);
        }
    }

    fn clear_missing_evidence_rejection(&self, attempt_id: AttemptId) {
        if let Ok(mut entries) = self.missing_evidence_rejections.lock() {
            entries.remove(&attempt_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_cached_missing_evidence_rejection(&self, attempt_id: AttemptId) -> bool {
        self.cached_missing_evidence_rejection(attempt_id).is_some()
    }

    #[cfg(test)]
    pub(crate) async fn configured_busy_timeout_millis(&self) -> StoreResult<i64> {
        Ok(sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&self.pool)
            .await?)
    }

    async fn create_assignment_impl(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        let admitted = self
            .create_assignment_with_admission_impl(repo_root, draft, false, false)
            .await?;
        Ok((admitted.assignment, admitted.attempt))
    }

    async fn create_assignment_with_admission_impl(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
        selective: bool,
        isolated_integrator_available: bool,
    ) -> StoreResult<AdmittedAssignment> {
        let repository = repository_identity(repo_root)?;
        let mut assignment = draft.normalize(repo_root)?;
        if selective {
            assignment.validate_selective_role_contract()?;
        }
        if assignment.repository_id != repository.id {
            return Err(StoreError::InvalidScope(
                "repository root changed while the assignment was normalized".to_string(),
            ));
        }
        assignment.workspace_id = repository.workspace_id.clone();
        if assignment.workspace_strategy == WorkspaceStrategy::Auto {
            assignment.workspace_strategy = WorkspaceStrategy::Shared;
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
        crate::workspace::ensure_workspace_tx(&mut transaction, &repository).await?;
        assignment.start_epoch =
            crate::workspace::current_epoch_tx(&mut transaction, &repository.workspace_id).await?;
        // The assignment insert acquires SQLite's writer lock before dependency and coordination
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
        sqlx::query("INSERT INTO assignment_repositories (assignment_id, repository_id, canonical_root, bound_at, workspace_id) VALUES (?, ?, ?, ?, ?)")
            .bind(assignment.assignment_id.to_string())
            .bind(&repository.id)
            .bind(&repository.canonical_path)
            .bind(encode(&assignment.created_at)?)
            .bind(&repository.workspace_id)
            .execute(&mut *transaction)
            .await?;
        release_orphaned_claims_tx(&mut transaction, &repository.workspace_id).await?;
        let allowed_pending_gate = assignment.relation.as_ref().and_then(|relation| {
            let gate = match (assignment.role, relation.kind) {
                (AgentRole::Reviewer, RelationKind::Review) => GateKind::Review,
                (AgentRole::Verifier, RelationKind::Verification) => GateKind::Verification,
                _ => return None,
            };
            relation
                .target_assignment_ids
                .first()
                .copied()
                .map(|target| (target, gate))
        });
        validate_dependencies_tx(
            &mut transaction,
            assignment.assignment_id,
            Some(&assignment.repository_id),
            &assignment.dependencies,
            allowed_pending_gate,
        )
        .await?;
        validate_architecture_contract_reference_tx(&mut transaction, &assignment).await?;
        let (overlaps, integration_plan) = if selective {
            selective_admission_tx(&mut transaction, &assignment, isolated_integrator_available)
                .await?
        } else {
            (
                AdmissionOverlapSummary::default(),
                IntegrationPlan::SingleWriter,
            )
        };
        let supersedes = planned_claim_supersessions_tx(&mut transaction, &assignment).await?;
        insert_attempt(&mut transaction, &attempt).await?;
        claim_isolated_handoffs_tx(&mut transaction, &assignment).await?;
        for superseded in &supersedes {
            sqlx::query("UPDATE write_claims SET active = 0, released_at = ?, superseded_by = ? WHERE assignment_id = ? AND active = 1")
                .bind(encode(&Utc::now())?)
                .bind(assignment.assignment_id.to_string())
                .bind(superseded.to_string())
                .execute(&mut *transaction)
                .await?;
            sqlx::query(
                "UPDATE contract_claims SET active = 0, released_at = ?
                 WHERE assignment_id = ? AND active = 1",
            )
            .bind(encode(&Utc::now())?)
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
        if !assignment.write_scope.is_empty() {
            for contract in &assignment.contract_claims {
                sqlx::query(
                    "INSERT INTO contract_claims (
                        workspace_id, contract_name, assignment_id, attempt_id, active,
                        created_at
                     ) VALUES (?, ?, ?, ?, 1, ?)",
                )
                .bind(&repository.workspace_id)
                .bind(contract)
                .bind(assignment.assignment_id.to_string())
                .bind(attempt.attempt_id.to_string())
                .bind(encode(&attempt.created_at)?)
                .execute(&mut *transaction)
                .await?;
            }
        }
        sqlx::query(
            "INSERT INTO workspace_actors (
                workspace_id, actor_id, root_session_id, kind, assignment_id, attempt_id,
                strategy, state, last_progress_at, lease_expires_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
        )
        .bind(&repository.workspace_id)
        .bind(format!("attempt:{}", attempt.attempt_id))
        .bind(&assignment.root_session_id)
        .bind(encode(&crate::WorkspaceActorKind::Typed)?)
        .bind(assignment.assignment_id.to_string())
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&assignment.workspace_strategy)?)
        .bind(encode(&attempt.created_at)?)
        .bind(encode(
            &(attempt.created_at
                + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
        )?)
        .execute(&mut *transaction)
        .await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::Accepted,
            if selective {
                format!(
                    "typed assignment admitted; integration={integration_plan:?}; benign_read_overlap={}",
                    overlaps.benign_read_overlap_count
                )
            } else {
                "typed assignment accepted".to_string()
            },
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(AdmittedAssignment {
            assignment,
            attempt,
            overlaps,
            integration_plan,
        })
    }

    async fn attach_task_capsule_impl(
        &self,
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
        canonical_payload: String,
    ) -> StoreResult<Assignment> {
        let capsule: TaskCapsuleV1 = serde_json::from_str(&canonical_payload)
            .map_err(|error| StoreError::InvalidTaskCapsule(error.to_string()))?;
        if capsule.schema_version != 1 {
            return Err(StoreError::InvalidTaskCapsule(format!(
                "unsupported schema version {}",
                capsule.schema_version
            )));
        }
        if capsule.assignment_id != assignment_id || capsule.attempt_id != attempt_id {
            return Err(StoreError::InvalidTaskCapsule(
                "assignment or attempt identity does not match attachment target".to_string(),
            ));
        }
        let rendered = serde_json::to_string(&capsule)?;
        if rendered.as_bytes() != canonical_payload.as_bytes() {
            return Err(StoreError::InvalidTaskCapsule(
                "payload is not the canonical TaskCapsuleV1 serialization".to_string(),
            ));
        }

        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let mut assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.attempt_id != attempt_id || attempt.state != AttemptState::Active {
            return Err(StoreError::AttemptNotActive(attempt_id));
        }
        let capsule_path = task_capsule_path(&self.coordination_root, assignment_id);
        if capsule_path.try_exists()? {
            return Err(StoreError::TaskCapsuleAlreadyAttached(assignment_id));
        }
        if let Some(parent) = capsule_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary_path = capsule_path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file
            .write_all(canonical_payload.as_bytes())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error.into());
        }
        drop(file);
        if let Err(error) = std::fs::hard_link(&temporary_path, &capsule_path) {
            let _ = std::fs::remove_file(&temporary_path);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(StoreError::TaskCapsuleAlreadyAttached(assignment_id));
            }
            return Err(error.into());
        }
        let _ = std::fs::remove_file(&temporary_path);
        if let Err(error) = transaction.commit().await {
            let _ = std::fs::remove_file(&capsule_path);
            return Err(error.into());
        }
        assignment.task_capsule = Some(canonical_payload);
        Ok(assignment)
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
        let mut assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
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
        let gates: Vec<AgentGate> = gate_rows
            .into_iter()
            .map(|row| decode(row.get::<String, _>("body_json").as_str()))
            .collect::<StoreResult<Vec<_>>>()?;
        let mut validation_calls = sqlx::query(
            "SELECT body_json FROM validation_calls WHERE attempt_id = ? ORDER BY julianday(json_extract(recorded_at, '$')) DESC, call_id DESC LIMIT ?",
        )
        .bind(current_attempt.attempt_id.to_string())
        .bind(MAX_VALIDATION_CALLS_PER_TASK as i64)
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(|row| decode(row.get::<String, _>("body_json").as_str()))
        .collect::<StoreResult<Vec<_>>>()?;
        validation_calls.reverse();
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
        let epoch =
            crate::workspace::current_epoch_tx(&mut transaction, &assignment.workspace_id).await?;
        let actor_row = sqlx::query(
            "SELECT state, last_progress_at, lease_expires_at, nudge_sent_at FROM workspace_actors
             WHERE workspace_id = ? AND attempt_id = ?",
        )
        .bind(&assignment.workspace_id)
        .bind(current_attempt.attempt_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        let last_progress_at = actor_row
            .as_ref()
            .map(|row| decode(row.get::<String, _>("last_progress_at").as_str()))
            .transpose()?;
        let lease_state = actor_row
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("lease_expires_at"))
            .map(|value| decode::<chrono::DateTime<Utc>>(&value))
            .transpose()?
            .map(|expires_at| {
                if expires_at < Utc::now() {
                    crate::LeaseState::Expired
                } else {
                    crate::LeaseState::Active
                }
            });
        let nudge_sent_at = actor_row
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("nudge_sent_at"))
            .map(|value| decode::<chrono::DateTime<Utc>>(&value))
            .transpose()?;
        let isolation_handoff = sqlx::query(
            "SELECT assignment_id, source_workspace_id, source_epoch, source_manifest_hash,
                    covered_manifest_json, state, integrator_assignment_id, created_at,
                    integrated_at,
                    assignment_repositories.canonical_root AS source_repository_root
             FROM isolated_handoffs
             JOIN assignment_repositories USING (assignment_id)
             WHERE assignment_id = ?",
        )
        .bind(assignment_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .map(|row| isolation_handoff_from_row(&row))
        .transpose()?;
        let integration_handoffs = sqlx::query(
            "SELECT isolated_handoffs.assignment_id, source_workspace_id, source_epoch,
                    source_manifest_hash, covered_manifest_json, state,
                    integrator_assignment_id, isolated_handoffs.created_at,
                    integrated_at,
                    assignment_repositories.canonical_root AS source_repository_root
             FROM isolated_handoffs
             JOIN assignment_repositories USING (assignment_id)
             WHERE integrator_assignment_id = ?
             ORDER BY isolated_handoffs.assignment_id",
        )
        .bind(assignment_id.to_string())
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(|row| isolation_handoff_from_row(&row))
        .collect::<StoreResult<Vec<_>>>()?;
        let stale_recovery = sqlx::query(
            "SELECT stale_events, last_reason FROM stale_recovery WHERE attempt_id = ?",
        )
        .bind(current_attempt.attempt_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        let stale_events = stale_recovery
            .as_ref()
            .map(|row| row.get::<i64, _>("stale_events"))
            .unwrap_or(0);
        let stale_reason = stale_recovery
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("last_reason"));
        let pending_gates = gates
            .iter()
            .filter(|gate| gate.status == GateStatus::Pending)
            .map(|gate| gate.kind)
            .collect::<Vec<_>>();
        let next_required_action = stale_reason
            .as_ref()
            .map(|_| {
                if stale_events >= 2 || current_attempt.state == AttemptState::NeedsMain {
                    "root must reconcile the repeated stale state or restart the work in an isolated workspace"
                        .to_string()
                } else {
                    "reconcile stale inputs and run one targeted validation".to_string()
                }
            })
            .or_else(|| {
                (!pending_gates.is_empty())
                    .then(|| "resolve pending gates before completion".to_string())
            });
        transaction.commit().await?;
        hydrate_task_capsule(&self.coordination_root, &mut assignment)?;
        Ok(AgentTask {
            assignment,
            current_attempt,
            gates,
            receipt,
            validation_calls,
            workspace_status: WorkspaceTaskStatus {
                epoch,
                last_progress_at,
                lease_state,
                pending_gates,
                stale_reason,
                next_required_action,
                nudge_sent_at,
            },
            isolation_handoff,
            integration_handoffs,
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
        self.clear_missing_evidence_rejection(binding.attempt_id);
        Ok(binding)
    }

    async fn remove_agent_task_binding_impl(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
    ) -> StoreResult<bool> {
        actor.require_root()?;
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if !matches!(
            attempt.state,
            AttemptState::NeedsMain | AttemptState::Abandoned
        ) || attempt.sealed_at.is_none()
        {
            return Err(StoreError::InvalidAssignment(
                "an agent task binding may be removed only after a failed start seals the current attempt"
                    .to_string(),
            ));
        }
        let deleted = sqlx::query("DELETE FROM agent_task_bindings WHERE assignment_id = ?")
            .bind(assignment_id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        self.clear_missing_evidence_rejection(attempt.attempt_id);
        Ok(deleted.rows_affected() != 0)
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
        let limit = match limit {
            Some(limit) if limit > MAX_BINDING_LIMIT => {
                return Err(StoreError::InvalidBindingLimit(limit));
            }
            Some(limit) => limit as i64,
            // None is the explicit exhaustive form used by coordination paths.
            // SQLite's LIMIT -1 means no limit without duplicating the query.
            None => -1,
        };
        let rows = sqlx::query(
            "SELECT agent_task_bindings.assignment_id, agent_task_bindings.attempt_id,
                    agent_task_bindings.root_session_id, agent_path, task_name, thread_id,
                    bound_at, agent_task_bindings.updated_at
             FROM agent_task_bindings
             JOIN attempts ON attempts.attempt_id = agent_task_bindings.attempt_id
             WHERE agent_task_bindings.root_session_id = ?
             ORDER BY CASE WHEN attempts.state = '\"active\"' THEN 0 ELSE 1 END,
                      julianday(json_extract(agent_task_bindings.updated_at, '$')) DESC, agent_path,
                      agent_task_bindings.assignment_id
             LIMIT ?",
        )
        .bind(root_session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|row| binding_from_row(&row)).collect()
    }

    async fn heartbeat_typed_workspace_actor_impl(
        &self,
        binding: AgentTaskBinding,
    ) -> StoreResult<bool> {
        if binding
            .thread_id
            .as_deref()
            .is_none_or(|thread_id| thread_id.trim().is_empty())
        {
            return Ok(false);
        }
        let mut transaction = self.pool.begin().await?;
        let workspace_id = sqlx::query_scalar::<_, String>(
            "SELECT workspace_id FROM assignment_repositories WHERE assignment_id = ?",
        )
        .bind(binding.assignment_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(false);
        };
        let updated =
            heartbeat_typed_workspace_actor_tx(&mut transaction, &workspace_id, &binding).await?;
        transaction.commit().await?;
        Ok(updated)
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
        let acquire_started = Instant::now();
        let mut connection = self.pool.acquire().await.inspect_err(|error| {
            record_coordination_timing(
                "append_observation",
                "connection_acquire",
                acquire_started,
                Some(error),
            );
        })?;
        record_coordination_timing(
            "append_observation",
            "connection_acquire",
            acquire_started,
            None,
        );
        let begin_started = Instant::now();
        let mut transaction = connection.begin().await.inspect_err(|error| {
            record_coordination_timing(
                "append_observation",
                "transaction_begin",
                begin_started,
                Some(error),
            );
        })?;
        record_coordination_timing(
            "append_observation",
            "transaction_begin",
            begin_started,
            None,
        );
        let writer_started = Instant::now();
        let writer_result = lock_attempt_tx(&mut transaction, attempt_id).await;
        let writer_error = writer_result.as_ref().err().and_then(|error| match error {
            StoreError::Sql(error) => Some(error),
            _ => None,
        });
        record_coordination_timing(
            "append_observation",
            "writer_lock",
            writer_started,
            writer_error,
        );
        writer_result?;
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
        if kind.is_meaningful_progress() {
            let workspace_id =
                assignment_workspace_id_tx(&mut transaction, assignment.assignment_id).await?;
            sqlx::query(
                "UPDATE workspace_actors
                 SET last_progress_at = ?, state = 'active', nudge_sent_at = NULL,
                     lease_expires_at = ?
                 WHERE workspace_id = ? AND attempt_id = ?",
            )
            .bind(encode(&observation.created_at)?)
            .bind(encode(
                &(observation.created_at
                    + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
            )?)
            .bind(workspace_id)
            .bind(attempt_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
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
        let mut call = prepare_validation_call(&self.pool, call).await?;
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, call.attempt_id).await?;
        let attempt = load_attempt_tx(&mut transaction, call.attempt_id).await?;
        let current = load_current_attempt_tx(&mut transaction, attempt.assignment_id).await?;
        if current.attempt_id != call.attempt_id {
            return Err(StoreError::AttemptNotActive(call.attempt_id));
        }
        let attempt_is_active =
            attempt.state == AttemptState::Active && attempt.sealed_at.is_none();
        let mut reconciliation_exhausted = false;
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
            let late_supersession = existing.status == crate::ValidationCallStatus::Succeeded
                && call.status == crate::ValidationCallStatus::Superseded;
            if !attempt_is_active && !late_supersession {
                return Err(StoreError::AttemptNotActive(call.attempt_id));
            }
            if existing.status.is_terminal() && !late_supersession
                || !call.status.is_terminal()
                || existing.command_summary != call.command_summary
                || existing.resolved_executable != call.resolved_executable
                || existing.proof_kind != call.proof_kind
                || existing.evidence.start_epoch != call.evidence.start_epoch
                || existing.evidence.covered_manifest != call.evidence.covered_manifest
                || existing.evidence.manifest_hash != call.evidence.manifest_hash
                || call.recorded_at < existing.recorded_at
            {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
            let expected_status = if late_supersession {
                crate::ValidationCallStatus::Succeeded
            } else {
                crate::ValidationCallStatus::Running
            };
            let result = sqlx::query("UPDATE validation_calls SET body_json = ?, status = ?, recorded_at = ? WHERE call_id = ? AND attempt_id = ? AND status = ?")
                .bind(encode(&call)?)
                .bind(encode(&call.status)?)
                .bind(encode(&call.recorded_at)?)
                .bind(&call.call_id)
                .bind(call.attempt_id.to_string())
                .bind(encode(&expected_status)?)
                .execute(&mut *transaction)
                .await?;
            if result.rows_affected() != 1 {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
        } else {
            if !attempt_is_active {
                return Err(StoreError::AttemptNotActive(call.attempt_id));
            }
            if call.status != crate::ValidationCallStatus::Running {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
            if call.proof_kind == crate::ValidationProofKind::Focused {
                if !call
                    .resolved_executable
                    .as_deref()
                    .is_some_and(|path| Path::new(path).is_absolute())
                {
                    return Err(StoreError::InvalidAssignment(
                        "focused validation requires absolute resolved executable provenance"
                            .to_string(),
                    ));
                }
                let assignment =
                    load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
                if !matches!(
                    assignment.role,
                    AgentRole::Worker | AgentRole::Verifier | AgentRole::Integrator
                ) {
                    return Err(StoreError::InvalidAssignment(format!(
                        "{:?} assignments are not authorized to run focused validation",
                        assignment.role
                    )));
                }
                if !assignment
                    .required_evidence
                    .iter()
                    .any(|requirement| requirement == &call.command_summary)
                {
                    return Err(StoreError::InvalidAssignment(format!(
                        "focused validation command is not an exact required-evidence match: {}",
                        call.command_summary
                    )));
                }
            }
            let mut is_singleflight_leader = false;
            if call.proof_kind == crate::ValidationProofKind::Focused {
                let workspace_id =
                    assignment_workspace_id_tx(&mut transaction, attempt.assignment_id).await?;
                if let Some(evidence_key) = validation_evidence_key(&call)? {
                    let successful = sqlx::query(
                        "SELECT validation_calls.body_json
                         FROM validation_calls
                         JOIN attempts
                           ON attempts.attempt_id = validation_calls.attempt_id
                         JOIN assignment_repositories
                           ON assignment_repositories.assignment_id = attempts.assignment_id
                         WHERE assignment_repositories.workspace_id = ?
                           AND validation_calls.status = ?
                         ORDER BY validation_calls.recorded_at DESC",
                    )
                    .bind(&workspace_id)
                    .bind(encode(&crate::ValidationCallStatus::Succeeded)?)
                    .fetch_all(&mut *transaction)
                    .await?;
                    for row in successful {
                        let candidate: ValidationCall = decode(&row.get::<String, _>("body_json"))?;
                        if candidate.status != crate::ValidationCallStatus::Succeeded
                            || !candidate.evidence.is_reusable_success()
                        {
                            continue;
                        }
                        if validation_evidence_key(&candidate)?.as_ref() == Some(&evidence_key) {
                            call.evidence.shared_from_call_id = Some(
                                candidate
                                    .evidence
                                    .shared_from_call_id
                                    .unwrap_or(candidate.call_id),
                            );
                            break;
                        }
                    }
                }
                let fingerprint = validation_inflight_fingerprint(&call)?;
                let now = encode(&comparison_now())?;
                if call.evidence.shared_from_call_id.is_none() {
                    if let Some(leader) = sqlx::query_scalar::<_, String>(
                        "SELECT leader_call_id FROM validation_singleflight
                         WHERE workspace_id = ? AND start_epoch = ? AND fingerprint = ?
                           AND state = 'running'
                           AND julianday(json_extract(lease_expires_at, '$')) >= julianday(json_extract(?, '$'))",
                    )
                    .bind(&workspace_id)
                    .bind(call.evidence.start_epoch as i64)
                    .bind(&fingerprint)
                    .bind(&now)
                    .fetch_optional(&mut *transaction)
                    .await?
                    {
                        call.evidence.shared_from_call_id = Some(leader);
                    } else {
                        is_singleflight_leader = true;
                    }
                }
            }
            sqlx::query("INSERT INTO validation_calls (call_id, attempt_id, body_json, status, recorded_at) VALUES (?, ?, ?, ?, ?)")
                .bind(&call.call_id)
                .bind(call.attempt_id.to_string())
                .bind(encode(&call)?)
                .bind(encode(&call.status)?)
                .bind(encode(&call.recorded_at)?)
                .execute(&mut *transaction)
                .await?;
            if is_singleflight_leader {
                let workspace_id =
                    assignment_workspace_id_tx(&mut transaction, attempt.assignment_id).await?;
                sqlx::query(
                    "INSERT INTO validation_singleflight (
                        workspace_id, start_epoch, fingerprint, leader_call_id, state,
                        lease_expires_at, updated_at
                     ) VALUES (?, ?, ?, ?, 'running', ?, ?)
                     ON CONFLICT(workspace_id, start_epoch, fingerprint) DO UPDATE SET
                        leader_call_id = excluded.leader_call_id,
                        state = 'running',
                        lease_expires_at = excluded.lease_expires_at,
                        updated_at = excluded.updated_at",
                )
                .bind(workspace_id)
                .bind(call.evidence.start_epoch as i64)
                .bind(validation_inflight_fingerprint(&call)?)
                .bind(&call.call_id)
                .bind(
                    call.evidence
                        .lease_expires_at
                        .map(|value| encode(&value))
                        .transpose()?
                        .unwrap_or_else(|| encode(&comparison_now()).unwrap_or_default()),
                )
                .bind(encode(&comparison_now())?)
                .execute(&mut *transaction)
                .await?;
            }
            reconciliation_exhausted =
                reserve_reconciliation_validation_tx(&mut transaction, &attempt, &call.call_id)
                    .await?;
        }
        if call.status.is_terminal() {
            sqlx::query(
                "UPDATE validation_singleflight SET state = ?, updated_at = ?
                 WHERE leader_call_id = ? AND state = 'running'",
            )
            .bind(if call.status == crate::ValidationCallStatus::Superseded {
                "superseded"
            } else {
                "terminal"
            })
            .bind(encode(&comparison_now())?)
            .bind(&call.call_id)
            .execute(&mut *transaction)
            .await?;
            if call.status == crate::ValidationCallStatus::Superseded {
                record_stale_event_tx(
                    &mut transaction,
                    &attempt,
                    call.evidence.end_epoch.unwrap_or(call.evidence.start_epoch),
                    call.evidence
                        .stale_reason
                        .as_deref()
                        .unwrap_or("validation inputs changed"),
                )
                .await?;
            }
            if attempt_is_active {
                let workspace_id =
                    assignment_workspace_id_tx(&mut transaction, attempt.assignment_id).await?;
                let progress_at = comparison_now();
                sqlx::query(
                    "UPDATE workspace_actors
                     SET last_progress_at = ?, nudge_sent_at = NULL, lease_expires_at = ?
                     WHERE workspace_id = ? AND attempt_id = ? AND state <> 'terminal'",
                )
                .bind(encode(&progress_at)?)
                .bind(encode(
                    &(progress_at
                        + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
                )?)
                .bind(workspace_id)
                .bind(attempt.attempt_id.to_string())
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        if reconciliation_exhausted {
            return Err(StoreError::StaleRecoveryExhausted(call.attempt_id));
        }
        Ok(())
    }

    async fn get_validation_call_impl(
        &self,
        call_id: String,
    ) -> StoreResult<Option<ValidationCall>> {
        if call_id.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "validation call id cannot be empty".to_string(),
            ));
        }
        sqlx::query_scalar::<_, String>("SELECT body_json FROM validation_calls WHERE call_id = ?")
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|body| decode(&body))
            .transpose()
    }

    async fn heartbeat_validation_call_impl(
        &self,
        call_id: String,
        lease_expires_at: chrono::DateTime<Utc>,
    ) -> StoreResult<bool> {
        let mut transaction = self.pool.begin().await?;
        let body = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM validation_calls
             WHERE call_id = ? AND status = ?",
        )
        .bind(&call_id)
        .bind(encode(&crate::ValidationCallStatus::Running)?)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(body) = body else {
            transaction.commit().await?;
            return Ok(false);
        };
        let mut call: ValidationCall = decode(&body)?;
        let binding = sqlx::query(
            "SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id,
                    bound_at, updated_at
             FROM agent_task_bindings
             WHERE attempt_id = ?",
        )
        .bind(call.attempt_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .map(|row| binding_from_row(&row))
        .transpose()?;
        if let Some(binding) = binding {
            let workspace_id =
                assignment_workspace_id_tx(&mut transaction, binding.assignment_id).await?;
            if !heartbeat_typed_workspace_actor_tx(&mut transaction, &workspace_id, &binding)
                .await?
            {
                transaction.commit().await?;
                return Ok(false);
            }
        }
        call.evidence.lease_expires_at = Some(lease_expires_at);
        let updated = sqlx::query(
            "UPDATE validation_calls SET body_json = ?
             WHERE call_id = ? AND status = ?",
        )
        .bind(encode(&call)?)
        .bind(&call_id)
        .bind(encode(&crate::ValidationCallStatus::Running)?)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE validation_singleflight
                 SET lease_expires_at = ?, updated_at = ?
                 WHERE leader_call_id = ? AND state = 'running'",
            )
            .bind(encode(&lease_expires_at)?)
            .bind(encode(&Utc::now())?)
            .bind(&call_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn refresh_validation_calls_impl(
        &self,
        attempt_id: AttemptId,
        validation_call_ids: &[String],
    ) -> StoreResult<Vec<String>> {
        let mut stale_call_ids = Vec::new();
        for call_id in validation_call_ids {
            let Some(body) = sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM validation_calls WHERE call_id = ?",
            )
            .bind(call_id)
            .fetch_optional(&self.pool)
            .await?
            else {
                continue;
            };
            let mut refreshed: ValidationCall = decode(&body)?;
            if refreshed.attempt_id != attempt_id
                || refreshed.status != crate::ValidationCallStatus::Succeeded
            {
                continue;
            }
            refreshed.recorded_at = Utc::now();
            refreshed = prepare_validation_call(&self.pool, refreshed).await?;
            if refreshed.status == crate::ValidationCallStatus::Superseded {
                stale_call_ids.push(call_id.clone());
                self.record_validation_call_impl(refreshed).await?;
            }
        }
        Ok(stale_call_ids)
    }

    async fn replay_required_evidence_missing_impl(
        &self,
        attempt_id: AttemptId,
        receipt: &ReceiptDraft,
    ) -> StoreResult<Option<Vec<MissingEvidenceObligation>>> {
        let Some(cached) = self.cached_missing_evidence_rejection(attempt_id) else {
            return Ok(None);
        };
        if receipt.status != AgentStatusClaim::Completed || !required_evidence_cache_eligible() {
            self.clear_missing_evidence_rejection(attempt_id);
            return Ok(None);
        }

        let Some(initial_fingerprint) = self
            .current_missing_evidence_fingerprint(attempt_id, receipt)
            .await?
        else {
            self.clear_missing_evidence_rejection(attempt_id);
            return Ok(None);
        };
        if initial_fingerprint != cached.fingerprint {
            self.clear_missing_evidence_rejection(attempt_id);
            return Ok(None);
        }

        // A partial-missing result may become stale due to host mutation. Refresh
        // every referenced successful validation before consulting its revision.
        // A full-missing result has no validation obligation that host mutation
        // can satisfy, so the cheap persisted workspace epoch is sufficient.
        if cached.obligations.len() < cached.total_obligation_count {
            let _ = self
                .refresh_validation_calls_impl(attempt_id, &receipt.validation_call_ids)
                .await?;
            let refreshed_fingerprint = self
                .current_missing_evidence_fingerprint(attempt_id, receipt)
                .await?;
            if refreshed_fingerprint.as_ref() != Some(&cached.fingerprint) {
                self.clear_missing_evidence_rejection(attempt_id);
                return Ok(None);
            }
        }

        Ok(Some(cached.obligations))
    }

    async fn current_missing_evidence_fingerprint(
        &self,
        attempt_id: AttemptId,
        receipt: &ReceiptDraft,
    ) -> StoreResult<Option<MissingEvidenceFingerprint>> {
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = load_attempt_tx(&mut transaction, attempt_id).await?;
        if attempt.state.is_terminal() || attempt.sealed_at.is_some() {
            transaction.commit().await?;
            return Ok(None);
        }
        let current = load_current_attempt_tx(&mut transaction, attempt.assignment_id).await?;
        if current.attempt_id != attempt_id {
            transaction.commit().await?;
            return Ok(None);
        }
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        let fingerprint =
            missing_evidence_fingerprint_tx(&mut transaction, &assignment, &attempt, receipt)
                .await?;
        transaction.commit().await?;
        Ok(fingerprint)
    }

    async fn require_root_receipt_evidence_current_impl(
        &self,
        root_session_id: &str,
    ) -> StoreResult<()> {
        let rows = sqlx::query(
            "SELECT receipts.body_json
             FROM receipts
             JOIN assignments USING (assignment_id)
             JOIN attempts USING (attempt_id)
             WHERE assignments.root_session_id = ?
               AND NOT EXISTS (
                   SELECT 1 FROM attempts AS newer
                   WHERE newer.assignment_id = attempts.assignment_id
                     AND newer.ordinal > attempts.ordinal
               )",
        )
        .bind(root_session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut stale_call_ids = BTreeSet::new();
        for row in rows {
            let receipt: AgentReceipt = decode(&row.get::<String, _>("body_json"))?;
            if !receipt.status.is_success() {
                continue;
            }
            stale_call_ids.extend(
                self.refresh_validation_calls_impl(
                    receipt.attempt_id,
                    &receipt.validation_call_ids,
                )
                .await?,
            );
            for call_id in &receipt.validation_call_ids {
                let call = self
                    .get_validation_call_impl(call_id.clone())
                    .await?
                    .ok_or_else(|| {
                        StoreError::CorruptData(format!(
                            "sealed receipt references missing validation call {call_id}"
                        ))
                    })?;
                if call.status == crate::ValidationCallStatus::Superseded
                    || call.evidence.stale_reason.is_some()
                {
                    stale_call_ids.insert(call_id.clone());
                } else if !call.status.is_success() {
                    return Err(StoreError::ValidationCallStatusInvalid {
                        call_ids: vec![call_id.clone()],
                    });
                }
            }
        }
        if stale_call_ids.is_empty() {
            Ok(())
        } else {
            Err(StoreError::EvidenceSuperseded {
                call_ids: stale_call_ids.into_iter().collect(),
            })
        }
    }

    async fn submit_agent_receipt_impl(
        &self,
        attempt_id: AttemptId,
        mut draft: ReceiptDraft,
        review_reason: Option<String>,
    ) -> StoreResult<AgentReceipt> {
        if draft.summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "receipt summary cannot be empty".to_string(),
            ));
        }
        if draft.status == AgentStatusClaim::Completed {
            let stale_call_ids = self
                .refresh_validation_calls_impl(attempt_id, &draft.validation_call_ids)
                .await?;
            if !stale_call_ids.is_empty() {
                return Err(StoreError::EvidenceSuperseded {
                    call_ids: stale_call_ids,
                });
            }
        }
        let handoff_action = if draft.status == AgentStatusClaim::Completed {
            self.prepare_receipt_handoff_action(attempt_id).await?
        } else {
            None
        };
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
        let mut validation_summaries = HashSet::new();
        let mut validation_manifest_hashes = BTreeSet::new();
        let mut transaction_stale_call_ids = Vec::new();
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
            if draft.status == AgentStatusClaim::Completed
                && call.status == crate::ValidationCallStatus::Succeeded
                && let Some((reason, current_epoch)) =
                    validation_stale_event_reason_tx(&mut transaction, &assignment, &call).await?
            {
                let mut superseded = call.clone();
                superseded.status = crate::ValidationCallStatus::Superseded;
                superseded.evidence.end_epoch = Some(current_epoch);
                superseded.evidence.stale_reason = Some(reason.clone());
                superseded.recorded_at = Utc::now();
                let updated = sqlx::query(
                    "UPDATE validation_calls
                     SET body_json = ?, status = ?, recorded_at = ?
                     WHERE call_id = ? AND attempt_id = ? AND status = ?",
                )
                .bind(encode(&superseded)?)
                .bind(encode(&superseded.status)?)
                .bind(encode(&superseded.recorded_at)?)
                .bind(call_id)
                .bind(attempt_id.to_string())
                .bind(encode(&crate::ValidationCallStatus::Succeeded)?)
                .execute(&mut *transaction)
                .await?;
                if updated.rows_affected() != 1 {
                    return Err(StoreError::ValidationCallImmutable(call_id.clone()));
                }
                record_stale_event_tx(&mut transaction, &attempt, current_epoch, &reason).await?;
                transaction_stale_call_ids.push(call_id.clone());
                continue;
            }
            let completion_proof = call.status.is_success()
                && call.proof_kind == crate::ValidationProofKind::Focused
                && call.evidence.end_epoch.is_some()
                && call.evidence.stale_reason.is_none()
                && call
                    .resolved_executable
                    .as_deref()
                    .is_some_and(|path| Path::new(path).is_absolute());
            if !call.status.is_terminal()
                || draft.status == AgentStatusClaim::Completed && !completion_proof
            {
                invalid_statuses.push(call_id.clone());
            }
            if completion_proof {
                validation_summaries.insert(call.command_summary);
                validation_manifest_hashes.insert(call.evidence.manifest_hash);
            }
        }
        if !transaction_stale_call_ids.is_empty() {
            transaction.commit().await?;
            return Err(StoreError::EvidenceSuperseded {
                call_ids: transaction_stale_call_ids,
            });
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
        if draft.status == AgentStatusClaim::Completed {
            let missing_obligations =
                missing_evidence_obligations(&assignment, &validation_summaries);
            if !missing_obligations.is_empty() {
                if let Some(fingerprint) =
                    missing_evidence_fingerprint_tx(&mut transaction, &assignment, &attempt, &draft)
                        .await?
                {
                    self.cache_missing_evidence_rejection(
                        attempt_id,
                        MissingEvidenceRejection {
                            fingerprint,
                            obligations: missing_obligations.clone(),
                            total_obligation_count: assignment.required_evidence.len(),
                        },
                    );
                }
                return Err(StoreError::RequiredEvidenceMissing {
                    obligations: missing_obligations,
                });
            }
            validate_completed_mutation_evidence_tx(
                &mut transaction,
                &assignment,
                attempt_id,
                &mut draft,
            )
            .await?;
        }
        if let Some(review_reason) = review_reason.as_deref() {
            if draft.status != AgentStatusClaim::Completed {
                return Err(StoreError::InvalidAssignment(
                    "cold review may be required only for a completed receipt".to_string(),
                ));
            }
            insert_risk_review_gates_tx(
                &mut transaction,
                attempt.assignment_id,
                attempt_id,
                review_reason,
            )
            .await?;
        }
        let evidence_epoch = assignment_epoch_tx(&mut transaction, attempt.assignment_id).await?;
        let evidence_manifest_hash = format!(
            "{:x}",
            Sha256::digest(encode(&validation_manifest_hashes)?.as_bytes())
        );
        let architecture_contract = seal_architecture_contract_for_receipt_tx(
            &mut transaction,
            &assignment,
            draft.status,
            draft.architecture_contract.take(),
        )
        .await?;
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
            architecture_contract,
            evidence_epoch,
            evidence_manifest_hash,
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
        if let Some(action) = handoff_action {
            persist_receipt_handoff_action_tx(&mut transaction, action).await?;
        }
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
        queue_collectible_snapshots_tx(&mut transaction, attempt.assignment_id).await?;
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("receipt submission")
            .await;
        self.clear_missing_evidence_rejection(attempt_id);
        Ok(receipt)
    }

    async fn prepare_receipt_handoff_action(
        &self,
        attempt_id: AttemptId,
    ) -> StoreResult<Option<ReceiptHandoffAction>> {
        let context = validation_context(&self.pool, attempt_id).await?;
        if context.assignment.workspace_strategy == WorkspaceStrategy::Isolated {
            let paths = context
                .assignment
                .write_scope
                .iter()
                .map(|scope| scope.path.clone())
                .collect::<Vec<_>>();
            let revision =
                crate::workspace::capture_revision(&self.pool, &context.repo_root, paths).await?;
            return Ok(Some(ReceiptHandoffAction::Publish(IsolationHandoff {
                assignment_id: context.assignment.assignment_id,
                source_workspace_id: revision.workspace_id,
                source_repository_root: Some(context.repo_root.to_string_lossy().into_owned()),
                source_epoch: revision.epoch,
                source_manifest_hash: revision.manifest_hash,
                covered_manifest: revision.files,
                state: IsolationHandoffState::Ready,
                integrator_assignment_id: None,
                created_at: Utc::now(),
                integrated_at: None,
            })));
        }
        let Some(relation) = context.assignment.relation.as_ref() else {
            return Ok(None);
        };
        if context.assignment.role != AgentRole::Integrator
            || relation.kind != RelationKind::Integration
        {
            return Ok(None);
        }
        let mut integrated_targets = Vec::new();
        for target in &relation.target_assignment_ids {
            let row = sqlx::query(
                "SELECT assignments.body_json, assignment_repositories.canonical_root
                 FROM assignments
                 JOIN assignment_repositories USING (assignment_id)
                 WHERE assignments.assignment_id = ?",
            )
            .bind(target.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::AssignmentNotFound(*target))?;
            let target_assignment: Assignment = decode(row.get::<String, _>("body_json").as_str())?;
            if target_assignment.workspace_strategy != WorkspaceStrategy::Isolated {
                continue;
            }
            let handoff_row = sqlx::query(
                "SELECT assignment_id, source_workspace_id, source_epoch, source_manifest_hash,
                        covered_manifest_json, state, integrator_assignment_id, created_at,
                        integrated_at
                 FROM isolated_handoffs WHERE assignment_id = ?",
            )
            .bind(target.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| {
                StoreError::InvalidAssignment(format!(
                    "isolated dependency {target} has no versioned handoff"
                ))
            })?;
            let mut handoff = isolation_handoff_from_row(&handoff_row)?;
            handoff.source_repository_root = Some(row.get::<String, _>("canonical_root"));
            if handoff.state != IsolationHandoffState::Claimed
                || handoff.integrator_assignment_id != Some(context.assignment.assignment_id)
            {
                return Err(StoreError::InvalidAssignment(format!(
                    "isolated handoff {target} is not claimed by integrator {}",
                    context.assignment.assignment_id
                )));
            }
            let paths = handoff
                .covered_manifest
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>();
            let canonical_root = row.get::<String, _>("canonical_root");
            let current =
                crate::workspace::capture_revision(&self.pool, Path::new(&canonical_root), paths)
                    .await?;
            if current.manifest_hash != handoff.source_manifest_hash {
                return Err(StoreError::IsolationHandoffSuperseded(*target));
            }
            integrated_targets.push(*target);
        }
        Ok((!integrated_targets.is_empty())
            .then_some(ReceiptHandoffAction::Integrate(integrated_targets)))
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
        release_orphaned_claims_tx(&mut transaction, &assignment.workspace_id).await?;
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
        insert_attempt(&mut transaction, &next).await?;
        let binding_attempt = sqlx::query_scalar::<_, String>(
            "SELECT attempt_id FROM agent_task_bindings WHERE assignment_id = ?",
        )
        .bind(assignment_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(binding_attempt) = binding_attempt {
            if binding_attempt != current.attempt_id.to_string() {
                return Err(StoreError::CorruptData(format!(
                    "assignment {assignment_id} binding does not reference current attempt {}",
                    current.attempt_id
                )));
            }
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
                    "assignment {assignment_id} binding changed during amendment"
                )));
            }
        }
        if !assignment.write_scope.is_empty() {
            sqlx::query(
                "INSERT INTO write_claims (
                    assignment_id, attempt_id, scopes_json, supersedes_json, active, created_at,
                    released_at, superseded_by
                 ) VALUES (?, ?, ?, ?, 1, ?, NULL, NULL)
                 ON CONFLICT(assignment_id) DO UPDATE SET
                    attempt_id = excluded.attempt_id,
                    scopes_json = excluded.scopes_json,
                    supersedes_json = excluded.supersedes_json,
                    active = 1,
                    released_at = NULL,
                    superseded_by = NULL",
            )
            .bind(assignment_id.to_string())
            .bind(next.attempt_id.to_string())
            .bind(encode(&assignment.write_scope)?)
            .bind(encode(&Vec::<AssignmentId>::new())?)
            .bind(encode(&next.created_at)?)
            .execute(&mut *transaction)
            .await?;
        }
        for contract in &assignment.contract_claims {
            sqlx::query(
                "INSERT INTO contract_claims (
                    workspace_id, contract_name, assignment_id, attempt_id, active,
                    created_at, released_at
                 ) VALUES (?, ?, ?, ?, 1, ?, NULL)
                 ON CONFLICT(workspace_id, contract_name, assignment_id) DO UPDATE SET
                    attempt_id = excluded.attempt_id,
                    active = 1,
                    released_at = NULL",
            )
            .bind(&assignment.workspace_id)
            .bind(contract)
            .bind(assignment_id.to_string())
            .bind(next.attempt_id.to_string())
            .bind(encode(&next.created_at)?)
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query(
            "UPDATE workspace_actors SET actor_id = ?, attempt_id = ?, state = 'active',
             last_progress_at = ?, lease_expires_at = ?, nudge_sent_at = NULL
             WHERE assignment_id = ? AND attempt_id = ?",
        )
        .bind(format!("attempt:{}", next.attempt_id))
        .bind(next.attempt_id.to_string())
        .bind(encode(&next.created_at)?)
        .bind(encode(
            &(next.created_at + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
        )?)
        .bind(assignment_id.to_string())
        .bind(current.attempt_id.to_string())
        .execute(&mut *transaction)
        .await?;
        let gate_now = Utc::now();
        let gate_epoch = assignment_epoch_tx(&mut transaction, assignment_id).await?;
        let correction_gate = AgentGate {
            assignment_id,
            kind: GateKind::Review,
            status: GateStatus::Pending,
            reason: "correction attempt requires a new review verdict".to_string(),
            waiver_reason: None,
            evidence_epoch: gate_epoch,
            updated_at: gate_now,
            sealed_at: None,
        };
        let reset_gate = sqlx::query("UPDATE gates SET status = ?, body_json = ?, updated_at = ?, sealed_at = NULL WHERE assignment_id = ? AND kind = ? AND status = ?")
            .bind(encode(&GateStatus::Pending)?)
            .bind(encode(&correction_gate)?)
            .bind(encode(&gate_now)?)
            .bind(assignment_id.to_string())
            .bind(encode(&GateKind::Review)?)
            .bind(encode(&GateStatus::ChangesRequested)?)
            .execute(&mut *transaction)
            .await?;
        if reset_gate.rows_affected() != 1 {
            return Err(StoreError::CorruptData(format!(
                "assignment {assignment_id} review gate changed during amendment"
            )));
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
        self.clear_missing_evidence_rejection(current.attempt_id);
        self.clear_missing_evidence_rejection(next.attempt_id);
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
            architecture_contract: None,
            evidence_epoch: assignment_epoch_tx(&mut transaction, assignment_id).await?,
            evidence_manifest_hash: String::new(),
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
        queue_collectible_snapshots_tx(&mut transaction, assignment_id).await?;
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("assignment abandonment")
            .await;
        self.clear_missing_evidence_rejection(attempt.attempt_id);
        Ok(receipt)
    }

    async fn set_agent_gate_impl(
        &self,
        actor: TaskActor,
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
        if status == GateStatus::Waived {
            return Err(StoreError::GateWaiverRequired {
                gate: kind.to_string(),
            });
        }
        if status.is_sealed()
            && let Some((target_attempt_id, validation_call_ids)) =
                assignment_receipt_validation_ids(&self.pool, assignment_id).await?
        {
            let stale_call_ids = self
                .refresh_validation_calls_impl(target_attempt_id, &validation_call_ids)
                .await?;
            if !stale_call_ids.is_empty() {
                return Err(StoreError::EvidenceSuperseded {
                    call_ids: stale_call_ids,
                });
            }
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        require_gate_actor_tx(&mut transaction, actor, &assignment, kind).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if status.is_sealed() {
            let stale_call_ids =
                supersede_stale_receipt_validations_tx(&mut transaction, &assignment, &attempt)
                    .await?;
            if !stale_call_ids.is_empty() {
                transaction.commit().await?;
                return Err(StoreError::EvidenceSuperseded {
                    call_ids: stale_call_ids,
                });
            }
        }
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
        let evidence_epoch = assignment_epoch_tx(&mut transaction, assignment_id).await?;
        let gate = AgentGate {
            assignment_id,
            kind,
            status,
            reason,
            waiver_reason: None,
            evidence_epoch,
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
        if gate.status.is_sealed() {
            record_gate_verdict_tx(&mut transaction, attempt.attempt_id, &gate).await?;
        }
        let needs_main = gate_requires_main_intervention(&attempt, kind, status);
        if needs_main {
            transition_attempt_to_needs_main_tx(&mut transaction, &attempt).await?;
        }
        if kind == GateKind::Review && status == GateStatus::Passed {
            ensure_pending_verification_for_risk_review_tx(&mut transaction, assignment_id).await?;
        }
        release_successful_claim_if_unblocked_tx(&mut transaction, assignment_id).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::GateChanged,
            format!("{kind} gate is {status:?}"),
            None,
        )
        .await?;
        if needs_main {
            append_observation_tx(
                &mut transaction,
                &assignment,
                attempt.attempt_id,
                ObservationKind::NeedsMain,
                "review or verification could not be resolved within the bounded workflow"
                    .to_string(),
                None,
            )
            .await?;
        }
        queue_collectible_snapshots_tx(&mut transaction, assignment_id).await?;
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("gate verdict submission")
            .await;
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
        if let Some((target_attempt_id, validation_call_ids)) =
            assignment_receipt_validation_ids(&self.pool, assignment_id).await?
        {
            let stale_call_ids = self
                .refresh_validation_calls_impl(target_attempt_id, &validation_call_ids)
                .await?;
            if !stale_call_ids.is_empty() {
                return Err(StoreError::EvidenceSuperseded {
                    call_ids: stale_call_ids,
                });
            }
        }
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        let stale_call_ids =
            supersede_stale_receipt_validations_tx(&mut transaction, &assignment, &attempt).await?;
        if !stale_call_ids.is_empty() {
            transaction.commit().await?;
            return Err(StoreError::EvidenceSuperseded {
                call_ids: stale_call_ids,
            });
        }
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
        let evidence_epoch = assignment_epoch_tx(&mut transaction, assignment_id).await?;
        let gate = AgentGate {
            assignment_id,
            kind,
            status: GateStatus::Waived,
            reason: "root waived soft gate".to_string(),
            waiver_reason: Some(reason),
            evidence_epoch,
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
        record_gate_verdict_tx(&mut transaction, attempt.attempt_id, &gate).await?;
        if kind == GateKind::Review {
            ensure_pending_verification_for_risk_review_tx(&mut transaction, assignment_id).await?;
        }
        release_successful_claim_if_unblocked_tx(&mut transaction, assignment_id).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::GateChanged,
            format!("{kind} gate is waived"),
            None,
        )
        .await?;
        queue_collectible_snapshots_tx(&mut transaction, assignment_id).await?;
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("gate waiver")
            .await;
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
                lost_to_retention_count: 0,
                remaining_count: 0,
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
            lost_to_retention_count: lost_to_retention,
            remaining_count: not_returned,
            truncated_count: lost_to_retention + not_returned,
        })
    }

    async fn automatic_wake_cursor_impl(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
    ) -> StoreResult<Option<WakeEventId>> {
        if root_session_id.trim().is_empty() || consuming_agent_path.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "automatic wake cursor keys cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        if let Some(row) = sqlx::query(
            "SELECT event_id FROM automatic_wake_cursors
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .fetch_optional(&mut *transaction)
        .await?
        {
            let event_id = row
                .get::<Option<String>, _>("event_id")
                .map(|value| WakeEventId::parse(&value))
                .transpose()?;
            transaction.commit().await?;
            return Ok(event_id);
        }

        let snapshot_before = sqlx::query(
            "SELECT event_id FROM wake_events
             WHERE root_session_id = ?
             ORDER BY wake_sequence DESC
             LIMIT 1 OFFSET ?",
        )
        .bind(&root_session_id)
        .bind(MAX_WAKE_EVENTS_PER_READ as i64)
        .fetch_optional(&mut *transaction)
        .await?
        .map(|row| row.get::<String, _>("event_id"));
        sqlx::query(
            "INSERT OR IGNORE INTO automatic_wake_cursors
             (root_session_id, consuming_agent_path, event_id, updated_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .bind(&snapshot_before)
        .bind(encode(&Utc::now())?)
        .execute(&mut *transaction)
        .await?;
        let stored = sqlx::query(
            "SELECT event_id FROM automatic_wake_cursors
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .fetch_one(&mut *transaction)
        .await?
        .get::<Option<String>, _>("event_id")
        .map(|value| WakeEventId::parse(&value))
        .transpose()?;
        transaction.commit().await?;
        Ok(stored)
    }

    async fn compare_and_swap_automatic_wake_cursor_impl(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
        expected: Option<WakeEventId>,
        next: WakeEventId,
    ) -> StoreResult<bool> {
        let expected = expected.map(|value| value.to_string());
        let changed = sqlx::query(
            "UPDATE automatic_wake_cursors
             SET event_id = ?, updated_at = ?
             WHERE root_session_id = ? AND consuming_agent_path = ?
               AND ((event_id = ?) OR (event_id IS NULL AND ? IS NULL))",
        )
        .bind(next.to_string())
        .bind(encode(&Utc::now())?)
        .bind(root_session_id)
        .bind(consuming_agent_path)
        .bind(&expected)
        .bind(&expected)
        .execute(&self.pool)
        .await?;
        Ok(changed.rows_affected() == 1)
    }

    async fn reserve_stalled_nudge_impl(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> StoreResult<bool> {
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.state != AttemptState::Active {
            transaction.commit().await?;
            return Ok(false);
        }
        let workspace_id = assignment_workspace_id_tx(&mut transaction, assignment_id).await?;
        let now = comparison_now();
        let active_operation = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                SELECT 1 FROM validation_calls
                WHERE attempt_id = ? AND status = ?
                  AND julianday(json_extract(body_json, '$.evidence.lease_expires_at'))
                      >= julianday(json_extract(?, '$'))
             )",
        )
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&ValidationCallStatus::Running)?)
        .bind(encode(&now)?)
        .fetch_one(&mut *transaction)
        .await?
            != 0;
        if active_operation {
            transaction.commit().await?;
            return Ok(false);
        }
        let updated = sqlx::query(
            "UPDATE workspace_actors
             SET nudge_sent_at = ?
             WHERE workspace_id = ? AND attempt_id = ? AND state <> 'terminal'
               AND julianday(json_extract(last_progress_at, '$')) <= julianday(json_extract(?, '$'))
               AND nudge_sent_at IS NULL",
        )
        .bind(encode(&now)?)
        .bind(workspace_id)
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&no_progress_before)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn recover_nonproductive_assignment_impl(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> StoreResult<NonproductiveRecovery> {
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.state != AttemptState::Active || attempt.sealed_at.is_some() {
            transaction.commit().await?;
            return Ok(NonproductiveRecovery::NotEligible);
        }
        let workspace_id = assignment_workspace_id_tx(&mut transaction, assignment_id).await?;
        let now = comparison_now();
        let idle = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                SELECT 1 FROM workspace_actors
                WHERE workspace_id = ? AND attempt_id = ? AND state <> 'terminal'
                  AND julianday(json_extract(last_progress_at, '$'))
                      <= julianday(json_extract(?, '$'))
             )",
        )
        .bind(&workspace_id)
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&no_progress_before)?)
        .fetch_one(&mut *transaction)
        .await?
            != 0;
        if !idle {
            transaction.commit().await?;
            return Ok(NonproductiveRecovery::NotEligible);
        }
        let active_owned_operation_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM validation_calls
             WHERE attempt_id = ? AND status = ?
               AND julianday(json_extract(body_json, '$.evidence.lease_expires_at'))
                   >= julianday(json_extract(?, '$'))",
        )
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&ValidationCallStatus::Running)?)
        .bind(encode(&now)?)
        .fetch_one(&mut *transaction)
        .await?;
        let active_owned_operation_count =
            u32::try_from(active_owned_operation_count).unwrap_or(u32::MAX);
        if active_owned_operation_count > 0 {
            transaction.commit().await?;
            return Ok(NonproductiveRecovery::Suspended(ProductivitySummary {
                active_owned_operation_count,
                cancelled_expired_operation_count: 0,
            }));
        }

        let expired_calls = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM validation_calls
             WHERE attempt_id = ? AND status = ?
               AND (
                   json_extract(body_json, '$.evidence.lease_expires_at') IS NULL
                   OR julianday(json_extract(body_json, '$.evidence.lease_expires_at'))
                       < julianday(json_extract(?, '$'))
               )",
        )
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&ValidationCallStatus::Running)?)
        .bind(encode(&now)?)
        .fetch_all(&mut *transaction)
        .await?;
        let cancelled_expired_operation_count =
            u32::try_from(expired_calls.len()).unwrap_or(u32::MAX);
        let current_epoch = assignment_epoch_tx(&mut transaction, assignment_id).await?;
        for body in expired_calls {
            let mut call: ValidationCall = decode(&body)?;
            call.status = ValidationCallStatus::Cancelled;
            call.evidence.end_epoch = Some(current_epoch);
            call.evidence.successful_result = Some(false);
            call.evidence
                .stale_reason
                .get_or_insert_with(|| "owned operation hard deadline elapsed".to_string());
            call.recorded_at = now;
            sqlx::query(
                "UPDATE validation_calls SET body_json = ?, status = ?, recorded_at = ?
                 WHERE call_id = ? AND attempt_id = ? AND status = ?",
            )
            .bind(encode(&call)?)
            .bind(encode(&call.status)?)
            .bind(encode(&call.recorded_at)?)
            .bind(&call.call_id)
            .bind(attempt.attempt_id.to_string())
            .bind(encode(&ValidationCallStatus::Running)?)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE validation_singleflight SET state = 'terminal', updated_at = ?
                 WHERE leader_call_id = ? AND state = 'running'",
            )
            .bind(encode(&now)?)
            .bind(&call.call_id)
            .execute(&mut *transaction)
            .await?;
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
            summary: "assignment abandoned after 120 seconds without meaningful progress"
                .to_string(),
            criterion_results,
            declared_changes: Vec::new(),
            validation_call_ids: Vec::new(),
            blockers: Vec::new(),
            risks: Vec::new(),
            next_action: None,
            architecture_contract: None,
            evidence_epoch: current_epoch,
            evidence_manifest_hash: String::new(),
            sealed_at: now,
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
            "UPDATE attempts SET state = ?, sealed_at = ?
             WHERE attempt_id = ? AND state = ? AND sealed_at IS NULL",
        )
        .bind(encode(&AttemptState::Abandoned)?)
        .bind(encode(&receipt.sealed_at)?)
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&AttemptState::Active)?)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(NonproductiveRecovery::NotEligible);
        }
        release_claim(&mut transaction, assignment_id, None).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::Abandoned,
            "nonproductive assignment recovered by the typed heartbeat watcher".to_string(),
            None,
        )
        .await?;
        queue_collectible_snapshots_tx(&mut transaction, assignment_id).await?;
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("nonproductive recovery")
            .await;
        Ok(NonproductiveRecovery::Recovered {
            receipt: Box::new(receipt),
            productivity: ProductivitySummary {
                active_owned_operation_count: 0,
                cancelled_expired_operation_count,
            },
        })
    }

    async fn release_stalled_nudge_impl(&self, assignment_id: AssignmentId) -> StoreResult<bool> {
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        let workspace_id = assignment_workspace_id_tx(&mut transaction, assignment_id).await?;
        let updated = sqlx::query(
            "UPDATE workspace_actors
             SET nudge_sent_at = NULL
             WHERE workspace_id = ? AND attempt_id = ? AND nudge_sent_at IS NOT NULL",
        )
        .bind(workspace_id)
        .bind(attempt.attempt_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(updated.rows_affected() == 1)
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
        let transaction_started = Instant::now();
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        require_repository_identity_tx(&mut transaction, &assignment, &repository).await?;
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
            let start_epoch =
                assignment_epoch_tx(&mut transaction, assignment.assignment_id).await?;
            let start_epoch = i64::try_from(start_epoch).map_err(|_| {
                StoreError::CorruptData("workspace epoch exceeds SQLite integer range".to_string())
            })?;
            let absolute = absolute_repo_path(&repository.canonical_root, &normalized);
            let snapshot_name = snapshot_name(
                assignment.assignment_id,
                attempt_id,
                &normalized,
                MutationSnapshotVersion::PreWrite,
                absolute.exists(),
            );
            let snapshot_name = snapshot_name.to_string_lossy().into_owned();
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            let snapshot_started = Instant::now();
            let pre_write =
                capture_snapshot_atomic(absolute, snapshot_path, normalized.clone()).await?;
            record_coordination_timing(
                "begin_mutation",
                "snapshot_capture",
                snapshot_started,
                None,
            );
            sqlx::query("INSERT INTO mutation_files (attempt_id, assignment_id, path, pre_write_hash, pre_write_existed, attribution_confidence, snapshot_name, snapshot_retained, first_observed_at, start_epoch) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?)")
                .bind(attempt_id.to_string())
                .bind(assignment.assignment_id.to_string())
                .bind(&normalized)
                .bind(pre_write.hash)
                .bind(i64::from(pre_write.existed))
                .bind(encode(&confidence)?)
                .bind(snapshot_name)
                .bind(encode(&Utc::now())?)
                .bind(start_epoch)
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
        let commit_result = transaction.commit().await;
        record_coordination_timing(
            "begin_mutation",
            "write_transaction",
            transaction_started,
            commit_result.as_ref().err(),
        );
        commit_result?;
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
        let acquire_started = Instant::now();
        let mut connection = self.pool.acquire().await.inspect_err(|error| {
            record_coordination_timing(
                "finalize_mutation",
                "connection_acquire",
                acquire_started,
                Some(error),
            );
        })?;
        record_coordination_timing(
            "finalize_mutation",
            "connection_acquire",
            acquire_started,
            None,
        );
        let begin_started = Instant::now();
        let mut transaction = connection.begin().await.inspect_err(|error| {
            record_coordination_timing(
                "finalize_mutation",
                "transaction_begin",
                begin_started,
                Some(error),
            );
        })?;
        record_coordination_timing(
            "finalize_mutation",
            "transaction_begin",
            begin_started,
            None,
        );
        let transaction_started = Instant::now();
        let writer_started = Instant::now();
        let writer_result = lock_attempt_tx(&mut transaction, attempt_id).await;
        let writer_error = writer_result.as_ref().err().and_then(|error| match error {
            StoreError::Sql(error) => Some(error),
            _ => None,
        });
        record_coordination_timing(
            "finalize_mutation",
            "writer_lock",
            writer_started,
            writer_error,
        );
        writer_result?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
        require_repository_identity_tx(&mut transaction, &assignment, &repository).await?;
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
        let final_snapshot_name = snapshot_name(
            assignment.assignment_id,
            attempt_id,
            &normalized,
            MutationSnapshotVersion::Final,
            absolute.exists(),
        );
        let final_snapshot_name = final_snapshot_name.to_string_lossy().into_owned();
        let snapshot_path = private_snapshot_path(&self.coordination_root, &final_snapshot_name)?;
        let snapshot_started = Instant::now();
        let final_write =
            capture_snapshot_atomic(absolute, snapshot_path.clone(), normalized.clone()).await?;
        record_coordination_timing(
            "finalize_mutation",
            "snapshot_capture",
            snapshot_started,
            None,
        );
        let finalized_at = Utc::now();
        let end_epoch = assignment_epoch_tx(&mut transaction, assignment.assignment_id).await?;
        let end_epoch = i64::try_from(end_epoch).map_err(|_| {
            StoreError::CorruptData("workspace epoch exceeds SQLite integer range".to_string())
        })?;
        let updated = sqlx::query("UPDATE mutation_files SET final_hash = ?, final_write_existed = ?, final_snapshot_name = ?, finalized_at = ?, end_epoch = ? WHERE attempt_id = ? AND path = ? AND finalized_at IS NULL")
            .bind(&final_write.hash)
            .bind(i64::from(final_write.existed))
            .bind(final_snapshot_name)
            .bind(encode(&finalized_at)?)
            .bind(end_epoch)
            .bind(attempt_id.to_string())
            .bind(&normalized)
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() != 1 {
            let _ = tokio::fs::remove_file(snapshot_path).await;
            return Err(StoreError::MutationAlreadyFinalized {
                attempt_id,
                path: normalized,
            });
        }
        let evidence = load_mutation_evidence_tx(&mut transaction, attempt_id, &normalized).await?;
        let commit_result = transaction.commit().await;
        record_coordination_timing(
            "finalize_mutation",
            "write_transaction",
            transaction_started,
            commit_result.as_ref().err(),
        );
        commit_result?;
        Ok(evidence)
    }

    async fn finalize_pending_mutations_impl(
        &self,
        attempt_id: AttemptId,
    ) -> StoreResult<Vec<MutationEvidence>> {
        let mut transaction = self.pool.begin().await?;
        lock_attempt_tx(&mut transaction, attempt_id).await?;
        let attempt = require_active_current_attempt_tx(&mut transaction, attempt_id).await?;
        let canonical_root = sqlx::query_scalar::<_, String>(
            "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
        )
        .bind(attempt.assignment_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(StoreError::RepositoryBindingMissing(attempt.assignment_id))?;
        let paths = sqlx::query_scalar::<_, String>(
            "SELECT path FROM mutation_files WHERE attempt_id = ? AND finalized_at IS NULL ORDER BY first_observed_at, path",
        )
        .bind(attempt_id.to_string())
        .fetch_all(&mut *transaction)
        .await?;
        transaction.commit().await?;

        let repo_root = PathBuf::from(canonical_root);
        let mut finalized = Vec::with_capacity(paths.len());
        for path in paths {
            finalized.push(
                self.finalize_mutation_impl(attempt_id, &repo_root, path)
                    .await?,
            );
        }
        Ok(finalized)
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
        let row = sqlx::query("SELECT assignment_id, pre_write_hash, pre_write_existed, final_hash, final_write_existed, snapshot_name, final_snapshot_name, snapshot_retained, finalized_at FROM mutation_files WHERE attempt_id = ? AND path = ?")
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
        let (existed, snapshot_name, expected_hash) = match version {
            MutationSnapshotVersion::PreWrite => (
                row.get::<i64, _>("pre_write_existed") != 0,
                row.get::<String, _>("snapshot_name"),
                row.get::<Option<String>, _>("pre_write_hash"),
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
                (
                    existed,
                    snapshot_name,
                    row.get::<Option<String>, _>("final_hash"),
                )
            }
        };
        transaction.commit().await?;

        let (total_bytes, bytes) = if existed {
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            let expected_hash = expected_hash.ok_or_else(|| {
                StoreError::CorruptData(format!(
                    "retained snapshot for {path} has no persisted hash"
                ))
            })?;
            read_verified_snapshot_chunk(
                snapshot_path,
                attempt_id,
                path.clone(),
                expected_hash,
                offset,
                max_bytes,
            )
            .await?
        } else {
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            verify_nonexistent_snapshot_marker(snapshot_path, attempt_id, path.clone()).await?;
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

    async fn queue_eligible_retained_snapshot_candidates(&self) -> StoreResult<()> {
        let assignment_ids = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT assignment_id FROM mutation_files
             WHERE snapshot_retained = 1 ORDER BY assignment_id",
        )
        .fetch_all(&self.pool)
        .await?;
        for assignment_id in assignment_ids {
            let assignment_id = AssignmentId::parse(&assignment_id)?;
            let mut transaction = self.pool.begin().await?;
            lock_assignment_tx(&mut transaction, assignment_id).await?;
            queue_collectible_snapshots_tx(&mut transaction, assignment_id).await?;
            transaction.commit().await?;
        }
        Ok(())
    }

    async fn drain_snapshot_gc_queue(&self) -> StoreResult<()> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT snapshot_name FROM snapshot_gc_queue ORDER BY snapshot_name",
        )
        .fetch_all(&self.pool)
        .await?;
        for snapshot_name in rows {
            let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
            match tokio::fs::remove_file(snapshot_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            sqlx::query("DELETE FROM snapshot_gc_queue WHERE snapshot_name = ?")
                .bind(snapshot_name)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    async fn drain_snapshot_gc_queue_best_effort(&self, operation: &'static str) {
        if let Err(error) = self.drain_snapshot_gc_queue().await {
            tracing::warn!(
                target: "codex_agent_task_store::snapshot_gc",
                operation,
                %error,
                "private snapshot deletion failed; queued deletion will be retried"
            );
        }
    }

    async fn reconcile_snapshot_files(&self) -> StoreResult<()> {
        let mut transaction = self.pool.begin().await?;
        let rows = sqlx::query("SELECT attempt_id, path, snapshot_name, final_snapshot_name, finalized_at FROM mutation_files WHERE snapshot_retained = 1")
            .fetch_all(&mut *transaction)
            .await?;
        let mut retained_paths = HashSet::new();
        for row in rows {
            let attempt_id = row.get::<String, _>("attempt_id");
            let path = row.get::<String, _>("path");
            let pre_write = row.get::<String, _>("snapshot_name");
            let final_write = row.get::<Option<String>, _>("final_snapshot_name");
            let finalized = row.get::<Option<String>, _>("finalized_at").is_some();
            let required_names = [
                Some(pre_write.clone()),
                finalized.then_some(final_write.clone()).flatten(),
            ];
            let missing_final_name = finalized && final_write.is_none();
            let mut required_paths = Vec::new();
            let mut missing_file = missing_final_name;
            for snapshot_name in required_names.into_iter().flatten() {
                let snapshot_path = private_snapshot_path(&self.coordination_root, &snapshot_name)?;
                match tokio::fs::metadata(&snapshot_path).await {
                    Ok(_) => required_paths.push((snapshot_name, snapshot_path)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing_file = true;
                        required_paths.push((snapshot_name, snapshot_path));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if missing_file {
                for (snapshot_name, _) in &required_paths {
                    sqlx::query("INSERT OR IGNORE INTO snapshot_gc_queue (snapshot_name, queued_at) VALUES (?, ?)")
                        .bind(snapshot_name)
                        .bind(encode(&Utc::now())?)
                        .execute(&mut *transaction)
                        .await?;
                }
                sqlx::query("UPDATE mutation_files SET snapshot_retained = 0 WHERE attempt_id = ? AND path = ?")
                    .bind(attempt_id)
                    .bind(path)
                    .execute(&mut *transaction)
                    .await?;
            } else {
                retained_paths.extend(
                    required_paths
                        .into_iter()
                        .map(|(_, snapshot_path)| snapshot_path),
                );
            }
        }
        transaction.commit().await?;
        self.drain_snapshot_gc_queue_best_effort("snapshot reconciliation")
            .await;

        let snapshot_root = self.coordination_root.join("snapshots");
        let mut pending_directories = vec![snapshot_root];
        while let Some(directory) = pending_directories.pop() {
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                let path = entry.path();
                if file_type.is_dir() && !file_type.is_symlink() {
                    pending_directories.push(path);
                } else if !retained_paths.contains(&path) {
                    tokio::fs::remove_file(path).await?;
                }
            }
        }
        Ok(())
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

    fn create_admitted_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
        isolated_integrator_available: bool,
    ) -> TaskStoreFuture<'a, AdmittedAssignment> {
        Box::pin(async move {
            self.create_assignment_with_admission_impl(
                repo_root,
                draft,
                true,
                isolated_integrator_available,
            )
            .await
        })
    }

    fn attach_task_capsule(
        &self,
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
        canonical_payload: String,
    ) -> TaskStoreFuture<'_, Assignment> {
        Box::pin(async move {
            self.attach_task_capsule_impl(assignment_id, attempt_id, canonical_payload)
                .await
        })
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

    fn remove_agent_task_binding(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.remove_agent_task_binding_impl(actor, assignment_id)
                .await
        })
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

    fn heartbeat_typed_workspace_actor(
        &self,
        binding: AgentTaskBinding,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move { self.heartbeat_typed_workspace_actor_impl(binding).await })
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

    fn get_validation_call(&self, call_id: String) -> TaskStoreFuture<'_, Option<ValidationCall>> {
        Box::pin(async move { self.get_validation_call_impl(call_id).await })
    }

    fn heartbeat_validation_call(
        &self,
        call_id: String,
        lease_expires_at: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.heartbeat_validation_call_impl(call_id, lease_expires_at)
                .await
        })
    }

    fn replay_required_evidence_missing<'a>(
        &'a self,
        attempt_id: AttemptId,
        receipt: &'a ReceiptDraft,
    ) -> TaskStoreFuture<'a, Option<Vec<MissingEvidenceObligation>>> {
        Box::pin(async move {
            self.replay_required_evidence_missing_impl(attempt_id, receipt)
                .await
        })
    }

    fn submit_agent_receipt(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            self.submit_agent_receipt_impl(attempt_id, receipt, None)
                .await
        })
    }

    fn submit_agent_receipt_with_review(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
        review_reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            self.submit_agent_receipt_impl(attempt_id, receipt, Some(review_reason))
                .await
        })
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
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        status: GateStatus,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate> {
        Box::pin(async move {
            self.set_agent_gate_impl(actor, assignment_id, kind, status, reason)
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

    fn automatic_wake_cursor(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
    ) -> TaskStoreFuture<'_, Option<WakeEventId>> {
        Box::pin(async move {
            self.automatic_wake_cursor_impl(root_session_id, consuming_agent_path)
                .await
        })
    }

    fn compare_and_swap_automatic_wake_cursor(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
        expected: Option<WakeEventId>,
        next: WakeEventId,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.compare_and_swap_automatic_wake_cursor_impl(
                root_session_id,
                consuming_agent_path,
                expected,
                next,
            )
            .await
        })
    }

    fn reserve_stalled_nudge(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.reserve_stalled_nudge_impl(assignment_id, no_progress_before)
                .await
        })
    }

    fn recover_nonproductive_assignment(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, NonproductiveRecovery> {
        Box::pin(async move {
            self.recover_nonproductive_assignment_impl(assignment_id, no_progress_before)
                .await
        })
    }

    fn release_stalled_nudge(&self, assignment_id: AssignmentId) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move { self.release_stalled_nudge_impl(assignment_id).await })
    }

    fn capture_workspace_revision<'a>(
        &'a self,
        repo_root: &'a Path,
        paths: Vec<String>,
    ) -> TaskStoreFuture<'a, WorkspaceRevision> {
        Box::pin(
            async move { crate::workspace::capture_revision(&self.pool, repo_root, paths).await },
        )
    }

    fn read_workspace_events<'a>(
        &'a self,
        repo_root: &'a Path,
        after_epoch: u64,
    ) -> TaskStoreFuture<'a, Vec<crate::WorkspaceEvent>> {
        Box::pin(
            async move { crate::workspace::read_events(&self.pool, repo_root, after_epoch).await },
        )
    }

    fn register_workspace_actor<'a>(
        &'a self,
        repo_root: &'a Path,
        registration: WorkspaceActorRegistration,
    ) -> TaskStoreFuture<'a, ()> {
        Box::pin(async move {
            crate::workspace::register_actor(&self.pool, repo_root, registration).await
        })
    }

    fn check_quiescence(
        &self,
        root_session_id: String,
    ) -> TaskStoreFuture<'_, crate::QuiescenceStatus> {
        Box::pin(async move {
            self.require_root_receipt_evidence_current_impl(&root_session_id)
                .await?;
            crate::workspace::quiescence(&self.pool, &root_session_id).await
        })
    }

    fn inspect_quiescence(
        &self,
        root_session_id: String,
    ) -> TaskStoreFuture<'_, crate::QuiescenceStatus> {
        Box::pin(
            async move { crate::workspace::inspect_quiescence(&self.pool, &root_session_id).await },
        )
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

    fn finalize_pending_mutations(
        &self,
        attempt_id: AttemptId,
    ) -> TaskStoreFuture<'_, Vec<MutationEvidence>> {
        Box::pin(async move { self.finalize_pending_mutations_impl(attempt_id).await })
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

async fn queue_collectible_snapshots_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<usize> {
    // This is the sole snapshot-retention eligibility gate. Startup discovers
    // candidates only; it uses this same helper instead of inferring eligibility
    // from the assignment or current-attempt status.
    let assignment_key = assignment_id.to_string();
    let attempt_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM attempts WHERE assignment_id = ?")
            .bind(&assignment_key)
            .fetch_one(&mut **transaction)
            .await?;
    if attempt_count == 0 {
        return Ok(0);
    }
    let ineligible_attempt_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM attempts
         WHERE assignment_id = ?
           AND (
               sealed_at IS NULL
               OR (SELECT COUNT(*) FROM receipts
                   WHERE receipts.attempt_id = attempts.attempt_id) <> 1
           )",
    )
    .bind(&assignment_key)
    .fetch_one(&mut **transaction)
    .await?;
    if ineligible_attempt_count != 0 {
        return Ok(0);
    }

    let current_attempt = load_current_attempt_tx(transaction, assignment_id).await?;
    if !current_attempt.state.is_terminal() || current_attempt.sealed_at.is_none() {
        return Ok(0);
    }

    let unsealed_gate_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM gates
         WHERE assignment_id = ? AND (sealed_at IS NULL OR status = ?)",
    )
    .bind(&assignment_key)
    .bind(encode(&GateStatus::Pending)?)
    .fetch_one(&mut **transaction)
    .await?;
    if unsealed_gate_count != 0 {
        return Ok(0);
    }

    let assignment = load_assignment_tx(transaction, assignment_id).await?;
    let correction_can_reopen = assignment.role == AgentRole::Worker
        && current_attempt.ordinal == 0
        && sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM gates
             WHERE assignment_id = ? AND kind = ? AND status = ?",
        )
        .bind(&assignment_key)
        .bind(encode(&GateKind::Review)?)
        .bind(encode(&GateStatus::ChangesRequested)?)
        .fetch_one(&mut **transaction)
        .await?
            != 0;
    if correction_can_reopen {
        return Ok(0);
    }

    let rows = sqlx::query(
        "SELECT snapshot_name, final_snapshot_name FROM mutation_files
         WHERE assignment_id = ? AND snapshot_retained = 1",
    )
    .bind(&assignment_key)
    .fetch_all(&mut **transaction)
    .await?;
    let queued_at = encode(&Utc::now())?;
    for row in &rows {
        let snapshot_names = [
            Some(row.get::<String, _>("snapshot_name")),
            row.get::<Option<String>, _>("final_snapshot_name"),
        ];
        for snapshot_name in snapshot_names.into_iter().flatten() {
            sqlx::query(
                "INSERT OR IGNORE INTO snapshot_gc_queue (snapshot_name, queued_at) VALUES (?, ?)",
            )
            .bind(snapshot_name)
            .bind(&queued_at)
            .execute(&mut **transaction)
            .await?;
        }
    }
    sqlx::query(
        "UPDATE mutation_files SET snapshot_retained = 0
         WHERE assignment_id = ? AND snapshot_retained = 1",
    )
    .bind(&assignment_key)
    .execute(&mut **transaction)
    .await?;
    Ok(rows.len())
}

async fn prepare_validation_call(
    pool: &SqlitePool,
    mut call: ValidationCall,
) -> StoreResult<ValidationCall> {
    let existing =
        sqlx::query_scalar::<_, String>("SELECT body_json FROM validation_calls WHERE call_id = ?")
            .bind(&call.call_id)
            .fetch_optional(pool)
            .await?
            .map(|value| decode::<ValidationCall>(&value))
            .transpose()?;
    if let Some(existing) = existing {
        if existing.attempt_id != call.attempt_id {
            return Err(StoreError::ValidationCallOwnership {
                call_ids: vec![call.call_id],
            });
        }
        let retained_output_ref = call.evidence.retained_output_ref.clone();
        let output_summary = call.evidence.output_summary.clone();
        call.evidence = existing.evidence.clone();
        if call.status.is_terminal() {
            let context = validation_context(pool, call.attempt_id).await?;
            let paths = if existing.evidence.covered_scopes.is_empty() {
                existing
                    .evidence
                    .covered_manifest
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect::<Vec<_>>()
            } else {
                existing
                    .evidence
                    .covered_scopes
                    .iter()
                    .map(|scope| scope.path.clone())
                    .collect::<Vec<_>>()
            };
            let current =
                crate::workspace::capture_revision(pool, &context.repo_root, paths).await?;
            let execution_current = if existing.status == crate::ValidationCallStatus::Running {
                Some(
                    crate::workspace::capture_revision(
                        pool,
                        &context.repo_root,
                        vec![crate::workspace::REPOSITORY_WIDE_PATH.to_string()],
                    )
                    .await?,
                )
            } else {
                None
            };
            let execution_discovered_new_epoch = execution_current
                .as_ref()
                .is_some_and(|revision| revision.epoch > current.epoch);
            let execution_changes = execution_current
                .as_ref()
                .zip(existing.evidence.execution_snapshot.as_deref())
                .filter(|(_, snapshot)| !snapshot.manifest_hash.is_empty())
                .map(|(execution_current, snapshot)| {
                    crate::workspace::changed_manifest_paths(
                        &snapshot.manifest,
                        &execution_current.files,
                    )
                })
                .unwrap_or_default();
            let stale_reason = if execution_changes.is_empty() || !execution_discovered_new_epoch {
                validation_stale_reason(
                    pool,
                    &context.repo_root,
                    &context.assignment,
                    &existing,
                    &current,
                )
                .await?
            } else {
                Some(format!(
                    "repository contents changed while validation was running: {}",
                    execution_changes.join(", ")
                ))
            };
            call.evidence.end_epoch = Some(
                execution_current
                    .as_ref()
                    .map_or(current.epoch, |revision| revision.epoch),
            );
            call.evidence.retained_output_ref =
                retained_output_ref.or(existing.evidence.retained_output_ref);
            call.evidence.output_summary = output_summary.or(existing.evidence.output_summary);
            if let Some(reason) = stale_reason {
                call.status = crate::ValidationCallStatus::Superseded;
                call.evidence.stale_reason = Some(reason);
            }
            call.evidence.successful_result = Some(
                call.status == crate::ValidationCallStatus::Succeeded
                    && call.evidence.stale_reason.is_none(),
            );
            call.evidence.retained_output_digest = validation_output_digest(&call)?;
        }
        return Ok(call);
    }

    if call.proof_kind != crate::ValidationProofKind::Focused {
        return Ok(call);
    }
    let context = validation_context(pool, call.attempt_id).await?;
    let mut covered_scopes = context.assignment.write_scope.clone();
    let mut covered_contracts = context.assignment.contract_claims.clone();
    if covered_scopes.is_empty()
        && let Some(relation) = &context.assignment.relation
    {
        for target in &relation.target_assignment_ids {
            if let Some(body) = sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM assignments WHERE assignment_id = ?",
            )
            .bind(target.to_string())
            .fetch_optional(pool)
            .await?
            {
                let target: Assignment = decode(&body)?;
                covered_scopes.extend(target.write_scope);
                covered_contracts.extend(target.contract_claims);
            }
        }
    }
    let repository_wide = covered_scopes.is_empty();
    covered_scopes.extend(repository_global_validation_scopes(&context.repo_root)?);
    covered_scopes = merge_validation_scopes(covered_scopes);
    let paths = if repository_wide {
        vec![crate::workspace::REPOSITORY_WIDE_PATH.to_string()]
    } else {
        covered_scopes
            .iter()
            .map(|scope| scope.path.clone())
            .collect::<Vec<_>>()
    };
    covered_contracts.sort();
    covered_contracts.dedup();
    let revision = crate::workspace::capture_revision(pool, &context.repo_root, paths).await?;
    let execution_revision = crate::workspace::capture_revision(
        pool,
        &context.repo_root,
        vec![crate::workspace::REPOSITORY_WIDE_PATH.to_string()],
    )
    .await?;
    let covered_input_manifest_hash = revision.manifest_hash.clone();
    let dependency_manifest_hash = revision.manifest_hash.clone();
    let normalized_invocation = validation_normalized_invocation(&call)?;
    let coverage_identity =
        validation_coverage_identity(&covered_scopes, &covered_contracts, repository_wide)?;
    let features_configuration_identity = validation_features_configuration_identity(
        call.evidence.environment_hash.as_deref(),
        call.evidence.toolchain.as_deref(),
    )?;
    let implementation_identity =
        validation_implementation_identity(&context.assignment, &revision.manifest_hash)?;
    let candidate_id = validation_candidate_identity(
        &implementation_identity,
        &normalized_invocation,
        &coverage_identity,
        &features_configuration_identity,
        &revision.manifest_hash,
        &revision.manifest_hash,
    )?;
    call.evidence = ValidationEvidence {
        candidate_id,
        implementation_identity,
        source_evidence_epoch: Some(execution_revision.epoch),
        normalized_invocation,
        coverage_identity,
        start_epoch: execution_revision.epoch,
        end_epoch: None,
        covered_scopes,
        covered_manifest: revision.files,
        execution_snapshot: Some(Box::new(crate::ValidationExecutionSnapshot {
            manifest: execution_revision.files,
            manifest_hash: execution_revision.manifest_hash,
        })),
        covered_contracts,
        manifest_hash: revision.manifest_hash,
        repository_wide,
        cwd: call
            .evidence
            .cwd
            .or_else(|| Some(context.repo_root.to_string_lossy().into_owned())),
        environment_hash: call.evidence.environment_hash,
        toolchain: call.evidence.toolchain,
        features_configuration_identity,
        covered_input_manifest_hash,
        dependency_manifest_hash,
        successful_result: None,
        retained_output_digest: String::new(),
        retained_output_ref: call.evidence.retained_output_ref,
        output_summary: call.evidence.output_summary,
        validation_result: call.evidence.validation_result,
        lease_expires_at: call
            .evidence
            .lease_expires_at
            .or_else(|| Some(Utc::now() + chrono::Duration::minutes(15))),
        shared_from_call_id: None,
        stale_reason: None,
    };
    Ok(call)
}

async fn claim_isolated_handoffs_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
) -> StoreResult<()> {
    let Some(relation) = assignment.relation.as_ref() else {
        return Ok(());
    };
    if assignment.role != AgentRole::Integrator || relation.kind != RelationKind::Integration {
        return Ok(());
    }
    for target in &relation.target_assignment_ids {
        let target_body = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM assignments WHERE assignment_id = ?",
        )
        .bind(target.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or(StoreError::AssignmentNotFound(*target))?;
        let target_assignment: Assignment = decode(&target_body)?;
        if target_assignment.workspace_strategy != WorkspaceStrategy::Isolated {
            continue;
        }
        let handoff = sqlx::query(
            "SELECT state, integrator_assignment_id
             FROM isolated_handoffs WHERE assignment_id = ?",
        )
        .bind(target.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            StoreError::InvalidAssignment(format!(
                "isolated dependency {target} has no versioned handoff"
            ))
        })?;
        let state: IsolationHandoffState = decode(handoff.get::<String, _>("state").as_str())?;
        let claimed_by = handoff.get::<Option<String>, _>("integrator_assignment_id");
        if state != IsolationHandoffState::Ready || claimed_by.is_some() {
            return Err(StoreError::InvalidAssignment(format!(
                "isolated handoff {target} is already claimed or integrated"
            )));
        }
        let updated = sqlx::query(
            "UPDATE isolated_handoffs
             SET state = ?, integrator_assignment_id = ?
             WHERE assignment_id = ? AND state = ? AND integrator_assignment_id IS NULL",
        )
        .bind(encode(&IsolationHandoffState::Claimed)?)
        .bind(assignment.assignment_id.to_string())
        .bind(target.to_string())
        .bind(encode(&IsolationHandoffState::Ready)?)
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::InvalidAssignment(format!(
                "isolated handoff {target} changed while the integrator was being created"
            )));
        }
    }
    Ok(())
}

async fn persist_receipt_handoff_action_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    action: ReceiptHandoffAction,
) -> StoreResult<()> {
    match action {
        ReceiptHandoffAction::Publish(handoff) => {
            sqlx::query(
                "INSERT INTO isolated_handoffs (
                    assignment_id, source_workspace_id, source_epoch,
                    source_manifest_hash, covered_manifest_json, state,
                    integrator_assignment_id, created_at, integrated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, NULL)
                 ON CONFLICT(assignment_id) DO UPDATE SET
                    source_workspace_id = excluded.source_workspace_id,
                    source_epoch = excluded.source_epoch,
                    source_manifest_hash = excluded.source_manifest_hash,
                    covered_manifest_json = excluded.covered_manifest_json,
                    state = excluded.state,
                    integrator_assignment_id = NULL,
                    created_at = excluded.created_at,
                    integrated_at = NULL",
            )
            .bind(handoff.assignment_id.to_string())
            .bind(&handoff.source_workspace_id)
            .bind(handoff.source_epoch as i64)
            .bind(&handoff.source_manifest_hash)
            .bind(encode(&handoff.covered_manifest)?)
            .bind(encode(&IsolationHandoffState::Ready)?)
            .bind(encode(&handoff.created_at)?)
            .execute(&mut **transaction)
            .await?;
        }
        ReceiptHandoffAction::Integrate(targets) => {
            let integrated_at = Utc::now();
            for target in targets {
                let updated = sqlx::query(
                    "UPDATE isolated_handoffs
                     SET state = ?, integrated_at = ?
                     WHERE assignment_id = ? AND state = ?",
                )
                .bind(encode(&IsolationHandoffState::Integrated)?)
                .bind(encode(&integrated_at)?)
                .bind(target.to_string())
                .bind(encode(&IsolationHandoffState::Claimed)?)
                .execute(&mut **transaction)
                .await?;
                if updated.rows_affected() != 1 {
                    return Err(StoreError::InvalidAssignment(format!(
                        "isolated handoff {target} changed before integration sealed"
                    )));
                }
            }
        }
    }
    Ok(())
}

struct ValidationContext {
    assignment: Assignment,
    repo_root: PathBuf,
}

async fn assignment_receipt_validation_ids(
    pool: &SqlitePool,
    assignment_id: AssignmentId,
) -> StoreResult<Option<(AttemptId, Vec<String>)>> {
    let row = sqlx::query(
        "SELECT receipts.attempt_id, receipts.body_json
         FROM receipts
         JOIN attempts USING (attempt_id)
         WHERE receipts.assignment_id = ?
         ORDER BY attempts.ordinal DESC
         LIMIT 1",
    )
    .bind(assignment_id.to_string())
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let receipt: AgentReceipt = decode(&row.get::<String, _>("body_json"))?;
    Ok(Some((
        AttemptId::parse(&row.get::<String, _>("attempt_id"))?,
        receipt.validation_call_ids,
    )))
}

async fn validation_context(
    pool: &SqlitePool,
    attempt_id: AttemptId,
) -> StoreResult<ValidationContext> {
    let row = sqlx::query(
        "SELECT assignments.body_json, assignment_repositories.canonical_root,
                assignment_repositories.repository_id, assignment_repositories.workspace_id
         FROM attempts
         JOIN assignments USING (assignment_id)
         JOIN assignment_repositories USING (assignment_id)
         WHERE attempts.attempt_id = ?",
    )
    .bind(attempt_id.to_string())
    .fetch_optional(pool)
    .await?
    .ok_or(StoreError::AttemptNotFound(attempt_id))?;
    let mut assignment: Assignment = decode(&row.get::<String, _>("body_json"))?;
    assignment.repository_id = row.get("repository_id");
    assignment.workspace_id = row.get("workspace_id");
    Ok(ValidationContext {
        assignment,
        repo_root: PathBuf::from(row.get::<String, _>("canonical_root")),
    })
}

async fn supersede_stale_receipt_validations_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt: &Attempt,
) -> StoreResult<Vec<String>> {
    let receipt = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM receipts WHERE assignment_id = ? AND attempt_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .bind(attempt.attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .map(|body| decode::<AgentReceipt>(&body))
    .transpose()?;
    let Some(receipt) = receipt.filter(|receipt| receipt.status.is_success()) else {
        return Ok(Vec::new());
    };

    let mut stale_call_ids = Vec::new();
    for call_id in &receipt.validation_call_ids {
        let row = sqlx::query(
            "SELECT body_json, status FROM validation_calls
             WHERE call_id = ? AND attempt_id = ?",
        )
        .bind(call_id)
        .bind(attempt.attempt_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| {
            StoreError::CorruptData(format!(
                "sealed receipt references missing validation call {call_id}"
            ))
        })?;
        let call: ValidationCall = decode(&row.get::<String, _>("body_json"))?;
        let stored_status: crate::ValidationCallStatus = decode(&row.get::<String, _>("status"))?;
        if call.status != stored_status || call.attempt_id != attempt.attempt_id {
            return Err(StoreError::CorruptData(format!(
                "validation call {call_id} has inconsistent persisted identity or status"
            )));
        }
        if call.status == crate::ValidationCallStatus::Superseded {
            stale_call_ids.push(call_id.clone());
            continue;
        }
        if call.status != crate::ValidationCallStatus::Succeeded {
            return Err(StoreError::ValidationCallStatusInvalid {
                call_ids: vec![call_id.clone()],
            });
        }
        let Some((reason, current_epoch)) =
            validation_stale_event_reason_tx(transaction, assignment, &call).await?
        else {
            continue;
        };
        let mut superseded = call;
        superseded.status = crate::ValidationCallStatus::Superseded;
        superseded.evidence.end_epoch = Some(current_epoch);
        superseded.evidence.stale_reason = Some(reason.clone());
        superseded.recorded_at = Utc::now();
        let updated = sqlx::query(
            "UPDATE validation_calls
             SET body_json = ?, status = ?, recorded_at = ?
             WHERE call_id = ? AND attempt_id = ? AND status = ?",
        )
        .bind(encode(&superseded)?)
        .bind(encode(&superseded.status)?)
        .bind(encode(&superseded.recorded_at)?)
        .bind(call_id)
        .bind(attempt.attempt_id.to_string())
        .bind(encode(&crate::ValidationCallStatus::Succeeded)?)
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::ValidationCallImmutable(call_id.clone()));
        }
        record_stale_event_tx(transaction, attempt, current_epoch, &reason).await?;
        stale_call_ids.push(call_id.clone());
    }
    Ok(stale_call_ids)
}

async fn validation_stale_reason(
    pool: &SqlitePool,
    repo_root: &Path,
    assignment: &Assignment,
    call: &ValidationCall,
    current: &WorkspaceRevision,
) -> StoreResult<Option<String>> {
    if call.evidence.repository_wide && current.epoch != call.evidence.start_epoch {
        return Ok(Some(format!(
            "repository-wide validation started at epoch {} but workspace is at epoch {}",
            call.evidence.start_epoch, current.epoch
        )));
    }
    if !call.evidence.covered_scopes.is_empty() {
        let newly_global = repository_global_validation_scopes(repo_root)?
            .into_iter()
            .filter(|current_scope| {
                !call
                    .evidence
                    .covered_scopes
                    .iter()
                    .any(|covered| covered.covers_scope(current_scope))
            })
            .map(|scope| scope.path)
            .collect::<Vec<_>>();
        if !newly_global.is_empty() {
            return Ok(Some(format!(
                "repository-wide schema, lockfile, or build configuration appeared: {}",
                newly_global.join(", ")
            )));
        }
    }
    let changed =
        crate::workspace::changed_manifest_paths(&call.evidence.covered_manifest, &current.files);
    if !changed.is_empty() {
        return Ok(Some(format!(
            "covered validation inputs changed: {}",
            changed.join(", ")
        )));
    }
    if current.epoch > call.evidence.start_epoch {
        let rows = sqlx::query(
            "SELECT epoch, paths_json, contracts_json FROM workspace_events
             WHERE workspace_id = ? AND epoch > ? AND epoch <= ? ORDER BY epoch",
        )
        .bind(&assignment.workspace_id)
        .bind(call.evidence.start_epoch as i64)
        .bind(current.epoch as i64)
        .fetch_all(pool)
        .await?;
        let mut expected_epoch = call.evidence.start_epoch + 1;
        for row in rows {
            let event_epoch = u64::try_from(row.get::<i64, _>("epoch")).map_err(|_| {
                StoreError::CorruptData("workspace event epoch is negative".to_string())
            })?;
            if event_epoch != expected_epoch {
                return Ok(Some(format!(
                    "workspace event coverage is incomplete after validation epoch {}",
                    call.evidence.start_epoch
                )));
            }
            expected_epoch += 1;
            let paths: Vec<String> = decode(&row.get::<String, _>("paths_json"))?;
            let path_overlap = paths
                .into_iter()
                .filter(|path| validation_event_affects_path(&call.evidence, path))
                .collect::<Vec<_>>();
            let contracts: Vec<String> = decode(&row.get::<String, _>("contracts_json"))?;
            let contract_overlap = contracts
                .into_iter()
                .filter(|contract| call.evidence.covered_contracts.contains(contract))
                .collect::<Vec<_>>();
            if !path_overlap.is_empty() || !contract_overlap.is_empty() {
                let mut changes = Vec::new();
                if !path_overlap.is_empty() {
                    changes.push(format!("paths {}", path_overlap.join(", ")));
                }
                if !contract_overlap.is_empty() {
                    changes.push(format!("contracts {}", contract_overlap.join(", ")));
                }
                return Ok(Some(format!(
                    "workspace event at epoch {event_epoch} changed covered {}",
                    changes.join(" and ")
                )));
            }
        }
        if expected_epoch <= current.epoch {
            return Ok(Some(format!(
                "workspace event coverage ends at epoch {} but workspace is at epoch {}",
                expected_epoch - 1,
                current.epoch
            )));
        }
    }
    Ok(None)
}

async fn validation_stale_event_reason_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    call: &ValidationCall,
) -> StoreResult<Option<(String, u64)>> {
    let current_epoch = assignment_epoch_tx(transaction, assignment.assignment_id).await?;
    let evidence_epoch = call.evidence.end_epoch.unwrap_or(call.evidence.start_epoch);
    if current_epoch < evidence_epoch {
        return Ok(Some((
            format!(
                "workspace epoch regressed from evidence epoch {evidence_epoch} to {current_epoch}"
            ),
            current_epoch,
        )));
    }
    if current_epoch == evidence_epoch {
        return Ok(None);
    }
    if call.evidence.repository_wide {
        return Ok(Some((
            format!(
                "repository-wide validation ended at epoch {evidence_epoch} but workspace is at epoch {current_epoch}"
            ),
            current_epoch,
        )));
    }

    let rows = sqlx::query(
        "SELECT epoch, paths_json, contracts_json FROM workspace_events
         WHERE workspace_id = ? AND epoch > ? ORDER BY epoch",
    )
    .bind(&assignment.workspace_id)
    .bind(evidence_epoch as i64)
    .fetch_all(&mut **transaction)
    .await?;
    let mut expected_epoch = evidence_epoch + 1;
    for row in rows {
        let event_epoch = u64::try_from(row.get::<i64, _>("epoch")).map_err(|_| {
            StoreError::CorruptData("workspace event epoch is negative".to_string())
        })?;
        if event_epoch != expected_epoch {
            return Ok(Some((
                format!(
                    "workspace event coverage is incomplete after evidence epoch {evidence_epoch}"
                ),
                current_epoch,
            )));
        }
        expected_epoch += 1;

        let event_paths: Vec<String> = decode(&row.get::<String, _>("paths_json"))?;
        let changed_paths = event_paths
            .into_iter()
            .filter(|path| validation_event_affects_path(&call.evidence, path))
            .collect::<Vec<_>>();
        let event_contracts: Vec<String> = decode(&row.get::<String, _>("contracts_json"))?;
        let changed_contracts = event_contracts
            .into_iter()
            .filter(|contract| call.evidence.covered_contracts.contains(contract))
            .collect::<Vec<_>>();
        if !changed_paths.is_empty() || !changed_contracts.is_empty() {
            let mut changes = Vec::new();
            if !changed_paths.is_empty() {
                changes.push(format!("paths {}", changed_paths.join(", ")));
            }
            if !changed_contracts.is_empty() {
                changes.push(format!("contracts {}", changed_contracts.join(", ")));
            }
            return Ok(Some((
                format!(
                    "workspace event at epoch {event_epoch} changed covered {}",
                    changes.join(" and ")
                ),
                current_epoch,
            )));
        }
    }
    if expected_epoch <= current_epoch {
        return Ok(Some((
            format!(
                "workspace event coverage ends at epoch {} but workspace is at epoch {current_epoch}",
                expected_epoch - 1
            ),
            current_epoch,
        )));
    }
    Ok(None)
}

fn validation_event_affects_path(evidence: &ValidationEvidence, path: &str) -> bool {
    if path == crate::REPOSITORY_WIDE_PATH || validation_event_is_repository_global(path) {
        return true;
    }
    let event_scope = RepoScope {
        path: path.to_string(),
        recursive: true,
    };
    if !evidence.covered_scopes.is_empty() {
        return evidence
            .covered_scopes
            .iter()
            .any(|scope| event_scope.overlaps(scope));
    }
    evidence.covered_manifest.iter().any(|entry| {
        event_scope.overlaps(&RepoScope {
            path: entry.path.clone(),
            recursive: false,
        })
    })
}

fn validation_event_is_repository_global(path: &str) -> bool {
    if is_repository_global_validation_file(Path::new(path)) {
        return true;
    }
    let path = if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_string()
    };
    [
        "codex-rs/app-server-protocol/schema",
        "codex-rs/hooks/schema/generated",
    ]
    .iter()
    .any(|directory| path == *directory || path.starts_with(&format!("{directory}/")))
}

fn merge_validation_scopes(scopes: Vec<RepoScope>) -> Vec<RepoScope> {
    let mut merged = HashMap::<String, RepoScope>::new();
    for scope in scopes {
        let key = if cfg!(windows) {
            scope.path.to_lowercase()
        } else {
            scope.path.clone()
        };
        merged
            .entry(key)
            .and_modify(|existing| existing.recursive |= scope.recursive)
            .or_insert(scope);
    }
    let mut scopes = merged.into_values().collect::<Vec<_>>();
    scopes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.recursive.cmp(&right.recursive))
    });
    scopes
}

fn repository_global_validation_scopes(repo_root: &Path) -> StoreResult<Vec<RepoScope>> {
    let mut scopes = Vec::new();
    collect_repository_global_validation_files(repo_root, repo_root, &mut scopes)?;
    for schema_dir in [
        "codex-rs/app-server-protocol/schema",
        "codex-rs/hooks/schema/generated",
    ] {
        if repo_root.join(schema_dir).is_dir() {
            scopes.push(RepoScope {
                path: schema_dir.to_string(),
                recursive: true,
            });
        }
    }
    Ok(merge_validation_scopes(scopes))
}

fn collect_repository_global_validation_files(
    repo_root: &Path,
    directory: &Path,
    scopes: &mut Vec<RepoScope>,
) -> StoreResult<()> {
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name == ".git"
                    || name == "node_modules"
                    || name == ".venv"
                    || name == "venv"
                    || name == "dist"
                    || name == "build"
                    || name.starts_with("target")
                {
                    continue;
                }
                directories.push(path);
            } else if file_type.is_file() && is_repository_global_validation_file(&path) {
                let relative = path.strip_prefix(repo_root).map_err(|_| {
                    StoreError::InvalidScope(format!(
                        "global validation input escaped repository root: {}",
                        path.display()
                    ))
                })?;
                scopes.push(RepoScope {
                    path: relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                    recursive: false,
                });
            }
        }
    }
    Ok(())
}

fn is_repository_global_validation_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "pnpm-workspace.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "uv.lock"
            | "rust-toolchain"
            | "rust-toolchain.toml"
            | "justfile"
            | "makefile"
            | "build.rs"
            | "build.bazel"
            | "module.bazel"
            | ".bazelrc"
            | "deno.json"
            | "tsconfig.json"
    ) || name.ends_with(".schema.json")
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
        || name.ends_with("-lock.yml")
        || name == "config.toml"
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|parent| parent.eq_ignore_ascii_case(".cargo"))
}

fn validation_normalized_invocation(call: &ValidationCall) -> StoreResult<String> {
    #[derive(Serialize)]
    struct Invocation<'a> {
        command: &'a str,
        executable: &'a Option<String>,
        cwd: &'a Option<String>,
    }

    Ok(hash_bytes(
        encode(&Invocation {
            command: call.command_summary.trim(),
            executable: &call.resolved_executable,
            cwd: &call.evidence.cwd,
        })?
        .as_bytes(),
    ))
}

fn validation_coverage_identity(
    covered_scopes: &[RepoScope],
    covered_contracts: &[String],
    repository_wide: bool,
) -> StoreResult<String> {
    #[derive(Serialize)]
    struct Coverage<'a> {
        scopes: &'a [RepoScope],
        contracts: &'a [String],
        repository_wide: bool,
    }

    Ok(hash_bytes(
        encode(&Coverage {
            scopes: covered_scopes,
            contracts: covered_contracts,
            repository_wide,
        })?
        .as_bytes(),
    ))
}

fn validation_features_configuration_identity(
    environment_hash: Option<&str>,
    toolchain: Option<&str>,
) -> StoreResult<String> {
    Ok(hash_bytes(
        encode(&(environment_hash, toolchain))?.as_bytes(),
    ))
}

fn validation_implementation_identity(
    assignment: &Assignment,
    covered_manifest_hash: &str,
) -> StoreResult<String> {
    #[derive(Serialize)]
    struct Implementation<'a> {
        repository_id: &'a str,
        workspace_id: &'a str,
        covered_manifest_hash: &'a str,
    }

    Ok(hash_bytes(
        encode(&Implementation {
            repository_id: &assignment.repository_id,
            workspace_id: &assignment.workspace_id,
            covered_manifest_hash,
        })?
        .as_bytes(),
    ))
}

fn validation_candidate_identity(
    implementation_identity: &str,
    normalized_invocation: &str,
    coverage_identity: &str,
    features_configuration_identity: &str,
    covered_input_manifest_hash: &str,
    dependency_manifest_hash: &str,
) -> StoreResult<String> {
    Ok(hash_bytes(
        encode(&(
            implementation_identity,
            normalized_invocation,
            coverage_identity,
            features_configuration_identity,
            covered_input_manifest_hash,
            dependency_manifest_hash,
        ))?
        .as_bytes(),
    ))
}

fn validation_output_digest(call: &ValidationCall) -> StoreResult<String> {
    Ok(hash_bytes(
        encode(&(
            call.status,
            call.evidence.retained_output_ref.as_deref(),
            call.evidence.output_summary.as_deref(),
            call.evidence.validation_result.as_ref(),
        ))?
        .as_bytes(),
    ))
}

fn validation_inflight_fingerprint(call: &ValidationCall) -> StoreResult<String> {
    #[derive(Serialize)]
    struct Fingerprint<'a> {
        command: &'a str,
        executable: &'a Option<String>,
        cwd: &'a Option<String>,
        environment_hash: &'a Option<String>,
        toolchain: &'a Option<String>,
        covered_scopes: &'a [RepoScope],
        covered_contracts: &'a [String],
        repository_wide: bool,
        candidate_id: &'a str,
        implementation_identity: &'a str,
        source_evidence_epoch: Option<u64>,
        normalized_invocation: &'a str,
        coverage_identity: &'a str,
        features_configuration_identity: &'a str,
    }
    let payload = encode(&Fingerprint {
        command: &call.command_summary,
        executable: &call.resolved_executable,
        cwd: &call.evidence.cwd,
        environment_hash: &call.evidence.environment_hash,
        toolchain: &call.evidence.toolchain,
        covered_scopes: &call.evidence.covered_scopes,
        covered_contracts: &call.evidence.covered_contracts,
        repository_wide: call.evidence.repository_wide,
        candidate_id: &call.evidence.candidate_id,
        implementation_identity: &call.evidence.implementation_identity,
        source_evidence_epoch: call.evidence.source_evidence_epoch,
        normalized_invocation: &call.evidence.normalized_invocation,
        coverage_identity: &call.evidence.coverage_identity,
        features_configuration_identity: &call.evidence.features_configuration_identity,
    })?;
    Ok(format!("{:x}", Sha256::digest(payload.as_bytes())))
}

fn validation_evidence_key(call: &ValidationCall) -> StoreResult<Option<String>> {
    if !call.evidence.has_complete_request_identity() || call.evidence.covered_manifest.is_empty() {
        return Ok(None);
    }
    #[derive(Serialize)]
    struct EvidenceKey<'a> {
        command: &'a str,
        executable: &'a Option<String>,
        cwd: &'a Option<String>,
        environment_hash: &'a Option<String>,
        toolchain: &'a Option<String>,
        covered_scopes: &'a [RepoScope],
        covered_contracts: &'a [String],
        repository_wide: bool,
        manifest_hash: &'a str,
        candidate_id: &'a str,
        implementation_identity: &'a str,
        normalized_invocation: &'a str,
        coverage_identity: &'a str,
        features_configuration_identity: &'a str,
        covered_input_manifest_hash: &'a str,
        dependency_manifest_hash: &'a str,
    }
    let payload = encode(&EvidenceKey {
        command: &call.command_summary,
        executable: &call.resolved_executable,
        cwd: &call.evidence.cwd,
        environment_hash: &call.evidence.environment_hash,
        toolchain: &call.evidence.toolchain,
        covered_scopes: &call.evidence.covered_scopes,
        covered_contracts: &call.evidence.covered_contracts,
        repository_wide: call.evidence.repository_wide,
        manifest_hash: &call.evidence.manifest_hash,
        candidate_id: &call.evidence.candidate_id,
        implementation_identity: &call.evidence.implementation_identity,
        normalized_invocation: &call.evidence.normalized_invocation,
        coverage_identity: &call.evidence.coverage_identity,
        features_configuration_identity: &call.evidence.features_configuration_identity,
        covered_input_manifest_hash: &call.evidence.covered_input_manifest_hash,
        dependency_manifest_hash: &call.evidence.dependency_manifest_hash,
    })?;
    Ok(Some(format!("{:x}", Sha256::digest(payload.as_bytes()))))
}

async fn assignment_workspace_id_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT workspace_id FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(assignment_id))
}

async fn reserve_reconciliation_validation_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
    call_id: &str,
) -> StoreResult<bool> {
    let row = sqlx::query(
        "SELECT stale_events, reconciliation_call_id FROM stale_recovery WHERE attempt_id = ?",
    )
    .bind(attempt.attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    if row.get::<i64, _>("stale_events") == 0 {
        return Ok(false);
    }
    if let Some(existing) = row.get::<Option<String>, _>("reconciliation_call_id") {
        if existing != call_id {
            pause_active_attempt_for_stale_recovery_tx(transaction, attempt).await?;
            release_claim(transaction, attempt.assignment_id, None).await?;
            return Ok(true);
        }
        return Ok(false);
    }
    sqlx::query(
        "UPDATE stale_recovery SET reconciliation_call_id = ?, updated_at = ?
         WHERE attempt_id = ? AND reconciliation_call_id IS NULL",
    )
    .bind(call_id)
    .bind(encode(&Utc::now())?)
    .bind(attempt.attempt_id.to_string())
    .execute(&mut **transaction)
    .await?;
    Ok(false)
}

async fn record_stale_event_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
    stale_epoch: u64,
    reason: &str,
) -> StoreResult<()> {
    let existing = sqlx::query(
        "SELECT stale_events, last_stale_epoch FROM stale_recovery WHERE attempt_id = ?",
    )
    .bind(attempt.attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?;
    if existing.as_ref().and_then(|row| {
        row.get::<Option<i64>, _>("last_stale_epoch")
            .and_then(|value| u64::try_from(value).ok())
    }) == Some(stale_epoch)
    {
        sqlx::query(
            "UPDATE stale_recovery SET last_reason = ?, updated_at = ? WHERE attempt_id = ?",
        )
        .bind(reason)
        .bind(encode(&Utc::now())?)
        .bind(attempt.attempt_id.to_string())
        .execute(&mut **transaction)
        .await?;
        return Ok(());
    }
    let existing_count = existing
        .as_ref()
        .map(|row| row.get::<i64, _>("stale_events"))
        .unwrap_or(0);
    let stale_events = (existing_count + 1).min(2);
    sqlx::query(
        "INSERT INTO stale_recovery (
            attempt_id, stale_events, reconciliation_call_id, last_stale_epoch,
            last_reason, updated_at
         ) VALUES (?, ?, NULL, ?, ?, ?)
         ON CONFLICT(attempt_id) DO UPDATE SET
            stale_events = excluded.stale_events,
            reconciliation_call_id = NULL,
            last_stale_epoch = excluded.last_stale_epoch,
            last_reason = excluded.last_reason,
            updated_at = excluded.updated_at",
    )
    .bind(attempt.attempt_id.to_string())
    .bind(stale_events)
    .bind(stale_epoch as i64)
    .bind(reason)
    .bind(encode(&Utc::now())?)
    .execute(&mut **transaction)
    .await?;
    if stale_events >= 2 && attempt.state == AttemptState::Active && attempt.sealed_at.is_none() {
        pause_active_attempt_for_stale_recovery_tx(transaction, attempt).await?;
        release_claim(transaction, attempt.assignment_id, None).await?;
    }
    Ok(())
}

async fn pause_active_attempt_for_stale_recovery_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
) -> StoreResult<()> {
    let now = Utc::now();
    let updated = sqlx::query(
        "UPDATE attempts SET state = ?, sealed_at = ?
         WHERE attempt_id = ? AND state = ? AND sealed_at IS NULL",
    )
    .bind(encode(&AttemptState::NeedsMain)?)
    .bind(encode(&now)?)
    .bind(attempt.attempt_id.to_string())
    .bind(encode(&AttemptState::Active)?)
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StoreError::InvalidAssignment(
            "stale-evidence escalation requires an active unsealed attempt".to_string(),
        ));
    }
    Ok(())
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
    let row = sqlx::query("SELECT a.root_session_id, a.body_json, ar.repository_id, ar.workspace_id FROM assignments a LEFT JOIN assignment_repositories ar ON ar.assignment_id = a.assignment_id WHERE a.assignment_id = ?")
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
    let bound_repository_id = row.get::<Option<String>, _>("repository_id");
    let bound_workspace_id = row.get::<Option<String>, _>("workspace_id");
    if let Some(bound_repository_id) = bound_repository_id {
        let legacy_body_identity = bound_workspace_id
            .as_deref()
            .is_some_and(|workspace_id| assignment.repository_id == workspace_id);
        if assignment.repository_id.is_empty() || legacy_body_identity {
            assignment.repository_id = bound_repository_id;
        } else if assignment.repository_id != bound_repository_id {
            return Err(StoreError::CorruptData(format!(
                "assignment repository identity does not match {assignment_id}"
            )));
        }
    }
    if let Some(bound_workspace_id) = bound_workspace_id {
        if assignment.workspace_id.is_empty() {
            assignment.workspace_id = bound_workspace_id;
        } else if assignment.workspace_id != bound_workspace_id {
            return Err(StoreError::CorruptData(format!(
                "assignment workspace identity does not match {assignment_id}"
            )));
        }
    }
    Ok(assignment)
}

async fn upgrade_legacy_repository_bindings(pool: &SqlitePool) -> StoreResult<()> {
    let rows = sqlx::query(
        "SELECT assignment_id, repository_id, workspace_id, canonical_root
         FROM assignment_repositories
         WHERE repository_id = workspace_id
         ORDER BY assignment_id",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let assignment_id = row.get::<String, _>("assignment_id");
        let legacy_repository_id = row.get::<String, _>("repository_id");
        let workspace_id = row.get::<String, _>("workspace_id");
        let canonical_root = row.get::<String, _>("canonical_root");
        let repository = match repository_identity(Path::new(&canonical_root)) {
            Ok(repository) => repository,
            Err(StoreError::InvalidScope(_)) => continue,
            Err(error) => return Err(error),
        };
        if repository.workspace_id != workspace_id {
            return Err(StoreError::CorruptData(format!(
                "legacy workspace identity does not match assignment {assignment_id}"
            )));
        }
        if repository.id == legacy_repository_id {
            continue;
        }
        let now = Utc::now();
        let mut transaction = pool.begin().await?;
        let binding_update = sqlx::query(
            "UPDATE assignment_repositories
             SET repository_id = ?
             WHERE assignment_id = ? AND repository_id = ? AND workspace_id = ?",
        )
        .bind(&repository.id)
        .bind(&assignment_id)
        .bind(&legacy_repository_id)
        .bind(&workspace_id)
        .execute(&mut *transaction)
        .await?;
        if binding_update.rows_affected() != 1 {
            return Err(StoreError::CorruptData(format!(
                "legacy repository binding changed during upgrade for assignment {assignment_id}"
            )));
        }
        sqlx::query(
            "UPDATE workspace_repositories
             SET repository_id = ?, updated_at = ?
             WHERE workspace_id = ? AND repository_id = ?",
        )
        .bind(&repository.id)
        .bind(encode(&now)?)
        .bind(&workspace_id)
        .bind(&legacy_repository_id)
        .execute(&mut *transaction)
        .await?;
        let workspace_repository_id = sqlx::query_scalar::<_, String>(
            "SELECT repository_id FROM workspace_repositories WHERE workspace_id = ?",
        )
        .bind(&workspace_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| {
            StoreError::CorruptData(format!(
                "legacy workspace binding is missing for assignment {assignment_id}"
            ))
        })?;
        if workspace_repository_id != repository.id {
            return Err(StoreError::CorruptData(format!(
                "legacy workspace lineage does not match assignment {assignment_id}"
            )));
        }
        transaction.commit().await?;
    }
    Ok(())
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
    allowed_pending_gate: Option<(AssignmentId, GateKind)>,
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
            continue;
        }
        let gate_rows =
            sqlx::query("SELECT body_json FROM gates WHERE assignment_id = ? ORDER BY kind")
                .bind(dependency.to_string())
                .fetch_all(&mut **transaction)
                .await?;
        let mut blocking_gates = Vec::new();
        for row in gate_rows {
            let gate: AgentGate = decode(row.get::<String, _>("body_json").as_str())?;
            if gate.assignment_id != *dependency {
                return Err(StoreError::CorruptData(format!(
                    "gate identity does not match dependency {dependency}"
                )));
            }
            let allowed_for_relation = gate.status == GateStatus::Pending
                && allowed_pending_gate == Some((*dependency, gate.kind));
            if !allowed_for_relation
                && !matches!(gate.status, GateStatus::Passed | GateStatus::Waived)
            {
                blocking_gates.push((gate.kind, gate.status));
            }
        }
        if !blocking_gates.is_empty() {
            let state = if blocking_gates
                .iter()
                .all(|(_, status)| *status == GateStatus::Pending)
            {
                DependencyState::Incomplete
            } else {
                DependencyState::Blocked
            };
            let detail = blocking_gates
                .iter()
                .map(|(kind, status)| format!("{kind:?}={status:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state,
                detail: format!("dependency gates are not cleared: {detail}"),
            });
        }
    }
    if blockers.is_empty() {
        Ok(())
    } else {
        Err(StoreError::DependencyBlocked { blockers })
    }
}

async fn seal_architecture_contract_for_receipt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    status: AgentStatusClaim,
    contract: Option<ArchitectureContractV1>,
) -> StoreResult<Option<SealedArchitectureContractV1>> {
    if assignment.role != AgentRole::Architect {
        if contract.is_some() {
            return Err(StoreError::InvalidAssignment(
                "only an Architect receipt may seal ArchitectureContractV1".to_string(),
            ));
        }
        return Ok(None);
    }
    if !status.is_success() {
        if contract.is_some() {
            return Err(StoreError::InvalidAssignment(
                "an unsuccessful Architect receipt cannot seal an architecture contract"
                    .to_string(),
            ));
        }
        return Ok(None);
    }
    let contract = contract.ok_or_else(|| {
        StoreError::InvalidAssignment(
            "a successful Architect receipt requires ArchitectureContractV1".to_string(),
        )
    })?;
    let canonical_root = sqlx::query_scalar::<_, String>(
        "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_one(&mut **transaction)
    .await?;
    let contract = canonicalize_architecture_contract(Path::new(&canonical_root), contract)?;
    let contract_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&contract)?));
    Ok(Some(SealedArchitectureContractV1 {
        contract,
        contract_sha256,
    }))
}

async fn validate_architecture_contract_reference_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    worker: &Assignment,
) -> StoreResult<()> {
    let mut architect_dependency_ids = Vec::new();
    for dependency_id in &worker.dependencies {
        let dependency = load_assignment_tx(transaction, *dependency_id).await?;
        if dependency.role == AgentRole::Architect {
            architect_dependency_ids.push(dependency.assignment_id);
        }
    }
    let Some(reference) = worker.architecture_contract_ref.as_ref() else {
        if !architect_dependency_ids.is_empty() {
            return Err(StoreError::InvalidAssignment(
                "architect-dependent assignment is missing its architecture contract reference"
                    .to_string(),
            ));
        }
        return Ok(());
    };
    if !architect_dependency_ids.contains(&reference.architect_assignment_id) {
        return Err(StoreError::InvalidAssignment(
            "architecture contract reference must name a declared Architect dependency".to_string(),
        ));
    }
    let architect = load_assignment_tx(transaction, reference.architect_assignment_id).await?;
    if architect.role != AgentRole::Architect {
        return Err(StoreError::InvalidAssignment(
            "architecture contract reference does not name an Architect assignment".to_string(),
        ));
    }
    let current_attempt = load_current_attempt_tx(transaction, architect.assignment_id).await?;
    if current_attempt.attempt_id != reference.architect_attempt_id {
        return Err(StoreError::InvalidAssignment(
            "architecture contract reference is stale".to_string(),
        ));
    }
    let receipt_json = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM receipts WHERE assignment_id = ? AND attempt_id = ?",
    )
    .bind(architect.assignment_id.to_string())
    .bind(reference.architect_attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or_else(|| {
        StoreError::InvalidAssignment(
            "architecture contract reference has no sealed Architect receipt".to_string(),
        )
    })?;
    let receipt: AgentReceipt = decode(&receipt_json)?;
    if !receipt.status.is_success()
        || receipt.assignment_id != architect.assignment_id
        || receipt.attempt_id != reference.architect_attempt_id
    {
        return Err(StoreError::InvalidAssignment(
            "architecture contract reference does not resolve to a successful receipt".to_string(),
        ));
    }
    let sealed = receipt.architecture_contract.ok_or_else(|| {
        StoreError::InvalidAssignment(
            "successful Architect receipt is missing its sealed architecture contract".to_string(),
        )
    })?;
    if reference.contract_version != ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION
        || sealed.contract.schema_version != reference.contract_version
        || sealed.contract_sha256 != reference.contract_sha256
    {
        return Err(StoreError::InvalidAssignment(
            "architecture contract version or hash does not match the sealed receipt".to_string(),
        ));
    }
    let canonical_root = sqlx::query_scalar::<_, String>(
        "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(worker.assignment_id.to_string())
    .fetch_one(&mut **transaction)
    .await?;
    let repo_root = Path::new(&canonical_root);
    let sealed_contract = canonicalize_architecture_contract(repo_root, sealed.contract)?;
    let sealed_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&sealed_contract)?)
    );
    if sealed_hash != reference.contract_sha256 {
        return Err(StoreError::InvalidAssignment(
            "sealed architecture contract is not canonical for the worker repository".to_string(),
        ));
    }
    let worker_projection = canonicalize_architecture_contract(
        repo_root,
        ArchitectureContractV1 {
            schema_version: ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION,
            objective: worker.objective.clone(),
            acceptance_criteria: worker.acceptance_criteria.clone(),
            read_scope: worker.read_scope.clone(),
            write_scope: worker.write_scope.clone(),
            stop_condition: worker.stop_condition.clone(),
            risk_hints: worker.risk_hints.clone(),
            required_evidence: worker.required_evidence.clone(),
            prohibited_changes: worker.prohibited_changes.clone(),
            contract_claims: worker.contract_claims.clone(),
        },
    )?;
    if worker_projection != sealed_contract {
        return Err(StoreError::InvalidAssignment(
            "worker scope or claims are incompatible with the authoritative architecture contract"
                .to_string(),
        ));
    }
    Ok(())
}

fn canonicalize_architecture_contract(
    repo_root: &Path,
    mut contract: ArchitectureContractV1,
) -> StoreResult<ArchitectureContractV1> {
    if contract.schema_version != ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION {
        return Err(StoreError::InvalidAssignment(format!(
            "unsupported ArchitectureContractV1 version {}",
            contract.schema_version
        )));
    }
    contract.objective = normalized_required_text("architecture objective", contract.objective)?;
    contract.stop_condition =
        normalized_required_text("architecture stop condition", contract.stop_condition)?;
    if contract.acceptance_criteria.is_empty() {
        return Err(StoreError::InvalidAssignment(
            "architecture contract requires at least one acceptance criterion".to_string(),
        ));
    }
    for criterion in &mut contract.acceptance_criteria {
        criterion.id = normalized_required_text("architecture criterion id", criterion.id.clone())?;
        criterion.text =
            normalized_required_text("architecture criterion text", criterion.text.clone())?;
    }
    contract.acceptance_criteria.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.text.cmp(&right.text))
    });
    if contract
        .acceptance_criteria
        .windows(2)
        .any(|pair| pair[0].id == pair[1].id)
    {
        return Err(StoreError::InvalidAssignment(
            "architecture contract contains duplicate acceptance criterion ids".to_string(),
        ));
    }
    contract.read_scope = normalize_repo_scopes(repo_root, &contract.read_scope)?;
    contract.write_scope = normalize_repo_scopes(repo_root, &contract.write_scope)?;
    canonicalize_scopes(&mut contract.read_scope);
    canonicalize_scopes(&mut contract.write_scope);
    canonicalize_text_set("architecture risk hint", &mut contract.risk_hints)?;
    canonicalize_text_set(
        "architecture required evidence",
        &mut contract.required_evidence,
    )?;
    canonicalize_text_set(
        "architecture prohibited change",
        &mut contract.prohibited_changes,
    )?;
    canonicalize_text_set("architecture contract claim", &mut contract.contract_claims)?;
    Ok(contract)
}

fn canonicalize_scopes(scopes: &mut Vec<RepoScope>) {
    scopes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.recursive.cmp(&right.recursive))
    });
    scopes.dedup();
}

fn canonicalize_text_set(field: &str, values: &mut Vec<String>) -> StoreResult<()> {
    for value in values.iter_mut() {
        *value = normalized_required_text(field, std::mem::take(value))?;
    }
    values.sort();
    values.dedup();
    Ok(())
}

fn normalized_required_text(field: &str, value: String) -> StoreResult<String> {
    let normalized = value.trim().to_string();
    if normalized.is_empty() {
        return Err(StoreError::InvalidAssignment(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(normalized)
}

async fn require_gate_actor_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: TaskActor,
    target: &Assignment,
    kind: GateKind,
) -> StoreResult<()> {
    let actor_attempt_id = match actor {
        TaskActor::Root => return Ok(()),
        TaskActor::Attempt(actor_attempt_id) => actor_attempt_id,
    };
    let actor_attempt = load_attempt_tx(transaction, actor_attempt_id).await?;
    let current_actor_attempt =
        load_current_attempt_tx(transaction, actor_attempt.assignment_id).await?;
    if current_actor_attempt.attempt_id != actor_attempt_id {
        return Err(StoreError::GateAuthorityRequired {
            gate: kind.to_string(),
        });
    }
    let actor_assignment = load_assignment_tx(transaction, actor_attempt.assignment_id).await?;
    let expected_relation = match kind {
        GateKind::Review if actor_assignment.role == AgentRole::Reviewer => RelationKind::Review,
        GateKind::Verification if actor_assignment.role == AgentRole::Verifier => {
            RelationKind::Verification
        }
        _ => {
            return Err(StoreError::GateAuthorityRequired {
                gate: kind.to_string(),
            });
        }
    };
    let authorized = actor_assignment.root_session_id == target.root_session_id
        && actor_assignment.repository_id == target.repository_id
        && actor_assignment.relation.as_ref().is_some_and(|relation| {
            relation.kind == expected_relation
                && relation
                    .target_assignment_ids
                    .contains(&target.assignment_id)
        });
    if !authorized {
        return Err(StoreError::GateAuthorityRequired {
            gate: kind.to_string(),
        });
    }
    Ok(())
}

async fn record_gate_verdict_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
    gate: &AgentGate,
) -> StoreResult<()> {
    let sealed_at = gate.sealed_at.ok_or_else(|| {
        StoreError::CorruptData("cannot record an unsealed gate verdict".to_string())
    })?;
    sqlx::query("INSERT INTO gate_verdicts (attempt_id, assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(attempt_id.to_string())
        .bind(gate.assignment_id.to_string())
        .bind(encode(&gate.kind)?)
        .bind(encode(&gate.status)?)
        .bind(encode(gate)?)
        .bind(encode(&gate.updated_at)?)
        .bind(encode(&sealed_at)?)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn insert_risk_review_gates_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
    attempt_id: AttemptId,
    review_reason: &str,
) -> StoreResult<()> {
    if review_reason.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "cold-review reason cannot be empty".to_string(),
        ));
    }

    let now = Utc::now();
    let evidence_epoch = assignment_epoch_tx(transaction, assignment_id).await?;
    let risk_gate = AgentGate {
        assignment_id,
        kind: GateKind::Risk,
        status: GateStatus::Passed,
        reason: review_reason.to_string(),
        waiver_reason: None,
        evidence_epoch,
        updated_at: now,
        sealed_at: Some(now),
    };
    let existing_risk = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateKind::Risk)?)
    .fetch_optional(&mut **transaction)
    .await?
    .map(|value| decode::<AgentGate>(&value))
    .transpose()?;
    match existing_risk {
        Some(mut existing) if existing.status == GateStatus::Passed => {
            if cold_review_reason_contains(review_reason, CONCURRENT_DRIFT_REASON)
                && !cold_review_reason_contains(&existing.reason, CONCURRENT_DRIFT_REASON)
            {
                existing.reason = format!("{}; {CONCURRENT_DRIFT_REASON}", existing.reason);
                existing.updated_at = now;
                existing.sealed_at = Some(now);
                let updated = sqlx::query("UPDATE gates SET body_json = ?, updated_at = ?, sealed_at = ? WHERE assignment_id = ? AND kind = ? AND status = ?")
                    .bind(encode(&existing)?)
                    .bind(encode(&now)?)
                    .bind(encode(&now)?)
                    .bind(assignment_id.to_string())
                    .bind(encode(&GateKind::Risk)?)
                    .bind(encode(&GateStatus::Passed)?)
                    .execute(&mut **transaction)
                    .await?;
                if updated.rows_affected() != 1 {
                    return Err(StoreError::CorruptData(format!(
                        "assignment {assignment_id} risk gate changed while aggregating correction-attempt drift"
                    )));
                }
            }
        }
        Some(existing) => {
            return Err(StoreError::CorruptData(format!(
                "assignment {assignment_id} has incompatible risk gate {:?}",
                existing.status
            )));
        }
        None => {
            sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(assignment_id.to_string())
                .bind(encode(&GateKind::Risk)?)
                .bind(encode(&GateStatus::Passed)?)
                .bind(encode(&risk_gate)?)
                .bind(encode(&now)?)
                .bind(encode(&now)?)
                .execute(&mut **transaction)
                .await?;
        }
    }
    record_gate_verdict_tx(transaction, attempt_id, &risk_gate).await?;

    let review_gate = AgentGate {
        assignment_id,
        kind: GateKind::Review,
        status: GateStatus::Pending,
        reason: review_reason.to_string(),
        waiver_reason: None,
        evidence_epoch,
        updated_at: now,
        sealed_at: None,
    };
    let existing_review = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateKind::Review)?)
    .fetch_optional(&mut **transaction)
    .await?
    .map(|value| decode::<AgentGate>(&value))
    .transpose()?;
    match existing_review {
        Some(existing) if existing.status == GateStatus::Pending => {
            sqlx::query("UPDATE gates SET status = ?, body_json = ?, updated_at = ?, sealed_at = NULL WHERE assignment_id = ? AND kind = ?")
                .bind(encode(&GateStatus::Pending)?)
                .bind(encode(&review_gate)?)
                .bind(encode(&now)?)
                .bind(assignment_id.to_string())
                .bind(encode(&GateKind::Review)?)
                .execute(&mut **transaction)
                .await?;
        }
        Some(existing) => {
            return Err(StoreError::CorruptData(format!(
                "assignment {assignment_id} has incompatible review gate {:?}",
                existing.status
            )));
        }
        None => {
            sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, NULL)")
                .bind(assignment_id.to_string())
                .bind(encode(&GateKind::Review)?)
                .bind(encode(&GateStatus::Pending)?)
                .bind(encode(&review_gate)?)
                .bind(encode(&now)?)
                .execute(&mut **transaction)
                .await?;
        }
    }
    Ok(())
}

fn cold_review_reason_contains(reason: &str, expected: &str) -> bool {
    reason
        .strip_prefix(COLD_REVIEW_REASON_PREFIX)
        .unwrap_or(reason)
        .split("; ")
        .any(|reason| reason == expected)
}

async fn ensure_pending_verification_for_risk_review_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<()> {
    let risk_gate = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateKind::Risk)?)
    .fetch_optional(&mut **transaction)
    .await?
    .map(|value| decode::<AgentGate>(&value))
    .transpose()?;
    if risk_gate
        .as_ref()
        .is_none_or(|gate| gate.status != GateStatus::Passed)
    {
        return Ok(());
    }

    let existing = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateKind::Verification)?)
    .fetch_optional(&mut **transaction)
    .await?
    .map(|value| decode::<AgentGate>(&value))
    .transpose()?;
    if let Some(existing) = existing {
        if existing.status == GateStatus::Pending {
            return Ok(());
        }
        return Err(StoreError::GateAlreadySealed {
            gate: GateKind::Verification.to_string(),
        });
    }

    let now = Utc::now();
    let evidence_epoch = assignment_epoch_tx(transaction, assignment_id).await?;
    let gate = AgentGate {
        assignment_id,
        kind: GateKind::Verification,
        status: GateStatus::Pending,
        reason: "independent verification required after risk-gated cold review".to_string(),
        waiver_reason: None,
        evidence_epoch,
        updated_at: now,
        sealed_at: None,
    };
    sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, NULL)")
        .bind(assignment_id.to_string())
        .bind(encode(&GateKind::Verification)?)
        .bind(encode(&GateStatus::Pending)?)
        .bind(encode(&gate)?)
        .bind(encode(&now)?)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn validate_completed_mutation_evidence_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    draft: &mut ReceiptDraft,
) -> StoreResult<()> {
    let canonical_root = sqlx::query_scalar::<_, String>(
        "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(
        assignment.assignment_id,
    ))?;
    let repo_root = Path::new(&canonical_root);
    let mut declared = BTreeSet::new();
    for change in &mut draft.declared_changes {
        if change.summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "declared change summary cannot be empty".to_string(),
            ));
        }
        change.path = normalize_repo_path(repo_root, &change.path)?;
        if !declared.insert(change.path.clone()) {
            return Err(StoreError::InvalidAssignment(format!(
                "duplicate declared change {}",
                change.path
            )));
        }
    }
    let rows = sqlx::query(
        "SELECT path, finalized_at FROM mutation_files WHERE attempt_id = ? ORDER BY path",
    )
    .bind(attempt_id.to_string())
    .fetch_all(&mut **transaction)
    .await?;
    let mut finalized = BTreeSet::new();
    for row in rows {
        let path = normalize_repo_path(repo_root, row.get::<String, _>("path").as_str())?;
        if row.get::<Option<String>, _>("finalized_at").is_none() {
            return Err(StoreError::MutationNotFinalized { attempt_id, path });
        }
        finalized.insert(path);
    }
    if declared != finalized {
        return Err(StoreError::MutationEvidenceMismatch {
            declared: declared.into_iter().collect(),
            finalized: finalized.into_iter().collect(),
        });
    }
    Ok(())
}

pub(crate) async fn heartbeat_typed_workspace_actor_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace_id: &str,
    binding: &AgentTaskBinding,
) -> StoreResult<bool> {
    let Some(thread_id) = binding
        .thread_id
        .as_deref()
        .filter(|thread_id| !thread_id.trim().is_empty())
    else {
        return Ok(false);
    };

    // Match orphan scavenging's lock order: workspace writer first, then assignment.
    sqlx::query("UPDATE workspace_repositories SET epoch = epoch WHERE workspace_id = ?")
        .bind(workspace_id)
        .execute(&mut **transaction)
        .await?;
    lock_assignment_tx(transaction, binding.assignment_id).await?;

    let now = Utc::now();
    let lease_expires_at = now + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS);
    let updated = sqlx::query(
        "UPDATE workspace_actors
         SET state = 'active', lease_expires_at = ?
         WHERE workspace_id = ?
           AND actor_id = ?
           AND root_session_id = ?
           AND kind = ?
           AND assignment_id = ?
           AND attempt_id = ?
           AND state <> 'terminal'
           AND EXISTS (
               SELECT 1
               FROM agent_task_bindings bindings
               JOIN attempts ON attempts.attempt_id = bindings.attempt_id
               JOIN assignments ON assignments.assignment_id = bindings.assignment_id
               JOIN assignment_repositories repositories
                 ON repositories.assignment_id = bindings.assignment_id
               WHERE bindings.assignment_id = ?
                 AND bindings.attempt_id = ?
                 AND bindings.root_session_id = ?
                 AND bindings.agent_path = ?
                 AND bindings.task_name = ?
                 AND bindings.thread_id = ?
                 AND attempts.assignment_id = bindings.assignment_id
                 AND attempts.state = ?
                 AND attempts.sealed_at IS NULL
                 AND attempts.ordinal = (
                     SELECT MAX(current.ordinal)
                     FROM attempts current
                     WHERE current.assignment_id = bindings.assignment_id
                 )
                 AND assignments.root_session_id = ?
                 AND repositories.workspace_id = ?
           )",
    )
    .bind(encode(&lease_expires_at)?)
    .bind(workspace_id)
    .bind(format!("attempt:{}", binding.attempt_id))
    .bind(&binding.root_session_id)
    .bind(encode(&crate::WorkspaceActorKind::Typed)?)
    .bind(binding.assignment_id.to_string())
    .bind(binding.attempt_id.to_string())
    .bind(binding.assignment_id.to_string())
    .bind(binding.attempt_id.to_string())
    .bind(&binding.root_session_id)
    .bind(&binding.agent_path)
    .bind(&binding.task_name)
    .bind(thread_id)
    .bind(encode(&AttemptState::Active)?)
    .bind(&binding.root_session_id)
    .bind(workspace_id)
    .execute(&mut **transaction)
    .await?;
    Ok(updated.rows_affected() == 1)
}

async fn selective_admission_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    isolated_integrator_available: bool,
) -> StoreResult<(AdmissionOverlapSummary, IntegrationPlan)> {
    let rows = sqlx::query(
        "SELECT assignments.body_json, repositories.workspace_id
         FROM assignments
         JOIN assignment_repositories repositories USING (assignment_id)
         JOIN attempts ON attempts.assignment_id = assignments.assignment_id
         WHERE repositories.repository_id = ?
           AND assignments.root_session_id = ?
           AND attempts.state = ?
           AND attempts.sealed_at IS NULL
           AND attempts.ordinal = (
               SELECT MAX(current.ordinal)
               FROM attempts current
               WHERE current.assignment_id = assignments.assignment_id
           )
         ORDER BY assignments.assignment_id",
    )
    .bind(&assignment.repository_id)
    .bind(&assignment.root_session_id)
    .bind(encode(&AttemptState::Active)?)
    .fetch_all(&mut **transaction)
    .await?;

    let candidate_identity = assignment.primary_investigation_identity();
    let mut overlaps = AdmissionOverlapSummary::default();
    let mut active_writer_count = 0_u32;
    let mut writer_in_another_workspace = false;
    for row in rows {
        let existing: Assignment = decode(row.get::<String, _>("body_json").as_str())?;
        let existing_workspace_id = row.get::<String, _>("workspace_id");
        let read_overlaps = existing.read_scope.iter().any(|existing_scope| {
            assignment
                .read_scope
                .iter()
                .any(|scope| existing_scope.overlaps(scope))
        });
        if read_overlaps {
            overlaps.benign_read_overlap_count =
                overlaps.benign_read_overlap_count.saturating_add(1);
        }

        if candidate_identity.is_some()
            && candidate_identity == existing.primary_investigation_identity()
        {
            return Err(StoreError::AdmissionRejected {
                reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
            });
        }

        if !existing.write_scope.is_empty() {
            active_writer_count = active_writer_count.saturating_add(1);
            writer_in_another_workspace |= existing_workspace_id != assignment.workspace_id;
        }
    }

    let integration_plan = if assignment.write_scope.is_empty() || active_writer_count == 0 {
        IntegrationPlan::SingleWriter
    } else if assignment.workspace_strategy == WorkspaceStrategy::Isolated
        || writer_in_another_workspace
    {
        if !isolated_integrator_available {
            return Err(StoreError::AdmissionRejected {
                reason: AdmissionRejectionReason::IsolatedIntegratorUnavailable,
            });
        }
        IntegrationPlan::TypedIntegratorRequired
    } else {
        IntegrationPlan::RootOwned
    };
    Ok((overlaps, integration_plan))
}

pub(crate) async fn release_orphaned_claims_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace_id: &str,
) -> StoreResult<Vec<AssignmentId>> {
    let now = comparison_now();
    let recent_after = now - Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS);

    // Acquire SQLite's writer lock before evaluating liveness so a concurrent heartbeat or
    // relation spawn cannot be missed between the read and the claim release.
    sqlx::query("UPDATE workspace_repositories SET epoch = epoch WHERE workspace_id = ?")
        .bind(workspace_id)
        .execute(&mut **transaction)
        .await?;

    let live_actor_rows = sqlx::query(
        "SELECT assignments.body_json
         FROM workspace_actors
         JOIN assignments USING (assignment_id)
         WHERE workspace_actors.workspace_id = ?
           AND workspace_actors.state <> 'terminal'
           AND (
               julianday(json_extract(workspace_actors.lease_expires_at, '$')) >= julianday(json_extract(?, '$'))
               OR julianday(json_extract(workspace_actors.last_progress_at, '$')) >= julianday(json_extract(?, '$'))
           )",
    )
    .bind(workspace_id)
    .bind(encode(&now)?)
    .bind(encode(&recent_after)?)
    .fetch_all(&mut **transaction)
    .await?;
    let mut live_relation_targets = HashSet::new();
    for row in live_actor_rows {
        let assignment: Assignment = decode(row.get::<String, _>("body_json").as_str())?;
        if let Some(relation) = assignment.relation {
            live_relation_targets.extend(relation.target_assignment_ids);
        }
    }

    let claim_rows = sqlx::query(
        "SELECT assignment_id, attempt_id
         FROM (
             SELECT write_claims.assignment_id, write_claims.attempt_id
             FROM write_claims
             JOIN assignment_repositories USING (assignment_id)
             WHERE write_claims.active = 1 AND assignment_repositories.workspace_id = ?
             UNION
             SELECT contract_claims.assignment_id, contract_claims.attempt_id
             FROM contract_claims
             WHERE contract_claims.active = 1 AND contract_claims.workspace_id = ?
         ) claims
         ORDER BY assignment_id",
    )
    .bind(workspace_id)
    .bind(workspace_id)
    .fetch_all(&mut **transaction)
    .await?;

    let mut released = Vec::new();
    for row in claim_rows {
        let assignment_id = AssignmentId::parse(&row.get::<String, _>("assignment_id"))?;
        let attempt_id = AttemptId::parse(&row.get::<String, _>("attempt_id"))?;
        lock_assignment_tx(transaction, assignment_id).await?;

        let claim_is_active = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                 SELECT 1
                 FROM write_claims
                 JOIN assignment_repositories USING (assignment_id)
                 WHERE write_claims.assignment_id = ?
                   AND write_claims.attempt_id = ?
                   AND write_claims.active = 1
                   AND assignment_repositories.workspace_id = ?
                 UNION
                 SELECT 1
                 FROM contract_claims
                 WHERE contract_claims.assignment_id = ?
                   AND contract_claims.attempt_id = ?
                   AND contract_claims.active = 1
                   AND contract_claims.workspace_id = ?
             )",
        )
        .bind(assignment_id.to_string())
        .bind(attempt_id.to_string())
        .bind(workspace_id)
        .bind(assignment_id.to_string())
        .bind(attempt_id.to_string())
        .bind(workspace_id)
        .fetch_one(&mut **transaction)
        .await?
            != 0;
        if !claim_is_active {
            continue;
        }

        let owner_is_live = sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(
                 SELECT 1
                 FROM workspace_actors
                 WHERE workspace_id = ?
                   AND assignment_id = ?
                   AND attempt_id = ?
                   AND state <> 'terminal'
                   AND (
                       julianday(json_extract(lease_expires_at, '$')) >= julianday(json_extract(?, '$'))
                       OR julianday(json_extract(last_progress_at, '$')) >= julianday(json_extract(?, '$'))
                   )
             )",
        )
        .bind(workspace_id)
        .bind(assignment_id.to_string())
        .bind(attempt_id.to_string())
        .bind(encode(&now)?)
        .bind(encode(&recent_after)?)
        .fetch_one(&mut **transaction)
        .await?
            != 0;
        if owner_is_live || live_relation_targets.contains(&assignment_id) {
            continue;
        }

        let attempt = load_attempt_tx(transaction, attempt_id).await?;
        let assignment = load_assignment_tx(transaction, assignment_id).await?;
        let escalated = if attempt.state == AttemptState::Active && attempt.sealed_at.is_none() {
            pause_active_attempt_for_stale_recovery_tx(transaction, &attempt).await?;
            true
        } else if attempt.state == AttemptState::Completed && attempt.sealed_at.is_some() {
            transition_attempt_to_needs_main_tx(transaction, &attempt).await?;
            true
        } else {
            false
        };
        release_claim(transaction, assignment_id, None).await?;
        if escalated {
            append_observation_tx(
                transaction,
                &assignment,
                attempt_id,
                ObservationKind::NeedsMain,
                "workspace claim released after its owner lease expired with no live related agent"
                    .to_string(),
                None,
            )
            .await?;
        }
        released.push(assignment_id);
    }
    Ok(released)
}

async fn planned_claim_supersessions_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
) -> StoreResult<Vec<AssignmentId>> {
    if assignment.write_scope.is_empty() || assignment.role != AgentRole::Integrator {
        return Ok(Vec::new());
    }
    let binding = sqlx::query(
        "SELECT repository_id, workspace_id FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(
        assignment.assignment_id,
    ))?;
    let bound_repository_id = binding.get::<String, _>("repository_id");
    let bound_workspace_id = binding.get::<String, _>("workspace_id");
    if assignment.repository_id.is_empty()
        || assignment.repository_id != bound_repository_id
        || assignment.workspace_id != bound_workspace_id
    {
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
    let rows = sqlx::query(
        "SELECT wc.assignment_id, wc.scopes_json, ar.repository_id, ar.canonical_root
         FROM write_claims wc
         LEFT JOIN assignment_repositories ar ON ar.assignment_id = wc.assignment_id
         WHERE wc.active = 1 AND (ar.workspace_id = ? OR ar.workspace_id IS NULL)",
    )
    .bind(&bound_workspace_id)
    .fetch_all(&mut **transaction)
    .await?;
    let mut supersedes = HashSet::new();
    for row in rows {
        let existing_id = AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?;
        if !integrator_targets.contains(&existing_id) {
            continue;
        }
        let existing_repository_id = row.get::<Option<String>, _>("repository_id");
        let mut scopes: Vec<RepoScope> = decode(row.get::<String, _>("scopes_json").as_str())?;
        if let Some(canonical_root) = row.get::<Option<String>, _>("canonical_root") {
            scopes = scopes
                .into_iter()
                .map(|scope| {
                    normalize_repo_scopes(Path::new(&canonical_root), std::slice::from_ref(&scope))
                        .map(|mut scopes| scopes.remove(0))
                })
                .collect::<StoreResult<Vec<_>>>()?;
        }
        let fully_covered = scopes.iter().all(|existing_scope| {
            assignment
                .write_scope
                .iter()
                .any(|requested_scope| requested_scope.covers_scope(existing_scope))
        });
        if existing_repository_id.is_some() && fully_covered {
            supersedes.insert(existing_id);
        }
    }
    let mut supersedes: Vec<_> = supersedes.into_iter().collect();
    supersedes.sort();
    Ok(supersedes)
}

async fn require_repository_identity_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    repository: &RepositoryIdentity,
) -> StoreResult<()> {
    let row = sqlx::query(
        "SELECT repository_id, workspace_id, canonical_root FROM assignment_repositories WHERE assignment_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(
        assignment.assignment_id,
    ))?;
    let bound_id = row.get::<String, _>("repository_id");
    let bound_workspace_id = row.get::<String, _>("workspace_id");
    let bound_root = row.get::<String, _>("canonical_root");
    let root_matches = if cfg!(windows) {
        bound_root.to_lowercase() == repository.canonical_path.to_lowercase()
    } else {
        bound_root == repository.canonical_path
    };
    if assignment.repository_id.is_empty()
        || assignment.repository_id != bound_id
        || repository.id != bound_id
        || assignment.workspace_id != bound_workspace_id
        || repository.workspace_id != bound_workspace_id
        || !root_matches
    {
        return Err(StoreError::RepositoryMismatch(assignment.assignment_id));
    }
    Ok(())
}

async fn load_mutation_evidence_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt_id: AttemptId,
    path: &str,
) -> StoreResult<MutationEvidence> {
    let row = sqlx::query("SELECT assignment_id, pre_write_hash, pre_write_existed, final_hash, final_write_existed, attribution_confidence, snapshot_retained, first_observed_at, finalized_at, start_epoch, end_epoch FROM mutation_files WHERE attempt_id = ? AND path = ?")
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
    let start_epoch = u64::try_from(row.get::<i64, _>("start_epoch"))
        .map_err(|_| StoreError::CorruptData("mutation start epoch is negative".to_string()))?;
    let end_epoch = row
        .get::<Option<i64>, _>("end_epoch")
        .map(|epoch| {
            u64::try_from(epoch)
                .map_err(|_| StoreError::CorruptData("mutation end epoch is negative".to_string()))
        })
        .transpose()?;
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
        start_epoch,
        end_epoch,
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

fn gate_requires_main_intervention(attempt: &Attempt, kind: GateKind, status: GateStatus) -> bool {
    matches!(
        (kind, status),
        (GateKind::Review, GateStatus::Failed)
            | (
                GateKind::Verification,
                GateStatus::Failed | GateStatus::ChangesRequested
            )
    ) || kind == GateKind::Review && status == GateStatus::ChangesRequested && attempt.ordinal > 0
}

async fn transition_attempt_to_needs_main_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
) -> StoreResult<()> {
    let updated = sqlx::query(
        "UPDATE attempts SET state = ? WHERE attempt_id = ? AND state = ? AND sealed_at IS NOT NULL",
    )
    .bind(encode(&AttemptState::NeedsMain)?)
    .bind(attempt.attempt_id.to_string())
    .bind(encode(&AttemptState::Completed)?)
    .execute(&mut **transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StoreError::InvalidAssignment(
            "a failed review or verification verdict requires a sealed completed attempt"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn binding_from_row(row: &sqlx::sqlite::SqliteRow) -> StoreResult<AgentTaskBinding> {
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

fn isolation_handoff_from_row(
    row: &sqlx::sqlite::SqliteRow,
) -> StoreResult<crate::IsolationHandoff> {
    Ok(crate::IsolationHandoff {
        assignment_id: AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?,
        source_workspace_id: row.get("source_workspace_id"),
        source_repository_root: row.try_get("source_repository_root").ok(),
        source_epoch: u64::try_from(row.get::<i64, _>("source_epoch")).map_err(|_| {
            StoreError::CorruptData("isolated handoff source epoch is negative".to_string())
        })?,
        source_manifest_hash: row.get("source_manifest_hash"),
        covered_manifest: decode(row.get::<String, _>("covered_manifest_json").as_str())?,
        state: decode(row.get::<String, _>("state").as_str())?,
        integrator_assignment_id: row
            .get::<Option<String>, _>("integrator_assignment_id")
            .map(|value| AssignmentId::parse(&value))
            .transpose()?,
        created_at: decode(row.get::<String, _>("created_at").as_str())?,
        integrated_at: row
            .get::<Option<String>, _>("integrated_at")
            .map(|value| decode(&value))
            .transpose()?,
    })
}

fn task_capsule_path(coordination_root: &Path, assignment_id: AssignmentId) -> PathBuf {
    coordination_root
        .join("task_capsules")
        .join(format!("{assignment_id}.json"))
}

fn hydrate_task_capsule(coordination_root: &Path, assignment: &mut Assignment) -> StoreResult<()> {
    let capsule_path = task_capsule_path(coordination_root, assignment.assignment_id);
    let canonical_payload = match std::fs::read_to_string(capsule_path) {
        Ok(payload) => payload,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let capsule: TaskCapsuleV1 = serde_json::from_str(&canonical_payload)?;
    if capsule.schema_version != 1 || capsule.assignment_id != assignment.assignment_id {
        return Err(StoreError::CorruptData(
            "persisted TaskCapsuleV1 identity or schema version is invalid".to_string(),
        ));
    }
    if serde_json::to_string(&capsule)?.as_bytes() != canonical_payload.as_bytes() {
        return Err(StoreError::CorruptData(
            "persisted TaskCapsuleV1 payload is not canonical".to_string(),
        ));
    }
    assignment.task_capsule = Some(canonical_payload);
    Ok(())
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

struct SnapshotCapture {
    existed: bool,
    hash: Option<String>,
}

async fn capture_snapshot_atomic(
    source_path: PathBuf,
    snapshot_path: PathBuf,
    logical_path: String,
) -> StoreResult<SnapshotCapture> {
    tokio::task::spawn_blocking(move || {
        let parent = snapshot_path.parent().ok_or_else(|| {
            StoreError::CorruptData("private snapshot has no parent directory".to_string())
        })?;
        std::fs::create_dir_all(parent)?;
        let file_name = snapshot_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StoreError::CorruptData("private snapshot name is not valid UTF-8".to_string())
            })?;
        let temporary_path =
            snapshot_path.with_file_name(format!(".{file_name}.tmp-{}", MutationEventId::new()));
        let result = (|| {
            let mut destination = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)?;
            let capture = match std::fs::File::open(&source_path) {
                Ok(mut source) => {
                    let initial_bytes = source.metadata()?.len();
                    if initial_bytes > MAX_MUTATION_SNAPSHOT_BYTES {
                        return Err(StoreError::SnapshotTooLarge {
                            path: logical_path.clone(),
                            bytes: initial_bytes,
                            max_bytes: MAX_MUTATION_SNAPSHOT_BYTES,
                        });
                    }
                    let mut hasher = Sha256::new();
                    let mut total_bytes = 0_u64;
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        let read = source.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        total_bytes = total_bytes.saturating_add(read as u64);
                        if total_bytes > MAX_MUTATION_SNAPSHOT_BYTES {
                            return Err(StoreError::SnapshotTooLarge {
                                path: logical_path.clone(),
                                bytes: total_bytes,
                                max_bytes: MAX_MUTATION_SNAPSHOT_BYTES,
                            });
                        }
                        hasher.update(&buffer[..read]);
                        destination.write_all(&buffer[..read])?;
                    }
                    SnapshotCapture {
                        existed: true,
                        hash: Some(format!("{:x}", hasher.finalize())),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    destination.write_all(NONEXISTENT_SENTINEL)?;
                    SnapshotCapture {
                        existed: false,
                        hash: None,
                    }
                }
                Err(error) => return Err(error.into()),
            };
            destination.flush()?;
            destination.sync_all()?;
            std::fs::rename(&temporary_path, &snapshot_path)?;
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(capture)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        result
    })
    .await
    .map_err(|error| {
        StoreError::Io(std::io::Error::other(format!(
            "snapshot capture task failed: {error}"
        )))
    })?
}

async fn read_verified_snapshot_chunk(
    snapshot_path: PathBuf,
    attempt_id: AttemptId,
    logical_path: String,
    expected_hash: String,
    offset: u64,
    max_bytes: usize,
) -> StoreResult<(u64, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(snapshot_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreError::SnapshotUnavailable {
                    attempt_id,
                    path: logical_path.clone(),
                }
            } else {
                error.into()
            }
        })?;
        let initial_bytes = file.metadata()?.len();
        if initial_bytes > MAX_MUTATION_SNAPSHOT_BYTES {
            return Err(StoreError::SnapshotTooLarge {
                path: logical_path,
                bytes: initial_bytes,
                max_bytes: MAX_MUTATION_SNAPSHOT_BYTES,
            });
        }
        if offset > initial_bytes {
            return Err(StoreError::InvalidSnapshotOffset {
                offset,
                total_bytes: initial_bytes,
            });
        }
        let requested_end = offset.saturating_add(max_bytes as u64);
        let mut hasher = Sha256::new();
        let mut position = 0_u64;
        let mut bytes = Vec::with_capacity(max_bytes.min((initial_bytes - offset) as usize));
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let chunk_start = position;
            let chunk_end = position.saturating_add(read as u64);
            if chunk_end > MAX_MUTATION_SNAPSHOT_BYTES {
                return Err(StoreError::SnapshotTooLarge {
                    path: logical_path,
                    bytes: chunk_end,
                    max_bytes: MAX_MUTATION_SNAPSHOT_BYTES,
                });
            }
            hasher.update(&buffer[..read]);
            if chunk_end > offset && chunk_start < requested_end {
                let copy_start = offset.saturating_sub(chunk_start) as usize;
                let copy_end = read.min(requested_end.saturating_sub(chunk_start) as usize);
                bytes.extend_from_slice(&buffer[copy_start..copy_end]);
            }
            position = chunk_end;
        }
        if offset > position {
            return Err(StoreError::InvalidSnapshotOffset {
                offset,
                total_bytes: position,
            });
        }
        if format!("{:x}", hasher.finalize()) != expected_hash {
            return Err(StoreError::SnapshotHashMismatch {
                attempt_id,
                path: logical_path,
            });
        }
        Ok((position, bytes))
    })
    .await
    .map_err(|error| {
        StoreError::Io(std::io::Error::other(format!(
            "snapshot read task failed: {error}"
        )))
    })?
}

async fn verify_nonexistent_snapshot_marker(
    snapshot_path: PathBuf,
    attempt_id: AttemptId,
    logical_path: String,
) -> StoreResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut marker_file = std::fs::File::open(snapshot_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                StoreError::SnapshotUnavailable {
                    attempt_id,
                    path: logical_path.clone(),
                }
            } else {
                error.into()
            }
        })?;
        if marker_file.metadata()?.len() != NONEXISTENT_SENTINEL.len() as u64 {
            return Err(StoreError::SnapshotHashMismatch {
                attempt_id,
                path: logical_path,
            });
        }
        let mut marker = vec![0_u8; NONEXISTENT_SENTINEL.len()];
        marker_file.read_exact(&mut marker)?;
        let mut trailing = [0_u8; 1];
        if marker != NONEXISTENT_SENTINEL || marker_file.read(&mut trailing)? != 0 {
            return Err(StoreError::SnapshotHashMismatch {
                attempt_id,
                path: logical_path,
            });
        }
        Ok(())
    })
    .await
    .map_err(|error| {
        StoreError::Io(std::io::Error::other(format!(
            "snapshot marker verification task failed: {error}"
        )))
    })?
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

async fn assignment_epoch_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<u64> {
    let epoch = sqlx::query_scalar::<_, i64>(
        "SELECT workspace_repositories.epoch
         FROM assignment_repositories
         JOIN workspace_repositories USING (workspace_id)
         WHERE assignment_repositories.assignment_id = ?",
    )
    .bind(assignment_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(StoreError::RepositoryBindingMissing(assignment_id))?;
    u64::try_from(epoch)
        .map_err(|_| StoreError::CorruptData("workspace epoch is negative".to_string()))
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
    sqlx::query(
        "UPDATE contract_claims SET active = 0, released_at = ?
         WHERE assignment_id = ? AND active = 1",
    )
    .bind(encode(&Utc::now())?)
    .bind(assignment_id.to_string())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "UPDATE workspace_actors SET state = 'terminal', last_progress_at = ?,
         lease_expires_at = NULL WHERE assignment_id = ?",
    )
    .bind(encode(&Utc::now())?)
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
        .join(format!(
            "{}-{}.{}",
            hash_bytes(path.as_bytes()),
            MutationEventId::new(),
            extension
        ))
}

const CURRENT_REQUIRED_EVIDENCE_SOURCES: &[&str] = &["focused_validation_call_summary:v1"];
const REVISIONED_REQUIRED_EVIDENCE_SOURCES: &[&str] = &["focused_validation_call_summary:v1"];

fn required_evidence_cache_eligible() -> bool {
    // This exact-set check deliberately fails open when a new evidence source
    // becomes authoritative but has not yet supplied a trustworthy revision or
    // mandatory freshness check.
    required_evidence_sources_are_revisioned(CURRENT_REQUIRED_EVIDENCE_SOURCES)
}

pub(crate) fn required_evidence_sources_are_revisioned(authoritative_sources: &[&str]) -> bool {
    authoritative_sources == REVISIONED_REQUIRED_EVIDENCE_SOURCES
}

fn normalized_requirement_identity(requirement: &str) -> String {
    requirement.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn evidence_obligation(
    assignment: &Assignment,
    ordinal: usize,
    requirement: &str,
) -> MissingEvidenceObligation {
    let canonical_requirement = normalized_requirement_identity(requirement);
    MissingEvidenceObligation {
        id: format!(
            "required_evidence:v1:{}:{:04}:{}",
            assignment.assignment_id,
            ordinal + 1,
            hash_bytes(canonical_requirement.as_bytes())
        ),
        requirement: requirement.to_string(),
    }
}

fn missing_evidence_obligations(
    assignment: &Assignment,
    validation_summaries: &HashSet<String>,
) -> Vec<MissingEvidenceObligation> {
    assignment
        .required_evidence
        .iter()
        .enumerate()
        .filter(|(_, requirement)| !validation_summaries.contains(requirement.as_str()))
        .map(|(ordinal, requirement)| evidence_obligation(assignment, ordinal, requirement))
        .collect()
}

async fn missing_evidence_fingerprint_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt: &Attempt,
    receipt: &ReceiptDraft,
) -> StoreResult<Option<MissingEvidenceFingerprint>> {
    if !required_evidence_cache_eligible()
        || receipt.status != AgentStatusClaim::Completed
        || attempt.assignment_id != assignment.assignment_id
        || attempt.state != AttemptState::Active
        || attempt.sealed_at.is_some()
    {
        return Ok(None);
    }
    let binding = sqlx::query(
        "SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id,
                bound_at, updated_at
         FROM agent_task_bindings
         WHERE assignment_id = ? AND attempt_id = ?",
    )
    .bind(assignment.assignment_id.to_string())
    .bind(attempt.attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .map(|row| binding_from_row(&row))
    .transpose()?;
    let Some(binding) = binding else {
        return Ok(None);
    };
    let validation_evidence_revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM validation_evidence_revisions WHERE attempt_id = ?",
    )
    .bind(attempt.attempt_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .unwrap_or(0);
    let validation_evidence_revision =
        u64::try_from(validation_evidence_revision).map_err(|_| {
            StoreError::CorruptData("validation evidence revision is negative".to_string())
        })?;
    let all_obligations = assignment
        .required_evidence
        .iter()
        .enumerate()
        .map(|(ordinal, requirement)| evidence_obligation(assignment, ordinal, requirement))
        .collect::<Vec<_>>();
    Ok(Some(MissingEvidenceFingerprint {
        assignment_id: assignment.assignment_id,
        attempt_id: attempt.attempt_id,
        binding_hash: hash_bytes(encode(&binding)?.as_bytes()),
        receipt_hash: hash_bytes(encode(receipt)?.as_bytes()),
        obligation_set_hash: hash_bytes(encode(&all_obligations)?.as_bytes()),
        validation_evidence_revision,
        workspace_epoch: assignment_epoch_tx(transaction, assignment.assignment_id).await?,
    }))
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
