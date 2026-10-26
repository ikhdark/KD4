#[path = "wake_subscriptions.rs"]
mod wake_subscriptions;
#[path = "source_inspection.rs"]
mod source_inspection;

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
use sqlx::sqlite::SqliteSynchronous;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio::sync::watch;

use crate::ARCHITECTURE_CONTRACT_V1_SCHEMA_VERSION;
use crate::AdmissionOverlapSummary;
use crate::AdmissionRejectionReason;
use crate::AdmittedAssignment;
use crate::AgentGate;
use crate::AgentReceipt;
use crate::AgentRole;
use crate::AgentStatusClaim;
use crate::AgentTask;
use crate::AgentTaskAuthorization;
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
use crate::CriterionStatus;
use crate::DependencyBlocker;
use crate::DependencyState;
use crate::GateKind;
use crate::GateStatus;
use crate::IntegrationPlan;
use crate::MAX_BINDING_LIMIT;
use crate::MAX_OBSERVATION_LIMIT;
use crate::MAX_WAKE_EVENTS_PER_READ;
use crate::MAX_WAKE_EVENTS_PER_ROOT;
use crate::MutationEventId;
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
use crate::ValidationCall;
use crate::ValidationCallStatus;
use crate::WakeEvent;
use crate::WakeEventId;
use crate::WakeRead;
use crate::WakeReadStatus;
use crate::WorkspaceStrategy;
use crate::WorkspaceTaskStatus;
use crate::scope::CandidateScopeCase;
use crate::scope::RepositoryIdentity;
use crate::scope::normalize_repo_path_async;
use crate::scope::normalize_repo_scopes;
use crate::scope::repository_identity;
use crate::scope::repository_identity_async;
use crate::scope::scopes_overlap_observed;

const COORDINATION_DIR: &str = "agent-task-coordination";
const DATABASE_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const EXTERNAL_WAKE_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const DATABASE_FILENAME: &str = "agent_tasks.sqlite";
const DATABASE_MAX_CONNECTIONS: u32 = 4;

fn workspace_actor_lease_state(
    state: &str,
    lease_expires_at: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> Option<crate::LeaseState> {
    if state != "active" {
        return Some(crate::LeaseState::Released);
    }
    Some(match lease_expires_at {
        Some(expires_at) if expires_at >= now => crate::LeaseState::Active,
        _ => crate::LeaseState::Expired,
    })
}

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

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");





fn comparison_now() -> chrono::DateTime<Utc> {
    Utc::now()
}



#[derive(Clone)]
pub struct LocalAgentTaskStore {
    pool: SqlitePool,
    coordination_root: Arc<PathBuf>,
    wake_revision: Arc<watch::Sender<u64>>,
    durable_wake_poller: Arc<DurableWakePoller>,
}

/// Own bounded host evidence until it can share a receipt's transaction. No
/// observation or durable wake is visible before that boundary. The session's
/// pending terminal record retains crash recovery; Drop handles observer loss.
pub struct DeferredHostCompletionWake {
    store: LocalAgentTaskStore,
    attempt_id: AttemptId,
    pending: Option<(String, String)>,
}

impl DeferredHostCompletionWake {
    pub async fn publish(&mut self) -> StoreResult<()> {
        let Some((origin, summary)) = self.pending.as_ref() else {
            return Ok(());
        };
        self.store
            .record_host_completion_observation(self.attempt_id, origin.clone(), summary.clone())
            .await?;
        self.pending = None;
        Ok(())
    }

    pub async fn seal_receipt(
        &mut self,
        receipt: ReceiptDraft,
        host_legacy_outcome: bool,
    ) -> StoreResult<AgentReceipt> {
        let receipt = self.store.submit_agent_receipt_impl(
            self.attempt_id, receipt, None, host_legacy_outcome, self.pending.clone(),
        ).await?;
        self.pending = None;
        self.store.notify_wake_waiters();
        Ok(receipt)
    }
}

impl Drop for DeferredHostCompletionWake {
    fn drop(&mut self) {
        let Some((origin, summary)) = self.pending.take() else {
            return;
        };
        let store = self.store.clone();
        let attempt_id = self.attempt_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // The normal writer fences the exact attempt and deduplicates
                // the origin, including a receipt commit racing cancellation.
                if let Err(error) = store.record_host_completion_observation(attempt_id, origin, summary).await {
                    tracing::warn!(%attempt_id, %error, "deferred host completion remains pending for terminal recovery");
                }
            });
        }
    }
}

pub type TaskStoreFuture<'a, T> = Pin<Box<dyn Future<Output = StoreResult<T>> + Send + 'a>>;

#[derive(Debug)]
pub struct PendingTerminalOutcome {
    pub binding: AgentTaskBinding,
    pub turn_id: String,
    pub status_json: String,
    pub event_json: String,
}

struct DurableWakePoller {
    subscriptions: Arc<wake_subscriptions::WakeSubscriptions>,
    waiter_count: AtomicUsize,
    active: watch::Sender<bool>,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DurableWakePoller {
    fn spawn(
        mut connection: sqlx::pool::PoolConnection<sqlx::Sqlite>,
        wake_revision: Arc<watch::Sender<u64>>,
        watermark: i64,
    ) -> Arc<Self> {
        let (active, mut active_rx) = watch::channel(false);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let subscriptions = Arc::new(wake_subscriptions::WakeSubscriptions::default());
        let task_subscriptions = Arc::clone(&subscriptions);
        let mut local_wake = wake_revision.subscribe();
        let task = tokio::spawn(async move {
            let mut watermark = watermark;
            loop {
                while !*active_rx.borrow() {
                    tokio::select! {
                        changed = active_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                        }
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                return;
                            }
                        }
                    }
                }

                let locally_notified = tokio::select! {
                    changed = active_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        true
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return;
                        }
                        continue;
                    }
                    changed = local_wake.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        true
                    }
                    _ = tokio::time::sleep(EXTERNAL_WAKE_RECHECK_INTERVAL) => {
                        false
                    }
                };
                let refresh = match durable_wake_watermark(&mut connection).await {
                    Ok(next) => {
                        let changed = next != watermark;
                        watermark = next;
                        changed || locally_notified
                    }
                    Err(error) => {
                        tracing::warn!(%error, "durable wake poll failed");
                        task_subscriptions.notify_error();
                        continue;
                    }
                };
                if refresh && let Err(error) = task_subscriptions.refresh(&mut connection).await {
                    tracing::warn!(%error, "root wake refresh failed");
                    task_subscriptions.notify_error();
                }
            }
        });
        Arc::new(Self {
            subscriptions,
            waiter_count: AtomicUsize::new(0),
            active,
            shutdown,
            task: Mutex::new(Some(task)),
        })
    }

    fn register(self: &Arc<Self>, root: &str) -> DurableWakeWaiter {
        let wake_rx = self.subscriptions.register(root);
        if self.waiter_count.fetch_add(1, Ordering::AcqRel) == 0 {
            self.active.send_replace(true);
        }
        DurableWakeWaiter {
            poller: Arc::clone(self),
            root: root.into(),
            wake_rx,
        }
    }

    async fn close(&self) {
        self.shutdown.send_replace(true);
        let task = self.task.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

struct DurableWakeWaiter {
    poller: Arc<DurableWakePoller>,
    root: String,
    wake_rx: watch::Receiver<u64>,
}

impl Drop for DurableWakeWaiter {
    fn drop(&mut self) {
        self.poller.subscriptions.unregister(&self.root);
        if self.poller.waiter_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.poller.active.send_if_modified(|active| {
                if self.poller.waiter_count.load(Ordering::Acquire) == 0 {
                    *active = false;
                    true
                } else {
                    false
                }
            });
        }
    }
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
        let pool = codex_state::connect_sqlite_pool_with_retry(
            options,
            DATABASE_MAX_CONNECTIONS,
            Instant::now(),
        )
        .await
        .map_err(|error| match error.downcast::<sqlx::Error>() {
            Ok(error) => StoreError::Sql(error),
            Err(error) => StoreError::CorruptData(format!("connect task database: {error:#}")),
        })?;
        let pool_ref = &pool;
        if let Err(error) =
            codex_state::migrate_tolerating_concurrent_initializers(pool_ref, || async move {
                MIGRATOR.run(pool_ref).await.map_err(Into::into)
            })
            .await
        {
            pool.close().await;
            return Err(match error.downcast::<sqlx::migrate::MigrateError>() {
                Ok(error) => StoreError::Migration(error),
                Err(error) => StoreError::CorruptData(format!("task migration: {error:#}")),
            });
        }
        upgrade_legacy_repository_bindings(&pool).await?;
        let wake_revision = Arc::new(watch::channel(0).0);
        let mut wake_connection = pool.acquire().await?;
        let durable_wake_watermark = durable_wake_watermark(&mut wake_connection).await?;
        let durable_wake_poller = DurableWakePoller::spawn(
            wake_connection,
            Arc::clone(&wake_revision),
            durable_wake_watermark,
        );
        let store = Self {
            pool,
            coordination_root: Arc::new(coordination_root),
            wake_revision,
            durable_wake_poller,
        };
        store.reconcile_task_capsules().await?;
        store.rebuild_wake_streams_if_needed().await?;
        Ok(store)
    }

    pub async fn close(&self) {
        self.durable_wake_poller.close().await;
        self.pool.close().await;
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.pool.is_closed()
    }

    fn notify_wake_waiters(&self) {
        self.wake_revision
            .send_modify(|revision| *revision = revision.saturating_add(1));
    }

    async fn wait_for_wake_events_impl(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> StoreResult<WakeRead> {
        let waiter = self.durable_wake_poller.register(&root_session_id);
        let mut wake_rx = waiter.wake_rx.clone();
        let mut shutdown_rx = self.durable_wake_poller.shutdown.subscribe();
        loop {
            if *shutdown_rx.borrow() {
                return Err(sqlx::Error::PoolClosed.into());
            }
            let current = self
                .read_wake_events_impl(root_session_id.clone(), after_event_id)
                .await?;
            if !current.updated_agents.is_empty() {
                return Ok(current);
            }
            tokio::select! {
                changed = wake_rx.changed() => {
                    changed.map_err(|_| StoreError::from(sqlx::Error::PoolClosed))?;
                }
                _ = shutdown_rx.changed() => return Err(sqlx::Error::PoolClosed.into()),
            }
        }
    }

    pub async fn task_state_revision(&self) -> StoreResult<i64> {
        Ok(
            sqlx::query_scalar("SELECT revision FROM task_state_revision WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Durable, dependency-aware freshness fence for one wait owner.
    pub async fn task_state_revision_for_root(&self, root: &str) -> StoreResult<i64> {
        Ok(sqlx::query_scalar("SELECT revision FROM root_task_state_revision WHERE root_session_id = ?")
            .bind(root)
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or(0))
    }




    async fn create_assignment_impl(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
    ) -> StoreResult<(Assignment, Attempt)> {
        let admitted = self
            .create_assignment_with_admission_impl(repo_root, draft, false, false, None)
            .await?;
        Ok((admitted.assignment, admitted.attempt))
    }

    async fn create_assignment_with_admission_impl(
        &self,
        repo_root: &Path,
        draft: AssignmentDraft,
        selective: bool,
        isolated_integrator_available: bool,
        launch: Option<(String, String, String)>,
    ) -> StoreResult<AdmittedAssignment> {
        let root = repo_root.to_path_buf();
        let (repository, mut assignment, scope_case) = tokio::task::spawn_blocking(move || {
            let repository = repository_identity(&root)?;
            let assignment = draft.normalize(&root)?;
            // Observed before the admission transaction, which must not wait on the filesystem.
            let scope_case = if selective {
                CandidateScopeCase::observe(&root, &assignment.read_scope, &assignment.write_scope)
            } else {
                CandidateScopeCase::default()
            };
            Ok::<_, StoreError>((repository, assignment, scope_case))
        })
        .await
        .map_err(|error| {
            StoreError::CorruptData(format!("assignment normalization task failed: {error}"))
        })??;
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
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_task_workspace_tx(&mut transaction, &repository).await?;
        let dependency_admissions = validate_dependencies_tx(
            &mut transaction,
            assignment.assignment_id,
            Some(&assignment.repository_id),
            &assignment.dependencies,
        )
        .await?;
        validate_architecture_contract_reference_tx(
            &mut transaction,
            &assignment,
            Path::new(&repository.canonical_path),
        )
        .await?;
        let (overlaps, integration_plan) = if selective {
            selective_admission_tx(
                &mut transaction,
                &assignment,
                &scope_case,
                isolated_integrator_available,
            )
            .await?
        } else {
            (
                AdmissionOverlapSummary::default(),
                IntegrationPlan::SingleWriter,
            )
        };
        assignment.integration_plan = integration_plan;
        // Retain the admission-time overlap count in the same durable body so terminal
        // usage metrics can attribute duplicated repository discovery after the
        // transient admission summary is gone.
        assignment.benign_read_overlap_count = overlaps.benign_read_overlap_count;
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
        insert_attempt(&mut transaction, &attempt).await?;
        for dependency in &dependency_admissions {
            sqlx::query("INSERT INTO dependency_admissions (assignment_id, dependency_id, body_json) VALUES (?, ?, ?)")
                .bind(assignment.assignment_id.to_string()).bind(dependency.assignment_id.to_string())
                .bind(encode(dependency)?).execute(&mut *transaction).await?;
        }
        if let Some((agent_path, task_name, owner)) = launch {
            if agent_path.trim().is_empty() || task_name.trim().is_empty() {
                return Err(StoreError::InvalidAssignment(
                    "launch path and name cannot be empty".into(),
                ));
            }
            // A durable owner the in-memory registry no longer reserves (an
            // unavailable cold child, or a launch whose cleanup failed) is the
            // caller's name conflict, not a storage failure. UNIQUE stays the guard.
            let bound = sqlx::query_scalar::<_, String>(
                "SELECT assignment_id FROM agent_task_bindings WHERE root_session_id = ? AND agent_path = ?",
            )
            .bind(&assignment.root_session_id)
            .bind(&agent_path)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(bound) = bound {
                return Err(StoreError::InvalidAssignment(format!(
                    "task name `{task_name}` (agent path {agent_path}) is already bound to assignment {bound} in this root session; choose a different task name"
                )));
            }
            // None is the unstarted phase; binding the actual thread precedes input.
            sqlx::query("INSERT INTO agent_task_bindings (assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at, launch_owner) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?)")
                .bind(assignment.assignment_id.to_string()).bind(attempt.attempt_id.to_string())
                .bind(&assignment.root_session_id).bind(agent_path).bind(task_name)
                .bind(encode(&attempt.created_at)?).bind(encode(&attempt.created_at)?)
                .bind(owner)
                .execute(&mut *transaction).await?;
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

        let capsule_path = task_capsule_path(&self.coordination_root, assignment_id);
        // Capsule publication/recovery share a per-assignment OS lease. Never
        // wait for storage ownership while holding the home-wide SQL writer.
        let lease_deadline = tokio::time::Instant::now() + DATABASE_BUSY_TIMEOUT;
        let capsule_lease = loop {
            if let Some(lease) = try_capsule_lease(capsule_path.with_extension("lock")).await? { break lease; }
            if tokio::time::Instant::now() >= lease_deadline {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "task capsule publication is busy").into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        let staging_path = task_capsule_staging_path(&self.coordination_root, assignment_id);
        let staging_payload = canonical_payload.clone();
        let durable_payload = canonical_payload.clone();
        let pool = self.pool.clone();
        // Reuse Assignment's existing immutable capsule slot as an independent
        // authoritative copy. The worker owns publication AND DB commit through
        // observer cancellation; the sidecar alone is not an attachment receipt.
        tokio::task::spawn_blocking(move || -> StoreResult<Assignment> {
            let _capsule_lease = capsule_lease;
            if let Some(parent) = staging_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if staging_path.try_exists()? {
                return Err(StoreError::CorruptData(format!(
                    "unresolved capsule stage retained at {}", staging_path.display()
                )));
            }
            let writing_path = staging_path.with_extension(format!("{}.writing", AttemptId::new()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&writing_path)?;
            if let Err(error) = file
                .write_all(staging_payload.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(file);
                let _ = std::fs::remove_file(&writing_path);
                return Err(error.into());
            }
            drop(file);
            // Staging/fsync owns only the capsule lease. Acquire the shared
            // database writer afterwards, then recheck all durable authority.
            let authority = tokio::runtime::Handle::current().block_on(async {
                let mut transaction = pool.begin().await?;
                lock_assignment_tx(&mut transaction, assignment_id).await?;
                let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
                validate_capsule_contract_tx(&mut transaction, &assignment, &capsule).await
                    .map_err(|error| StoreError::InvalidTaskCapsule(error.to_string()))?;
                let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
                if attempt.attempt_id != attempt_id || attempt.state != AttemptState::Active {
                    return Err(StoreError::AttemptNotActive(attempt_id));
                }
                if assignment.task_capsule.is_some() || capsule_path.try_exists()? {
                    return Err(StoreError::TaskCapsuleAlreadyAttached(assignment_id));
                }
                Ok::<_, StoreError>((assignment, transaction))
            });
            let (mut assignment, mut transaction) = match authority {
                Ok(authority) => authority,
                Err(error) => {
                    let _ = std::fs::remove_file(&writing_path);
                    return Err(error);
                }
            };
            std::fs::rename(&writing_path, &staging_path)?;
            std::fs::rename(&staging_path, &capsule_path)?;
            tokio::runtime::Handle::current().block_on(async move {
                sqlx::query("UPDATE assignments SET body_json = json_set(body_json, '$.task_capsule', ?) WHERE assignment_id = ?")
                    .bind(durable_payload).bind(assignment_id.to_string())
                    .execute(&mut *transaction).await?;
                transaction.commit().await?;
                Ok::<_, StoreError>(())
            })?;
            assignment.task_capsule = Some(canonical_payload);
            Ok(assignment)
        })
        .await
        .map_err(std::io::Error::other)?
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
        let receipt = load_receipt_tx(&mut transaction, &current_attempt).await?
            .map(|(receipt, _)| receipt);
        let gate_rows =
            sqlx::query("SELECT body_json FROM gates WHERE assignment_id = ? ORDER BY kind")
                .bind(assignment_id.to_string())
                .fetch_all(&mut *transaction)
                .await?;
        let mut gates: Vec<AgentGate> = gate_rows
            .into_iter()
            .map(|row| decode(row.get::<String, _>("body_json").as_str()))
            .collect::<StoreResult<Vec<_>>>()?;
        // Sealed verdicts authorize exactly the attempt recorded in history.
        // This also fails closed for corrections persisted by older runtimes.
        for gate in &mut gates {
            if matches!(gate.kind, GateKind::Review | GateKind::Verification)
                && gate.status.is_sealed() {
                let verdict = sqlx::query_scalar::<_, String>(
                    "SELECT body_json FROM gate_verdicts WHERE attempt_id = ? AND kind = ?",
                )
                .bind(current_attempt.attempt_id.to_string())
                .bind(encode(&gate.kind)?)
                .fetch_optional(&mut *transaction)
                .await?;
                if verdict.as_deref() != Some(encode(gate)?.as_str()) {
                    gate.status = GateStatus::Pending;
                    gate.sealed_at = None;
                    gate.reason = "current attempt has no matching gate verdict".into();
                }
            }
        }
        let validation_calls: Vec<ValidationCall> = sqlx::query(
            "SELECT body_json FROM validation_calls WHERE attempt_id = ? ORDER BY julianday(json_extract(recorded_at, '$')), call_id",
        )
        .bind(current_attempt.attempt_id.to_string())
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .map(|row| decode(row.get::<String, _>("body_json").as_str()))
        .collect::<StoreResult<Vec<_>>>()?;
        let mut observations: Vec<RuntimeObservation> = if limit == 0 {
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
        // Terminal diagnostics are a status projection, not an optional general
        // history tail. Read the latest origin and its delivery recovery in the
        // same snapshot as the attempt/receipt, including when limit is zero.
        let terminal = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM observations WHERE attempt_id = ? AND kind = ?
             AND json_extract(body_json, '$.call_id') NOT LIKE '%:parent-delivery'
             ORDER BY sequence DESC LIMIT 1",
        )
        .bind(current_attempt.attempt_id.to_string())
        .bind(encode(&ObservationKind::HostCompletion)?)
        .fetch_optional(&mut *transaction)
        .await?
        .map(|body| decode::<RuntimeObservation>(&body))
        .transpose()?;
        if let Some(terminal) = terminal {
            let delivery_origin = format!(
                "{}:parent-delivery",
                terminal.call_id.as_deref().unwrap_or_default()
            );
            let delivery = sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM observations WHERE attempt_id = ? AND kind = ?
                 AND json_extract(body_json, '$.call_id') = ? ORDER BY sequence DESC LIMIT 1",
            )
            .bind(current_attempt.attempt_id.to_string())
            .bind(encode(&ObservationKind::HostCompletion)?)
            .bind(delivery_origin)
            .fetch_optional(&mut *transaction)
            .await?
            .map(|body| decode::<RuntimeObservation>(&body))
            .transpose()?;
            // Keep requested history retrievable, but put the current projection
            // last and include it only once. Consumers select this origin, not
            // arbitrary historical failures in the general tail.
            observations.retain(|observation| {
                observation.event_id != terminal.event_id
                    && delivery
                        .as_ref()
                        .is_none_or(|delivery| observation.event_id != delivery.event_id)
            });
            observations.push(terminal);
            observations.extend(delivery);
        } else {
            // Old clean turns had no terminal marker. Preserve a delivery-only
            // recovery fact without inventing a matching completion assessment.
            let delivery = sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM observations WHERE attempt_id = ? AND kind = ?
                 ORDER BY sequence DESC LIMIT 1",
            )
            .bind(current_attempt.attempt_id.to_string())
            .bind(encode(&ObservationKind::HostCompletion)?)
            .fetch_optional(&mut *transaction)
            .await?
            .map(|body| decode::<RuntimeObservation>(&body))
            .transpose()?;
            if let Some(delivery) = delivery {
                observations.retain(|observation| observation.event_id != delivery.event_id);
                observations.push(delivery);
            }
        }
        let epoch = 0; // Legacy field; workspace freshness is no longer tracked.
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
            .map(|row| {
                let state = row.get::<String, _>("state");
                let expires_at = row
                    .get::<Option<String>, _>("lease_expires_at")
                    .map(|value| decode::<chrono::DateTime<Utc>>(&value))
                    .transpose()?;
                Ok::<Option<crate::LeaseState>, StoreError>(workspace_actor_lease_state(
                    &state,
                    expires_at,
                    Utc::now(),
                ))
            })
            .transpose()?
            .flatten();
        let nudge_sent_at = actor_row
            .as_ref()
            .and_then(|row| row.get::<Option<String>, _>("nudge_sent_at"))
            .map(|value| decode::<chrono::DateTime<Utc>>(&value))
            .transpose()?;
        let pending_gates = gates
            .iter()
            .filter(|gate| gate.status == GateStatus::Pending)
            .map(|gate| gate.kind)
            .collect::<Vec<_>>();
        let mut next_required_action = (assignment.admission_origin
            == crate::AssignmentAdmissionOrigin::Typed
            && pending_gates.iter().any(|kind| !kind.is_waivable()))
        .then(|| "resolve pending gates before completion".to_string());
        if assignment.integration_plan == IntegrationPlan::TypedIntegratorRequired {
            next_required_action = Some("required destination-bound integration evidence is unavailable; isolated completion is not integration".into());
        }
        let dependency_admissions = load_dependency_admissions_tx(&mut transaction, assignment_id).await?;
        let dependency_blockers = dependency_authority_blockers_tx(
            &mut transaction, &assignment, &dependency_admissions,
        ).await?;
        let criterion_assessments = source_inspection::assess_with_calls_tx(
            &mut transaction, &assignment, &current_attempt,
            receipt.as_ref().map_or(&[], |receipt| receipt.criterion_results.as_slice()),
            receipt.as_ref().map_or(&[], |receipt| receipt.validation_call_ids.as_slice()), false,
            &validation_calls,
        ).await?;
        if receipt.as_ref().is_some_and(|receipt| receipt.status.is_success())
            && criterion_assessments.iter().any(|assessment| !assessment.satisfied)
        {
            next_required_action = Some("source-inspection evidence is no longer applicable; recover the criterion assessment before consuming this result".into());
        }
        if !dependency_blockers.is_empty() {
            let dependency_action = "prerequisite authority is unavailable or changed; recover dependency_blockers before consuming this result or create a replacement assignment against current prerequisites";
            if let Some(action) = &mut next_required_action {
                action.push_str("; ");
                action.push_str(dependency_action);
            } else {
                next_required_action = Some(dependency_action.into());
            }
        }
        let stale_reason = (!dependency_blockers.is_empty())
            .then(|| "dependency authority no longer matches admission".to_string());
        transaction.commit().await?;
        hydrate_task_capsule(&self.pool, &self.coordination_root, &mut assignment).await?;
        Ok(AgentTask {
            assignment,
            dependency_admissions,
            dependency_blockers,
            current_attempt,
            gates,
            receipt,
            validation_calls,
            criterion_assessments,
            workspace_status: WorkspaceTaskStatus {
                epoch,
                last_progress_at,
                lease_state,
                pending_gates,
                stale_reason,
                next_required_action,
                nudge_sent_at,
            },
            isolation_handoff: None,
            integration_handoffs: Vec::new(),
            observations,
        })
    }

    async fn get_agent_task_authorization_impl(
        &self,
        assignment_id: AssignmentId,
    ) -> StoreResult<AgentTaskAuthorization> {
        let row = sqlx::query(
            r#"
SELECT
    assignments.body_json AS assignment_body_json,
    attempts.attempt_id,
    attempts.assignment_id,
    attempts.ordinal,
    attempts.amendment_json,
    attempts.state,
    attempts.created_at,
    attempts.sealed_at
FROM assignments
JOIN attempts USING (assignment_id)
WHERE assignments.assignment_id = ?
ORDER BY attempts.ordinal DESC
LIMIT 1
            "#,
        )
        .bind(assignment_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::AssignmentNotFound(assignment_id))?;
        let assignment: Assignment = decode(row.get::<String, _>("assignment_body_json").as_str())?;
        let attempt_id = AttemptId::parse(row.get::<String, _>("attempt_id").as_str())?;

        Ok(AgentTaskAuthorization {
            root_session_id: assignment.root_session_id,
            admission_origin: assignment.admission_origin,
            current_attempt: attempt_from_row(attempt_id, &row)?,
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
        if binding.thread_id.is_some() {
            sqlx::query(
                "UPDATE agent_task_bindings SET launch_owner = NULL WHERE assignment_id = ?",
            )
            .bind(binding.assignment_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
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
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        actor.require_root_for(&assignment.root_session_id)?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if !attempt.state.is_terminal() || attempt.sealed_at.is_none() {
            return Err(StoreError::InvalidAssignment(
                "an agent task binding may be removed only after the current attempt is sealed"
                    .to_string(),
            ));
        }
        let deleted = sqlx::query("DELETE FROM agent_task_bindings WHERE assignment_id = ?")
            .bind(assignment_id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
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
        progress: bool,
    ) -> StoreResult<bool> {
        if binding
            .thread_id
            .as_deref()
            .is_none_or(|thread_id| thread_id.trim().is_empty())
        {
            return Ok(false);
        }
        // Reserve the writer before reading. Upgrading a deferred read snapshot to
        // renew the lease fails with SQLITE_BUSY at once instead of honoring busy_timeout.
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
            heartbeat_typed_workspace_actor_tx(&mut transaction, &workspace_id, &binding, progress)
                .await?;
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
        if kind == ObservationKind::HostCompletion {
            return Err(StoreError::InvalidAssignment(
                "host completion observations require the host terminal boundary".into(),
            ));
        }
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
        self.record_validation_call_transaction(call, false).await
    }

    async fn record_validation_call_transaction(
        &self,
        mut call: ValidationCall,
        completed_inspection: bool,
    ) -> StoreResult<()> {
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
        let attempt = load_attempt_tx(&mut transaction, call.attempt_id).await?;
        let current = load_current_attempt_tx(&mut transaction, attempt.assignment_id).await?;
        let attempt_is_active = current.attempt_id == call.attempt_id
            && attempt.state == AttemptState::Active
            && attempt.sealed_at.is_none();
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
            if completed_inspection {
                call.recorded_at = existing.recorded_at;
                call.evidence.start_epoch = existing.evidence.start_epoch;
                call.evidence.end_epoch = existing.evidence.end_epoch;
                call.evidence.lease_expires_at = existing.evidence.lease_expires_at;
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
            call.evidence.start_epoch = existing.evidence.start_epoch;
            call.evidence.end_epoch = attempt_is_active.then_some(existing.evidence.start_epoch);
            call.evidence.lease_expires_at = None;
            if call.evidence.retained_output_ref.is_none() {
                call.evidence.retained_output_ref = existing.evidence.retained_output_ref;
            }
            if call.evidence.output_summary.is_none() {
                call.evidence.output_summary = existing.evidence.output_summary;
            }
            if call.evidence.validation_result.is_none() {
                call.evidence.validation_result = existing.evidence.validation_result;
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
            if !attempt_is_active {
                return Err(StoreError::AttemptNotActive(call.attempt_id));
            }
            if call.status != crate::ValidationCallStatus::Running && !completed_inspection {
                return Err(StoreError::ValidationCallImmutable(call.call_id));
            }
            let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
            if !matches!(
                assignment.role,
                AgentRole::Worker | AgentRole::Verifier | AgentRole::Integrator
            ) {
                return Err(StoreError::InvalidAssignment(format!(
                    "{:?} assignments are not authorized to run validation",
                    assignment.role
                )));
            }
            if !assignment
                .required_evidence
                .iter()
                .any(|requirement| requirement == &call.command_summary)
            {
                return Err(StoreError::InvalidAssignment(format!(
                    "validation command is not an exact required-evidence match: {}",
                    call.command_summary
                )));
            }
            call.evidence.start_epoch = 0;
            call.evidence.end_epoch = completed_inspection.then_some(0);
            call.evidence.lease_expires_at = (!completed_inspection).then(||
                comparison_now() + chrono::Duration::seconds(crate::MAX_VALIDATION_LEASE_SECONDS),
            );
            sqlx::query("INSERT INTO validation_calls (call_id, attempt_id, body_json, status, recorded_at) VALUES (?, ?, ?, ?, ?)")
                .bind(&call.call_id)
                .bind(call.attempt_id.to_string())
                .bind(encode(&call)?)
                .bind(encode(&call.status)?)
                .bind(encode(&call.recorded_at)?)
                .execute(&mut *transaction)
                .await?;
        }
        if call.status.is_terminal()
            && attempt_is_active
            && call.status == crate::ValidationCallStatus::Succeeded
        {
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
                &(progress_at + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS)),
            )?)
            .bind(workspace_id)
            .bind(attempt.attempt_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
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
        _lease_expires_at: chrono::DateTime<Utc>,
    ) -> StoreResult<bool> {
        // Reserve the writer before reading; see heartbeat_typed_workspace_actor_impl.
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
            if !heartbeat_typed_workspace_actor_tx(
                &mut transaction,
                &workspace_id,
                &binding,
                /*progress*/ false,
            )
            .await?
            {
                transaction.commit().await?;
                return Ok(false);
            }
        }
        let lease_expires_at =
            comparison_now() + chrono::Duration::seconds(crate::MAX_VALIDATION_LEASE_SECONDS);
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
        transaction.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    async fn submit_agent_receipt_impl(
        &self,
        attempt_id: AttemptId,
        mut draft: ReceiptDraft,
        _review_reason: Option<String>,
        host_legacy_outcome: bool,
        host_completion: Option<(String, String)>,
    ) -> StoreResult<AgentReceipt> {
        if let Some((origin, summary)) = &host_completion {
            validate_host_completion_observation(origin, summary, None)?;
        }
        if draft.summary.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "receipt summary cannot be empty".to_string(),
            ));
        }
        let capsule_snapshot = if draft.status == AgentStatusClaim::Completed && !host_legacy_outcome {
            let body: String = sqlx::query_scalar("SELECT assignments.body_json FROM assignments JOIN attempts USING (assignment_id) WHERE attempt_id = ?")
                .bind(attempt_id.to_string()).fetch_optional(&self.pool).await?
                .ok_or(StoreError::AttemptNotFound(attempt_id))?;
            let mut assignment: Assignment = decode(&body)?;
            let lease = try_capsule_lease(task_capsule_path(&self.coordination_root, assignment.assignment_id).with_extension("lock")).await?
                .ok_or_else(|| StoreError::InvalidTaskCapsule("capsule publication or recovery is still active".into()))?;
            hydrate_task_capsule(&self.pool, &self.coordination_root, &mut assignment).await?;
            Some((assignment, lease))
        } else { None };
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
        if capsule_snapshot.as_ref().is_some_and(|(snapshot, _)| snapshot.task_capsule != assignment.task_capsule) {
            return Err(StoreError::InvalidTaskCapsule("capsule authority changed before completion".into()));
        }
        if draft.status == AgentStatusClaim::Completed {
            let admissions = load_dependency_admissions_tx(&mut transaction, assignment.assignment_id).await?;
            let blockers = dependency_authority_blockers_tx(&mut transaction, &assignment, &admissions).await?;
            if !blockers.is_empty() {
                return Err(StoreError::DependencyBlocked { blockers });
            }
        }
        if host_legacy_outcome {
            if !matches!(
                assignment.admission_origin,
                crate::AssignmentAdmissionOrigin::LegacyMessage { .. }
            ) || assignment.workspace_strategy != crate::WorkspaceStrategy::Shared
            {
                return Err(StoreError::InvalidAssignment(
                    "host turn outcomes require a shared plain-message assignment".to_string(),
                ));
            }
            // A final response establishes turn completion, not acceptance or validation.
            // This also handles persisted legacy assignments whose synthetic required_evidence
            // was prose rather than an executable validation obligation.
            draft.criterion_results = effective_criteria(&assignment, attempt.amendment.as_ref())
                .iter()
                .map(|criterion| crate::CriterionResult {
                    criterion_id: criterion.id.clone(),
                    status: crate::CriterionStatus::NotRun,
                    evidence: None,
                    evidence_ref: None,
                })
                .collect();
        } else {
            validate_criterion_results(&assignment, attempt.amendment.as_ref(), &draft)?;
            let assessments = source_inspection::assess_tx(
                &mut transaction, &assignment, &attempt, &draft.criterion_results,
                &draft.validation_call_ids, false,
            ).await?;
            source_inspection::require_passed(&assessments, &draft.criterion_results, draft.status)?;
        }
        if draft.status == AgentStatusClaim::Completed && !host_legacy_outcome {
            validate_declared_changes_tx(&mut transaction, &assignment, &mut draft).await?;
        }
        let evidence_epoch = 0;
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
            // Seal-time bytes do not identify the inputs the producer observed.
            read_scope_fingerprint: None,
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
        if host_legacy_outcome
            || !receipt.status.is_success()
            || pending_gate_count(&mut transaction, attempt.assignment_id).await? == 0
        {
            finish_task_actor(&mut transaction, attempt.assignment_id).await?;
        }
        if let Some((origin, summary)) = host_completion {
            append_host_completion_tx(
                &mut transaction, &assignment, attempt_id, origin, summary, None,
            ).await?;
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
        host_legacy_followup: bool,
    ) -> StoreResult<(Attempt, Option<AgentTaskBinding>)> {
        actor.require_root()?;
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        actor.require_root_for(&assignment.root_session_id)?;
        amendment.validate_for_assignment(&assignment)?;
        if host_legacy_followup
            && (!matches!(
                assignment.admission_origin,
                crate::AssignmentAdmissionOrigin::LegacyMessage { .. }
            ) || assignment.workspace_strategy != crate::WorkspaceStrategy::Shared)
        {
            return Err(StoreError::InvalidAssignment(
                "host follow-up attempts require a shared plain-message assignment".to_string(),
            ));
        }
        if assignment.role != AgentRole::Worker
            && !(host_legacy_followup && assignment.role == AgentRole::Explorer)
        {
            return Err(StoreError::WorkerCorrectionRequired(assignment_id));
        }
        let current = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if !host_legacy_followup && current.ordinal != 0 {
            return Err(StoreError::AmendmentLimitReached(assignment_id));
        }
        if !current.state.is_terminal() {
            if host_legacy_followup {
                return Ok((current, None));
            }
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
        if !host_legacy_followup && !changes_requested {
            return Err(StoreError::InvalidAssignment(
                "a correction attempt requires a changes_requested review gate".to_string(),
            ));
        }
        let next = Attempt {
            attempt_id: AttemptId::new(),
            assignment_id,
            ordinal: current
                .ordinal
                .checked_add(1)
                .ok_or(StoreError::AmendmentLimitReached(assignment_id))?,
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
        if !host_legacy_followup {
            let gate_now = Utc::now();
            let gate_epoch = 0;
            for kind in [GateKind::Review, GateKind::Verification] {
            let correction_gate = AgentGate {
                assignment_id,
                kind,
                status: GateStatus::Pending,
                reason: "correction attempt requires a new current-attempt verdict".to_string(),
                waiver_reason: None,
                evidence_epoch: gate_epoch,
                updated_at: gate_now,
                sealed_at: None,
            };
            sqlx::query("INSERT INTO gates (assignment_id, kind, status, body_json, updated_at, sealed_at) VALUES (?, ?, ?, ?, ?, NULL) ON CONFLICT(assignment_id, kind) DO UPDATE SET status = excluded.status, body_json = excluded.body_json, updated_at = excluded.updated_at, sealed_at = NULL")
            .bind(assignment_id.to_string())
            .bind(encode(&kind)?)
            .bind(encode(&GateStatus::Pending)?)
            .bind(encode(&correction_gate)?)
            .bind(encode(&gate_now)?)
            .execute(&mut *transaction)
            .await?;
            }
        }
        append_observation_tx(
            &mut transaction,
            &assignment,
            next.attempt_id,
            ObservationKind::Accepted,
            if host_legacy_followup {
                "plain-message follow-up turn accepted"
            } else {
                "single correction attempt accepted"
            }
            .to_string(),
            None,
        )
        .await?;
        // Read the committed binding inside the same transaction. No fallible
        // database work may be required to publish a successful amendment.
        let binding = sqlx::query("SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id, bound_at, updated_at FROM agent_task_bindings WHERE assignment_id = ?")
            .bind(assignment_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .map(|row| binding_from_row(&row))
            .transpose()?;
        transaction.commit().await?;
        Ok((next, binding))
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
        actor.require_root_for(&assignment.root_session_id)?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.state.is_terminal() || attempt.sealed_at.is_some() {
            return Err(StoreError::AttemptSealed(attempt.attempt_id));
        }
        let criterion_results = effective_criteria(&assignment, attempt.amendment.as_ref())
            .iter()
            .map(|criterion| crate::CriterionResult {
                evidence_ref: None,
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
            evidence_epoch: 0,
            // A recovery receipt records an abandoned attempt, never a result
            // whose inputs were observed, so it is never reusable.
            read_scope_fingerprint: None,
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
        finish_task_actor(&mut transaction, assignment_id).await?;
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
        actor: TaskActor,
        assignment_id: AssignmentId,
        expected_attempt_id: AttemptId,
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
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        require_gate_actor_tx(&mut transaction, actor.clone(), &assignment, kind).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if attempt.attempt_id != expected_attempt_id {
            return Err(StoreError::AttemptNotActive(expected_attempt_id));
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
                let current_verdict = sqlx::query_scalar::<_, String>(
                    "SELECT body_json FROM gate_verdicts WHERE attempt_id = ? AND kind = ?",
                ).bind(attempt.attempt_id.to_string()).bind(encode(&kind)?)
                    .fetch_optional(&mut *transaction).await?;
                if !matches!(kind, GateKind::Review | GateKind::Verification)
                    || current_verdict.as_deref() == Some(existing_json.as_str()) {
                    return Err(StoreError::GateAlreadySealed { gate: kind.to_string() });
                }
                // An authorized current verdict can replace a historical gate,
                // but immutable current-attempt history cannot be rewritten.
            }
        }
        if kind == GateKind::Verification && status == GateStatus::Passed
            && let TaskActor::Attempt(actor_attempt_id) = &actor
        {
            let actor_attempt = load_attempt_tx(&mut transaction, *actor_attempt_id).await?;
            let actor_assignment = load_assignment_tx(&mut transaction, actor_attempt.assignment_id).await?;
            let actor_receipt = load_receipt_tx(&mut transaction, &actor_attempt).await?.map(|(receipt, _)| receipt);
            let assessments = source_inspection::assess_tx(
                &mut transaction, &actor_assignment, &actor_attempt,
                actor_receipt.as_ref().map_or(&[], |receipt| receipt.criterion_results.as_slice()),
                actor_receipt.as_ref().map_or(&[], |receipt| receipt.validation_call_ids.as_slice()),
                actor_receipt.is_none(),
            ).await?;
            source_inspection::require_passed(&assessments, &[], AgentStatusClaim::Completed)?;
        }
        let now = Utc::now();
        let evidence_epoch = 0;
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
        if attempt.state == AttemptState::Completed
            && (matches!(
                (kind, status),
                (GateKind::Review, GateStatus::Failed)
                    | (GateKind::Verification, GateStatus::Failed | GateStatus::ChangesRequested)
            ) || kind == GateKind::Review
                && status == GateStatus::ChangesRequested)
        {
            transition_attempt_to_needs_main_tx(&mut transaction, &attempt).await?;
            append_observation_tx(
                &mut transaction,
                &assignment,
                attempt.attempt_id,
                ObservationKind::NeedsMain,
                format!("{kind} gate requires main intervention: {}", gate.reason),
                None,
            )
            .await?;
        }
        finish_successful_task_if_unblocked_tx(&mut transaction, assignment_id).await?;
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
        actor.require_root_for(&assignment.root_session_id)?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        let existing_json = sqlx::query_scalar::<_, String>(
            "SELECT body_json FROM gates WHERE assignment_id = ? AND kind = ?",
        )
        .bind(assignment_id.to_string())
        .bind(encode(&kind)?)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| StoreError::GateNotPending {
            gate: kind.to_string(),
        })?;
        let existing: AgentGate = decode(&existing_json)?;
        if existing.status != GateStatus::Pending {
            return Err(StoreError::GateAlreadySealed {
                gate: kind.to_string(),
            });
        }
        let now = Utc::now();
        let evidence_epoch = 0;
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
        finish_successful_task_if_unblocked_tx(&mut transaction, assignment_id).await?;
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
            if let Some(event_id) = after_event_id {
                return Err(StoreError::InvalidWakeWatermark(event_id.to_string()));
            }
            return Ok(WakeRead {
                status: WakeReadStatus::NoStream,
                reason: None,
                updated_agents: Vec::new(),
                latest_event_id: None,
                lost_to_retention_count: 0,
                remaining_count: 0,
                truncated_count: 0,
                timed_out: false,
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
        // Cursors and payloads come from the same append-only authority. A
        // damaged derived cache must never manufacture an unusable watermark.
        let rows = sqlx::query("SELECT body_json, assignment_id, attempt_id, wake_event_id FROM observations WHERE root_session_id = ? AND wake_sequence >= ? ORDER BY wake_sequence LIMIT ?")
            .bind(&root_session_id)
            .bind(start_sequence)
            .bind(MAX_WAKE_EVENTS_PER_READ as i64)
            .fetch_all(&mut *transaction)
            .await?;
        let updated_agents = rows
            .into_iter()
            .map(|row| wake_event_from_observation_row(&row))
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
            status: if updated_agents.is_empty() {
                WakeReadStatus::Empty
            } else {
                WakeReadStatus::EventsAvailable
            },
            reason,
            timed_out: false,
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
        // Existing cursors are read-only observations. Do not wait for a
        // maintenance writer before the wait owner's deadline can start.
        if let Some(row) = sqlx::query(
            "SELECT event_id FROM automatic_wake_cursors
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .fetch_optional(&self.pool)
        .await?
        {
            return row
                .get::<Option<String>, _>("event_id")
                .map(|value| WakeEventId::parse(&value))
                .transpose();
        }
        let mut transaction = self.pool.begin().await?;
        // Reserve the writer before sampling the initial watermark. Upgrading a
        // read snapshot after concurrent mailbox writes fails with SQLITE_BUSY_SNAPSHOT.
        sqlx::query("UPDATE automatic_wake_cursors SET updated_at = updated_at WHERE root_session_id = ? AND consuming_agent_path = ?")
            .bind(&root_session_id)
            .bind(&consuming_agent_path)
            .execute(&mut *transaction)
            .await?;
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
            "SELECT wake_event_id AS event_id FROM observations
             WHERE root_session_id = ?
               AND wake_sequence >= (SELECT retained_from_sequence FROM wake_streams WHERE root_session_id = ?)
             ORDER BY wake_sequence DESC
             LIMIT 1 OFFSET ?",
        )
        .bind(&root_session_id)
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
        delivery_json: Option<String>,
    ) -> StoreResult<bool> {
        async fn wake_event_sequence_tx(
            transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
            root_session_id: &str,
            event_id: WakeEventId,
        ) -> StoreResult<i64> {
            let Some(row) = sqlx::query(
                "SELECT root_session_id, wake_sequence
                 FROM observations WHERE wake_event_id = ?",
            )
            .bind(event_id.to_string())
            .fetch_optional(&mut **transaction)
            .await?
            else {
                return Err(StoreError::InvalidWakeWatermark(event_id.to_string()));
            };
            if row.get::<String, _>("root_session_id") != root_session_id {
                return Err(StoreError::InvalidWakeWatermark(event_id.to_string()));
            }
            Ok(row.get::<i64, _>("wake_sequence"))
        }

        if root_session_id.trim().is_empty() || consuming_agent_path.trim().is_empty() {
            return Err(StoreError::InvalidAssignment(
                "automatic wake cursor keys cannot be empty".to_string(),
            ));
        }
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "UPDATE automatic_wake_cursors SET updated_at = updated_at
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .execute(&mut *transaction)
        .await?;
        let Some(row) = sqlx::query(
            "SELECT event_id FROM automatic_wake_cursors
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .fetch_optional(&mut *transaction)
        .await?
        else {
            transaction.commit().await?;
            return Ok(false);
        };
        let stored = row.get::<Option<String>, _>("event_id");
        let expected = expected.map(|value| value.to_string());
        if stored != expected {
            transaction.commit().await?;
            return Ok(false);
        }
        let next_sequence =
            wake_event_sequence_tx(&mut transaction, &root_session_id, next).await?;
        if let Some(stored) = stored {
            let current = WakeEventId::parse(&stored)?;
            let current_sequence =
                wake_event_sequence_tx(&mut transaction, &root_session_id, current).await?;
            if next_sequence < current_sequence {
                return Err(StoreError::WakeWatermarkRegression {
                    current: current_sequence,
                    next: next_sequence,
                });
            }
        }
        let changed = sqlx::query(
            "UPDATE automatic_wake_cursors
             SET event_id = ?, updated_at = ?, delivery_json = ?
             WHERE root_session_id = ? AND consuming_agent_path = ?",
        )
        .bind(next.to_string())
        .bind(encode(&Utc::now())?)
        .bind(delivery_json)
        .bind(&root_session_id)
        .bind(&consuming_agent_path)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
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
        unstarted_owner: Option<Option<String>>,
    ) -> StoreResult<NonproductiveRecovery> {
        let mut transaction = self.pool.begin().await?;
        lock_assignment_tx(&mut transaction, assignment_id).await?;
        let assignment = load_assignment_tx(&mut transaction, assignment_id).await?;
        let attempt = load_current_attempt_tx(&mut transaction, assignment_id).await?;
        if let Some(owner) = &unstarted_owner {
            let eligible: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_task_bindings WHERE assignment_id = ? AND attempt_id = ? AND thread_id IS NULL AND launch_owner IS ?)")
                .bind(assignment_id.to_string()).bind(attempt.attempt_id.to_string())
                .bind(owner).fetch_one(&mut *transaction).await?;
            if eligible == 0 {
                return Ok(NonproductiveRecovery::NotEligible);
            }
        }
        if attempt.state != AttemptState::Active || attempt.sealed_at.is_some() {
            transaction.commit().await?;
            return Ok(NonproductiveRecovery::NotEligible);
        }
        let workspace_id = assignment_workspace_id_tx(&mut transaction, assignment_id).await?;
        let now = comparison_now();
        let recovery_threshold_seconds = u64::try_from(
            now.signed_duration_since(no_progress_before)
                .num_seconds()
                .max(0),
        )
        .unwrap_or(u64::MAX);
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
                recovery_threshold_seconds,
                recovery_policy_version: crate::NONPRODUCTIVE_RECOVERY_POLICY_VERSION,
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
        let current_epoch = 0;
        for body in expired_calls {
            let mut call: ValidationCall = decode(&body)?;
            call.status = ValidationCallStatus::Cancelled;
            call.evidence.end_epoch = Some(current_epoch);
            call.evidence.lease_expires_at = None;
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
        }

        let criterion_results = effective_criteria(&assignment, attempt.amendment.as_ref())
            .iter()
            .map(|criterion| crate::CriterionResult {
                evidence_ref: None,
                criterion_id: criterion.id.clone(),
                status: CriterionStatus::NotRun,
                evidence: None,
            })
            .collect();
        let receipt = AgentReceipt {
            assignment_id,
            attempt_id: attempt.attempt_id,
            status: AgentStatusClaim::Abandoned,
            summary: format!(
                "assignment abandoned after {recovery_threshold_seconds} seconds without meaningful progress (recovery policy v{})",
                crate::NONPRODUCTIVE_RECOVERY_POLICY_VERSION
            ),
            criterion_results,
            declared_changes: Vec::new(),
            validation_call_ids: Vec::new(),
            blockers: Vec::new(),
            risks: vec![format!(
                "nonproductive_recovery_policy_version={}",
                crate::NONPRODUCTIVE_RECOVERY_POLICY_VERSION
            )],
            next_action: None,
            architecture_contract: None,
            evidence_epoch: current_epoch,
            // An abandoned assignment produced no observed result to reuse.
            read_scope_fingerprint: None,
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
        finish_task_actor(&mut transaction, assignment_id).await?;
        append_observation_tx(
            &mut transaction,
            &assignment,
            attempt.attempt_id,
            ObservationKind::Abandoned,
            "nonproductive assignment recovered by the typed heartbeat watcher".to_string(),
            None,
        )
        .await?;

        if let Some(owner) = unstarted_owner {
            sqlx::query("DELETE FROM agent_task_bindings WHERE assignment_id = ? AND attempt_id = ? AND thread_id IS NULL AND launch_owner IS ?")
                .bind(assignment_id.to_string()).bind(attempt.attempt_id.to_string())
                .bind(owner).execute(&mut *transaction).await?;
        }

        transaction.commit().await?;
        Ok(NonproductiveRecovery::Recovered {
            receipt: Box::new(receipt),
            productivity: ProductivitySummary {
                active_owned_operation_count: 0,
                cancelled_expired_operation_count,
                recovery_threshold_seconds,
                recovery_policy_version: crate::NONPRODUCTIVE_RECOVERY_POLICY_VERSION,
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

    async fn reconcile_task_capsules(&self) -> StoreResult<()> {
        let capsule_dir = self.coordination_root.join("task_capsules");
        tokio::fs::create_dir_all(&capsule_dir).await?;
        let mut entries = tokio::fs::read_dir(&capsule_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let is_stage = name.starts_with('.') && name.ends_with(".staged.json");
            if !is_stage && (name.starts_with('.') || !name.ends_with(".json")) {
                continue;
            }
            let stem = if is_stage {
                name.strip_prefix('.').and_then(|name| name.strip_suffix(".staged.json"))
            } else { name.strip_suffix(".json") };
            let Some(stem) = stem else { continue; };
            if !is_stage {
                // A committed copy is immutable (0031) and every read checks its
                // sidecar against it, so only stages and uncommitted publications
                // are crash leftovers to settle here.
                let committed = sqlx::query_scalar::<_, i64>(
                    "SELECT 1 FROM assignments WHERE assignment_id = ? AND CASE WHEN json_valid(body_json) THEN json_extract(body_json, '$.task_capsule') END IS NOT NULL",
                )
                .bind(stem)
                .fetch_optional(&self.pool)
                .await?;
                if committed.is_some() {
                    continue;
                }
            }
            let Some(_capsule_lease) = try_capsule_lease(capsule_dir.join(format!("{stem}.lock"))).await? else {
                // A live publisher owns recovery; unrelated roots need not wait.
                continue;
            };
            // Each incomplete publication is isolated. Retain its bytes and
            // make the affected assignment fail closed, not the entire store.
            let mut invalid_payload = false;
            let result: StoreResult<()> = async {
                let payload = match tokio::fs::read_to_string(&path).await {
                    Ok(payload) => payload,
                    Err(error) => {
                        invalid_payload = error.kind() == std::io::ErrorKind::InvalidData;
                        return Err(error.into());
                    }
                };
                let capsule: TaskCapsuleV1 = match serde_json::from_str(&payload) {
                    Ok(capsule) => capsule,
                    Err(error) => {
                        invalid_payload = true;
                        return Err(error.into());
                    }
                };
                if capsule.schema_version != 1
                    || !is_canonical_persisted_task_capsule(&capsule, &payload)?
                {
                    invalid_payload = true;
                    return Err(StoreError::CorruptData("invalid or noncanonical capsule".into()));
                }
                // Acquire payloads before the shared writer. Another publisher
                // holds that writer through commit, so revalidate its durable
                // authority after admission instead of trusting this snapshot.
                let final_payload = if is_stage {
                    match tokio::fs::read(task_capsule_path(&self.coordination_root, capsule.assignment_id)).await {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    None
                };
                let mut transaction = self.pool.begin().await?;
                let assignment = match load_assignment_tx(&mut transaction, capsule.assignment_id).await {
                    Ok(assignment) => assignment,
                    Err(error) => {
                        invalid_payload = matches!(error, StoreError::AssignmentNotFound(_));
                        return Err(error);
                    }
                };
                if let Err(error) = validate_capsule_contract_tx(&mut transaction, &assignment, &capsule).await {
                    invalid_payload = matches!(error, StoreError::CorruptData(_) | StoreError::AttemptNotFound(_));
                    return Err(error);
                }
                let expected_path = if is_stage {
                    task_capsule_staging_path(&self.coordination_root, capsule.assignment_id)
                } else { task_capsule_path(&self.coordination_root, capsule.assignment_id) };
                if path != expected_path || capsule.integration_plan != assignment.integration_plan {
                    invalid_payload = true;
                    return Err(StoreError::CorruptData("capsule scope or filename mismatch".into()));
                }
                if assignment.task_capsule.as_ref().is_some_and(|expected| expected != &payload) {
                    invalid_payload = true;
                    return Err(StoreError::CorruptData("capsule conflicts with authoritative assignment copy".into()));
                }
                transaction.rollback().await?;
                if is_stage {
                    let final_path = task_capsule_path(&self.coordination_root, capsule.assignment_id);
                    if let Some(final_payload) = final_payload {
                        if final_payload != payload.as_bytes() {
                            invalid_payload = true;
                            return Err(StoreError::CorruptData("capsule stage conflicts with published bytes".into()));
                        }
                        tokio::fs::remove_file(&path).await?;
                    } else {
                        tokio::fs::rename(&path, final_path).await?;
                    }
                }
                let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
                let current = load_assignment_tx(&mut transaction, capsule.assignment_id).await?;
                validate_capsule_contract_tx(&mut transaction, &current, &capsule).await?;
                if current.task_capsule.as_ref().is_some_and(|expected| expected != &payload) {
                    return Err(StoreError::CorruptData("capsule authority changed during recovery".into()));
                }
                if current.task_capsule.is_none() {
                    sqlx::query("UPDATE assignments SET body_json = json_set(body_json, '$.task_capsule', ?) WHERE assignment_id = ?")
                        .bind(&payload).bind(capsule.assignment_id.to_string())
                        .execute(&mut *transaction).await?;
                }
                transaction.commit().await?;
                Ok(())
            }.await;
            if let Err(error) = result {
                // A sidecar's access failure is assignment-local, just like
                // payload damage. Never erase it or certify that task as read.
                // Database failures still invalidate the initialization.
                if matches!(error, StoreError::Io(_)) && !invalid_payload {
                    tracing::warn!(path = %path.display(), %error, "inaccessible task capsule retained; its assignment remains unavailable");
                    continue;
                }
                if !invalid_payload {
                    return Err(error);
                }
                if !is_stage {
                    // One damaged published capsule must not fail every store
                    // open, and with it every session start. Its bytes stay in
                    // place: attach cannot substitute them, nothing is
                    // backfilled, and each read of that assignment fails closed.
                    tracing::warn!(path = %path.display(), %error, "invalid published task capsule retained; its assignment fails closed and unrelated tasks remain available");
                    continue;
                }
                // Keep one diagnostic payload per staging name without making
                // it a candidate on every subsequent startup. Never overwrite
                // an earlier quarantined payload.
                let quarantine = path.with_extension("invalid");
                let quarantine_result: std::io::Result<()> = async {
                    if !tokio::fs::try_exists(&quarantine).await? {
                        tokio::fs::rename(&path, &quarantine).await?;
                    }
                    Ok(())
                }.await;
                if let Err(error) = quarantine_result {
                    // Another initializer may have moved it already, or its
                    // access mode may permit reads but forbid rename. Neither
                    // case invalidates unrelated assignments.
                    tracing::warn!(path = %path.display(), %error, "invalid task capsule could not be quarantined; unrelated tasks remain available");
                }
                tracing::warn!(path = %path.display(), %error, "invalid unpublished task capsule retained; unrelated tasks remain available");
            }
        }
        Ok(())
    }

    async fn rebuild_wake_streams_if_needed(&self) -> StoreResult<()> {
        if self.wake_streams_are_current().await? {
            return Ok(());
        }
        self.rebuild_wake_streams().await
    }

    async fn wake_streams_are_current(&self) -> StoreResult<bool> {
        let mismatch = sqlx::query_scalar::<_, i64>(
            r#"
            WITH expected_streams AS (
                SELECT observations.root_session_id,
                    MAX(observations.wake_sequence) + 1 AS next_sequence,
                    MAX(MAX(observations.wake_sequence) - ? + 1, 1) AS retained_from_sequence,
                    (SELECT latest.wake_event_id FROM observations AS latest
                     WHERE latest.root_session_id = observations.root_session_id
                     ORDER BY latest.wake_sequence DESC LIMIT 1) AS latest_event_id
                FROM observations GROUP BY observations.root_session_id
            )
            SELECT EXISTS (
                SELECT 1 FROM expected_streams
                LEFT JOIN wake_streams USING (root_session_id)
                WHERE wake_streams.root_session_id IS NULL
                    OR wake_streams.next_sequence != expected_streams.next_sequence
                    OR wake_streams.retained_from_sequence != expected_streams.retained_from_sequence
                    OR wake_streams.latest_event_id IS NOT expected_streams.latest_event_id
                UNION ALL
                SELECT 1 FROM wake_streams
                LEFT JOIN expected_streams USING (root_session_id)
                WHERE expected_streams.root_session_id IS NULL
            )
            "#,
        )
        .bind(MAX_WAKE_EVENTS_PER_ROOT as i64)
        .fetch_one(&self.pool)
        .await?;
        // Payload integrity is checked against observations by each root's
        // bounded read; there is no second payload cache to authenticate.
        Ok(mismatch == 0)
    }

    async fn rebuild_wake_streams(&self) -> StoreResult<()> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM wake_streams")
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO wake_streams (root_session_id, next_sequence, retained_from_sequence, latest_event_id)
             SELECT observations.root_session_id, MAX(wake_sequence) + 1,
                 MAX(MAX(wake_sequence) - ? + 1, 1),
                 (SELECT latest.wake_event_id FROM observations AS latest
                  WHERE latest.root_session_id = observations.root_session_id
                  ORDER BY latest.wake_sequence DESC LIMIT 1)
             FROM observations GROUP BY observations.root_session_id",
        )
        .bind(MAX_WAKE_EVENTS_PER_ROOT as i64)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }
}

impl LocalAgentTaskStore {
    pub fn create_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
    ) -> TaskStoreFuture<'a, (Assignment, Attempt)> {
        Box::pin(async move {
            let result = self.create_assignment_impl(repo_root, draft).await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Read-only early reuse check. Final admitted creation still repeats the
    /// same predicate under its transaction to handle simultaneous misses.
    pub fn reusable_explorer_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
    ) -> TaskStoreFuture<'a, Option<AssignmentId>> {
        Box::pin(async move {
            let root = repo_root.to_path_buf();
            let (repository, mut assignment, scope_case) = tokio::task::spawn_blocking(move || {
                let repository = repository_identity(&root)?;
                let assignment = draft.normalize(&root)?;
                let scope_case = CandidateScopeCase::observe(
                    &root,
                    &assignment.read_scope,
                    &assignment.write_scope,
                );
                Ok::<_, StoreError>((repository, assignment, scope_case))
            })
            .await
            .map_err(|error| StoreError::CorruptData(error.to_string()))??;
            assignment.validate_selective_role_contract()?;
            if assignment.repository_id != repository.id {
                return Err(StoreError::InvalidScope(
                    "repository root changed during reuse lookup".to_string(),
                ));
            }
            assignment.workspace_id = repository.workspace_id.clone();
            if assignment.role != AgentRole::Explorer
                || assignment.workspace_strategy == WorkspaceStrategy::Isolated
            {
                return Ok(None);
            }
            let mut transaction = self.pool.begin().await?;
            validate_dependencies_tx(
                &mut transaction,
                assignment.assignment_id,
                Some(&assignment.repository_id),
                &assignment.dependencies,
            )
            .await?;
            let result =
                selective_admission_tx(&mut transaction, &assignment, &scope_case, false).await;
            transaction.rollback().await?;
            match result {
                // Only an investigation still in flight can be shared. Sealed
                // receipts have no binding to the inputs the producer observed.
                Err(StoreError::AdmissionRejected {
                    reason: AdmissionRejectionReason::DuplicateExplorerInvestigation,
                    reusable_assignment_id,
                }) => Ok(reusable_assignment_id),
                Ok(_) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    pub fn create_admitted_assignment_for_agent<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
        isolated_integrator_available: bool,
        agent_path: String,
        task_name: String,
        owner: String,
    ) -> TaskStoreFuture<'a, AdmittedAssignment> {
        Box::pin(async move {
            let result = self
                .create_assignment_with_admission_impl(
                    repo_root,
                    draft,
                    true,
                    isolated_integrator_available,
                    Some((agent_path, task_name, owner)),
                )
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub async fn retain_terminal_outcome(
        &self,
        binding: crate::AgentTaskBinding,
        turn_id: String,
        status_json: String,
        event_json: String,
    ) -> StoreResult<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = load_current_attempt_tx(&mut tx, binding.assignment_id).await?;
        let assignment = load_assignment_tx(&mut tx, binding.assignment_id).await?;
        if current.attempt_id != binding.attempt_id
            || assignment.root_session_id != binding.root_session_id
        {
            return Ok(false);
        }
        let existing = sqlx::query(
            "SELECT binding_json, status_json, event_json FROM pending_terminal_outcomes WHERE attempt_id = ? AND turn_id = ?",
        )
        .bind(binding.attempt_id.to_string())
        .bind(&turn_id)
        .fetch_optional(&mut *tx)
        .await?;
        let encoded = encode(&binding)?;
        let prior_binding = sqlx::query_scalar::<_, String>(
            "SELECT binding_json FROM pending_terminal_outcomes WHERE attempt_id = ? LIMIT 1",
        ).bind(binding.attempt_id.to_string()).fetch_optional(&mut *tx).await?;
        if let Some(prior) = prior_binding {
            let mut prior: AgentTaskBinding = decode(&prior)?;
            prior.updated_at = binding.updated_at;
            prior.bound_at = binding.bound_at;
            if prior != binding {
                return Err(StoreError::CorruptData("terminal outcome binding changed across turns".into()));
            }
        }
        if let Some(existing) = existing {
            let mut prior: AgentTaskBinding = decode(&existing.get::<String, _>("binding_json"))?;
            prior.updated_at = binding.updated_at;
            prior.bound_at = binding.bound_at;
            if prior != binding
                || existing.get::<String, _>("status_json") != status_json
                || existing.get::<String, _>("event_json") != event_json
            {
                return Err(StoreError::CorruptData(
                    "terminal outcome identity has a conflicting binding or payload".into(),
                ));
            }
        }
        sqlx::query("INSERT INTO pending_terminal_outcomes (attempt_id, assignment_id, binding_json, turn_id, status_json, event_json) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(attempt_id, turn_id) DO NOTHING")
            .bind(binding.attempt_id.to_string()).bind(binding.assignment_id.to_string())
            .bind(encoded).bind(turn_id).bind(status_json).bind(event_json).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn pending_terminal_outcomes(
        &self,
        root: &str,
    ) -> StoreResult<Vec<StoreResult<PendingTerminalOutcome>>> {
        let rows = sqlx::query("SELECT p.* FROM pending_terminal_outcomes p JOIN assignments a USING (assignment_id) WHERE a.root_session_id = ? AND p.acknowledged = 0 AND NOT EXISTS (SELECT 1 FROM pending_terminal_outcomes newer WHERE newer.attempt_id = p.attempt_id AND newer.sequence > p.sequence) ORDER BY p.sequence")
            .bind(root).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let binding: AgentTaskBinding =
                    decode(row.get::<String, _>("binding_json").as_str())?;
                if binding.root_session_id != root
                    || binding.assignment_id.to_string() != row.get::<String, _>("assignment_id")
                    || binding.attempt_id.to_string() != row.get::<String, _>("attempt_id")
                {
                    return Err(StoreError::CorruptData(
                        "pending terminal binding identity mismatch".into(),
                    ));
                }
                Ok(PendingTerminalOutcome {
                    binding,
                    turn_id: row.get("turn_id"),
                    status_json: row.get("status_json"),
                    event_json: row.get("event_json"),
                })
            })
            .collect())
    }

    pub async fn acknowledge_terminal_outcome(&self, attempt: AttemptId, turn_id: &str) -> StoreResult<()> {
        sqlx::query("UPDATE pending_terminal_outcomes SET acknowledged = 1 WHERE attempt_id = ? AND turn_id = ?")
            .bind(attempt.to_string())
            .bind(turn_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn unstarted_assignments(
        &self,
        root: &str,
    ) -> StoreResult<Vec<(AssignmentId, Option<String>)>> {
        let rows = sqlx::query("SELECT a.assignment_id, b.launch_owner FROM assignments a JOIN attempts t USING (assignment_id) LEFT JOIN agent_task_bindings b USING (assignment_id) WHERE a.root_session_id = ? AND t.state = ? AND t.ordinal = (SELECT MAX(current.ordinal) FROM attempts current WHERE current.assignment_id = a.assignment_id) AND b.thread_id IS NULL")
            .bind(root).bind(encode(&AttemptState::Active)?).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    AssignmentId::parse(row.get::<String, _>("assignment_id").as_str())?,
                    row.get("launch_owner"),
                ))
            })
            .collect()
    }

    pub fn create_admitted_assignment<'a>(
        &'a self,
        repo_root: &'a Path,
        draft: AssignmentDraft,
        isolated_integrator_available: bool,
    ) -> TaskStoreFuture<'a, AdmittedAssignment> {
        Box::pin(async move {
            let result = self
                .create_assignment_with_admission_impl(
                    repo_root,
                    draft,
                    true,
                    isolated_integrator_available,
                    None,
                )
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn attach_task_capsule(
        &self,
        assignment_id: AssignmentId,
        attempt_id: AttemptId,
        canonical_payload: String,
    ) -> TaskStoreFuture<'_, Assignment> {
        Box::pin(async move {
            let result = self
                .attach_task_capsule_impl(assignment_id, attempt_id, canonical_payload)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Resolve an existing evaluation workspace without exposing another task's history.
    /// Root-session and repository-lineage authority are checked before returning its path.
    pub async fn evaluation_target_workspace(
        &self,
        caller_repo_root: &Path,
        root_session_id: &str,
        target_id: AssignmentId,
    ) -> StoreResult<PathBuf> {
        let caller = repository_identity_async(caller_repo_root).await?;
        let mut transaction = self.pool.begin().await?;
        let target = load_assignment_tx(&mut transaction, target_id).await?;
        if target.root_session_id != root_session_id || target.repository_id != caller.id {
            return Err(StoreError::RepositoryMismatch(target_id));
        }
        let root = sqlx::query_scalar::<_, String>(
            "SELECT canonical_root FROM assignment_repositories WHERE assignment_id = ?",
        )
        .bind(target_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(StoreError::RepositoryBindingMissing(target_id))?;
        transaction.commit().await?;
        // Never hold a SQLite connection while waiting on the filesystem/Git.
        let actual = repository_identity_async(Path::new(&root)).await?;
        if actual.id != target.repository_id || actual.workspace_id != target.workspace_id {
            return Err(StoreError::RepositoryMismatch(target_id));
        }
        Ok(actual.canonical_root)
    }

    pub fn get_agent_task(
        &self,
        assignment_id: AssignmentId,
        observation_limit: Option<usize>,
    ) -> TaskStoreFuture<'_, AgentTask> {
        Box::pin(async move {
            self.get_agent_task_impl(assignment_id, observation_limit)
                .await
        })
    }

    pub fn get_agent_task_authorization(
        &self,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, AgentTaskAuthorization> {
        Box::pin(async move { self.get_agent_task_authorization_impl(assignment_id).await })
    }

    pub fn bind_agent_task(
        &self,
        binding: AgentTaskBindingDraft,
    ) -> TaskStoreFuture<'_, AgentTaskBinding> {
        Box::pin(async move {
            let result = self.bind_agent_task_impl(binding).await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn remove_agent_task_binding(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            let result = self
                .remove_agent_task_binding_impl(actor, assignment_id)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn get_agent_task_binding(
        &self,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, Option<AgentTaskBinding>> {
        Box::pin(async move { self.get_agent_task_binding_impl(assignment_id).await })
    }

    pub fn list_agent_task_bindings(
        &self,
        root_session_id: String,
        limit: Option<usize>,
    ) -> TaskStoreFuture<'_, Vec<AgentTaskBinding>> {
        Box::pin(async move {
            self.list_agent_task_bindings_impl(root_session_id, limit)
                .await
        })
    }

    /// Renew the bound actor's lease. With `progress`, also record meaningful progress,
    /// which is what defers nonproductive recovery; a lease alone only proves liveness.
    pub fn heartbeat_typed_workspace_actor(
        &self,
        binding: AgentTaskBinding,
        progress: bool,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.heartbeat_typed_workspace_actor_impl(binding, progress)
                .await
        })
    }

    pub fn append_observation(
        &self,
        attempt_id: AttemptId,
        kind: ObservationKind,
        summary: String,
        call_id: Option<String>,
    ) -> TaskStoreFuture<'_, RuntimeObservation> {
        Box::pin(async move {
            let result = self
                .append_observation_impl(attempt_id, kind, summary, call_id)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Terminal diagnostics may follow an already-sealed authored receipt.
    /// The current-attempt fence and idempotency key are checked under the writer lock.
    pub fn record_host_completion_observation(
        &self,
        attempt_id: AttemptId,
        origin: String,
        summary: String,
    ) -> TaskStoreFuture<'_, ()> {
        self.record_host_completion_with_stop_cause(attempt_id, origin, summary, None)
    }

    pub fn record_host_completion_with_stop_cause(
        &self,
        attempt_id: AttemptId,
        origin: String,
        summary: String,
        stop_cause: Option<String>,
    ) -> TaskStoreFuture<'_, ()> {
        self.record_host_completion_impl(attempt_id, origin, summary, stop_cause)
    }

    pub fn record_host_completion_deferred(
        &self,
        attempt_id: AttemptId,
        origin: String,
        summary: String,
    ) -> TaskStoreFuture<'_, DeferredHostCompletionWake> {
        Box::pin(async move {
            validate_host_completion_observation(&origin, &summary, None)?;
            Ok(DeferredHostCompletionWake {
                store: self.clone(), attempt_id, pending: Some((origin, summary)),
            })
        })
    }

    fn record_host_completion_impl(
        &self,
        attempt_id: AttemptId,
        origin: String,
        summary: String,
        stop_cause: Option<String>,
    ) -> TaskStoreFuture<'_, ()> {
        Box::pin(async move {
            validate_host_completion_observation(&origin, &summary, stop_cause.as_deref())?;
            let mut transaction = self.pool.begin().await?;
            lock_attempt_tx(&mut transaction, attempt_id).await?;
            let attempt = load_attempt_tx(&mut transaction, attempt_id).await?;
            if load_current_attempt_tx(&mut transaction, attempt.assignment_id)
                .await?
                .attempt_id
                != attempt_id
            {
                return Err(StoreError::AttemptNotActive(attempt_id));
            }
            let assignment = load_assignment_tx(&mut transaction, attempt.assignment_id).await?;
            append_host_completion_tx(
                &mut transaction, &assignment, attempt_id, origin, summary, stop_cause,
            ).await?;
            transaction.commit().await?;
            self.notify_wake_waiters();
            Ok(())
        })
    }

    pub fn record_validation_call(&self, call: ValidationCall) -> TaskStoreFuture<'_, ()> {
        Box::pin(async move {
            let result = self.record_validation_call_impl(call).await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Only immutable assignment requirements in the explicit `inspect:path`
    /// form opt into source evidence. Ordinary reads create no proof records.
    pub fn prepare_source_inspection(
        &self,
        attempt_id: AttemptId,
        absolute_path: String,
    ) -> TaskStoreFuture<'_, Option<crate::SourceInspectionStart>> {
        Box::pin(async move {
            let context = validation_context(&self.pool, attempt_id).await?;
            if !context
                .assignment
                .required_evidence
                .iter()
                .any(|item| item.starts_with("inspect:"))
            {
                return Ok(None);
            }
            let Ok(absolute_path) = tokio::fs::canonicalize(&absolute_path).await else {
                return Ok(None);
            };
            let Ok(relative_path) = absolute_path.strip_prefix(&context.repo_root) else {
                return Ok(None);
            };
            let Ok(path) = crate::scope::normalize_repo_path_async(
                &context.repo_root,
                &relative_path.to_string_lossy(),
            )
            .await
            else {
                return Ok(None);
            };
            if !context
                .assignment
                .required_evidence
                .contains(&format!("inspect:{path}"))
            {
                return Ok(None);
            }
            Ok(Some(crate::SourceInspectionStart {
                canonical_path: Some(absolute_path),
                attempt_id,
                path,
                workspace_id: context.assignment.workspace_id,
                epoch: 0,
            }))
        })
    }

    pub fn record_source_inspection(
        &self,
        start: crate::SourceInspectionStart,
        call_id: String,
        source_sha256: String,
        bytes: u64,
    ) -> TaskStoreFuture<'_, crate::CriterionEvidenceRef> {
        Box::pin(async move {
            // Runtime call IDs are session-local (for example exec-1-tool-1),
            // while validation_calls is shared by every attempt in this home.
            // Keep acquisition/recovery IDs local; namespace only the durable
            // evidence identity and retain the store's ownership checks.
            let call_id = format!("inspection:{}:{call_id}", start.attempt_id);
            let result = crate::SourceInspectionResult {
                version: 1,
                kind: "source_inspection".into(),
                call_id: call_id.clone(),
                path: start.path.clone(),
                source_sha256,
                bytes,
            };
            let call = ValidationCall {
                call_id: call_id.clone(),
                attempt_id: start.attempt_id,
                command_summary: format!("inspect:{}", start.path),
                evidence: crate::ValidationEvidence {
                    validation_result: Some(
                        serde_json::to_value(result)
                            .map_err(|error| StoreError::CorruptData(error.to_string()))?,
                    ),
                    ..Default::default()
                },
                status: ValidationCallStatus::Succeeded,
                recorded_at: Utc::now(),
            };
            // The read already completed. Publish its result atomically; a
            // cancelled publisher must never leave a durable Running row.
            self.record_validation_call_transaction(call, true).await?;
            let recorded = self
                .get_validation_call(call_id.clone())
                .await?
                .ok_or_else(|| StoreError::CorruptData("inspection call disappeared".into()))?;
            if recorded.verified_evidence_kind()
                != Some(crate::CriterionEvidenceKind::SourceInspection)
            {
                return Err(StoreError::ValidationCallStatusInvalid {
                    call_ids: vec![call_id],
                });
            }
            Ok(crate::CriterionEvidenceRef {
                call_id,
                workspace_id: start.workspace_id,
                evidence_epoch: start.epoch,
                kind: crate::CriterionEvidenceKind::SourceInspection,
            })
        })
    }

    pub fn get_validation_call(
        &self,
        call_id: String,
    ) -> TaskStoreFuture<'_, Option<ValidationCall>> {
        Box::pin(async move { self.get_validation_call_impl(call_id).await })
    }

    pub fn heartbeat_validation_call(
        &self,
        call_id: String,
        lease_expires_at: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.heartbeat_validation_call_impl(call_id, lease_expires_at)
                .await
        })
    }

    pub fn submit_agent_receipt(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            let result = self
                .submit_agent_receipt_impl(attempt_id, receipt, None, false, None)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Persist an observed plain-message child outcome without treating its prose as proof.
    /// Explicit typed assignments must still submit and satisfy their receipt contract.
    pub fn record_legacy_agent_outcome(
        &self,
        attempt_id: AttemptId,
        status: AgentStatusClaim,
        summary: String,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            let receipt = ReceiptDraft {
                status,
                summary,
                criterion_results: Vec::new(),
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: None,
            };
            let result = self
                .submit_agent_receipt_impl(attempt_id, receipt, None, true, None)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn submit_agent_receipt_with_review(
        &self,
        attempt_id: AttemptId,
        receipt: ReceiptDraft,
        review_reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            let result = self
                .submit_agent_receipt_impl(attempt_id, receipt, Some(review_reason), false, None)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn amend_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> TaskStoreFuture<'_, Attempt> {
        Box::pin(async move {
            self.amend_agent_task_with_binding(actor, assignment_id, amendment)
                .await
                .map(|(attempt, _)| attempt)
        })
    }

    pub fn amend_agent_task_with_binding(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        amendment: AttemptAmendment,
    ) -> TaskStoreFuture<'_, (Attempt, Option<AgentTaskBinding>)> {
        Box::pin(async move {
            let result = self
                .amend_agent_task_impl(actor, assignment_id, amendment, false)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Reuse an active ordinary attempt or start a fresh one for a subsequent model turn.
    /// This host operation cannot amend an explicit typed contract.
    pub fn begin_legacy_agent_turn(
        &self,
        assignment_id: AssignmentId,
    ) -> TaskStoreFuture<'_, Attempt> {
        Box::pin(async move {
            let result = self
                .amend_agent_task_impl(
                    TaskActor::Root,
                    assignment_id,
                    AttemptAmendment {
                        reason: "plain-message follow-up turn".to_string(),
                        objective: None,
                        acceptance_criteria: None,
                        stop_condition: None,
                    },
                    true,
                )
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result.map(|(attempt, _)| attempt)
        })
    }

    pub fn abandon_agent_task(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentReceipt> {
        Box::pin(async move {
            let result = self
                .abandon_agent_task_impl(actor, assignment_id, reason)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn set_agent_gate(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        expected_attempt_id: AttemptId,
        kind: GateKind,
        status: GateStatus,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate> {
        Box::pin(async move {
            let result = self
                .set_agent_gate_impl(
                    actor,
                    assignment_id,
                    expected_attempt_id,
                    kind,
                    status,
                    reason,
                )
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn waive_agent_gate(
        &self,
        actor: TaskActor,
        assignment_id: AssignmentId,
        kind: GateKind,
        reason: String,
    ) -> TaskStoreFuture<'_, AgentGate> {
        Box::pin(async move {
            let result = self
                .waive_agent_gate_impl(actor, assignment_id, kind, reason)
                .await;
            if result.is_ok() {
                self.notify_wake_waiters();
            }
            result
        })
    }

    pub fn read_wake_events(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> TaskStoreFuture<'_, WakeRead> {
        Box::pin(async move {
            self.read_wake_events_impl(root_session_id, after_event_id)
                .await
        })
    }

    pub fn wait_for_wake_events(
        &self,
        root_session_id: String,
        after_event_id: Option<WakeEventId>,
    ) -> TaskStoreFuture<'_, WakeRead> {
        Box::pin(async move {
            self.wait_for_wake_events_impl(root_session_id, after_event_id)
                .await
        })
    }

    pub fn automatic_wake_cursor(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
    ) -> TaskStoreFuture<'_, Option<WakeEventId>> {
        Box::pin(async move {
            self.automatic_wake_cursor_impl(root_session_id, consuming_agent_path)
                .await
        })
    }

    pub fn compare_and_swap_automatic_wake_cursor(
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
                None,
            )
            .await
        })
    }

    pub fn publish_automatic_wake_delivery(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
        expected: Option<WakeEventId>,
        next: WakeEventId,
        delivery_json: String,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.compare_and_swap_automatic_wake_cursor_impl(
                root_session_id,
                consuming_agent_path,
                expected,
                next,
                Some(delivery_json),
            )
            .await
        })
    }

    pub fn automatic_wake_delivery(
        &self,
        root_session_id: String,
        consuming_agent_path: String,
    ) -> TaskStoreFuture<'_, Option<String>> {
        Box::pin(async move {
            let row = sqlx::query("SELECT delivery_json FROM automatic_wake_cursors WHERE root_session_id = ? AND consuming_agent_path = ?")
                .bind(root_session_id).bind(consuming_agent_path).fetch_optional(&self.pool).await?;
            Ok(row.and_then(|row| row.get::<Option<String>, _>("delivery_json")))
        })
    }

    pub fn reserve_stalled_nudge(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move {
            self.reserve_stalled_nudge_impl(assignment_id, no_progress_before)
                .await
        })
    }

    pub fn recover_nonproductive_assignment(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
    ) -> TaskStoreFuture<'_, NonproductiveRecovery> {
        Box::pin(async move {
            let result = self
                .recover_nonproductive_assignment_impl(assignment_id, no_progress_before, None)
                .await;
            if matches!(&result, Ok(NonproductiveRecovery::Recovered { .. })) {
                self.notify_wake_waiters();
            }
            result
        })
    }

    /// Caller must establish that this launch owner is no longer live. The
    /// transaction rechecks its exact attempt/owner and never retires a worker.
    pub async fn recover_unstarted_assignment(
        &self,
        assignment_id: AssignmentId,
        no_progress_before: chrono::DateTime<Utc>,
        owner: Option<String>,
    ) -> StoreResult<NonproductiveRecovery> {
        let result = self.recover_nonproductive_assignment_impl(assignment_id, no_progress_before, Some(owner)).await;
        if matches!(&result, Ok(NonproductiveRecovery::Recovered { .. })) { self.notify_wake_waiters(); }
        result
    }

    pub fn release_stalled_nudge(&self, assignment_id: AssignmentId) -> TaskStoreFuture<'_, bool> {
        Box::pin(async move { self.release_stalled_nudge_impl(assignment_id).await })
    }
}

struct ValidationContext {
    assignment: Assignment,
    repo_root: PathBuf,
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

#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredValidationResult {
    argv: Vec<String>,
    covered_paths: Vec<String>,
    call_id: String,
    #[serde(default)]
    process_id: Option<String>,
    status: StoredValidationTerminalStatus,
    duration_ms: u64,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    failure_excerpt: Option<String>,
    #[serde(default)]
    raw_artifact_ref: Option<String>,
    #[serde(default)]
    raw_artifact_sha256: Option<String>,
}

#[derive(PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredValidationTerminalStatus {
    Succeeded,
    Unverified,
    Failed,
}

/// A native execution observation has no caller-authored coverage requirement.
/// It can establish historical execution, never current input binding or scope.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredNativeCommandResult {
    kind: String,
    argv: Vec<String>,
    call_id: String,
    status: StoredValidationTerminalStatus,
    #[serde(rename = "durationMs")]
    _duration_ms: u64,
}

pub(crate) fn verified_evidence_kind(
    call: &ValidationCall,
) -> Option<crate::CriterionEvidenceKind> {
    if call.status != ValidationCallStatus::Succeeded
        || call.evidence.end_epoch != Some(call.evidence.start_epoch)
    {
        return None;
    }
    if let Some(inspection) = call.evidence.validation_result.as_ref().and_then(|value| {
        serde_json::from_value::<crate::SourceInspectionResult>(value.clone()).ok()
    }) {
        return (inspection.version == 1
            && inspection.kind == "source_inspection"
            && inspection.call_id == call.call_id
            && is_normalized_repository_relative_scope(&inspection.path)
            && inspection.path != "."
            && call.command_summary == format!("inspect:{}", inspection.path)
            && inspection.source_sha256.len() == 64
            && inspection
                .source_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()))
        .then_some(crate::CriterionEvidenceKind::SourceInspection);
    }
    if let Some(result) =
        call.evidence.validation_result.as_ref().and_then(|value| {
            serde_json::from_value::<StoredNativeCommandResult>(value.clone()).ok()
        })
    {
        return (result.kind == "native_command_execution_v1"
            && result.call_id == call.call_id
            && !result.call_id.trim().is_empty()
            && result.status == StoredValidationTerminalStatus::Succeeded
            && result
                .argv
                .first()
                .is_some_and(|value| !value.trim().is_empty()))
        .then_some(crate::CriterionEvidenceKind::ValidationExecution);
    }
    let result = call
        .evidence
        .validation_result
        .as_ref()
        .and_then(|result| serde_json::from_value::<StoredValidationResult>(result.clone()).ok())?;
    (call.status == ValidationCallStatus::Succeeded
        && call.evidence.end_epoch == Some(call.evidence.start_epoch)
        && result.call_id == call.call_id
        && result.status == StoredValidationTerminalStatus::Succeeded
        // Empty positional arguments are valid; only the executable must be
        // nonempty, matching the public ValidationResult wire contract.
        && result.argv.first().is_some_and(|value| !value.trim().is_empty())
        && !result.covered_paths.is_empty()
        && stored_covered_paths_are_normalized(&result.covered_paths))
    .then_some(crate::CriterionEvidenceKind::ValidationExecution)
}

fn stored_covered_paths_are_normalized(covered_paths: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    covered_paths.iter().all(|path| {
        if !is_normalized_repository_relative_scope(path) {
            return false;
        }
        let identity = path.to_ascii_lowercase();
        seen.insert(identity)
    })
}

fn is_normalized_repository_relative_scope(path: &str) -> bool {
    if path == "." {
        return true;
    }
    if path.is_empty()
        || path.contains('\0')
        || path.trim() != path
        || path.contains('\\')
        || path.starts_with('/')
        || path.starts_with('~')
        || path.ends_with('/')
        || path.contains("//")
        || path.as_bytes().get(1) == Some(&b':')
    {
        return false;
    }
    path.split('/')
        .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
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

/// Marks a workspace row whose canonical-root claim was moved aside. The original
/// root follows the row's own workspace ID, so the marker stays unique and recoverable.
const RETIRED_FOLDED_ROOT_PREFIX: &str = "retired-folded-identity:";

// Compatibility metadata for durable task identities, not a workspace scanner.
async fn ensure_task_workspace_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    repository: &RepositoryIdentity,
) -> StoreResult<()> {
    let now = Utc::now();
    // Rows written before exact identities hold this root under its case-folded
    // ID, and `canonical_root` is UNIQUE, so the exact row could never be added:
    // every admission in that checkout failed. Move only that historical claim
    // aside; the row and everything referencing its ID stay intact, unmerged.
    let holder = sqlx::query_scalar::<_, String>(
        "SELECT workspace_id FROM workspace_repositories
         WHERE canonical_root = ? AND workspace_id <> ?",
    )
    .bind(&repository.canonical_path)
    .bind(&repository.workspace_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(holder) = holder {
        if !crate::scope::is_legacy_workspace_id(&repository.canonical_root, &holder) {
            return Err(StoreError::CorruptData(format!(
                "workspace root {} is already bound to another workspace identity",
                repository.canonical_path
            )));
        }
        let retired = sqlx::query(
            "UPDATE workspace_repositories SET canonical_root = ?
             WHERE workspace_id = ? AND canonical_root = ?",
        )
        .bind(format!(
            "{RETIRED_FOLDED_ROOT_PREFIX}{holder}:{}",
            repository.canonical_path
        ))
        .bind(&holder)
        .bind(&repository.canonical_path)
        .execute(&mut **transaction)
        .await?;
        if retired.rows_affected() != 1 {
            return Err(StoreError::CorruptData(
                "legacy workspace root changed during retirement".to_string(),
            ));
        }
        tracing::info!(
            legacy_workspace_id = %holder,
            root = %repository.canonical_path,
            "retired a case-folded legacy workspace root claim; its history is retained"
        );
    }
    sqlx::query(
        "INSERT INTO workspace_repositories (
            workspace_id, repository_id, canonical_root, epoch, updated_at
         ) VALUES (?, ?, ?, 0, ?)
         ON CONFLICT(workspace_id) DO NOTHING",
    )
    .bind(&repository.workspace_id)
    .bind(&repository.id)
    .bind(&repository.canonical_path)
    .bind(encode(&now)?)
    .execute(&mut **transaction)
    .await?;
    let row = sqlx::query(
        "SELECT repository_id, canonical_root FROM workspace_repositories WHERE workspace_id = ?",
    )
    .bind(&repository.workspace_id)
    .fetch_one(&mut **transaction)
    .await?;
    let stored_repository_id = row.get::<String, _>("repository_id");
    if !crate::scope::filesystem_paths_equal(
        &row.get::<String, _>("canonical_root"),
        &repository.canonical_path,
    ) {
        return Err(StoreError::CorruptData(
            "workspace identity resolved to a different repository root".to_string(),
        ));
    }
    if stored_repository_id != repository.id {
        if stored_repository_id != repository.workspace_id {
            return Err(StoreError::CorruptData(
                "workspace identity resolved to a different repository lineage".to_string(),
            ));
        }
        let updated = sqlx::query(
            "UPDATE workspace_repositories SET repository_id = ?, updated_at = ?
             WHERE workspace_id = ? AND repository_id = ?",
        )
        .bind(&repository.id)
        .bind(encode(&now)?)
        .bind(&repository.workspace_id)
        .bind(stored_repository_id)
        .execute(&mut **transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::CorruptData(
                "legacy workspace lineage changed during upgrade".to_string(),
            ));
        }
    }
    Ok(())
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
    // Non-Git roots legitimately retain repository_id == workspace_id. Resolve
    // each distinct root once per open, not once per historical assignment.
    let mut identities = std::collections::HashMap::new();
    for row in rows {
        let assignment_id = row.get::<String, _>("assignment_id");
        let legacy_repository_id = row.get::<String, _>("repository_id");
        let workspace_id = row.get::<String, _>("workspace_id");
        let canonical_root = row.get::<String, _>("canonical_root");
        if !identities.contains_key(&canonical_root) {
            let identity = match repository_identity_async(Path::new(&canonical_root)).await {
                Ok(repository) => Some(repository),
                Err(StoreError::InvalidScope(_)) => None,
                Err(error) => return Err(error),
            };
            identities.insert(canonical_root.clone(), identity);
        }
        let Some(repository) = identities.get(&canonical_root).and_then(Option::as_ref) else {
            continue;
        };
        if repository.workspace_id != workspace_id {
            if crate::scope::is_legacy_workspace_id(&repository.canonical_root, &workspace_id) {
                // Folded identities may have merged case-sensitive checkouts.
                // Preserve these historical bindings without authorizing reuse.
                continue;
            }
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
            let current: Option<(String, String, String)> = sqlx::query_as(
                "SELECT repository_id, workspace_id, canonical_root FROM assignment_repositories WHERE assignment_id = ?",
            )
            .bind(&assignment_id)
            .fetch_optional(&mut *transaction)
            .await?;
            if current
                != Some((
                    repository.id.clone(),
                    workspace_id.clone(),
                    canonical_root.clone(),
                ))
            {
                return Err(StoreError::CorruptData(format!(
                    "legacy repository binding changed during upgrade for assignment {assignment_id}"
                )));
            }
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

/// Receipt bodies are evidence only when they agree with their relational seal.
async fn load_receipt_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &Attempt,
) -> StoreResult<Option<(AgentReceipt, String)>> {
    let row = sqlx::query("SELECT attempt_id, assignment_id, status, body_json, sealed_at FROM receipts WHERE attempt_id = ?")
        .bind(attempt.attempt_id.to_string()).fetch_optional(&mut **transaction).await?;
    let Some(row) = row else {
        if attempt.state.is_terminal() || attempt.sealed_at.is_some() {
            return Err(StoreError::CorruptData("sealed attempt has no receipt".into()));
        }
        return Ok(None);
    };
    let body: String = row.get("body_json");
    let receipt: AgentReceipt = decode(&body)?;
    let status: AgentStatusClaim = decode(&row.get::<String, _>("status"))?;
    let sealed_at: chrono::DateTime<Utc> = decode(&row.get::<String, _>("sealed_at"))?;
    // Failed host review can downgrade lifecycle state without rewriting the
    // worker's immutable completed claim or its relational seal.
    let mut state_matches = status.attempt_state() == attempt.state;
    if !state_matches
        && status == AgentStatusClaim::Completed
        && attempt.state == AttemptState::NeedsMain
    {
        let verdicts = sqlx::query("SELECT assignment_id, kind, status, body_json, updated_at, sealed_at FROM gate_verdicts WHERE attempt_id = ?")
            .bind(attempt.attempt_id.to_string())
            .fetch_all(&mut **transaction)
            .await?;
        for verdict in verdicts {
            let gate: AgentGate = decode(&verdict.get::<String, _>("body_json"))?;
            let gate_sealed_at: chrono::DateTime<Utc> = decode(&verdict.get::<String, _>("sealed_at"))?;
            let gate_updated_at: chrono::DateTime<Utc> =
                decode(&verdict.get::<String, _>("updated_at"))?;
            if verdict.get::<String, _>("assignment_id") != attempt.assignment_id.to_string()
                || gate.assignment_id != attempt.assignment_id
                || verdict.get::<String, _>("kind") != encode(&gate.kind)?
                || verdict.get::<String, _>("status") != encode(&gate.status)?
                || gate.sealed_at != Some(gate_sealed_at)
                || gate.updated_at != gate_updated_at
            {
                return Err(StoreError::CorruptData(
                    "gate verdict does not match its relational seal".into(),
                ));
            }
            state_matches |= gate_sealed_at >= sealed_at
                && gate.status.is_sealed()
                && (matches!((gate.kind, gate.status),
                    (GateKind::Review, GateStatus::Failed)
                    | (GateKind::Verification, GateStatus::Failed | GateStatus::ChangesRequested))
                    || gate.kind == GateKind::Review
                        && gate.status == GateStatus::ChangesRequested);
        }
    }
    if row.get::<String, _>("attempt_id") != attempt.attempt_id.to_string()
        || row.get::<String, _>("assignment_id") != attempt.assignment_id.to_string()
        || receipt.assignment_id != attempt.assignment_id
        || receipt.attempt_id != attempt.attempt_id
        || receipt.status != status || !state_matches
        || receipt.sealed_at != sealed_at || attempt.sealed_at != Some(sealed_at)
    {
        return Err(StoreError::CorruptData("receipt identity or seal does not match its attempt".into()));
    }
    Ok(Some((receipt, body)))
}

async fn validate_capsule_contract_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    capsule: &TaskCapsuleV1,
) -> StoreResult<()> {
    let attempt = load_attempt_tx(transaction, capsule.attempt_id).await?;
    // Missing legacy strategy means Auto, which admission resolves to Shared.
    let workspace_strategy = match capsule.workspace_strategy.unwrap_or_default() {
        WorkspaceStrategy::Auto => WorkspaceStrategy::Shared,
        strategy => strategy,
    };
    // Corrections keep the original immutable capsule, not the current attempt.
    if capsule.schema_version != 1 || capsule.assignment_id != assignment.assignment_id
        || attempt.assignment_id != assignment.assignment_id || attempt.ordinal != 0
        || capsule.role != assignment.role
        || capsule.capability_profile != assignment.capability_profile
        || capsule.objective != assignment.objective
        || capsule.requirements != assignment.acceptance_criteria
        || capsule.read_scope != assignment.read_scope || capsule.write_scope != assignment.write_scope
        || capsule.stop_condition != assignment.stop_condition
        || capsule.dependencies != assignment.dependencies || capsule.risk_hints != assignment.risk_hints
        || capsule.contract_claims != assignment.contract_claims
        || workspace_strategy != assignment.workspace_strategy
        || capsule.relation != assignment.relation
        || capsule.architecture_contract_ref != assignment.architecture_contract_ref
        || capsule.integration_plan != assignment.integration_plan
        || capsule.prohibited_changes != assignment.prohibited_changes
        || capsule.required_evidence != assignment.required_evidence
    {
        return Err(StoreError::CorruptData("capsule does not match its original admitted contract".into()));
    }
    Ok(())
}

fn wake_event_from_observation_row(row: &sqlx::sqlite::SqliteRow) -> StoreResult<WakeEvent> {
    let observation: RuntimeObservation = decode(&row.get::<String, _>("body_json"))?;
    if observation.assignment_id.to_string() != row.get::<String, _>("assignment_id")
        || observation.attempt_id.to_string() != row.get::<String, _>("attempt_id")
        || observation.wake_event_id.to_string() != row.get::<String, _>("wake_event_id")
    {
        return Err(StoreError::CorruptData("authoritative wake observation identity mismatch".into()));
    }
    Ok(WakeEvent { event_id: observation.wake_event_id, assignment_id: observation.assignment_id,
        attempt_id: observation.attempt_id, reason: observation.kind,
        summary: observation.summary, created_at: observation.created_at })
}

async fn restore_missing_capsule(path: PathBuf, payload: String) -> StoreResult<()> {
    tokio::task::spawn_blocking(move || -> StoreResult<()> {
        let temporary = path.with_extension(format!("{}.repair", AttemptId::new()));
        let result = (|| -> StoreResult<()> {
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            drop(file);
            // Atomic no-clobber publication: never overwrite conflicting evidence.
            match std::fs::hard_link(&temporary, &path) {
                Ok(()) => tracing::info!("restored missing capsule from validated immutable authority"),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::fs::read(&path)? != payload.as_bytes() {
                        return Err(StoreError::CorruptData("capsule repair conflicts with existing sidecar".into()));
                    }
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(temporary);
        result
    }).await.map_err(std::io::Error::other)?
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
    cached_dependencies: &mut HashMap<AssignmentId, Vec<AssignmentId>>,
) -> StoreResult<bool> {
    let mut pending = vec![start];
    let mut seen = HashSet::new();
    while let Some(next) = pending.pop() {
        if !seen.insert(next) {
            continue;
        }
        if !cached_dependencies.contains_key(&next) {
            let json = sqlx::query_scalar::<_, String>(
                "SELECT body_json FROM assignments WHERE assignment_id = ?",
            )
            .bind(next.to_string())
            .fetch_optional(&mut **transaction)
            .await?;
            let dependencies = match json {
                Some(json) => decode::<Assignment>(&json)?.dependencies,
                None => Vec::new(),
            };
            cached_dependencies.insert(next, dependencies);
        }
        let dependencies = cached_dependencies.get(&next).expect("visited dependency cached");
        if dependencies.contains(&target) {
            return Ok(true);
        }
        pending.extend(dependencies.iter().copied());
    }
    Ok(false)
}


async fn validate_dependencies_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    candidate_id: AssignmentId,
    repository_id: Option<&str>,
    dependencies: &[AssignmentId],
) -> StoreResult<Vec<crate::DependencyAdmission>> {
    let admitted = validate_direct_dependencies_tx(transaction, candidate_id, repository_id, dependencies, &[]).await?;
    // Follow prerequisite provenance iteratively: a sealed intermediate result
    // cannot hide a superseded input. One database snapshot, no recursive tasks
    // or cross-transaction freshness cache; shared DAG nodes are visited once.
    let mut pending = dependencies.to_vec();
    let mut visited = HashSet::new();
    while let Some(dependency_id) = pending.pop() {
        if !visited.insert(dependency_id) {
            continue;
        }
        let dependency = load_assignment_tx(transaction, dependency_id).await?;
        let judged_targets = judged_dependency_targets(&dependency);
        let current = validate_direct_dependencies_tx(
            transaction, dependency_id, Some(&dependency.repository_id), &dependency.dependencies,
            judged_targets,
        ).await?;
        let captured = load_dependency_admissions_tx(transaction, dependency_id).await?;
        let blockers = compare_dependency_admissions(&dependency.dependencies, &captured, &current);
        if !blockers.is_empty() {
            return Err(StoreError::DependencyBlocked { blockers });
        }
        pending.extend(dependency.dependencies.iter().copied()
            .filter(|dependency_id| !judged_targets.contains(dependency_id)));
    }
    Ok(admitted)
}

async fn load_dependency_admissions_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<Vec<crate::DependencyAdmission>> {
    sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM dependency_admissions WHERE assignment_id = ? ORDER BY dependency_id",
    ).bind(assignment_id.to_string()).fetch_all(&mut **transaction).await?
        .into_iter().map(|body| decode(&body)).collect()
}

fn compare_dependency_admissions(
    dependencies: &[AssignmentId],
    captured: &[crate::DependencyAdmission],
    current: &[crate::DependencyAdmission],
) -> Vec<DependencyBlocker> {
    dependencies.iter().filter_map(|dependency_id| {
        let pin = captured.iter().find(|pin| pin.assignment_id == *dependency_id);
        let authority = current.iter().find(|authority| authority.assignment_id == *dependency_id);
        match (pin, authority) {
            (Some(pin), Some(authority))
                if pin.attempt_id == authority.attempt_id
                    && pin.receipt_sha256 == authority.receipt_sha256 => None,
            (Some(_), Some(_)) => Some(DependencyBlocker {
                assignment_id: *dependency_id,
                state: DependencyState::Blocked,
                detail: "dependency attempt or sealed receipt changed since admission; create a replacement assignment against current prerequisites".into(),
            }),
            _ => Some(DependencyBlocker {
                assignment_id: *dependency_id,
                state: DependencyState::Unknown,
                detail: "dependency admission provenance or current authority is unavailable".into(),
            }),
        }
    }).collect()
}

fn judged_dependency_targets(assignment: &Assignment) -> &[AssignmentId] {
    assignment.relation.as_ref().filter(|relation| matches!(
        (assignment.role, relation.kind),
        (AgentRole::Reviewer, RelationKind::Review)
            | (AgentRole::Verifier, RelationKind::Verification)
    )).map(|relation| relation.target_assignment_ids.as_slice()).unwrap_or_default()
}

async fn dependency_authority_blockers_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    captured: &[crate::DependencyAdmission],
) -> StoreResult<Vec<DependencyBlocker>> {
    // A review/verification target is an immutable object being judged, not
    // an accepted input. Its own negative verdict must not prevent the judge
    // from sealing a report. A new target attempt still invalidates that pin.
    let judged_targets = judged_dependency_targets(assignment);
    let prerequisites = assignment.dependencies.iter().copied()
        .filter(|dependency| !judged_targets.contains(dependency)).collect::<Vec<_>>();
    match validate_dependencies_tx(
        transaction, assignment.assignment_id, Some(&assignment.repository_id), &prerequisites,
    ).await {
        Ok(mut current) => {
            let mut blockers = Vec::new();
            for target_id in judged_targets {
                let target = load_current_attempt_tx(transaction, *target_id).await?;
                if let Some((receipt, body)) = load_receipt_tx(transaction, &target).await? {
                    current.push(crate::DependencyAdmission {
                        assignment_id: *target_id,
                        attempt_id: receipt.attempt_id,
                        receipt_sha256: hash_bytes(body.as_bytes()),
                        gates: Vec::new(),
                    });
                } else {
                    blockers.push(DependencyBlocker {
                        assignment_id: *target_id,
                        state: DependencyState::Incomplete,
                        detail: "review/verification target has no current sealed receipt".into(),
                    });
                }
            }
            blockers.extend(compare_dependency_admissions(&assignment.dependencies, captured, &current));
            Ok(blockers)
        }
        Err(StoreError::DependencyBlocked { blockers }) => Ok(blockers),
        Err(error) => Err(error),
    }
}

async fn validate_direct_dependencies_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    candidate_id: AssignmentId,
    repository_id: Option<&str>,
    dependencies: &[AssignmentId],
    judged_targets: &[AssignmentId],
) -> StoreResult<Vec<crate::DependencyAdmission>> {
    let mut blockers = Vec::new();
    let mut admitted = Vec::new();
    // One transaction-local adjacency observation for overlapping DAG roots.
    // JSON authority is decoded on first visit; no cross-transaction cache can
    // hide amendments, missing rows or corrupt dependency bodies.
    let mut cached_dependencies = HashMap::new();
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
        cached_dependencies.insert(*dependency, dependency_assignment.dependencies.clone());
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
        if dependency_reaches_tx(transaction, *dependency, candidate_id, &mut cached_dependencies).await? {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Cyclic,
                detail: "dependency would create a cycle".to_string(),
            });
            continue;
        }
        let dependency_attempt = load_current_attempt_tx(transaction, *dependency).await?;
        let Some((receipt, receipt_json)) = load_receipt_tx(transaction, &dependency_attempt).await? else {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Incomplete,
                detail: "dependency has no sealed receipt".to_string(),
            });
            continue;
        };
        if judged_targets.contains(dependency) {
            admitted.push(crate::DependencyAdmission {
                assignment_id: *dependency,
                attempt_id: receipt.attempt_id,
                receipt_sha256: hash_bytes(receipt_json.as_bytes()),
                gates: Vec::new(),
            });
            continue;
        }
        if dependency_attempt.state == AttemptState::NeedsMain {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: DependencyState::Blocked,
                detail: "dependency requires main intervention".to_string(),
            });
            continue;
        }
        if !receipt.status.is_success() {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency,
                state: dependency_state(receipt.status),
                detail: format!("dependency receipt is {:?}", receipt.status),
            });
            continue;
        }
        let assessments = source_inspection::assess_tx(
            transaction, &dependency_assignment, &dependency_attempt,
            &receipt.criterion_results, &receipt.validation_call_ids, false,
        ).await?;
        if let Some(assessment) = assessments.iter().find(|assessment| !assessment.satisfied) {
            blockers.push(DependencyBlocker {
                assignment_id: *dependency, state: DependencyState::Blocked,
                detail: format!("{}: {}", assessment.criterion_id, assessment.reason),
            });
            continue;
        }
        let gate_rows =
            sqlx::query("SELECT body_json FROM gates WHERE assignment_id = ? ORDER BY kind")
                .bind(dependency.to_string())
                .fetch_all(&mut **transaction)
                .await?;
        let mut blocking_gates = Vec::new();
        let mut accepted_gates = Vec::new();
        for row in gate_rows {
            let gate: AgentGate = decode(row.get::<String, _>("body_json").as_str())?;
            if gate.assignment_id != *dependency {
                return Err(StoreError::CorruptData(format!(
                    "gate identity does not match dependency {dependency}"
                )));
            }
            if matches!(gate.kind, GateKind::Review | GateKind::Verification) {
                // Pending review remains deliberately speculative. Only an
                // explicit verdict bound to the current attempt fences it.
                if matches!(gate.status, GateStatus::Failed | GateStatus::ChangesRequested | GateStatus::Violated) {
                    let verdict = sqlx::query_scalar::<_, String>(
                        "SELECT body_json FROM gate_verdicts WHERE attempt_id = ? AND kind = ?",
                    ).bind(dependency_attempt.attempt_id.to_string()).bind(encode(&gate.kind)?)
                        .fetch_optional(&mut **transaction).await?;
                    if verdict.as_deref() == Some(encode(&gate)?.as_str()) {
                        blocking_gates.push((gate.kind, gate.status));
                    }
                }
                continue;
            }
            if !matches!(gate.status, GateStatus::Passed | GateStatus::Waived)
            {
                blocking_gates.push((gate.kind, gate.status));
            }
            accepted_gates.push(gate);
        }
        admitted.push(crate::DependencyAdmission {
            assignment_id: *dependency,
            attempt_id: receipt.attempt_id,
            receipt_sha256: hash_bytes(receipt_json.as_bytes()),
            gates: accepted_gates,
        });
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
        Ok(admitted)
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
    let contract =
        canonicalize_architecture_contract_async(Path::new(&canonical_root), contract).await?;
    let contract_sha256 = hex::encode(Sha256::digest(serde_json::to_vec(&contract)?));
    Ok(Some(SealedArchitectureContractV1 {
        contract,
        contract_sha256,
    }))
}

async fn validate_architecture_contract_reference_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    worker: &Assignment,
    repo_root: &Path,
) -> StoreResult<()> {
    // Evaluation dependencies inspect the Architect's output; only Workers consume
    // its exact implementation contract (the draft validator rejects refs on other roles).
    if worker.role != AgentRole::Worker {
        return Ok(());
    }
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
    let sealed_contract =
        canonicalize_architecture_contract_async(repo_root, sealed.contract).await?;
    let sealed_hash = hex::encode(Sha256::digest(serde_json::to_vec(&sealed_contract)?));
    if sealed_hash != reference.contract_sha256 {
        return Err(StoreError::InvalidAssignment(
            "sealed architecture contract is not canonical for the worker repository".to_string(),
        ));
    }
    let worker_projection = canonicalize_architecture_contract_async(
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
    )
    .await?;
    if worker_projection != sealed_contract {
        return Err(StoreError::InvalidAssignment(
            "worker scope or claims are incompatible with the authoritative architecture contract"
                .to_string(),
        ));
    }
    Ok(())
}

async fn canonicalize_architecture_contract_async(
    repo_root: &Path,
    contract: ArchitectureContractV1,
) -> StoreResult<ArchitectureContractV1> {
    let repo_root = repo_root.to_path_buf();
    tokio::task::spawn_blocking(move || canonicalize_architecture_contract(&repo_root, contract))
        .await
        .map_err(|error| {
            StoreError::CorruptData(format!("contract normalization task failed: {error}"))
        })?
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
        TaskActor::RootSession(_) => return actor.require_root_for(&target.root_session_id),
        TaskActor::Attempt(actor_attempt_id) => actor_attempt_id,
    };
    let actor_attempt = load_attempt_tx(transaction, actor_attempt_id).await?;
    let current_actor_attempt =
        load_current_attempt_tx(transaction, actor_attempt.assignment_id).await?;
    if current_actor_attempt.attempt_id != actor_attempt_id
        || current_actor_attempt.state != AttemptState::Active
        || current_actor_attempt.sealed_at.is_some()
    {
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





async fn validate_declared_changes_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
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
        change.path = normalize_repo_path_async(repo_root, &change.path).await?;
        if !declared.insert(change.path.clone()) {
            return Err(StoreError::InvalidAssignment(format!(
                "duplicate declared change {}",
                change.path
            )));
        }
    }
    Ok(())
}

pub(crate) async fn heartbeat_typed_workspace_actor_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace_id: &str,
    binding: &AgentTaskBinding,
    progress: bool,
) -> StoreResult<bool> {
    let Some(thread_id) = binding
        .thread_id
        .as_deref()
        .filter(|thread_id| !thread_id.trim().is_empty())
    else {
        return Ok(false);
    };

    lock_assignment_tx(transaction, binding.assignment_id).await?;

    let now = Utc::now();
    let lease_expires_at = now + chrono::Duration::seconds(crate::DEFAULT_WORKSPACE_LEASE_SECONDS);
    // Liveness alone renews the lease. Progress also restarts the nonproductive-recovery
    // clock and its one-nudge budget, like a meaningful observation, without a wake event.
    let progress_at = progress.then(|| encode(&now)).transpose()?;
    let updated = sqlx::query(
        "UPDATE workspace_actors
         SET state = 'active', lease_expires_at = ?,
             last_progress_at = COALESCE(?, last_progress_at),
             nudge_sent_at = CASE WHEN ? IS NULL THEN nudge_sent_at ELSE NULL END
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
    .bind(&progress_at)
    .bind(&progress_at)
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
    scope_case: &CandidateScopeCase,
    isolated_integrator_available: bool,
) -> StoreResult<(AdmissionOverlapSummary, IntegrationPlan)> {
    // Admission and integration planning concern this root's team only.
    // Other sessions in the checkout do not constrain spawning.
    let rows = sqlx::query(
        "SELECT assignments.body_json, repositories.workspace_id, attempts.state,
                attempts.sealed_at
         FROM assignments
         JOIN assignment_repositories repositories USING (assignment_id)
         JOIN attempts ON attempts.assignment_id = assignments.assignment_id
         WHERE repositories.repository_id = ?
           AND assignments.root_session_id = ?
           AND attempts.state = ? AND attempts.sealed_at IS NULL
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

    let mut overlaps = AdmissionOverlapSummary::default();
    let mut conflicting_writer_count = 0_u32;
    let mut conflicting_writer_in_another_workspace = false;
    for row in rows {
        let existing: Assignment = decode(row.get::<String, _>("body_json").as_str())?;
        let existing_workspace_id = row.get::<String, _>("workspace_id");
        let existing_state: AttemptState = decode(row.get::<String, _>("state").as_str())?;
        let existing_is_active = existing_state == AttemptState::Active
            && row.try_get::<Option<String>, _>("sealed_at")?.is_none();
        // An identical task description is not an observed-input contract.
        // Active producers may already have read obsolete bytes. Until source
        // generations and dependency receipts are bound, admit fresh reads.

        if !existing_is_active {
            // Even persisted seal-time fingerprints cannot prove what the
            // producer read. Until observed inputs are bound to the result,
            // completed investigations never certify fresh reuse.
            continue;
        }
        let read_overlaps = existing.read_scope.iter().any(|existing_scope| {
            assignment
                .read_scope
                .iter()
                .enumerate()
                .any(|(index, scope)| {
                    scopes_overlap_observed(existing_scope, scope, scope_case.read(index))
                })
        });
        if read_overlaps {
            overlaps.benign_read_overlap_count =
                overlaps.benign_read_overlap_count.saturating_add(1);
        }
        // Only a candidate that writes can conflict with a writer. A read-only
        // candidate's contract claims stay advisory, like its read overlap.
        if existing.write_scope.is_empty() || assignment.write_scope.is_empty() {
            continue;
        }
        let write_scope_overlaps = existing.write_scope.iter().any(|existing_scope| {
            assignment
                .write_scope
                .iter()
                .enumerate()
                .any(|(index, scope)| {
                    scopes_overlap_observed(existing_scope, scope, scope_case.write(index))
                })
        });
        let contract_overlaps = existing.contract_claims.iter().any(|existing_contract| {
            assignment
                .contract_claims
                .iter()
                .any(|contract| contract == existing_contract)
        });
        if write_scope_overlaps || contract_overlaps {
            conflicting_writer_count = conflicting_writer_count.saturating_add(1);
            conflicting_writer_in_another_workspace |=
                existing_workspace_id != assignment.workspace_id;
        }
    }

    let integration_plan = if assignment.write_scope.is_empty() {
        IntegrationPlan::SingleWriter
    } else if assignment.workspace_strategy == WorkspaceStrategy::Isolated {
        if !isolated_integrator_available {
            return Err(StoreError::AdmissionRejected {
                reason: AdmissionRejectionReason::IsolatedIntegratorUnavailable,
                reusable_assignment_id: None,
            });
        }
        IntegrationPlan::TypedIntegratorRequired
    } else if conflicting_writer_count == 0 {
        IntegrationPlan::SingleWriter
    } else if conflicting_writer_in_another_workspace {
        if !isolated_integrator_available {
            return Err(StoreError::AdmissionRejected {
                reason: AdmissionRejectionReason::IsolatedIntegratorUnavailable,
                reusable_assignment_id: None,
            });
        }
        IntegrationPlan::TypedIntegratorRequired
    } else {
        IntegrationPlan::RootOwned
    };
    Ok((overlaps, integration_plan))
}

async fn finish_successful_task_if_unblocked_tx(
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
        finish_task_actor(transaction, assignment_id).await?;
    }
    Ok(())
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
    finish_task_actor(transaction, attempt.assignment_id).await
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

fn task_capsule_path(coordination_root: &Path, assignment_id: AssignmentId) -> PathBuf {
    coordination_root
        .join("task_capsules")
        .join(format!("{assignment_id}.json"))
}

/// Lock files are retained: deleting an unlocked pathname can erase a live
/// successor's lock. File drop releases the OS lease, including process death.
async fn try_capsule_lease(path: PathBuf) -> StoreResult<Option<std::fs::File>> {
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<std::fs::File>> {
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }).await.map_err(std::io::Error::other)?.map_err(StoreError::from)
}

fn task_capsule_staging_path(coordination_root: &Path, assignment_id: AssignmentId) -> PathBuf {
    coordination_root
        .join("task_capsules")
        .join(format!(".{assignment_id}.staged.json"))
}

async fn hydrate_task_capsule(
    pool: &SqlitePool,
    coordination_root: &Path,
    assignment: &mut Assignment,
) -> StoreResult<()> {
    let capsule_path = task_capsule_path(coordination_root, assignment.assignment_id);
    let (canonical_payload, restore_missing) = match tokio::fs::read_to_string(&capsule_path).await {
        Ok(payload) => (payload, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // The committed assignment copy is the attachment authority. A stage
            // without it is an uncommitted publication: attach stages, publishes
            // and only then commits that copy, while readers do not take the
            // assignment lock, so a concurrent read can observe the stage before
            // the published file. Startup reconciliation settles crash leftovers,
            // and attach still refuses to publish over an unresolved stage.
            let Some(payload) = assignment.task_capsule.as_ref() else { return Ok(()); };
            (payload.clone(), true)
        }
        Err(error) => return Err(error.into()),
    };
    let capsule: TaskCapsuleV1 = serde_json::from_str(&canonical_payload)?;
    let mut transaction = pool.begin().await?;
    validate_capsule_contract_tx(&mut transaction, assignment, &capsule).await?;
    transaction.commit().await?;
    if capsule.schema_version != 1 || capsule.assignment_id != assignment.assignment_id {
        return Err(StoreError::CorruptData(
            "persisted TaskCapsuleV1 identity or schema version is invalid".to_string(),
        ));
    }
    // Startup retains a mismatched published capsule instead of failing the
    // store, so reads must reject it here rather than rely on reconciliation.
    if capsule.integration_plan != assignment.integration_plan {
        return Err(StoreError::CorruptData(
            "persisted TaskCapsuleV1 integration plan does not match its assignment".to_string(),
        ));
    }
    if !is_canonical_persisted_task_capsule(&capsule, &canonical_payload)? {
        return Err(StoreError::CorruptData(
            if restore_missing {
                "noncanonical authoritative capsule"
            } else {
                "persisted TaskCapsuleV1 payload is not canonical"
            }.to_string(),
        ));
    }
    if assignment
        .task_capsule
        .as_ref()
        .is_some_and(|expected| expected != &canonical_payload)
    {
        return Err(StoreError::CorruptData(
            "task capsule does not match its authoritative assignment copy".to_string(),
        ));
    }
    if restore_missing {
        // Repair only after the common authority, identity and canonical-byte
        // checks. The immutable payload needs no second decode or SQL read.
        restore_missing_capsule(capsule_path, canonical_payload).await?;
    }
    // Publication precedes commit. A sidecar without the authoritative copy
    // cannot turn this database snapshot into an attachment receipt. Startup
    // reconciliation may settle crash leftovers under the assignment writer.
    Ok(())
}

fn is_canonical_persisted_task_capsule(
    capsule: &TaskCapsuleV1,
    payload: &str,
) -> StoreResult<bool> {
    let canonical = serde_json::to_string(capsule)?;
    if canonical == payload {
        return Ok(true);
    }

    // Capsules written before integration_plan was added are immutable. Their
    // assignments deserialize the missing field as SingleWriter too. Accept
    // exactly that earlier compact serialization without rewriting either copy.
    if capsule.integration_plan == IntegrationPlan::SingleWriter {
        const DEFAULT_PLAN_FIELD: &str = ",\"integration_plan\":\"single_writer\"";
        if let Some((before, after)) = canonical.split_once(DEFAULT_PLAN_FIELD) {
            return Ok(payload == format!("{before}{after}"));
        }
    }
    Ok(false)
}

async fn durable_wake_watermark(connection: &mut sqlx::SqliteConnection) -> StoreResult<i64> {
    // SQLite increments data_version on this connection whenever another
    // connection commits. Unlike a MAX(rowid) watermark, it also observes
    // delete-and-reinsert sequences that reuse a rowid.
    Ok(sqlx::query_scalar::<_, i64>("PRAGMA data_version")
        .fetch_one(connection)
        .await?)
}

async fn append_observation_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    kind: ObservationKind,
    summary: String,
    call_id: Option<String>,
) -> StoreResult<RuntimeObservation> {
    append_observation_with_stop_cause_tx(
        transaction,
        assignment,
        attempt_id,
        kind,
        summary,
        call_id,
        None,
    )
    .await
}

fn validate_host_completion_observation(
    origin: &str,
    summary: &str,
    stop_cause: Option<&str>,
) -> StoreResult<()> {
    if stop_cause.is_some_and(|cause| cause.is_empty() || cause.len() > 128) {
        return Err(StoreError::InvalidAssignment("invalid bounded execution stop cause".into()));
    }
    if origin.is_empty() || origin.len() > 256 || summary.is_empty() || summary.len() > 64 * 1024 {
        return Err(StoreError::InvalidAssignment("invalid bounded host completion observation".into()));
    }
    Ok(())
}

async fn append_host_completion_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    origin: String,
    summary: String,
    stop_cause: Option<String>,
) -> StoreResult<()> {
    let existing = sqlx::query_scalar::<_, String>(
        "SELECT body_json FROM observations WHERE attempt_id = ? AND kind = ? AND json_extract(body_json, '$.call_id') = ? LIMIT 1")
        .bind(attempt_id.to_string()).bind(encode(&ObservationKind::HostCompletion)?).bind(&origin)
        .fetch_optional(&mut **transaction).await?;
    if existing.is_none() {
        append_observation_with_stop_cause_tx(
            transaction, assignment, attempt_id, ObservationKind::HostCompletion,
            summary, Some(origin), stop_cause,
        ).await?;
    }
    Ok(())
}

async fn append_observation_with_stop_cause_tx(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment: &Assignment,
    attempt_id: AttemptId,
    kind: ObservationKind,
    summary: String,
    call_id: Option<String>,
    execution_stop_cause: Option<String>,
) -> StoreResult<RuntimeObservation> {
    let observation = RuntimeObservation {
        execution_stop_cause,
        event_id: MutationEventId::new(),
        wake_event_id: WakeEventId::new(),
        assignment_id: assignment.assignment_id,
        attempt_id,
        kind,
        summary,
        call_id,
        created_at: Utc::now(),
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
        "SELECT COUNT(*) FROM gates WHERE assignment_id = ? AND status = ? AND kind NOT IN (?, ?)",
    )
    .bind(assignment_id.to_string())
    .bind(encode(&GateStatus::Pending)?)
    .bind(encode(&GateKind::Review)?)
    .bind(encode(&GateKind::Verification)?)
    .fetch_one(&mut **transaction)
    .await?)
}

async fn finish_task_actor(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    assignment_id: AssignmentId,
) -> StoreResult<()> {
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
    assignment.effective_criteria(amendment)
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







fn hash_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn encode<T: Serialize + ?Sized>(value: &T) -> StoreResult<String> {
    Ok(serde_json::to_string(value)?)
}

fn decode<T: DeserializeOwned>(value: &str) -> StoreResult<T> {
    Ok(serde_json::from_str(value)?)
}
