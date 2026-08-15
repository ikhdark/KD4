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

use crate::AssignmentId;
use crate::AttributionConfidence;
use crate::QuiescenceStatus;
use crate::StoreError;
use crate::StoreResult;
use crate::WorkspaceActorKind;
use crate::WorkspaceActorRegistration;
use crate::WorkspaceManifestEntry;
use crate::WorkspaceRevision;
use crate::scope::RepositoryIdentity;
use crate::scope::absolute_repo_path;
use crate::scope::normalize_repo_path;
use crate::scope::repository_identity;

/// Reserved scope for repository-wide revision capture and workspace event coverage.
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
    let epoch =
        reconcile_entries_tx(&mut transaction, &repository, &normalized, &mut entries).await?;
    transaction.commit().await?;
    revision(&repository, epoch, entries)
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
    let before = FileSnapshotIdentity::capture(&file).ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "workspace manifest cannot establish a trustworthy file identity for {path}"
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
            "workspace manifest lost the file identity for {path}"
        )))
    })?;
    let path_file = File::open(&absolute)?;
    let path_after = FileSnapshotIdentity::capture(&path_file).ok_or_else(|| {
        StoreError::Io(std::io::Error::other(format!(
            "workspace manifest cannot revalidate the file identity for {path}"
        )))
    })?;
    if before != opened_after || before != path_after {
        return Err(StoreError::Io(std::io::Error::other(format!(
            "workspace file changed or was atomically replaced while hashing {path}"
        ))));
    }
    Ok(WorkspaceManifestEntry {
        path,
        content_hash: Some(format!("{:x}", digest.finalize())),
        existed: true,
    })
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
    #[cfg(windows)]
    Windows { volume: u64, index: [u8; 16] },
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
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

#[cfg(windows)]
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

#[cfg(unix)]
fn stable_snapshot_file_id(file: &File) -> Option<StableSnapshotFileId> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().ok()?;

    Some(StableSnapshotFileId::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(any(windows, unix)))]
fn stable_snapshot_file_id(_file: &File) -> Option<StableSnapshotFileId> {
    None
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
            "target-codex-agent-task-store-revision/debug/deps/store.pdb"
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
