//! Local hard-delete support for persisted threads.
//!
//! Local rollout removal can be staged so callers that also own SQLite state have one logical
//! commit point. Staged files are restored on drop until the caller commits the deletion.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_rollout::ARCHIVED_SESSIONS_SUBDIR;
use codex_rollout::SESSIONS_SUBDIR;
use codex_rollout::remove_threads_name_entries;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::scoped_rollout_path;
use crate::DeleteThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

use codex_rollout::RECOVERABLE_DELETE_PREFIX;
const MAX_RECOVERY_BATCHES: usize = 128;
const MAX_RECOVERY_ENTRIES: usize = 4096;

/// Receiver mail is retired under the existing per-thread storage owner.
pub fn thread_mailbox_path(home: &Path, id: codex_protocol::ThreadId) -> PathBuf {
    home.join("agent-mailbox").join(format!("{id}.json"))
}

fn auxiliary_paths(id: codex_protocol::ThreadId) -> [PathBuf; 5] {
    [
        PathBuf::from("tool-history").join(format!("{id}.json")),
        PathBuf::from("tool-history").join(format!("{id}.journal.jsonl")),
        PathBuf::from("command-execution-cache").join(format!("{id}.json")),
        PathBuf::from("code-mode-state").join(format!("{id}.json")),
        thread_mailbox_path(Path::new(""), id),
    ]
}

fn auxiliary_owners(
    home: &Path,
    id: codex_protocol::ThreadId,
) -> std::io::Result<Vec<std::fs::File>> {
    // All acquisitions are nonblocking. Stable lock files are never staged or
    // unlinked: sessions, detached cache commits and snapshot runtimes share them.
    let mut owners = Vec::new();
    for relative in [
        PathBuf::from("tool-output-residency").join(format!("{id}.lock")),
        PathBuf::from("tool-history").join(format!(".{id}.owner.lock")),
        PathBuf::from("code-mode-state").join(format!("{id}.lock")),
    ] {
        let path = home.join(relative);
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| std::io::Error::other("owner has no parent"))?,
        )?;
        let owner = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        owner.try_lock().map_err(std::io::Error::from)?;
        owners.push(owner);
    }
    Ok(owners)
}

const STORAGE_RETIRED_MARKER: &[u8] = b"KD4_THREAD_STORAGE_RETIRED_V1\n";

fn storage_retired_marker_path(home: &Path, id: codex_protocol::ThreadId) -> PathBuf {
    home.join("tool-history").join(format!("{id}.retired"))
}

fn validate_storage_retired_marker(path: &Path) -> std::io::Result<()> {
    use std::io::Read;
    use std::os::windows::fs::MetadataExt;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let invalid_marker = || std::io::Error::other("invalid retired storage marker; files retained");

    // Validate the opened file, without following a final-component reparse
    // point or sharing the handle with a writer or deleter during inspection.
    let file = std::fs::File::options()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.len() != STORAGE_RETIRED_MARKER.len() as u64
        || std::fs::canonicalize(path)? != path
    {
        return Err(invalid_marker());
    }

    let mut contents = Vec::new();
    file.take(STORAGE_RETIRED_MARKER.len() as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents != STORAGE_RETIRED_MARKER {
        return Err(invalid_marker());
    }
    Ok(())
}

fn mark_storage_retired(home: &Path, id: codex_protocol::ThreadId) -> std::io::Result<()> {
    let directory = home.join("tool-history");
    std::fs::create_dir_all(&directory)?;
    let home = std::fs::canonicalize(home)?;
    let path = storage_retired_marker_path(&home, id);
    let directory = home.join("tool-history");
    if std::fs::canonicalize(&directory)? != directory {
        return Err(std::io::Error::other(
            "indirect retired storage marker directory; files retained",
        ));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(_) => return validate_storage_retired_marker(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    // Publish only complete, synced bytes. The temporary file owns cleanup
    // on failure; no path here overwrites or removes an existing marker.
    let mut staged = tempfile::NamedTempFile::new_in(&directory)?;
    std::io::Write::write_all(staged.as_file_mut(), STORAGE_RETIRED_MARKER)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_storage_retired_marker(&path)
        }
        Err(error) => Err(error.error),
    }
}

fn retire_auxiliary_storage(home: &Path, id: codex_protocol::ThreadId) -> std::io::Result<()> {
    let _owners = auxiliary_owners(home, id)?;
    let home = std::fs::canonicalize(home)?;
    for relative in auxiliary_paths(id) {
        let path = home.join(relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || std::fs::canonicalize(&path)? != path {
            return Err(std::io::Error::other("indirect auxiliary storage retained"));
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

const AUXILIARY_RETIREMENT_PENDING: &str = ".auxiliary-retirement.pending";
const SQLITE_BACKED_STAGING: &str = "sqlite-backed";
const SQLITE_LESS_STAGING: &str = "sqlite-less";
const COMMITTED_STAGING: &str = "committed";
const STAGING_KINDS: [&str; 5] = [
    "indexed",
    "unindexed",
    SQLITE_BACKED_STAGING,
    SQLITE_LESS_STAGING,
    COMMITTED_STAGING,
];

async fn staging_marker_metadata(path: &Path) -> ThreadStoreResult<Option<std::fs::Metadata>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err(delete_recovery_error(
            "invalid staging marker; files retained",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(delete_recovery_error(error)),
    }
}

async fn has_storage_retired_marker(
    home: &Path,
    id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let home = home.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
        let path = storage_retired_marker_path(&home, id);
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let home = std::fs::canonicalize(home)?;
        validate_storage_retired_marker(&storage_retired_marker_path(&home, id))?;
        Ok(true)
    })
    .await
    .map_err(delete_recovery_error)?
    .map_err(delete_recovery_error)
}

async fn require_sqlite_absent(store: &LocalThreadStore) -> ThreadStoreResult<()> {
    let path = codex_state::state_db_path(&store.config.sqlite_home);
    match tokio::fs::symlink_metadata(&path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(delete_recovery_error(error)),
        Ok(_) => Err(delete_recovery_error(
            "state database exists but is unavailable; staged files retained",
        )),
    }
}

async fn should_restore_staged_delete(
    store: &LocalThreadStore,
    kind: &str,
    root: &Path,
    id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    if kind == COMMITTED_STAGING {
        return Ok(false);
    }
    if let Some(db) = &store.state_db
        && db
            .is_thread_owner_deleted(id)
            .await
            .map_err(delete_recovery_error)?
    {
        return Ok(false);
    }
    if has_storage_retired_marker(&store.config.codex_home, id).await? {
        return Ok(false);
    }
    if kind == SQLITE_LESS_STAGING {
        if store.state_db.is_none() {
            require_sqlite_absent(store).await?;
        }
        return Ok(true);
    }
    if kind != SQLITE_BACKED_STAGING {
        let mut entries = tokio::fs::read_dir(root).await.map_err(delete_recovery_error)?;
        if entries.next_entry().await.map_err(delete_recovery_error)?.is_none() {
            // A previous cleanup can stop after discarding its last artifact
            // but before removing the empty per-thread directory.
            return Ok(true);
        }
    }
    let db = store.state_db.as_ref().ok_or_else(|| {
        delete_recovery_error(
            "staged deletion recovery requires its state database; staged files retained",
        )
    })?;
    if kind == SQLITE_BACKED_STAGING {
        return Ok(true);
    }
    // Both legacy directory names predate the transactional tombstone. The
    // pending-retirement file was introduced with it, and identifies a stage
    // for which a successful tombstone read can prove the delete never committed.
    let pending = staging_marker_metadata(&root.join(AUXILIARY_RETIREMENT_PENDING)).await?;
    if let Some(pending) = pending {
        if pending.len() != 0 {
            return Err(delete_recovery_error(
                "invalid pending retirement marker; files retained",
            ));
        }
        return Ok(true);
    }
    if kind == "indexed"
        && db
            .get_thread(id)
            .await
            .map_err(delete_recovery_error)?
            .is_some()
    {
        return Ok(true);
    }
    Err(delete_recovery_error(
        "legacy deletion has no commit or rollback witness; staged files retained",
    ))
}

pub(super) fn delete_recovery_error(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("staged thread deletion: {error}"),
    }
}

/// Recover the existing staging layout, not a second journal. A live staging
/// owner holds .owner until its database decision and filesystem cleanup finish.
pub(super) async fn recover_staged_deletes(store: &LocalThreadStore) -> ThreadStoreResult<bool> {
    // Recovery can be requested while a live staged-delete guard owns this lock.
    // Defer rather than waiting on that same caller; the initialization cell must retry.
    let Some(moves) = codex_rollout::try_lock_rollout_moves(&store.config.codex_home)
        .await
        .map_err(delete_recovery_error)?
    else {
        return Ok(false);
    };
    let moves = std::sync::Arc::new(moves);
    let home = store.config.codex_home.clone();
    let batches = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let entries = match std::fs::read_dir(&home) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut batches = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(RECOVERABLE_DELETE_PREFIX)
                || !entry.file_type()?.is_dir()
            {
                continue;
            }
            let directory = entry.path();
            let owner = match std::fs::File::options()
                .read(true)
                .write(true)
                .open(directory.join(".owner"))
            {
                Ok(owner) => owner,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            match owner.try_lock() {
                Ok(()) => {
                    if batches.len() == MAX_RECOVERY_BATCHES {
                        return Err(std::io::Error::other(
                            "staged deletion recovery batch limit exceeded; files retained",
                        ));
                    }
                    batches.push((directory, owner));
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => return Err(err),
            }
        }
        Ok(batches)
    })
    .await
    .map_err(delete_recovery_error)?
    .map_err(delete_recovery_error)?;
    for (directory, owner) in batches {
        // Capture DB decisions before filesystem mutation. Cancellation here
        // simply releases the batch for the next initialization attempt.
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .map_err(delete_recovery_error)?;
        while let Some(entry) = entries.next_entry().await.map_err(delete_recovery_error)? {
            let kind = entry.file_type().await.map_err(delete_recovery_error)?;
            let name = entry.file_name();
            let known = if name == ".owner" {
                kind.is_file()
            } else {
                kind.is_dir() && STAGING_KINDS.iter().any(|known| name == *known)
            };
            if !known {
                return Err(delete_recovery_error(
                    "unknown staged deletion layout; files retained",
                ));
            }
        }
        let mut decisions = Vec::new();
        for kind in STAGING_KINDS {
            let root = directory.join(kind);
            let entries = match tokio::fs::read_dir(&root).await {
                Ok(entries) => entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(delete_recovery_error(err)),
            };
            let mut entries = entries;
            while let Some(entry) = entries.next_entry().await.map_err(delete_recovery_error)? {
                if decisions.len() == MAX_RECOVERY_ENTRIES {
                    return Err(delete_recovery_error(
                        "staged deletion recovery entry limit exceeded; files retained",
                    ));
                }
                if !entry
                    .file_type()
                    .await
                    .map_err(delete_recovery_error)?
                    .is_dir()
                {
                    return Err(delete_recovery_error("invalid staged thread directory"));
                }
                let id =
                    codex_protocol::ThreadId::from_string(&entry.file_name().to_string_lossy())
                        .map_err(delete_recovery_error)?;
                let root = entry.path();
                let restore = should_restore_staged_delete(store, kind, &root, id).await?;
                decisions.push((root, id, restore));
            }
        }
        let home = store.config.codex_home.clone();
        let mut ids: Vec<_> = decisions.iter().map(|(_, id, _)| *id).collect();
        ids.sort_by_key(ToString::to_string);
        ids.dedup();
        let mut leases = Vec::new();
        for id in ids {
            leases.push(store.acquire_writer_lease(id, false).await?);
        }
        let moves = std::sync::Arc::clone(&moves);
        tokio::task::spawn_blocking(move || -> ThreadStoreResult<()> {
            let _leases = leases;
            let _moves = moves;
            let mut rollout_locks = std::collections::BTreeMap::new();
            let home = std::fs::canonicalize(home).map_err(delete_recovery_error)?;
            let mut visited = 0usize;
            for (root, id, restore) in decisions {
                if !restore { mark_storage_retired(&home, id).map_err(delete_recovery_error)?; }
                let mut directories = vec![root.clone()];
                let mut empty_directories = Vec::new();
                let mut pending_retirement = None;
                while let Some(parent) = directories.pop() {
                    empty_directories.push(parent.clone());
                    for entry in std::fs::read_dir(parent).map_err(delete_recovery_error)? {
                        visited += 1;
                        if visited > MAX_RECOVERY_ENTRIES {
                            return Err(delete_recovery_error("staged deletion recovery entry limit exceeded; remaining files retained"));
                        }
                        let entry = entry.map_err(delete_recovery_error)?;
                        let kind = entry.file_type().map_err(delete_recovery_error)?;
                        if kind.is_dir() { directories.push(entry.path()); continue; }
                        if !kind.is_file() { return Err(delete_recovery_error("non-regular staged rollout")); }
                        let path = entry.path();
                        let relative = path.strip_prefix(&root).map_err(delete_recovery_error)?;
                        if relative == Path::new(AUXILIARY_RETIREMENT_PENDING) {
                            if !restore { retire_auxiliary_storage(&home, id).map_err(delete_recovery_error)?; }
                            pending_retirement = Some(path);
                            continue;
                        }
                        let first = relative.components().next().ok_or_else(|| delete_recovery_error("empty staged identity"))?;
                        if first.as_os_str() != SESSIONS_SUBDIR && first.as_os_str() != ARCHIVED_SESSIONS_SUBDIR {
                            return Err(delete_recovery_error("staged rollout is outside session storage"));
                        }
                        let original = home.join(relative);
                        matching_rollout_file_name(&path, id, &original)?;
                        let plain = codex_rollout::plain_rollout_path(&original);
                        if let std::collections::btree_map::Entry::Vacant(entry) = rollout_locks.entry(plain) {
                            let lock = codex_rollout::try_lock_rollout_for_maintenance(entry.key()).map_err(delete_recovery_error)?;
                            entry.insert(lock);
                        }
                        if restore {
                            if original.try_exists().map_err(delete_recovery_error)?
                                && !codex_rollout::rollout_files_same_identity(&path, &original).map_err(delete_recovery_error)?
                            {
                                return Err(delete_recovery_error(format!("restore conflict at {}; both files retained", original.display())));
                            }
                            let parent = original.parent().ok_or_else(|| {
                                delete_recovery_error("staged rollout has no parent; files retained")
                            })?;
                            std::fs::create_dir_all(parent).map_err(delete_recovery_error)?;
                            if !std::fs::canonicalize(parent).map_err(delete_recovery_error)?.starts_with(&home) {
                                return Err(delete_recovery_error("restore parent escapes codex home"));
                            }
                            restore_staged_rollout(&path, &original).map_err(delete_recovery_error)?;
                        } else {
                            std::fs::remove_file(path).map_err(delete_recovery_error)?;
                        }
                    }
                }
                for directory in empty_directories.into_iter().rev() {
                    // Keep the legacy rollback witness until the last staged
                    // rollout was restored, including across a second crash.
                    if directory == root
                        && let Some(path) = &pending_retirement
                    {
                        std::fs::remove_file(path).map_err(delete_recovery_error)?;
                    }
                    std::fs::remove_dir(directory).map_err(delete_recovery_error)?;
                }
            }
            // Remove only directories proven empty. Never recursively erase
            // unknown evidence or files added while recovery was in progress.
            for kind in STAGING_KINDS {
                match std::fs::remove_dir(directory.join(kind)) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(delete_recovery_error(err)),
                }
            }
            drop(owner);
            std::fs::remove_file(directory.join(".owner")).map_err(delete_recovery_error)?;
            std::fs::remove_dir(directory).map_err(delete_recovery_error)?;
            Ok(())
        }).await.map_err(delete_recovery_error)??;
    }
    Ok(true)
}

#[derive(Debug)]
struct StagedRolloutFile {
    original_path: PathBuf,
    staged_path: PathBuf,
}



/// A rollback guard for one or more local thread rollout deletions.
///
/// Dropping an uncommitted guard schedules restoration of every staged rollout. Committing keeps the rollout paths
/// undiscoverable, then performs compatibility-index and recorder cleanup on a best-effort basis.
#[derive(Debug)]
pub struct StagedThreadDelete<'a> {
    store: &'a LocalThreadStore,
    thread_ids: Vec<codex_protocol::ThreadId>,
    found_thread_ids: Vec<codex_protocol::ThreadId>,
    files: StagingCustody,
}

// This guard owns every filesystem mutation, including while a blocking worker
// runs after its async caller is cancelled.
#[derive(Debug)]
struct StagedRolloutFiles {
    staged_files: Vec<StagedRolloutFile>,
    staging_dir: Option<tempfile::TempDir>,
    owner_lock: Option<std::fs::File>,
    committed: bool,
    // Retained through rollback/commit, including detached filesystem work.
    _writer_leases: Vec<std::sync::Arc<std::fs::File>>,
    _publication_orders: Vec<Arc<tokio::sync::OwnedMutexGuard<()>>>,
    recorder_pauses: Vec<codex_rollout::RolloutMaintenanceGuard>,
    _rollout_locks: Vec<std::fs::File>,
    retirement_pending: bool,
    _moves: Option<std::fs::File>,
}

// Keep staging and its leases owned even if the async observer is cancelled.
// Return this wrapper from blocking workers so dropping a completed but unread
// result cannot run restoration or TempDir cleanup on an async worker.
#[derive(Debug)]
struct StagingCustody {
    files: Option<StagedRolloutFiles>,
    runtime: tokio::runtime::Handle,
}

impl StagingCustody {
    fn release(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let files = self.files.take()?;
        Some(self.runtime.spawn_blocking(move || drop(files)))
    }
}

impl Drop for StagingCustody {
    fn drop(&mut self) {
        if let Some(worker) = self.release() {
            // Dropping the observer doesn't cancel an admitted blocking task.
            drop(worker);
        }
    }
}

impl StagedThreadDelete<'_> {
    pub fn found_thread(&self, thread_id: codex_protocol::ThreadId) -> bool {
        self.found_thread_ids.contains(&thread_id)
    }

    /// Wait for restoration before reporting an explicit pre-commit failure.
    pub async fn rollback(mut self) {
        if let Some(worker) = self.files.release()
            && let Err(error) = worker.await
        {
            tracing::warn!(%error, "staged rollout rollback did not finish");
        }
    }

    pub async fn commit(self) {
        let home = self.store.config.codex_home.clone();
        let ids = self.thread_ids.clone();
        let mut custody = self.files;
        if let Some(files) = &mut custody.files {
            files.committed = true;
            files.retirement_pending = true;
        }
        // Once the database commit has succeeded, even a queued job must not
        // restore the rollout if its observer disappears.
        let mut committed = tokio::task::spawn_blocking(move || {
            let Some(mut files) = custody.files.take() else {
                return custody;
            };
            let staging = files.staging_dir.as_ref().map(|directory| directory.path().to_path_buf());
            // The primary transaction witnesses all SQLite-backed threads,
            // including absent rows. Only a filesystem-owned batch needs a rename.
            if let Some(directory) = staging.as_ref() {
                let pending = directory.join(SQLITE_LESS_STAGING);
                if pending.exists()
                    && let Err(err) = std::fs::rename(&pending, directory.join(COMMITTED_STAGING))
                {
                    tracing::warn!(%err, "failed to mark SQLite-less staged deletion committed");
                }
            }
            let mut incomplete = false;
            for id in ids {
                let result = mark_storage_retired(&home, id)
                    .and_then(|()| retire_auxiliary_storage(&home, id)).and_then(|()| {
                    if let Some(directory) = &staging {
                        for kind in STAGING_KINDS {
                            let pending = directory.join(kind).join(id.to_string()).join(AUXILIARY_RETIREMENT_PENDING);
                            match std::fs::remove_file(pending) {
                                Ok(()) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    incomplete = true;
                    tracing::warn!(%error, thread_id = %id, "committed auxiliary retirement deferred until writers close");
                }
            }
            files.retirement_pending = incomplete;
            custody.files = Some(files);
            custody
        }).await.map_err(|error| {
            tracing::warn!(%error, "auxiliary retirement worker incomplete; recovery queue retained");
        });
        if let Err(err) = remove_threads_name_entries(
            self.store.config.codex_home.as_path(),
            &self.thread_ids,
        )
        .await
        {
            tracing::warn!(
                "failed to delete thread name index entries after committing thread deletion: {err}"
            );
        }

        {
            let mut live_recorders = self.store.live_recorders.lock().await;
            for thread_id in &self.thread_ids {
                live_recorders.remove(thread_id);
            }
        }

        {
            let mut projections = self.store.projections.lock().await;
            for thread_id in &self.thread_ids {
                projections.remove(thread_id);
            }
        }
        if let Ok(staging) = &mut committed
            && let Some(worker) = staging.release()
            && let Err(error) = worker.await
        {
            tracing::warn!(%error, "committed staging cleanup did not finish");
        }
        // Payload blob collection is deferred by design, so deletion does not scan
        // `rollout-payloads`; the compression worker reclaims abandoned stages.
    }
}

impl StagedRolloutFiles {
    fn restore(&mut self) -> bool {
        let mut retain_staging = false;
        for staged in self.staged_files.iter().rev() {
            if matches!(staged.staged_path.try_exists(), Ok(false)) {
                continue;
            }
            let restore = restore_staged_rollout(&staged.staged_path, &staged.original_path);
            if let Err(err) = restore {
                retain_staging = true;
                tracing::error!(
                    "failed to restore staged rollout `{}` to `{}`: {err}",
                    staged.staged_path.display(),
                    staged.original_path.display()
                );
            }
        }
        if retain_staging && let Some(staging_dir) = self.staging_dir.take() {
            let retained_path = staging_dir.keep();
            tracing::error!(
                "retained unrestored rollouts in `{}`",
                retained_path.display()
            );
        }
        !retain_staging
    }
}

fn restore_staged_rollout(stage: &Path, original: &Path) -> std::io::Result<()> {
    // Validate even the absent-destination branch. Hard-linking a substituted
    // symlink must not install it as a restored ordinary rollout.
    if !codex_rollout::rollout_files_same_identity(stage, stage)? {
        return Err(std::io::Error::other(
            "staged rollout identity changed; evidence retained",
        ));
    }
    match std::fs::hard_link(stage, original) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !codex_rollout::rollout_files_same_identity(stage, original)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "rollout restore conflict; both files retained",
        ));
    }
    std::fs::remove_file(stage)
}

impl Drop for StagedRolloutFiles {
    fn drop(&mut self) {
        let restored = self.committed || self.restore();
        if self.committed
            && self.retirement_pending
            && let Some(directory) = self.staging_dir.take()
        {
            let retained = directory.keep();
            tracing::warn!(path = %retained.display(), "committed thread storage retirement queued for recovery");
        }
        // Release the open Windows handle before TempDir removes the lock file.
        self.owner_lock.take();
        // A resumed recorder must never race restoration or reacquire its own
        // physical maintenance lock. Make this order explicit, including cancellation.
        self._rollout_locks.clear();
        self._moves.take();
        for pause in self.recorder_pauses.drain(..) {
            if self.committed {
                pause.commit();
            } else if !restored {
                pause.restore_failed();
            } else {
                drop(pause);
            }
        }
    }
}

#[derive(Clone)]
struct RollbackStagingRetry {
    deadline: tokio::time::Instant,
    conflict_message: String,
}

fn check_rollback_staging_cutoff(retry: Option<&RollbackStagingRetry>) -> ThreadStoreResult<()> {
    if let Some(retry) = retry
        && tokio::time::Instant::now() >= retry.deadline
    {
        return Err(ThreadStoreError::Conflict {
            message: retry.conflict_message.clone(),
        });
    }
    Ok(())
}

pub(super) async fn rollback_created_thread(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<()> {
    let store = store.clone();
    // StateRuntime may await auxiliary cleanup after its primary transaction
    // commits. Retain the staged files until the whole commit path finishes,
    // so dropping the caller cannot restore files for an already-deleted row.
    tokio::spawn(async move {
        if store.live_recorder(thread_id).await.is_ok() {
            super::live_writer::discard_thread(&store, thread_id).await?;
        }
        // Background rollout maintenance may briefly own the moves lock just
        // when startup needs to roll back an unadmitted thread. Retry only
        // pre-staging ownership conflicts, never a database commit or an
        // arbitrary persistence failure. Each failed attempt releases its
        // guards before sleeping; the existing owned task retains cleanup if
        // the request stops waiting.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut retry = None;
        let staged = loop {
            match stage_thread_deletes_inner(&store, &[thread_id], None, retry.clone()).await {
                Ok(staged) => break staged,
                Err(ThreadStoreError::Conflict { message }) => {
                    let retry = retry.get_or_insert(RollbackStagingRetry {
                        deadline,
                        conflict_message: message,
                    });
                    check_rollback_staging_cutoff(Some(retry))?;
                    tokio::time::sleep_until(
                        deadline.min(
                            tokio::time::Instant::now() + std::time::Duration::from_millis(25),
                        ),
                    )
                    .await;
                }
                Err(error) => return Err(error),
            }
        };
        commit_thread_delete(staged, thread_id).await
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("thread rollback persistence task failed: {err}"),
    })?
}

pub(super) async fn delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let staged = stage_thread_deletes(store, &[thread_id]).await?;
    if !staged.found_thread(thread_id) {
        staged.rollback().await;
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }
    // Cancellation during staging still restores files. Once SQLite deletion
    // starts, an owned task must finish the same commit path as rollback.
    let StagedThreadDelete {
        store,
        thread_ids,
        found_thread_ids,
        files,
    } = staged;
    let store = store.clone();
    tokio::spawn(async move {
        let staged = StagedThreadDelete {
            store: &store,
            thread_ids,
            found_thread_ids,
            files,
        };
        commit_thread_delete(staged, thread_id).await
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("thread deletion persistence task failed: {err}"),
    })?
}

async fn commit_thread_delete(
    staged: StagedThreadDelete<'_>,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<()> {
    // Recovery already ran before staging acquired rollout-moves.lock.
    if let Some(state_db) = staged.store.state_db.as_ref()
        && let Err(err) = state_db.delete_thread(thread_id).await
    {
        staged.rollback().await;
        return Err(ThreadStoreError::Internal {
            message: format!("failed to delete state for thread {thread_id}: {err}"),
        });
    }
    staged.commit().await;
    Ok(())
}

pub(super) async fn stage_thread_deletes<'a>(
    store: &'a LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<StagedThreadDelete<'a>> {
    stage_thread_deletes_inner(store, thread_ids, None, None).await
}

async fn stage_thread_deletes_inner<'a>(
    store: &'a LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
    archive_cutoff: Option<i64>,
    rollback_retry: Option<RollbackStagingRetry>,
) -> ThreadStoreResult<StagedThreadDelete<'a>> {
    check_rollback_staging_cutoff(rollback_retry.as_ref())?;
    // Retry recovery even if initialization skipped a then-live owner.
    if !recover_staged_deletes(store).await? {
        return Err(ThreadStoreError::Conflict {
            message: "rollout maintenance is busy; deletion recovery deferred".to_string(),
        });
    }
    let mut seen = std::collections::HashSet::new();
    let requested_ids: Vec<_> = thread_ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    let mut ordered_ids = requested_ids.clone();
    ordered_ids.sort_by_key(ToString::to_string);
    ordered_ids.dedup();
    // Match metadata/live writer lock order. Stale staging recovery above
    // owns moves first and must never wait for a local publication order.
    // Obtain all orders in a stable order before pausing any recorder.
    let mut publication_orders = Vec::with_capacity(ordered_ids.len());
    for &id in &ordered_ids {
        publication_orders.push(Arc::new(store.lock_rollout_publication(id).await));
    }
    let mut writer_leases = Vec::new();
    let mut recorder_pauses = Vec::new();
    for (id, order) in ordered_ids.into_iter().zip(&publication_orders) {
        // Never hold the registry mutex or moves lock while waiting for a writer.
        if let Ok(recorder) = store.live_recorder(id).await {
            let lease = match recorder.writer_lease() {
                Some(lease) => lease,
                None => store.acquire_writer_lease(id, false).await?,
            };
            // Inspect the authoritative head under the order and exact writer
            // lease, without reentering staged recovery via state_db().
            let preserve_bytes = if let Some(db) = store.state_db.as_ref() {
                match db.next_thread_git_compatibility_intent(id).await {
                    Ok(intent) => intent.is_some(),
                    Err(error) => {
                        // Unknown is not permission to repair or append. The
                        // authoritative delete transaction can still retire it.
                        tracing::warn!(thread_id = %id, %error, "preserving rollout bytes during deletion: Git intent state is unknown");
                        true
                    }
                }
            } else {
                false
            };
            let pause = recorder
                .pause_for_maintenance_ordered(Arc::clone(order), preserve_bytes)
                .await
                .map_err(delete_recovery_error)?;
            writer_leases.push(lease);
            recorder_pauses.push(pause);
        } else {
            writer_leases.push(store.acquire_writer_lease(id, false).await?);
        }
    }
    let moves = codex_rollout::lock_rollout_moves(&store.config.codex_home)
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                ThreadStoreError::Conflict {
                    message: error.to_string(),
                }
            } else {
                delete_recovery_error(error)
            }
        })?;
    let (moves, recorder_pauses, writer_leases, publication_orders) =
        if let Some(cutoff) = archive_cutoff {
            // Keep the move fence and all writer ownership until the age check settles.
            let owners = (moves, recorder_pauses, writer_leases, publication_orders);
            tokio::task::spawn_blocking(move || {
                for lease in &owners.2 {
                    let modified: chrono::DateTime<chrono::Utc> = lease
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .map_err(delete_recovery_error)?
                        .into();
                    if modified.timestamp() > cutoff {
                        return Err(ThreadStoreError::Conflict {
                            message: "recent archive or newly observed writer lease".into(),
                        });
                    }
                }
                Ok(owners)
            })
            .await
            .map_err(delete_recovery_error)??
        } else {
            (moves, recorder_pauses, writer_leases, publication_orders)
        };
    let staging_kind = if store.state_db.is_some() {
        SQLITE_BACKED_STAGING
    } else {
        // No handle may mean that initialization failed, not that this home
        // has no SQLite owner. Never stage a filesystem-only deletion then.
        require_sqlite_absent(store).await?;
        SQLITE_LESS_STAGING
    };
    let mut found_thread_ids = Vec::new();
    let mut candidates = Vec::new();
    let mut auxiliary_threads = Vec::new();
    let mut inventory = codex_rollout::ThreadRolloutInventory::scan(
        &store.config.codex_home, &requested_ids,
    ).await;
    for thread_id in &requested_ids {
        let paths = inventory.take_paths(*thread_id).await.map_err(delete_recovery_error)?;
        if archive_cutoff.is_some() && (paths.is_empty() || paths.iter().any(|p| p.parent() != Some(store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR).as_path()))) {
            return Err(ThreadStoreError::Conflict {message: "archive has no unique archived location".into()});
        }
        if !paths.is_empty() {
            found_thread_ids.push(*thread_id);
        }
        let indexed = if let Some(db) = store.state_db.as_ref() {
            db.get_thread(*thread_id)
                .await
                .map_err(delete_recovery_error)?
                .is_some()
        } else {
            false
        };
        if !paths.is_empty() || indexed {
            auxiliary_threads.push(*thread_id);
        }
        candidates.extend(paths.into_iter().map(|path| (*thread_id, path)));
    }
    check_rollback_staging_cutoff(rollback_retry.as_ref())?;
    let codex_home = store.config.codex_home.clone();
    let runtime = tokio::runtime::Handle::current();
    let files =
        tokio::task::spawn_blocking(move || {
            // Own pauses and locks before any fallible filesystem work so every
            // return path restores/unlocks before the existing writers resume.
            let mut files = StagedRolloutFiles {
                staged_files: Vec::new(),
                staging_dir: None,
                owner_lock: None,
                committed: false,
                _writer_leases: writer_leases,
                _publication_orders: publication_orders,
                recorder_pauses,
                _rollout_locks: Vec::new(),
                retirement_pending: false,
                _moves: Some(moves),
            };
            // A queued worker may not start until after the retry cutoff.
            check_rollback_staging_cutoff(rollback_retry.as_ref())?;
            let mut paths: Vec<_> = candidates
                .iter()
                .map(|(_, path)| codex_rollout::plain_rollout_path(path))
                .collect();
            paths.sort();
            paths.dedup();
            for path in paths {
                files._rollout_locks.push(
                    codex_rollout::try_lock_rollout_for_maintenance(&path)
                        .map_err(delete_recovery_error)?,
                );
            }
            let mut original_paths = Vec::new();
            let canonical_home =
                std::fs::canonicalize(&codex_home).map_err(delete_recovery_error)?;
            for (thread_id, rollout_path) in candidates {
                let plain_path = codex_rollout::plain_rollout_path(&rollout_path);
                for path in [plain_path.clone(), plain_path.with_extension("jsonl.zst")] {
                    if !path.try_exists().map_err(|err| ThreadStoreError::Internal {
                    message: format!(
                        "failed to inspect rollout file `{}` before staging deletion: {err}",
                        path.display()
                    ),
                })? {
                    continue;
                }
                    let checked_path = checked_rollout_path(&codex_home, &path, thread_id)?;
                    if !original_paths
                        .iter()
                        .any(|(_, path)| path == &checked_path)
                    {
                        original_paths.push((thread_id, checked_path));
                    }
                }
            }
            // Admit the retry before creating any staged state. Once admitted,
            // finish staging and commit (or restore) even if the cutoff passes.
            check_rollback_staging_cutoff(rollback_retry.as_ref())?;
            let staging_dir = if original_paths.is_empty() && auxiliary_threads.is_empty() {
                None
            } else {
                Some(
                    tempfile::Builder::new()
                        .prefix(RECOVERABLE_DELETE_PREFIX)
                        .tempdir_in(&codex_home)
                        .map_err(|err| ThreadStoreError::Internal {
                            message: format!(
                                "failed to create thread deletion staging directory: {err}"
                            ),
                        })?,
                )
            };
            let owner_lock = staging_dir
                .as_ref()
                .map(|directory| {
                    let lock = std::fs::File::options()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .open(directory.path().join(".owner"))?;
                    lock.lock()?;
                    Ok::<_, std::io::Error>(lock)
                })
                .transpose()
                .map_err(delete_recovery_error)?;
            files.staging_dir = staging_dir;
            files.owner_lock = owner_lock;
            if let Some(directory) = &files.staging_dir {
                for id in auxiliary_threads {
                    let pending = directory
                        .path()
                        .join(staging_kind)
                        .join(id.to_string())
                        .join(AUXILIARY_RETIREMENT_PENDING);
                    std::fs::create_dir_all(
                        pending
                            .parent()
                            .ok_or_else(|| delete_recovery_error("retirement has no parent"))?,
                    )
                    .map_err(delete_recovery_error)?;
                    std::fs::File::create(pending)
                        .and_then(|file| file.sync_all())
                        .map_err(delete_recovery_error)?;
                    files.retirement_pending = true;
                }
            }
            for (thread_id, original_path) in original_paths {
                let relative = original_path
                    .strip_prefix(&canonical_home)
                    .map_err(delete_recovery_error)?;
                let staged_path = files
                    .staging_dir
                    .as_ref()
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: "thread deletion staging directory is missing".to_string(),
                    })?
                    .path()
                    .join(staging_kind)
                    .join(thread_id.to_string())
                    .join(relative);
                let staged_parent = staged_path.parent().ok_or_else(|| {
                    delete_recovery_error("staged rollout has no parent; deletion not staged")
                })?;
                std::fs::create_dir_all(staged_parent).map_err(delete_recovery_error)?;
                std::fs::rename(&original_path, &staged_path).map_err(|err| {
                    ThreadStoreError::Internal {
                        message: format!(
                            "failed to stage rollout file `{}` for deletion: {err}",
                            original_path.display()
                        ),
                    }
                })?;
                files.staged_files.push(StagedRolloutFile {
                    original_path,
                    staged_path,
                });
            }
            Ok::<_, ThreadStoreError>(StagingCustody {
                files: Some(files),
                runtime,
            })
        })
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("thread deletion staging worker failed: {err}"),
        })??;
    Ok(StagedThreadDelete {
        store,
        thread_ids: requested_ids,
        found_thread_ids,
        files,
    })
}

/// Use a maintenance-only store (no process-local recorder to pause). A live
/// writer in any store therefore conflicts instead of being quiesced for expiry.
pub(super) async fn expire_archives(store: &LocalThreadStore, cutoff: i64) -> ThreadStoreResult<()> {
    let Some(db) = store.state_db.as_ref() else { return Ok(()) };
    let archive = store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
    let archive = match tokio::fs::canonicalize(archive).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(delete_recovery_error(error)),
    };
    // The index can outlive a rollout. Do not rescan all active histories for
    // each stale row or create writer leases for conversations with no archive.
    let mut physical = std::collections::HashSet::new();
    let mut entries = tokio::fs::read_dir(&archive).await.map_err(delete_recovery_error)?;
    while let Some(entry) = entries.next_entry().await.map_err(delete_recovery_error)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let name = name.strip_suffix(".zst").unwrap_or(name);
        if let Some(stem) = name.strip_suffix(".jsonl")
            && let Some(id) = stem.get(stem.len().saturating_sub(36)..)
            && let Ok(id) = codex_protocol::ThreadId::from_string(id) {
            physical.insert(id);
        }
    }
    if archive != tokio::fs::canonicalize(&store.config.codex_home).await.map_err(delete_recovery_error)?.join(ARCHIVED_SESSIONS_SUBDIR) {
        return Err(delete_recovery_error("archive root is redirected"));
    }
    for id in db.expired_archive_ids(cutoff).await.map_err(delete_recovery_error)? {
        if !physical.contains(&id) {continue}
        let Some(indexed) = db.get_thread(id).await.map_err(delete_recovery_error)? else {continue};
        let Some(parent) = indexed.rollout_path.parent() else {continue};
        if tokio::fs::canonicalize(parent).await.ok().as_ref() != Some(&archive) {continue}
        let indexed_name = codex_rollout::plain_rollout_path(&indexed.rollout_path);
        let staged = match stage_thread_deletes_inner(store, &[id], Some(cutoff), None).await {
            Ok(staged) => staged,
            Err(err) => { tracing::warn!(%id, %err, "archive expiry deferred"); continue }
        };
        // Filesystem wins over a stale index. No active alias may be retired.
        let only_archived = staged.files.files.as_ref().is_some_and(|files|
            !files.staged_files.is_empty() && files.staged_files.iter().all(|file|
                file.original_path.parent() == Some(archive.as_path()) &&
                codex_rollout::plain_rollout_path(&file.original_path).file_name() == indexed_name.file_name()));
        if !only_archived { staged.rollback().await; continue }
        match db.delete_expired_archive(id, cutoff, &indexed.rollout_path).await {
            Ok(_) => staged.commit().await,
            Err(err) => { staged.rollback().await; tracing::warn!(%id, %err, "archive expiry deferred"); }
        }
    }
    Ok(())
}

pub(super) async fn preflight_delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    // Unlike staging, preflight has not performed initialization recovery yet.
    let _ = store.state_db().await;
    let rollout_paths = rollout_paths(store, thread_id).await?;
    if rollout_paths.is_empty() {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }
    let codex_home = store.config.codex_home.clone();
    tokio::task::spawn_blocking(move || {
        for rollout_path in rollout_paths {
            preflight_rollout_file(&codex_home, rollout_path.as_path(), thread_id)?;
        }
        Ok(())
    })
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("thread deletion preflight worker failed: {err}"),
    })?
}

async fn rollout_paths(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<Vec<PathBuf>> {
    codex_rollout::find_thread_rollout_paths(&store.config.codex_home, thread_id)
        .await
        .map_err(delete_recovery_error)
}

fn preflight_rollout_file(
    codex_home: &Path,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<()> {
    let plain_path = codex_rollout::plain_rollout_path(rollout_path);
    for path in [plain_path.clone(), plain_path.with_extension("jsonl.zst")] {
        if !path
            .try_exists()
            .map_err(|err| ThreadStoreError::Internal {
                message: format!(
                    "failed to inspect rollout file `{}` before deletion: {err}",
                    path.display()
                ),
            })?
        {
            continue;
        }
        let checked_path = checked_rollout_path(codex_home, path.as_path(), thread_id)?;
        if checked_path
            .try_exists()
            .map_err(|err| ThreadStoreError::Internal {
                message: format!(
                    "failed to inspect rollout file `{}` before deletion: {err}",
                    checked_path.display()
                ),
            })?
        {
            preflight_delete_access(checked_path.as_path())?;
        }
    }
    Ok(())
}

fn preflight_delete_access(path: &Path) -> ThreadStoreResult<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const DELETE_ACCESS: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;

    std::fs::OpenOptions::new()
        .access_mode(DELETE_ACCESS)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .map(|_| ())
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("rollout file `{}` cannot be deleted: {err}", path.display()),
        })
}

fn checked_rollout_path(
    codex_home: &Path,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<PathBuf> {
    let canonical_rollout_path =
        scoped_rollout_path(codex_home.join(SESSIONS_SUBDIR), rollout_path, "sessions")
            .or_else(|_| {
                scoped_rollout_path(
                    codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                    rollout_path,
                    "archived sessions",
                )
            })
            .or_else(|err| match rollout_path.try_exists() {
                Ok(false) => Ok(rollout_path.to_path_buf()),
                Ok(true) | Err(_) => Err(err),
            })?;
    matching_rollout_file_name(&canonical_rollout_path, thread_id, rollout_path)?;
    Ok(canonical_rollout_path)
}

