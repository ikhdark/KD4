use chrono::Duration;
use chrono::Utc;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use uuid::Uuid;

use crate::AssignmentId;
use crate::AttemptId;
use crate::AttributionConfidence;
use crate::DEFAULT_WORKSPACE_LEASE_SECONDS;
use crate::LeaseState;
use crate::QuiescenceStatus;
use crate::RepoScope;
use crate::StoreError;
use crate::StoreResult;
use crate::WorkspaceActorKind;
use crate::WorkspaceActorRegistration;
use crate::WorkspaceFinalizationFence;
use crate::WorkspaceManifestEntry;
use crate::WorkspaceMutationLease;
use crate::WorkspaceMutationRequest;
use crate::WorkspaceMutationResult;
use crate::WorkspaceRevision;
use crate::WorkspaceStrategy;
use crate::scope::RepositoryIdentity;
use crate::scope::absolute_repo_path;
use crate::scope::normalize_repo_path;
use crate::scope::repository_identity;

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

/// Reserved manifest scope used by core-controlled writers whose shell command cannot be
/// narrowed to specific repository paths before execution.
pub const REPOSITORY_WIDE_PATH: &str = ":repository:";

pub(crate) async fn capture_revision(
    pool: &SqlitePool,
    repo_root: &Path,
    paths: Vec<String>,
) -> StoreResult<WorkspaceRevision> {
    let repository = repository_identity(repo_root)?;
    let normalized = normalize_paths(repo_root, paths)?;
    let root = repository.canonical_root.clone();
    let snapshot_paths = normalized.clone();
    let mut entries =
        tokio::task::spawn_blocking(move || collect_manifest_entries(&root, &snapshot_paths))
            .await
            .map_err(|error| StoreError::CorruptData(format!("manifest task failed: {error}")))??;
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;
    let epoch =
        reconcile_entries_tx(&mut transaction, &repository, &normalized, &mut entries).await?;
    transaction.commit().await?;
    revision(&repository, epoch, entries)
}

pub(crate) async fn record_supporting_read(
    pool: &SqlitePool,
    repo_root: &Path,
    actor_id: &str,
    paths: Vec<String>,
) -> StoreResult<WorkspaceRevision> {
    if actor_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "supporting read requires a durable actor identity".to_string(),
        ));
    }
    let revision = capture_revision(pool, repo_root, paths).await?;
    let now = Utc::now();
    let mut transaction = pool.begin().await?;
    for entry in &revision.files {
        sqlx::query(
            "INSERT INTO actor_supporting_reads (
                workspace_id, actor_id, path, manifest_entry_json, read_epoch, read_at
             ) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(workspace_id, actor_id, path) DO UPDATE SET
                manifest_entry_json = excluded.manifest_entry_json,
                read_epoch = excluded.read_epoch,
                read_at = excluded.read_at",
        )
        .bind(&revision.workspace_id)
        .bind(actor_id)
        .bind(&entry.path)
        .bind(json(entry)?)
        .bind(revision.epoch as i64)
        .bind(json(&now)?)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(revision)
}

pub(crate) async fn record_supporting_read_entries(
    pool: &SqlitePool,
    repo_root: &Path,
    actor_id: &str,
    mut entries: Vec<WorkspaceManifestEntry>,
) -> StoreResult<WorkspaceRevision> {
    if actor_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "supporting read requires a durable actor identity".to_string(),
        ));
    }
    if entries.is_empty() {
        return Err(StoreError::InvalidAssignment(
            "supporting read requires at least one manifest entry".to_string(),
        ));
    }
    for entry in &mut entries {
        entry.path = normalize_repo_path(repo_root, &entry.path)?;
    }
    entries.sort();
    entries.dedup_by(|left, right| left.path == right.path);
    let revision = capture_revision(
        pool,
        repo_root,
        entries.iter().map(|entry| entry.path.clone()).collect(),
    )
    .await?;
    let now = Utc::now();
    let mut transaction = pool.begin().await?;
    for entry in &entries {
        sqlx::query(
            "INSERT INTO actor_supporting_reads (
                workspace_id, actor_id, path, manifest_entry_json, read_epoch, read_at
             ) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(workspace_id, actor_id, path) DO UPDATE SET
                manifest_entry_json = excluded.manifest_entry_json,
                read_epoch = excluded.read_epoch,
                read_at = excluded.read_at",
        )
        .bind(&revision.workspace_id)
        .bind(actor_id)
        .bind(&entry.path)
        .bind(json(entry)?)
        .bind(revision.epoch as i64)
        .bind(json(&now)?)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    let manifest_hash = format!("{:x}", Sha256::digest(json(&entries)?.as_bytes()));
    Ok(WorkspaceRevision {
        repository_id: revision.repository_id,
        workspace_id: revision.workspace_id,
        epoch: revision.epoch,
        manifest_hash,
        files: entries,
    })
}

pub(crate) async fn supporting_read_manifest(
    pool: &SqlitePool,
    repo_root: &Path,
    actor_id: &str,
    paths: Vec<String>,
) -> StoreResult<Vec<WorkspaceManifestEntry>> {
    if actor_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "supporting read requires a durable actor identity".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let paths = normalize_paths(repo_root, paths)?;
    let mut entries = Vec::new();
    for path in paths {
        let body = sqlx::query_scalar::<_, String>(
            "SELECT manifest_entry_json FROM actor_supporting_reads
             WHERE workspace_id = ? AND actor_id = ? AND path = ?",
        )
        .bind(&repository.workspace_id)
        .bind(actor_id)
        .bind(path)
        .fetch_optional(pool)
        .await?;
        if let Some(body) = body {
            entries.push(serde_json::from_str(&body)?);
        }
    }
    entries.sort();
    Ok(entries)
}

pub(crate) async fn read_events(
    pool: &SqlitePool,
    repo_root: &Path,
    after_epoch: u64,
) -> StoreResult<Vec<crate::WorkspaceEvent>> {
    let repository = repository_identity(repo_root)?;
    let rows = sqlx::query(
        "SELECT workspace_id, epoch, actor_id, actor_kind, attribution_confidence,
                paths_json, contracts_json, created_at
         FROM workspace_events
         WHERE workspace_id = ? AND epoch > ?
         ORDER BY epoch",
    )
    .bind(&repository.workspace_id)
    .bind(after_epoch as i64)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(crate::WorkspaceEvent {
                workspace_id: row.get("workspace_id"),
                epoch: u64::try_from(row.get::<i64, _>("epoch")).map_err(|_| {
                    StoreError::CorruptData("workspace event epoch is negative".to_string())
                })?,
                actor_id: row.get("actor_id"),
                actor_kind: serde_json::from_str(row.get::<String, _>("actor_kind").as_str())?,
                attribution_confidence: serde_json::from_str(
                    row.get::<String, _>("attribution_confidence").as_str(),
                )?,
                paths: serde_json::from_str(row.get::<String, _>("paths_json").as_str())?,
                contracts: serde_json::from_str(row.get::<String, _>("contracts_json").as_str())?,
                created_at: serde_json::from_str(row.get::<String, _>("created_at").as_str())?,
            })
        })
        .collect()
}

pub(crate) async fn register_actor(
    pool: &SqlitePool,
    repo_root: &Path,
    registration: WorkspaceActorRegistration,
) -> StoreResult<()> {
    if registration.actor_id.trim().is_empty() || registration.root_session_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace actor and root session identities are required".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO workspace_actors (
            workspace_id, actor_id, root_session_id, kind, assignment_id, attempt_id,
            strategy, state, last_progress_at, lease_expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, 'idle', ?, NULL)
         ON CONFLICT(workspace_id, actor_id) DO UPDATE SET
            root_session_id = excluded.root_session_id,
            kind = excluded.kind,
            assignment_id = COALESCE(excluded.assignment_id, workspace_actors.assignment_id),
            attempt_id = COALESCE(excluded.attempt_id, workspace_actors.attempt_id),
            strategy = excluded.strategy,
            state = CASE
                WHEN workspace_actors.state = 'terminal' THEN workspace_actors.state
                ELSE 'idle'
            END,
            last_progress_at = excluded.last_progress_at",
    )
    .bind(&repository.workspace_id)
    .bind(&registration.actor_id)
    .bind(&registration.root_session_id)
    .bind(json(&registration.kind)?)
    .bind(registration.assignment_id.map(|id| id.to_string()))
    .bind(registration.attempt_id.map(|id| id.to_string()))
    .bind(json(&registration.strategy)?)
    .bind(json(&now)?)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

pub(crate) async fn begin_mutation(
    pool: &SqlitePool,
    repo_root: &Path,
    request: WorkspaceMutationRequest,
) -> StoreResult<WorkspaceMutationLease> {
    if request.actor_id.trim().is_empty() || request.root_session_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace mutation actor and root session identities are required".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let paths = normalize_paths(repo_root, request.paths)?;
    if paths.is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace mutation requires at least one path".to_string(),
        ));
    }
    let root = repository.canonical_root.clone();
    let snapshot_paths = paths.clone();
    let mut entries =
        tokio::task::spawn_blocking(move || collect_manifest_entries(&root, &snapshot_paths))
            .await
            .map_err(|error| StoreError::CorruptData(format!("manifest task failed: {error}")))??;

    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_mutation_leases_tx(&mut transaction, &repository.workspace_id).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;
    if let Some(finalizing_root_session_id) = sqlx::query_scalar::<_, String>(
        "SELECT root_session_id FROM workspace_finalization_fences
         WHERE workspace_id = ? AND state IN ('active', 'dispatching')
           AND julianday(json_extract(expires_at, '$')) > julianday('now')
         LIMIT 1",
    )
    .bind(&repository.workspace_id)
    .fetch_optional(&mut *transaction)
    .await?
    {
        return Err(StoreError::WorkspaceFinalizationActive {
            root_session_id: finalizing_root_session_id,
        });
    }
    if request.kind == WorkspaceActorKind::Typed {
        let Some(attempt_id) = request.attempt_id else {
            return Err(StoreError::InvalidAssignment(
                "typed workspace mutation requires an attempt identity".to_string(),
            ));
        };
        if request.actor_id != format!("attempt:{attempt_id}") {
            return Err(StoreError::AttemptNotActive(attempt_id));
        }
        let binding_row = sqlx::query(
            "SELECT assignment_id, attempt_id, root_session_id, agent_path, task_name, thread_id,
                    bound_at, updated_at
             FROM agent_task_bindings
             WHERE attempt_id = ?",
        )
        .bind(attempt_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(binding_row) = binding_row else {
            return Err(StoreError::AttemptNotActive(attempt_id));
        };
        let binding = crate::local::binding_from_row(&binding_row)?;
        if binding.root_session_id != request.root_session_id
            || !crate::local::heartbeat_typed_workspace_actor_tx(
                &mut transaction,
                &repository.workspace_id,
                &binding,
            )
            .await?
        {
            return Err(StoreError::AttemptNotActive(attempt_id));
        }
    }
    crate::local::release_orphaned_claims_tx(&mut transaction, &repository.workspace_id).await?;
    let start_epoch =
        reconcile_entries_tx(&mut transaction, &repository, &paths, &mut entries).await?;
    require_claim_authority_tx(
        &mut transaction,
        &repository.workspace_id,
        &request.root_session_id,
        request.attempt_id,
        &paths,
        &request.contracts,
    )
    .await?;
    require_no_mutation_lease_overlap_tx(
        &mut transaction,
        &repository.workspace_id,
        &request.root_session_id,
        &request.actor_id,
        &paths,
        &request.contracts,
    )
    .await?;

    let expected_manifest = if request.expected_manifest.is_empty() {
        entries.clone()
    } else {
        let expected = request
            .expected_manifest
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.path = normalize_repo_path(repo_root, &entry.path)?;
                Ok(entry)
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let mismatches = changed_manifest_paths(&expected, &entries);
        if !mismatches.is_empty() {
            return Err(StoreError::WorkspaceCasMismatch { paths: mismatches });
        }
        expected
    };

    let now = comparison_now();
    let expires_at = now + Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    let lease_id = Uuid::now_v7().to_string();
    let assignment_id = match request.attempt_id {
        Some(attempt_id) => {
            sqlx::query_scalar::<_, String>(
                "SELECT assignment_id FROM attempts WHERE attempt_id = ?",
            )
            .bind(attempt_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
        }
        None => None,
    };
    sqlx::query(
        "INSERT INTO workspace_actors (
            workspace_id, actor_id, root_session_id, kind, assignment_id, attempt_id,
            strategy, state, last_progress_at, lease_expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)
         ON CONFLICT(workspace_id, actor_id) DO UPDATE SET
            root_session_id = excluded.root_session_id,
            kind = excluded.kind,
            assignment_id = COALESCE(excluded.assignment_id, workspace_actors.assignment_id),
            attempt_id = COALESCE(excluded.attempt_id, workspace_actors.attempt_id),
            state = 'active',
            last_progress_at = excluded.last_progress_at,
            lease_expires_at = excluded.lease_expires_at",
    )
    .bind(&repository.workspace_id)
    .bind(&request.actor_id)
    .bind(&request.root_session_id)
    .bind(json(&request.kind)?)
    .bind(assignment_id)
    .bind(request.attempt_id.map(|id| id.to_string()))
    .bind(json(&WorkspaceStrategy::Shared)?)
    .bind(json(&now)?)
    .bind(json(&expires_at)?)
    .execute(&mut *transaction)
    .await?;
    sqlx::query(
        "INSERT INTO workspace_mutation_leases (
            lease_id, workspace_id, root_session_id, actor_id, actor_kind, attempt_id, start_epoch,
            paths_json, contracts_json, expected_manifest_json, state, created_at,
            heartbeat_at, expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?)",
    )
    .bind(&lease_id)
    .bind(&repository.workspace_id)
    .bind(&request.root_session_id)
    .bind(&request.actor_id)
    .bind(json(&request.kind)?)
    .bind(request.attempt_id.map(|id| id.to_string()))
    .bind(start_epoch as i64)
    .bind(json(&paths)?)
    .bind(json(&request.contracts)?)
    .bind(json(&expected_manifest)?)
    .bind(json(&now)?)
    .bind(json(&now)?)
    .bind(json(&expires_at)?)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;

    Ok(WorkspaceMutationLease {
        lease_id,
        repository_id: repository.id,
        workspace_id: repository.workspace_id,
        root_session_id: request.root_session_id,
        actor_id: request.actor_id,
        kind: request.kind,
        attempt_id: request.attempt_id,
        start_epoch,
        paths,
        contracts: request.contracts,
        expected_manifest,
        state: LeaseState::Active,
        expires_at,
    })
}

pub(crate) async fn begin_finalization(
    pool: &SqlitePool,
    repo_root: &Path,
    root_session_id: &str,
) -> StoreResult<WorkspaceFinalizationFence> {
    if root_session_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace finalization requires a root session identity".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let now = comparison_now();
    let expires_at = now + Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_mutation_leases_tx(&mut transaction, &repository.workspace_id).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;

    if let Some(finalizing_root_session_id) = sqlx::query_scalar::<_, String>(
        "SELECT root_session_id FROM workspace_finalization_fences
         WHERE workspace_id = ? AND state IN ('active', 'dispatching')
           AND julianday(json_extract(expires_at, '$')) > julianday('now')
         LIMIT 1",
    )
    .bind(&repository.workspace_id)
    .fetch_optional(&mut *transaction)
    .await?
    {
        return Err(StoreError::WorkspaceFinalizationActive {
            root_session_id: finalizing_root_session_id,
        });
    }

    let active_lease_ids = sqlx::query_scalar::<_, String>(
        "SELECT lease_id FROM workspace_mutation_leases
         WHERE workspace_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) >= julianday(json_extract(?, '$'))
         ORDER BY lease_id",
    )
    .bind(&repository.workspace_id)
    .bind(json(&now)?)
    .fetch_all(&mut *transaction)
    .await?;
    if !active_lease_ids.is_empty() {
        return Err(StoreError::WorkspaceFinalizationNotQuiescent {
            lease_ids: active_lease_ids,
        });
    }

    let fence_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO workspace_finalization_fences (
            fence_id, workspace_id, root_session_id, state, created_at, expires_at
         ) VALUES (?, ?, ?, 'active', ?, ?)",
    )
    .bind(&fence_id)
    .bind(&repository.workspace_id)
    .bind(root_session_id)
    .bind(json(&now)?)
    .bind(json(&expires_at)?)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(WorkspaceFinalizationFence {
        fence_id,
        repository_id: repository.id,
        workspace_id: repository.workspace_id,
        root_session_id: root_session_id.to_string(),
        expires_at,
    })
}

pub(crate) async fn seal_finalization_dispatch(
    pool: &SqlitePool,
    repo_root: &Path,
    mut fence: WorkspaceFinalizationFence,
) -> StoreResult<WorkspaceFinalizationFence> {
    let repository = repository_identity(repo_root)?;
    if repository.id != fence.repository_id || repository.workspace_id != fence.workspace_id {
        return Err(StoreError::WorkspaceStateInitialization(
            "workspace finalization fence belongs to a different repository".to_string(),
        ));
    }
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;
    let expires_at = Utc::now() + Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    let updated = sqlx::query(
        "UPDATE workspace_finalization_fences
         SET state = 'dispatching', expires_at = ?
         WHERE fence_id = ? AND workspace_id = ? AND root_session_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) > julianday('now')",
    )
    .bind(json(&expires_at)?)
    .bind(&fence.fence_id)
    .bind(&fence.workspace_id)
    .bind(&fence.root_session_id)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StoreError::WorkspaceLeaseUnavailable(fence.fence_id));
    }
    transaction.commit().await?;
    fence.expires_at = expires_at;
    Ok(fence)
}

pub(crate) async fn release_finalization(
    pool: &SqlitePool,
    repo_root: &Path,
    fence: WorkspaceFinalizationFence,
) -> StoreResult<()> {
    let repository = repository_identity(repo_root)?;
    if repository.id != fence.repository_id || repository.workspace_id != fence.workspace_id {
        return Err(StoreError::WorkspaceStateInitialization(
            "workspace finalization fence belongs to a different repository".to_string(),
        ));
    }
    let updated = sqlx::query(
        "UPDATE workspace_finalization_fences
         SET state = 'released', released_at = ?
         WHERE fence_id = ? AND workspace_id = ? AND root_session_id = ?
           AND state IN ('active', 'dispatching')",
    )
    .bind(json(&Utc::now())?)
    .bind(&fence.fence_id)
    .bind(&fence.workspace_id)
    .bind(&fence.root_session_id)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(StoreError::WorkspaceLeaseUnavailable(fence.fence_id));
    }
    Ok(())
}

pub(crate) async fn heartbeat_finalization(
    pool: &SqlitePool,
    repo_root: &Path,
    fence_id: &str,
    root_session_id: &str,
) -> StoreResult<bool> {
    if fence_id.trim().is_empty() || root_session_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace finalization heartbeat requires fence and root identities".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_finalization_fences_tx(&mut transaction, &repository.workspace_id).await?;
    let expires_at = Utc::now() + Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    let updated = sqlx::query(
        "UPDATE workspace_finalization_fences
         SET expires_at = ?
         WHERE fence_id = ? AND workspace_id = ? AND root_session_id = ?
           AND state IN ('active', 'dispatching')
           AND julianday(json_extract(expires_at, '$')) > julianday('now')",
    )
    .bind(json(&expires_at)?)
    .bind(fence_id)
    .bind(&repository.workspace_id)
    .bind(root_session_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(updated.rows_affected() == 1)
}

pub(crate) async fn heartbeat_mutation(
    pool: &SqlitePool,
    repo_root: &Path,
    lease_id: &str,
    actor_id: &str,
) -> StoreResult<bool> {
    if lease_id.trim().is_empty() || actor_id.trim().is_empty() {
        return Err(StoreError::InvalidAssignment(
            "workspace mutation heartbeat requires lease and actor identities".to_string(),
        ));
    }
    let repository = repository_identity(repo_root)?;
    let now = Utc::now();
    let expires_at = now + Duration::seconds(DEFAULT_WORKSPACE_LEASE_SECONDS);
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    expire_mutation_leases_tx(&mut transaction, &repository.workspace_id).await?;
    let updated = sqlx::query(
        "UPDATE workspace_mutation_leases
         SET heartbeat_at = ?, expires_at = ?
         WHERE lease_id = ? AND workspace_id = ? AND actor_id = ? AND state = 'active'",
    )
    .bind(json(&now)?)
    .bind(json(&expires_at)?)
    .bind(lease_id)
    .bind(&repository.workspace_id)
    .bind(actor_id)
    .execute(&mut *transaction)
    .await?;
    if updated.rows_affected() == 1 {
        sqlx::query(
            "UPDATE workspace_actors
             SET state = 'active', last_progress_at = ?, lease_expires_at = ?
             WHERE workspace_id = ? AND actor_id = ?",
        )
        .bind(json(&now)?)
        .bind(json(&expires_at)?)
        .bind(&repository.workspace_id)
        .bind(actor_id)
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(updated.rows_affected() == 1)
}

pub(crate) async fn finish_mutation(
    pool: &SqlitePool,
    repo_root: &Path,
    lease: WorkspaceMutationLease,
) -> StoreResult<WorkspaceMutationResult> {
    let repository = repository_identity(repo_root)?;
    let lease_id = lease.lease_id;
    let persisted = sqlx::query(
        "SELECT root_session_id, actor_id, actor_kind, attempt_id, start_epoch,
                paths_json, contracts_json, expected_manifest_json, state, expires_at
         FROM workspace_mutation_leases
         WHERE lease_id = ? AND workspace_id = ?",
    )
    .bind(&lease_id)
    .bind(&repository.workspace_id)
    .fetch_optional(pool)
    .await?;
    let Some(persisted) = persisted else {
        return Err(StoreError::WorkspaceLeaseUnavailable(lease_id));
    };
    let lease = WorkspaceMutationLease {
        lease_id,
        repository_id: repository.id.clone(),
        workspace_id: repository.workspace_id.clone(),
        root_session_id: persisted.get("root_session_id"),
        actor_id: persisted.get("actor_id"),
        kind: from_json(&persisted.get::<String, _>("actor_kind"))?,
        attempt_id: persisted
            .get::<Option<String>, _>("attempt_id")
            .map(|value| AttemptId::parse(&value))
            .transpose()?,
        start_epoch: u64::try_from(persisted.get::<i64, _>("start_epoch")).map_err(|_| {
            StoreError::CorruptData("workspace mutation start epoch is negative".to_string())
        })?,
        paths: from_json(&persisted.get::<String, _>("paths_json"))?,
        contracts: from_json(&persisted.get::<String, _>("contracts_json"))?,
        expected_manifest: from_json(&persisted.get::<String, _>("expected_manifest_json"))?,
        state: match persisted.get::<String, _>("state").as_str() {
            "active" => LeaseState::Active,
            "expired" => LeaseState::Expired,
            "released" => LeaseState::Released,
            value => {
                return Err(StoreError::CorruptData(format!(
                    "unknown workspace mutation lease state {value}"
                )));
            }
        },
        expires_at: from_json(&persisted.get::<String, _>("expires_at"))?,
    };
    if lease.state != LeaseState::Active {
        return Err(StoreError::WorkspaceLeaseUnavailable(lease.lease_id));
    }
    let root = repository.canonical_root.clone();
    let paths = lease.paths.clone();
    let expected_entries = lease.expected_manifest.clone();
    let final_entries = tokio::task::spawn_blocking(move || {
        let mut entries = collect_manifest_entries(&root, &paths)?;
        include_manifest_paths(&root, &expected_entries, &mut entries)?;
        Ok::<_, StoreError>(entries)
    })
    .await
    .map_err(|error| StoreError::CorruptData(format!("manifest task failed: {error}")))??;
    let changed_paths = changed_manifest_paths(&lease.expected_manifest, &final_entries);

    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    let row = sqlx::query(
        "SELECT state, expires_at FROM workspace_mutation_leases
         WHERE lease_id = ? AND workspace_id = ?",
    )
    .bind(&lease.lease_id)
    .bind(&repository.workspace_id)
    .fetch_optional(&mut *transaction)
    .await?;
    let Some(row) = row else {
        return Err(StoreError::WorkspaceLeaseUnavailable(lease.lease_id));
    };
    let state = row.get::<String, _>("state");
    let expires_at: chrono::DateTime<Utc> = from_json(&row.get::<String, _>("expires_at"))?;
    if state != "active" || expires_at < Utc::now() {
        sqlx::query(
            "UPDATE workspace_mutation_leases SET state = 'expired', released_at = ?
             WHERE lease_id = ? AND state = 'active'",
        )
        .bind(json(&Utc::now())?)
        .bind(&lease.lease_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        return Err(StoreError::WorkspaceLeaseUnavailable(lease.lease_id));
    }

    let current_epoch = current_epoch_tx(&mut transaction, &repository.workspace_id).await?;
    let end_epoch = if changed_paths.is_empty() {
        current_epoch
    } else {
        let next = current_epoch + 1;
        record_workspace_event_tx(
            &mut transaction,
            WorkspaceEventDraft {
                workspace_id: &repository.workspace_id,
                epoch: next,
                actor_id: Some(&lease.actor_id),
                actor_kind: lease.kind,
                confidence: AttributionConfidence::Definitive,
                paths: &changed_paths,
                contracts: &lease.contracts,
            },
        )
        .await?;
        update_workspace_entries_tx(
            &mut transaction,
            &repository.workspace_id,
            next,
            Some(&lease.actor_id),
            AttributionConfidence::Definitive,
            &final_entries,
        )
        .await?;
        set_epoch_tx(&mut transaction, &repository.workspace_id, next).await?;
        next
    };
    let now = comparison_now();
    sqlx::query(
        "UPDATE workspace_mutation_leases
         SET state = 'released', released_at = ?, heartbeat_at = ?
         WHERE lease_id = ? AND state = 'active'",
    )
    .bind(json(&now)?)
    .bind(json(&now)?)
    .bind(&lease.lease_id)
    .execute(&mut *transaction)
    .await?;
    let remaining_expirations = sqlx::query_scalar::<_, String>(
        "SELECT expires_at FROM workspace_mutation_leases
         WHERE workspace_id = ? AND actor_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) >= julianday(json_extract(?, '$'))",
    )
    .bind(&repository.workspace_id)
    .bind(&lease.actor_id)
    .bind(json(&now)?)
    .fetch_all(&mut *transaction)
    .await?;
    let remaining_expiration = remaining_expirations
        .into_iter()
        .map(|value| from_json::<chrono::DateTime<Utc>>(&value))
        .collect::<StoreResult<Vec<_>>>()?
        .into_iter()
        .max();
    sqlx::query(
        "UPDATE workspace_actors
         SET state = ?, last_progress_at = ?, lease_expires_at = ?
         WHERE workspace_id = ? AND actor_id = ?",
    )
    .bind(if remaining_expiration.is_some() {
        "active"
    } else {
        "idle"
    })
    .bind(json(&now)?)
    .bind(remaining_expiration.as_ref().map(json).transpose()?)
    .bind(&repository.workspace_id)
    .bind(&lease.actor_id)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(WorkspaceMutationResult {
        lease_id: lease.lease_id,
        start_epoch: lease.start_epoch,
        end_epoch,
        changed_paths,
        drift_paths: Vec::new(),
    })
}

pub(crate) async fn assert_unclaimed(
    pool: &SqlitePool,
    repo_root: &Path,
    actor_attempt_id: Option<AttemptId>,
) -> StoreResult<()> {
    let repository = repository_identity(repo_root)?;
    let mut transaction = pool.begin().await?;
    ensure_workspace_tx(&mut transaction, &repository).await?;
    crate::local::release_orphaned_claims_tx(&mut transaction, &repository.workspace_id).await?;
    let mut conflicts = Vec::new();
    let rows = sqlx::query(
        "SELECT write_claims.assignment_id, write_claims.attempt_id
         FROM write_claims
         JOIN assignment_repositories USING (assignment_id)
         WHERE write_claims.active = 1 AND assignment_repositories.workspace_id = ?",
    )
    .bind(&repository.workspace_id)
    .fetch_all(&mut *transaction)
    .await?;
    for row in rows {
        let attempt = AttemptId::parse(&row.get::<String, _>("attempt_id"))?;
        if Some(attempt) != actor_attempt_id {
            conflicts.push(format!(
                "path claim held by assignment {}",
                row.get::<String, _>("assignment_id")
            ));
        }
    }
    let rows = sqlx::query(
        "SELECT contract_name, assignment_id, attempt_id FROM contract_claims
         WHERE workspace_id = ? AND active = 1",
    )
    .bind(&repository.workspace_id)
    .fetch_all(&mut *transaction)
    .await?;
    for row in rows {
        let attempt = AttemptId::parse(&row.get::<String, _>("attempt_id"))?;
        if Some(attempt) != actor_attempt_id {
            conflicts.push(format!(
                "contract {} held by assignment {}",
                row.get::<String, _>("contract_name"),
                row.get::<String, _>("assignment_id")
            ));
        }
    }
    transaction.commit().await?;
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(StoreError::WorkspaceClaimConflict { details: conflicts })
    }
}

pub(crate) async fn quiescence(
    pool: &SqlitePool,
    root_session_id: &str,
) -> StoreResult<QuiescenceStatus> {
    let mut transaction = pool.begin().await?;
    let workspace_ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT assignment_repositories.workspace_id
         FROM assignments
         JOIN assignment_repositories USING (assignment_id)
         WHERE assignments.root_session_id = ?",
    )
    .bind(root_session_id)
    .fetch_all(&mut *transaction)
    .await?;
    for workspace_id in workspace_ids {
        crate::local::release_orphaned_claims_tx(&mut transaction, &workspace_id).await?;
    }
    transaction.commit().await?;

    let now = comparison_now();
    let encoded_now = json(&now)?;
    sqlx::query(
        "UPDATE workspace_mutation_leases
         SET state = 'expired', released_at = ?
         WHERE root_session_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) < julianday(json_extract(?, '$'))",
    )
    .bind(&encoded_now)
    .bind(root_session_id)
    .bind(&encoded_now)
    .execute(pool)
    .await?;
    inspect_quiescence(pool, root_session_id).await
}

pub(crate) async fn inspect_quiescence(
    pool: &SqlitePool,
    root_session_id: &str,
) -> StoreResult<QuiescenceStatus> {
    let now = comparison_now();
    let encoded_now = json(&now)?;
    let active_assignment_ids = query_assignment_ids(
        pool,
        "SELECT DISTINCT assignments.assignment_id
         FROM assignments
         JOIN attempts USING (assignment_id)
         WHERE assignments.root_session_id = ? AND attempts.state = '\"active\"'",
        root_session_id,
    )
    .await?;
    let validation_rows = sqlx::query(
        "SELECT validation_calls.call_id
         FROM validation_calls
         JOIN attempts USING (attempt_id)
         JOIN assignments USING (assignment_id)
         WHERE assignments.root_session_id = ? AND validation_calls.status = '\"running\"'",
    )
    .bind(root_session_id)
    .fetch_all(pool)
    .await?;
    let running_validation_call_ids = validation_rows
        .into_iter()
        .map(|row| row.get::<String, _>("call_id"))
        .collect::<Vec<_>>();
    let pending_gate_assignment_ids = query_assignment_ids(
        pool,
        "SELECT DISTINCT assignments.assignment_id
         FROM assignments
         JOIN gates USING (assignment_id)
         WHERE assignments.root_session_id = ? AND gates.status = '\"pending\"'",
        root_session_id,
    )
    .await?;
    let active_claim_assignment_ids = query_assignment_ids(
        pool,
        "SELECT DISTINCT assignments.assignment_id
         FROM assignments
         JOIN (
             SELECT assignment_id FROM write_claims WHERE active = 1
             UNION
             SELECT assignment_id FROM contract_claims WHERE active = 1
         ) active_claims USING (assignment_id)
         WHERE assignments.root_session_id = ?",
        root_session_id,
    )
    .await?;
    let active_mutation_lease_ids = sqlx::query_scalar::<_, String>(
        "SELECT lease_id FROM workspace_mutation_leases
         WHERE root_session_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) >= julianday(json_extract(?, '$'))
         ORDER BY lease_id",
    )
    .bind(root_session_id)
    .bind(&encoded_now)
    .fetch_all(pool)
    .await?;
    let linked_repository_ids = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT assignment_repositories.repository_id
         FROM assignments
         JOIN assignment_repositories USING (assignment_id)
         WHERE assignments.root_session_id = ?
         ORDER BY assignment_repositories.repository_id",
    )
    .bind(root_session_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();
    if !linked_repository_ids.is_empty() {
        let unrelated_assignment_rows = sqlx::query(
            "SELECT DISTINCT assignments.assignment_id, assignments.root_session_id,
                    assignment_repositories.repository_id
             FROM assignments
             JOIN assignment_repositories USING (assignment_id)
             WHERE assignments.root_session_id <> ?
               AND (
                   EXISTS (
                       SELECT 1 FROM attempts
                       WHERE attempts.assignment_id = assignments.assignment_id
                         AND attempts.state = '\"active\"'
                   )
                   OR EXISTS (
                       SELECT 1
                       FROM validation_calls
                       JOIN attempts validation_attempts USING (attempt_id)
                       WHERE validation_attempts.assignment_id = assignments.assignment_id
                         AND validation_calls.status = '\"running\"'
                   )
                   OR EXISTS (
                       SELECT 1 FROM gates
                       WHERE gates.assignment_id = assignments.assignment_id
                         AND gates.status = '\"pending\"'
                   )
                   OR EXISTS (
                       SELECT 1 FROM write_claims
                       WHERE write_claims.assignment_id = assignments.assignment_id
                         AND write_claims.active = 1
                   )
                   OR EXISTS (
                       SELECT 1 FROM contract_claims
                       WHERE contract_claims.assignment_id = assignments.assignment_id
                         AND contract_claims.active = 1
                   )
               )
             ORDER BY assignments.root_session_id, assignments.assignment_id",
        )
        .bind(root_session_id)
        .fetch_all(pool)
        .await?;
        for row in unrelated_assignment_rows {
            let repository_id = row.get::<String, _>("repository_id");
            if linked_repository_ids.contains(&repository_id) {
                warnings.push(format!(
                    "unrelated root {} still has active assignment {} in repository lineage {}",
                    row.get::<String, _>("root_session_id"),
                    row.get::<String, _>("assignment_id"),
                    repository_id
                ));
            }
        }
        let unrelated_mutation_rows = sqlx::query(
            "SELECT workspace_mutation_leases.lease_id,
                    workspace_mutation_leases.root_session_id,
                    workspace_repositories.repository_id
             FROM workspace_mutation_leases
             JOIN workspace_repositories USING (workspace_id)
             WHERE workspace_mutation_leases.root_session_id <> ?
               AND workspace_mutation_leases.state = 'active'
               AND julianday(json_extract(workspace_mutation_leases.expires_at, '$')) >= julianday(json_extract(?, '$'))
             ORDER BY workspace_mutation_leases.root_session_id,
                      workspace_mutation_leases.lease_id",
        )
        .bind(root_session_id)
        .bind(&encoded_now)
        .fetch_all(pool)
        .await?;
        for row in unrelated_mutation_rows {
            let repository_id = row.get::<String, _>("repository_id");
            if linked_repository_ids.contains(&repository_id) {
                warnings.push(format!(
                    "unrelated root {} has active mutation lease {} in repository lineage {}",
                    row.get::<String, _>("root_session_id"),
                    row.get::<String, _>("lease_id"),
                    repository_id
                ));
            }
        }
    }
    let quiescent = active_assignment_ids.is_empty()
        && running_validation_call_ids.is_empty()
        && pending_gate_assignment_ids.is_empty()
        && active_claim_assignment_ids.is_empty()
        && active_mutation_lease_ids.is_empty();
    Ok(QuiescenceStatus {
        quiescent,
        active_assignment_ids,
        running_validation_call_ids,
        pending_gate_assignment_ids,
        active_claim_assignment_ids,
        active_mutation_lease_ids,
        warnings,
    })
}

pub(crate) async fn ensure_workspace_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &RepositoryIdentity,
) -> StoreResult<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO workspace_repositories (
            workspace_id, repository_id, canonical_root, epoch, updated_at
         ) VALUES (?, ?, ?, 0, ?)
         ON CONFLICT(workspace_id) DO NOTHING",
    )
    .bind(&repository.workspace_id)
    .bind(&repository.id)
    .bind(&repository.canonical_path)
    .bind(json(&now)?)
    .execute(&mut **transaction)
    .await?;
    let row = sqlx::query(
        "SELECT repository_id, canonical_root FROM workspace_repositories WHERE workspace_id = ?",
    )
    .bind(&repository.workspace_id)
    .fetch_one(&mut **transaction)
    .await?;
    let stored_repository_id = row.get::<String, _>("repository_id");
    if !paths_equal(
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
        .bind(json(&now)?)
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

pub(crate) async fn current_epoch_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
) -> StoreResult<u64> {
    let value = sqlx::query_scalar::<_, i64>(
        "SELECT epoch FROM workspace_repositories WHERE workspace_id = ?",
    )
    .bind(workspace_id)
    .fetch_one(&mut **transaction)
    .await?;
    u64::try_from(value)
        .map_err(|_| StoreError::CorruptData("workspace epoch is negative".to_string()))
}

pub(crate) fn revision(
    repository: &RepositoryIdentity,
    epoch: u64,
    mut files: Vec<WorkspaceManifestEntry>,
) -> StoreResult<WorkspaceRevision> {
    files.sort();
    let manifest_hash = format!("{:x}", Sha256::digest(json(&files)?.as_bytes()));
    Ok(WorkspaceRevision {
        repository_id: repository.id.clone(),
        workspace_id: repository.workspace_id.clone(),
        epoch,
        manifest_hash,
        files,
    })
}

pub(crate) fn changed_manifest_paths(
    before: &[WorkspaceManifestEntry],
    after: &[WorkspaceManifestEntry],
) -> Vec<String> {
    let before = before
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    let after = after
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<std::collections::BTreeMap<_, _>>();
    before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(path).copied() != after.get(path).copied())
        .map(str::to_string)
        .collect()
}

fn normalize_paths(repo_root: &Path, paths: Vec<String>) -> StoreResult<Vec<String>> {
    let mut normalized = paths
        .into_iter()
        .map(|path| {
            if path == REPOSITORY_WIDE_PATH {
                Ok(path)
            } else {
                normalize_repo_path(repo_root, &path)
            }
        })
        .collect::<StoreResult<BTreeSet<_>>>()?
        .into_iter()
        .collect::<Vec<_>>();
    normalized.sort();
    Ok(normalized)
}

fn collect_manifest_entries(
    root: &Path,
    paths: &[String],
) -> StoreResult<Vec<WorkspaceManifestEntry>> {
    let mut files = BTreeSet::new();
    for path in paths {
        if path == REPOSITORY_WIDE_PATH {
            collect_repository_overlay_files(root, &mut files)?;
            continue;
        }
        let absolute = absolute_repo_path(root, path);
        if absolute.is_dir() {
            collect_directory_files(root, &absolute, &mut files)?;
        } else {
            files.insert(path.clone());
        }
    }
    let mut entries = Vec::with_capacity(files.len());
    for path in files {
        entries.push(snapshot_file(root, path)?);
    }
    Ok(entries)
}

fn include_manifest_paths(
    root: &Path,
    expected: &[WorkspaceManifestEntry],
    entries: &mut Vec<WorkspaceManifestEntry>,
) -> StoreResult<()> {
    let mut present = entries
        .iter()
        .map(|entry| comparison_path(&entry.path))
        .collect::<BTreeSet<_>>();
    for entry in expected {
        if present.insert(comparison_path(&entry.path)) {
            entries.push(snapshot_file(root, entry.path.clone())?);
        }
    }
    entries.sort();
    Ok(())
}

fn collect_repository_overlay_files(root: &Path, files: &mut BTreeSet<String>) -> StoreResult<()> {
    collect_repository_overlay_files_with(root, files, spawn_repository_overlay_command)
}

fn collect_repository_overlay_files_with(
    root: &Path,
    files: &mut BTreeSet<String>,
    mut spawn: impl FnMut(&Path, &[&str]) -> std::io::Result<Child>,
) -> StoreResult<()> {
    let tracked = spawn(
        root,
        &["diff", "--name-only", "-z", "--no-ext-diff", "HEAD", "--"],
    );
    let untracked = spawn(root, &["ls-files", "--others", "--exclude-standard", "-z"]);
    let tracked = tracked.and_then(Child::wait_with_output);
    let untracked = untracked.and_then(Child::wait_with_output);
    if let (Ok(tracked), Ok(untracked)) = (tracked, untracked)
        && tracked.status.success()
        && untracked.status.success()
    {
        for raw_path in tracked
            .stdout
            .split(|byte| *byte == 0)
            .chain(untracked.stdout.split(|byte| *byte == 0))
        {
            if raw_path.is_empty() {
                continue;
            }
            let path = String::from_utf8_lossy(raw_path).replace('\\', "/");
            if repository_overlay_path_is_excluded(&path) {
                continue;
            }
            let absolute = absolute_repo_path(root, &path);
            if absolute.is_dir() {
                collect_directory_files(root, &absolute, files)?;
            } else {
                files.insert(path);
            }
        }
        return Ok(());
    }

    collect_repository_files_fallback(root, root, files)
}

fn repository_overlay_path_is_excluded(path: &str) -> bool {
    let mut components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .peekable();
    let mut parent_components = Vec::new();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            break;
        }
        parent_components.push(component);
        let name = component.to_ascii_lowercase();
        if name == ".git"
            || name == "node_modules"
            || name == ".venv"
            || name == "venv"
            || name == "dist"
            || name == "build"
            || name.starts_with("target")
        {
            return true;
        }
    }
    parent_components.len() >= 2
        && parent_components[0].eq_ignore_ascii_case(".codex")
        && parent_components[1].eq_ignore_ascii_case("locks")
}

fn spawn_repository_overlay_command(root: &Path, args: &[&str]) -> std::io::Result<Child> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn collect_repository_files_fallback(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> StoreResult<()> {
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut children = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            if child.file_type()?.is_dir() {
                let name = child.file_name().to_string_lossy().to_ascii_lowercase();
                if name == ".git"
                    || name == "node_modules"
                    || name == ".venv"
                    || name == "venv"
                    || name == "dist"
                    || name == "build"
                    || name.starts_with("target")
                    || path == root.join(".codex").join("locks")
                {
                    continue;
                }
                directories.push(path);
            } else {
                let relative = path.strip_prefix(root).map_err(|_| {
                    StoreError::InvalidScope(format!(
                        "manifest path escaped repository root: {}",
                        path.display()
                    ))
                })?;
                files.insert(
                    relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
    Ok(())
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> StoreResult<()> {
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut children = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            if child.file_type()?.is_dir() {
                directories.push(path);
            } else {
                let relative = path.strip_prefix(root).map_err(|_| {
                    StoreError::InvalidScope(format!(
                        "manifest path escaped repository root: {}",
                        path.display()
                    ))
                })?;
                files.insert(
                    relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
    Ok(())
}

fn snapshot_file(root: &Path, path: String) -> StoreResult<WorkspaceManifestEntry> {
    let absolute = absolute_repo_path(root, &path);
    if !absolute.exists() {
        return Ok(WorkspaceManifestEntry {
            path,
            content_hash: None,
            existed: false,
        });
    }
    let mut file = File::open(&absolute)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(WorkspaceManifestEntry {
        path,
        content_hash: Some(format!("{:x}", digest.finalize())),
        existed: true,
    })
}

async fn reconcile_entries_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &RepositoryIdentity,
    observed_paths: &[String],
    entries: &mut Vec<WorkspaceManifestEntry>,
) -> StoreResult<u64> {
    include_missing_observed_entries_tx(
        transaction,
        &repository.workspace_id,
        &repository.canonical_root,
        observed_paths,
        entries,
    )
    .await?;
    let current = current_epoch_tx(transaction, &repository.workspace_id).await?;
    let mut drift = Vec::new();
    for entry in entries.iter() {
        let row = sqlx::query(
            "SELECT content_hash, existed FROM workspace_paths
             WHERE workspace_id = ? AND path = ?",
        )
        .bind(&repository.workspace_id)
        .bind(&entry.path)
        .fetch_optional(&mut **transaction)
        .await?;
        if let Some(row) = row {
            let hash = row.get::<Option<String>, _>("content_hash");
            let existed = row.get::<i64, _>("existed") != 0;
            if hash != entry.content_hash || existed != entry.existed {
                drift.push(entry.path.clone());
            }
        }
    }
    let epoch = if drift.is_empty() {
        current
    } else {
        let next = current + 1;
        record_workspace_event_tx(
            transaction,
            WorkspaceEventDraft {
                workspace_id: &repository.workspace_id,
                epoch: next,
                actor_id: None,
                actor_kind: WorkspaceActorKind::External,
                confidence: AttributionConfidence::DetectionOnly,
                paths: &drift,
                contracts: &[],
            },
        )
        .await?;
        set_epoch_tx(transaction, &repository.workspace_id, next).await?;
        next
    };
    update_workspace_entries_tx(
        transaction,
        &repository.workspace_id,
        epoch,
        None,
        AttributionConfidence::DetectionOnly,
        entries,
    )
    .await?;
    Ok(epoch)
}

async fn include_missing_observed_entries_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    repository_root: &Path,
    observed_paths: &[String],
    entries: &mut Vec<WorkspaceManifestEntry>,
) -> StoreResult<()> {
    let repository_wide = observed_paths
        .iter()
        .any(|path| path == REPOSITORY_WIDE_PATH);
    let present = entries
        .iter()
        .map(|entry| comparison_path(&entry.path))
        .collect::<BTreeSet<_>>();
    let rows = sqlx::query(
        "SELECT path, existed FROM workspace_paths
         WHERE workspace_id = ? AND existed = 1",
    )
    .bind(workspace_id)
    .fetch_all(&mut **transaction)
    .await?;
    for row in rows {
        let path = row.get::<String, _>("path");
        if present.contains(&comparison_path(&path)) {
            continue;
        }
        if repository_wide {
            entries.push(snapshot_file(repository_root, path)?);
        } else if observed_paths
            .iter()
            .any(|observed| observed_path_covers(observed, &path))
        {
            entries.push(WorkspaceManifestEntry {
                path,
                content_hash: None,
                existed: false,
            });
        }
    }
    entries.sort();
    Ok(())
}

fn observed_path_covers(observed: &str, path: &str) -> bool {
    if observed == REPOSITORY_WIDE_PATH {
        return false;
    }
    let observed = comparison_path(observed);
    let path = comparison_path(path);
    path == observed || path.starts_with(&format!("{observed}/"))
}

fn comparison_path(path: &str) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

async fn update_workspace_entries_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    epoch: u64,
    actor_id: Option<&str>,
    confidence: AttributionConfidence,
    entries: &[WorkspaceManifestEntry],
) -> StoreResult<()> {
    let now = Utc::now();
    for entry in entries {
        sqlx::query(
            "INSERT INTO workspace_paths (
                workspace_id, path, content_hash, existed, last_epoch, last_actor_id,
                attribution_confidence, observed_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(workspace_id, path) DO UPDATE SET
                content_hash = excluded.content_hash,
                existed = excluded.existed,
                last_epoch = excluded.last_epoch,
                last_actor_id = excluded.last_actor_id,
                attribution_confidence = excluded.attribution_confidence,
                observed_at = excluded.observed_at",
        )
        .bind(workspace_id)
        .bind(&entry.path)
        .bind(&entry.content_hash)
        .bind(i64::from(entry.existed))
        .bind(epoch as i64)
        .bind(actor_id)
        .bind(json(&confidence)?)
        .bind(json(&now)?)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn set_epoch_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    epoch: u64,
) -> StoreResult<()> {
    sqlx::query(
        "UPDATE workspace_repositories SET epoch = ?, updated_at = ? WHERE workspace_id = ?",
    )
    .bind(epoch as i64)
    .bind(json(&Utc::now())?)
    .bind(workspace_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

struct WorkspaceEventDraft<'a> {
    workspace_id: &'a str,
    epoch: u64,
    actor_id: Option<&'a str>,
    actor_kind: WorkspaceActorKind,
    confidence: AttributionConfidence,
    paths: &'a [String],
    contracts: &'a [String],
}

async fn record_workspace_event_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    event: WorkspaceEventDraft<'_>,
) -> StoreResult<()> {
    sqlx::query(
        "INSERT INTO workspace_events (
            workspace_id, epoch, actor_id, actor_kind, attribution_confidence,
            paths_json, contracts_json, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.workspace_id)
    .bind(event.epoch as i64)
    .bind(event.actor_id)
    .bind(json(&event.actor_kind)?)
    .bind(json(&event.confidence)?)
    .bind(json(event.paths)?)
    .bind(json(event.contracts)?)
    .bind(json(&Utc::now())?)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn expire_mutation_leases_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
) -> StoreResult<()> {
    let now = json(&comparison_now())?;
    sqlx::query(
        "UPDATE workspace_mutation_leases
         SET state = 'expired', released_at = ?
         WHERE workspace_id = ? AND state = 'active'
           AND julianday(json_extract(expires_at, '$')) < julianday(json_extract(?, '$'))",
    )
    .bind(&now)
    .bind(workspace_id)
    .bind(&now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn expire_finalization_fences_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
) -> StoreResult<()> {
    let now = json(&comparison_now())?;
    sqlx::query(
        "UPDATE workspace_finalization_fences
         SET state = 'expired', released_at = ?
         WHERE workspace_id = ? AND state IN ('active', 'dispatching')
           AND julianday(json_extract(expires_at, '$')) <= julianday(json_extract(?, '$'))",
    )
    .bind(&now)
    .bind(workspace_id)
    .bind(&now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn require_no_mutation_lease_overlap_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    root_session_id: &str,
    actor_id: &str,
    paths: &[String],
    contracts: &[String],
) -> StoreResult<()> {
    let rows = sqlx::query(
        "SELECT lease_id, actor_id, paths_json, contracts_json FROM workspace_mutation_leases
         WHERE workspace_id = ? AND root_session_id = ? AND state = 'active'",
    )
    .bind(workspace_id)
    .bind(root_session_id)
    .fetch_all(&mut **transaction)
    .await?;
    let mut conflicts = Vec::new();
    let mut superseded_lease_ids = Vec::new();
    for row in rows {
        let lease_id = row.get::<String, _>("lease_id");
        let existing_actor = row.get::<String, _>("actor_id");
        let existing: Vec<String> = from_json(&row.get::<String, _>("paths_json"))?;
        let existing_contracts: Vec<String> = from_json(&row.get::<String, _>("contracts_json"))?;
        let mut overlaps = false;
        for requested in paths {
            for claimed in &existing {
                if path_overlap(requested, claimed) {
                    overlaps = true;
                    if existing_actor != actor_id {
                        conflicts.push(format!(
                            "{requested} overlaps active mutation {claimed} held by {existing_actor}"
                        ));
                    }
                }
            }
        }
        let contract_overlap = contracts
            .iter()
            .filter(|contract| existing_contracts.contains(contract))
            .cloned()
            .collect::<Vec<_>>();
        if !contract_overlap.is_empty() {
            overlaps = true;
            if existing_actor != actor_id {
                conflicts.push(format!(
                    "contracts {} overlap an active mutation held by {existing_actor}",
                    contract_overlap.join(", ")
                ));
            }
        }
        if overlaps && existing_actor == actor_id {
            superseded_lease_ids.push(lease_id);
        }
    }
    if conflicts.is_empty() {
        let now = json(&comparison_now())?;
        for lease_id in superseded_lease_ids {
            sqlx::query(
                "UPDATE workspace_mutation_leases
                 SET state = 'expired', released_at = ?
                 WHERE lease_id = ? AND workspace_id = ? AND state = 'active'",
            )
            .bind(&now)
            .bind(lease_id)
            .bind(workspace_id)
            .execute(&mut **transaction)
            .await?;
        }
        Ok(())
    } else {
        Err(StoreError::WorkspaceClaimConflict {
            details: conflicts
                .into_iter()
                .map(|detail| format!("{detail}; requester={actor_id}"))
                .collect(),
        })
    }
}

async fn require_claim_authority_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    workspace_id: &str,
    root_session_id: &str,
    attempt_id: Option<AttemptId>,
    paths: &[String],
    contracts: &[String],
) -> StoreResult<()> {
    let rows = sqlx::query(
        "SELECT write_claims.assignment_id, write_claims.attempt_id, write_claims.scopes_json
         FROM write_claims
         JOIN assignment_repositories USING (assignment_id)
         JOIN assignments USING (assignment_id)
         WHERE write_claims.active = 1 AND assignment_repositories.workspace_id = ?
           AND assignments.root_session_id = ?",
    )
    .bind(workspace_id)
    .bind(root_session_id)
    .fetch_all(&mut **transaction)
    .await?;
    let mut own_scopes = Vec::new();
    let mut conflicts = Vec::new();
    for row in rows {
        let owner_attempt = AttemptId::parse(&row.get::<String, _>("attempt_id"))?;
        let scopes: Vec<RepoScope> = from_json(&row.get::<String, _>("scopes_json"))?;
        if Some(owner_attempt) == attempt_id {
            own_scopes.extend(scopes);
            continue;
        }
        for path in paths {
            if scopes.iter().any(|scope| scope_overlaps_path(scope, path)) {
                conflicts.push(format!(
                    "{path} is claimed by assignment {}",
                    row.get::<String, _>("assignment_id")
                ));
            }
        }
    }
    if attempt_id.is_some() {
        for path in paths {
            if !own_scopes.iter().any(|scope| scope.covers_path(path)) {
                conflicts.push(format!(
                    "{path} is outside the typed actor's active path claim"
                ));
            }
        }
    }
    let rows = sqlx::query(
        "SELECT contract_claims.contract_name, contract_claims.assignment_id,
                contract_claims.attempt_id
         FROM contract_claims
         JOIN assignments USING (assignment_id)
         WHERE contract_claims.workspace_id = ? AND contract_claims.active = 1
           AND assignments.root_session_id = ?",
    )
    .bind(workspace_id)
    .bind(root_session_id)
    .fetch_all(&mut **transaction)
    .await?;
    let untyped_actor = attempt_id.is_none();
    let mut own_contracts = BTreeSet::new();
    for row in rows {
        let contract = row.get::<String, _>("contract_name");
        if !untyped_actor && !contracts.iter().any(|requested| requested == &contract) {
            continue;
        }
        let owner_attempt = AttemptId::parse(&row.get::<String, _>("attempt_id"))?;
        if Some(owner_attempt) == attempt_id {
            own_contracts.insert(contract);
        } else {
            conflicts.push(format!(
                "contract {contract} is claimed by assignment {}",
                row.get::<String, _>("assignment_id")
            ));
        }
    }
    if attempt_id.is_some() {
        for contract in contracts {
            if !own_contracts.contains(contract) {
                conflicts.push(format!(
                    "contract {contract} is outside the typed actor's active contract claim"
                ));
            }
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(StoreError::WorkspaceClaimConflict { details: conflicts })
    }
}

async fn query_assignment_ids(
    pool: &SqlitePool,
    query: &'static str,
    root_session_id: &str,
) -> StoreResult<Vec<AssignmentId>> {
    let rows = sqlx::query(query)
        .bind(root_session_id)
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|row| AssignmentId::parse(&row.get::<String, _>("assignment_id")))
        .collect()
}

fn path_overlap(left: &str, right: &str) -> bool {
    if left == REPOSITORY_WIDE_PATH || right == REPOSITORY_WIDE_PATH {
        return true;
    }
    let left = if cfg!(windows) {
        left.to_lowercase()
    } else {
        left.to_string()
    };
    let right = if cfg!(windows) {
        right.to_lowercase()
    } else {
        right.to_string()
    };
    left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn scope_overlaps_path(scope: &RepoScope, path: &str) -> bool {
    path == REPOSITORY_WIDE_PATH || scope.covers_path(path) || path_overlap(&scope.path, path)
}

fn paths_equal(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> StoreResult<String> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: serde::de::DeserializeOwned>(value: &str) -> StoreResult<T> {
    Ok(serde_json::from_str(value)?)
}

#[cfg(test)]
mod overlay_observation_tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::time::Duration as StdDuration;
    use std::time::Instant;

    const HELPER_DIR: &str = "KD4_OVERLAY_HELPER_DIR";
    const HELPER_ROLE: &str = "KD4_OVERLAY_HELPER_ROLE";

    #[test]
    #[ignore = "invoked as a child process by overlay_commands_start_before_either_is_reaped"]
    fn overlay_observation_child() {
        let dir = PathBuf::from(std::env::var_os(HELPER_DIR).expect("helper dir"));
        let role = std::env::var(HELPER_ROLE).expect("helper role");
        let other = if role == "tracked" {
            "untracked"
        } else {
            "tracked"
        };
        std::fs::write(dir.join(&role), b"started").expect("write start marker");
        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !dir.join(other).exists() {
            assert!(
                Instant::now() < deadline,
                "other overlay command never started"
            );
            std::thread::sleep(StdDuration::from_millis(10));
        }
    }

    #[test]
    fn overlay_commands_start_before_either_is_reaped() {
        let temp = tempfile::tempdir().expect("temp dir");
        let helper_dir = temp.path().join("markers");
        std::fs::create_dir(&helper_dir).expect("create marker dir");
        let current_exe = std::env::current_exe().expect("current test executable");
        let spawn_count = Cell::new(0usize);
        let mut files = BTreeSet::new();

        collect_repository_overlay_files_with(temp.path(), &mut files, |_root, args| {
            let role = if args.first() == Some(&"diff") {
                "tracked"
            } else {
                "untracked"
            };
            spawn_count.set(spawn_count.get() + 1);
            Command::new(&current_exe)
                .arg("overlay_observation_child")
                .arg("--ignored")
                .env(HELPER_DIR, &helper_dir)
                .env(HELPER_ROLE, role)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
        .expect("collect concurrent overlay observation");

        assert_eq!(spawn_count.get(), 2);
        assert!(helper_dir.join("tracked").exists());
        assert!(helper_dir.join("untracked").exists());
    }

    #[test]
    fn repository_overlay_excludes_generated_directory_contents() {
        assert!(repository_overlay_path_is_excluded(
            "target-codex-agent-task-store-lease/debug/deps/store.pdb"
        ));
        assert!(repository_overlay_path_is_excluded(
            "nested/node_modules/package/index.js"
        ));
        assert!(repository_overlay_path_is_excluded(
            ".codex/locks/workspace.lock"
        ));
        assert!(!repository_overlay_path_is_excluded(
            "codex-rs/example/build.rs"
        ));
        assert!(!repository_overlay_path_is_excluded("src/targeting.rs"));
    }
}
