use chrono::Utc;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;

use crate::AssignmentId;
use crate::AttributionConfidence;
use crate::QuiescenceStatus;
use crate::StoreError;
use crate::StoreResult;
use crate::WorkspaceActorKind;
use crate::WorkspaceActorRegistration;
use crate::WorkspaceCaptureMode;
use crate::WorkspaceManifestEntry;
use crate::WorkspaceRevision;
use crate::scope::RepositoryIdentity;
use crate::scope::absolute_repo_path;
use crate::scope::filesystem_paths_equal;
use crate::scope::normalize_repo_path;
use crate::scope::path_comparison_key;
use crate::scope::relative_path_identity;
use crate::scope::repository_identity;

/// Reserved scope for repository-wide revision capture and workspace event coverage.
pub const REPOSITORY_WIDE_PATH: &str = ":repository:";

#[cfg(test)]
tokio::task_local! {
    static TEST_WORKSPACE_CAPTURE_PAUSE: std::sync::Arc<TestWorkspaceCapturePause>;
}

#[cfg(test)]
pub(crate) struct TestWorkspaceCapturePause {
    pub(crate) started: tokio::sync::Semaphore,
    pub(crate) release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl TestWorkspaceCapturePause {
    pub(crate) fn new() -> Self {
        Self {
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[cfg(test)]
pub(crate) async fn with_test_workspace_capture_pause<T>(
    pause: std::sync::Arc<TestWorkspaceCapturePause>,
    future: impl std::future::Future<Output = T>,
) -> T {
    TEST_WORKSPACE_CAPTURE_PAUSE.scope(pause, future).await
}

#[cfg(test)]
async fn pause_test_workspace_capture() {
    if let Ok(pause) = TEST_WORKSPACE_CAPTURE_PAUSE.try_with(std::sync::Arc::clone) {
        pause.started.add_permits(1);
        if let Ok(permit) = pause.release.acquire().await {
            permit.forget();
        }
    }
}

pub(crate) async fn capture_revision(
    pool: &SqlitePool,
    repo_root: &Path,
    paths: Vec<String>,
) -> StoreResult<WorkspaceRevision> {
    let mut transaction = pool.begin().await?;
    let revision = capture_revision_tx(&mut transaction, repo_root, paths).await?;
    transaction.commit().await?;
    Ok(revision)
}

pub(crate) async fn capture_revision_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    repo_root: &Path,
    paths: Vec<String>,
) -> StoreResult<WorkspaceRevision> {
    let repository = repository_identity(repo_root)?;
    let normalized = normalize_paths(repo_root, paths)?;
    ensure_workspace_tx(transaction, &repository).await?;
    // Acquire the SQLite writer lane before observing the filesystem. Every capture for a
    // workspace therefore scans in the same order in which its epoch can be published.
    sqlx::query("UPDATE workspace_repositories SET epoch = epoch WHERE workspace_id = ?")
        .bind(&repository.workspace_id)
        .execute(&mut **transaction)
        .await?;
    let root = repository.canonical_root.clone();
    let snapshot_paths = normalized.clone();
    let mut capture =
        tokio::task::spawn_blocking(move || collect_manifest_entries(&root, &snapshot_paths))
            .await
            .map_err(|error| StoreError::CorruptData(format!("manifest task failed: {error}")))??;
    #[cfg(test)]
    pause_test_workspace_capture().await;
    let epoch =
        reconcile_entries_tx(transaction, &repository, &normalized, &mut capture.entries).await?;
    revision(&repository, epoch, capture)
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
    .bind(sqlite_epoch(after_epoch)?)
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
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO workspace_actors (
            workspace_id, actor_id, root_session_id, kind, assignment_id, attempt_id,
            strategy, state, last_progress_at, lease_expires_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, 'idle', ?, NULL)
         ON CONFLICT(workspace_id, actor_id) DO UPDATE SET
            root_session_id = excluded.root_session_id,
            kind = excluded.kind,
            assignment_id = excluded.assignment_id,
            attempt_id = excluded.attempt_id,
            strategy = excluded.strategy,
            state = 'idle',
            last_progress_at = excluded.last_progress_at,
            lease_expires_at = NULL",
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
    inspect_quiescence(pool, root_session_id).await
}

pub(crate) async fn inspect_quiescence(
    pool: &SqlitePool,
    root_session_id: &str,
) -> StoreResult<QuiescenceStatus> {
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
    }
    let quiescent = active_assignment_ids.is_empty()
        && running_validation_call_ids.is_empty()
        && pending_gate_assignment_ids.is_empty();
    Ok(QuiescenceStatus {
        quiescent,
        active_assignment_ids,
        running_validation_call_ids,
        pending_gate_assignment_ids,
        active_claim_assignment_ids,
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
    if !filesystem_paths_equal(
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

fn revision(
    repository: &RepositoryIdentity,
    epoch: u64,
    mut capture: ManifestCapture,
) -> StoreResult<WorkspaceRevision> {
    capture.entries.sort();
    let manifest_hash = format!(
        "{:x}",
        Sha256::digest(
            json(&(
                capture.mode,
                capture.complete,
                &capture.discovery_errors,
                capture.ignored_path_count,
                capture.excluded_path_count,
                &capture.entries,
            ))?
            .as_bytes()
        )
    );
    Ok(WorkspaceRevision {
        repository_id: repository.id.clone(),
        workspace_id: repository.workspace_id.clone(),
        epoch,
        manifest_hash,
        files: capture.entries,
        capture_mode: capture.mode,
        complete: capture.complete,
        discovery_errors: capture.discovery_errors,
        ignored_path_count: capture.ignored_path_count,
        excluded_path_count: capture.excluded_path_count,
    })
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

struct ManifestCapture {
    entries: Vec<WorkspaceManifestEntry>,
    mode: WorkspaceCaptureMode,
    complete: bool,
    discovery_errors: Vec<String>,
    ignored_path_count: u64,
    excluded_path_count: u64,
}

fn collect_manifest_entries(root: &Path, paths: &[String]) -> StoreResult<ManifestCapture> {
    let mut files = BTreeSet::new();
    let mut repository_wide = false;
    let mut provenance = CaptureProvenance::explicit();
    for path in paths {
        if path == REPOSITORY_WIDE_PATH {
            repository_wide = true;
            provenance = collect_repository_overlay_files(root, &mut files)?;
            continue;
        }
        let absolute = absolute_repo_path(root, path);
        if is_unfollowed_directory(&absolute)? {
            collect_directory_files(root, &absolute, &mut files)?;
        } else {
            files.insert(path.clone());
        }
    }
    let mut entries = Vec::with_capacity(files.len());
    for path in files {
        entries.push(snapshot_file(root, path)?);
    }
    if repository_wide {
        entries.push(repository_head_entry(root));
        entries.sort();
    }
    Ok(ManifestCapture {
        entries,
        mode: provenance.mode,
        complete: provenance.complete,
        discovery_errors: provenance.discovery_errors,
        ignored_path_count: provenance.ignored_path_count,
        excluded_path_count: provenance.excluded_path_count,
    })
}

fn repository_head_entry(root: &Path) -> WorkspaceManifestEntry {
    let content_hash = repository_overlay_command(root, &["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!revision.is_empty()).then_some(revision)
        });
    WorkspaceManifestEntry {
        path: REPOSITORY_WIDE_PATH.to_string(),
        existed: content_hash.is_some(),
        content_hash,
    }
}

#[derive(Debug)]
struct CaptureProvenance {
    mode: WorkspaceCaptureMode,
    complete: bool,
    discovery_errors: Vec<String>,
    ignored_path_count: u64,
    excluded_path_count: u64,
}

impl CaptureProvenance {
    fn explicit() -> Self {
        Self {
            mode: WorkspaceCaptureMode::ExplicitPaths,
            complete: true,
            discovery_errors: Vec::new(),
            ignored_path_count: 0,
            excluded_path_count: 0,
        }
    }
}

fn collect_repository_overlay_files(
    root: &Path,
    files: &mut BTreeSet<String>,
) -> StoreResult<CaptureProvenance> {
    collect_repository_overlay_files_with(root, files, spawn_repository_overlay_command)
}

fn collect_repository_overlay_files_with(
    root: &Path,
    files: &mut BTreeSet<String>,
    mut spawn: impl FnMut(&Path, &[&str]) -> std::io::Result<Child>,
) -> StoreResult<CaptureProvenance> {
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
            let path = git_relative_path_identity(raw_path)?;
            let absolute = absolute_repo_path(root, &path);
            if is_unfollowed_directory(&absolute)? {
                collect_directory_files(root, &absolute, files)?;
            } else {
                files.insert(path);
            }
        }
        return Ok(CaptureProvenance {
            mode: WorkspaceCaptureMode::GitOverlay,
            complete: true,
            discovery_errors: Vec::new(),
            ignored_path_count: 0,
            excluded_path_count: 0,
        });
    }

    let mut excluded_path_count = 0;
    collect_repository_files_fallback(root, root, files, &mut excluded_path_count)?;
    Ok(CaptureProvenance {
        mode: WorkspaceCaptureMode::FilesystemFallback,
        complete: false,
        discovery_errors: vec![
            "Git overlay discovery failed; filesystem fallback cannot preserve tracked/ignored semantics"
                .to_string(),
        ],
        ignored_path_count: 0,
        excluded_path_count,
    })
}

fn is_unfollowed_directory(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && !metadata_is_reparse_point(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn directory_entry_is_unfollowed_directory(entry: &std::fs::DirEntry) -> std::io::Result<bool> {
    let file_type = entry.file_type()?;
    if !file_type.is_dir() || file_type.is_symlink() {
        return Ok(false);
    }
    Ok(!metadata_is_reparse_point(&entry.metadata()?))
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn git_relative_path_identity(raw_path: &[u8]) -> StoreResult<String> {
    let path = std::str::from_utf8(raw_path).map_err(|_| {
        StoreError::InvalidScope(
            "Git returned a path that cannot be represented losslessly on Windows".to_string(),
        )
    })?;
    Ok(relative_path_identity(Path::new(path)))
}

fn spawn_repository_overlay_command(root: &Path, args: &[&str]) -> std::io::Result<Child> {
    repository_overlay_command(root, args).spawn()
}

fn repository_overlay_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn collect_repository_files_fallback(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
    excluded_path_count: &mut u64,
) -> StoreResult<()> {
    let mut directories = vec![directory.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut children = std::fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            if directory_entry_is_unfollowed_directory(&child)? {
                if child.file_name() == ".git" {
                    *excluded_path_count = excluded_path_count.saturating_add(1);
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
                files.insert(relative_path_identity(relative));
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
            if directory_entry_is_unfollowed_directory(&child)? {
                directories.push(path);
            } else {
                let relative = path.strip_prefix(root).map_err(|_| {
                    StoreError::InvalidScope(format!(
                        "manifest path escaped repository root: {}",
                        path.display()
                    ))
                })?;
                files.insert(relative_path_identity(relative));
            }
        }
    }
    Ok(())
}

fn snapshot_file(root: &Path, path: String) -> StoreResult<WorkspaceManifestEntry> {
    let absolute = absolute_repo_path(root, &path);
    let link_metadata = match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceManifestEntry {
                path,
                content_hash: None,
                existed: false,
            });
        }
        Err(error) => return Err(error.into()),
    };
    if link_metadata.file_type().is_symlink() || metadata_is_reparse_point(&link_metadata) {
        let target = std::fs::read_link(&absolute)?;
        let target_identity = relative_path_identity(&target);
        let target_state = match std::fs::metadata(&absolute) {
            Ok(metadata) if metadata.is_file() => hash_regular_file(&absolute, &path)?,
            Ok(metadata) if metadata.is_dir() => "directory".to_string(),
            Ok(_) => "other".to_string(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "broken".to_string(),
            Err(error) => return Err(error.into()),
        };
        let content_hash = format!(
            "{:x}",
            Sha256::digest(json(&("symlink", target_identity, target_state))?.as_bytes())
        );
        return Ok(WorkspaceManifestEntry {
            path,
            content_hash: Some(content_hash),
            existed: true,
        });
    }
    let content_hash = hash_regular_file(&absolute, &path)?;
    Ok(WorkspaceManifestEntry {
        path,
        content_hash: Some(content_hash),
        existed: true,
    })
}

fn hash_regular_file(absolute: &Path, logical_path: &str) -> StoreResult<String> {
    let mut file = File::open(absolute)?;
    let before = FileSnapshotIdentity::capture(&file).ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "workspace manifest cannot establish a trustworthy file identity for {logical_path}"
        )))
    })?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let opened_after = FileSnapshotIdentity::capture(&file).ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "workspace manifest lost the file identity for {logical_path}"
        )))
    })?;
    let path_file = File::open(absolute)?;
    let path_after = FileSnapshotIdentity::capture(&path_file).ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "workspace manifest cannot revalidate the file identity for {logical_path}"
        )))
    })?;
    if before != opened_after || before != path_after {
        return Err(StoreError::Io(std::io::Error::other(format!(
            "workspace file changed or was atomically replaced while hashing {logical_path}"
        ))));
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug, Eq, PartialEq)]
struct FileSnapshotIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    stable_id: StableSnapshotFileId,
}

#[derive(Debug, Eq, PartialEq)]
enum StableSnapshotFileId {
    Windows { volume: u64, index: [u8; 16] },
}

impl FileSnapshotIdentity {
    fn capture(file: &File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        metadata.is_file().then_some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            stable_id: stable_snapshot_file_id(file)?,
        })
    }
}

fn stable_snapshot_file_id(file: &File) -> Option<StableSnapshotFileId> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: `file` owns a valid handle and `info` is correctly sized writable storage.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: a successful call initialized the complete structure.
    let info = unsafe { info.assume_init() };
    Some(StableSnapshotFileId::Windows {
        volume: info.VolumeSerialNumber,
        index: info.FileId.Identifier,
    })
}

async fn reconcile_entries_tx(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &RepositoryIdentity,
    observed_paths: &[String],
    entries: &mut Vec<WorkspaceManifestEntry>,
) -> StoreResult<u64> {
    let repository_wide = observed_paths
        .iter()
        .any(|path| path == REPOSITORY_WIDE_PATH);
    let repository_wide_baseline = repository_wide
        && sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM workspace_paths WHERE workspace_id = ? AND path = ?",
        )
        .bind(&repository.workspace_id)
        .bind(REPOSITORY_WIDE_PATH)
        .fetch_one(&mut **transaction)
        .await?
            != 0;
    include_missing_observed_entries_tx(
        transaction,
        &repository.workspace_id,
        &repository.canonical_root,
        observed_paths,
        entries,
    )
    .await?;
    let current = current_epoch_tx(transaction, &repository.workspace_id).await?;
    let stored_rows = sqlx::query(
        "SELECT path, content_hash, existed FROM workspace_paths WHERE workspace_id = ?",
    )
    .bind(&repository.workspace_id)
    .fetch_all(&mut **transaction)
    .await?;
    let stored_by_key = stored_rows
        .into_iter()
        .map(|row| {
            let path = row.get::<String, _>("path");
            (
                path_comparison_key(&path),
                (
                    path,
                    row.get::<Option<String>, _>("content_hash"),
                    row.get::<i64, _>("existed") != 0,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut drift = Vec::new();
    for entry in entries.iter() {
        if let Some((stored_path, hash, existed)) =
            stored_by_key.get(&path_comparison_key(&entry.path))
        {
            if stored_path != &entry.path
                || hash != &entry.content_hash
                || *existed != entry.existed
            {
                drift.push(entry.path.clone());
            }
        } else if repository_wide_baseline && entry.path != REPOSITORY_WIDE_PATH {
            drift.push(entry.path.clone());
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
        .map(|entry| path_comparison_key(&entry.path))
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
        if present.contains(&path_comparison_key(&path)) {
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
    let observed = path_comparison_key(observed);
    let path = path_comparison_key(path);
    path == observed || path.starts_with(&format!("{observed}/"))
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
    let mut stored_paths =
        sqlx::query_scalar::<_, String>("SELECT path FROM workspace_paths WHERE workspace_id = ?")
            .bind(workspace_id)
            .fetch_all(&mut **transaction)
            .await?;
    for entry in entries {
        let comparison_key = path_comparison_key(&entry.path);
        let conflicting_paths = stored_paths
            .iter()
            .filter(|path| {
                path.as_str() != entry.path && path_comparison_key(path) == comparison_key
            })
            .cloned()
            .collect::<Vec<_>>();
        for path in conflicting_paths {
            sqlx::query("DELETE FROM workspace_paths WHERE workspace_id = ? AND path = ?")
                .bind(workspace_id)
                .bind(&path)
                .execute(&mut **transaction)
                .await?;
            stored_paths.retain(|stored| stored != &path);
        }
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
        .bind(sqlite_epoch(epoch)?)
        .bind(actor_id)
        .bind(json(&confidence)?)
        .bind(json(&now)?)
        .execute(&mut **transaction)
        .await?;
        if !stored_paths.iter().any(|path| path == &entry.path) {
            stored_paths.push(entry.path.clone());
        }
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
    .bind(sqlite_epoch(epoch)?)
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
    .bind(sqlite_epoch(event.epoch)?)
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

fn sqlite_epoch(epoch: u64) -> StoreResult<i64> {
    i64::try_from(epoch)
        .map_err(|_| StoreError::CorruptData("workspace epoch exceeds SQLite integer range".into()))
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

fn json<T: serde::Serialize + ?Sized>(value: &T) -> StoreResult<String> {
    Ok(serde_json::to_string(value)?)
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
    fn repository_overlay_preserves_git_reported_paths() {
        assert_eq!(
            git_relative_path_identity(
                b"target-codex-agent-task-store-revision/debug/deps/store.pdb"
            )
            .expect("Git path is represented"),
            "target-codex-agent-task-store-revision/debug/deps/store.pdb"
        );
        assert_eq!(
            git_relative_path_identity(b"build/source.rs").expect("Git path is represented"),
            "build/source.rs"
        );
    }

    #[test]
    fn repository_overlay_commands_do_not_refresh_the_git_index() {
        let command = repository_overlay_command(Path::new("repo"), &["diff", "--name-only"]);

        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == "GIT_OPTIONAL_LOCKS")
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("0"))
        );
    }
}
