//! Local hard-delete support for persisted threads.
//!
//! Local rollout removal can be staged so callers that also own SQLite state have one logical
//! commit point. Staged files are restored on drop until the caller commits the deletion.

use std::path::Path;
use std::path::PathBuf;

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

fn auxiliary_paths(id: codex_protocol::ThreadId) -> [PathBuf; 4] {
    [
        PathBuf::from("tool-history").join(format!("{id}.json")),
        PathBuf::from("tool-history").join(format!("{id}.journal.jsonl")),
        PathBuf::from("command-execution-cache").join(format!("{id}.json")),
        PathBuf::from("code-mode-state").join(format!("{id}.json")),
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

fn mark_storage_retired(home: &Path, id: codex_protocol::ThreadId) -> std::io::Result<()> {
    let directory = home.join("tool-history");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{id}.retired"));
    match std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            std::io::Write::write_all(&mut file, b"KD4_THREAD_STORAGE_RETIRED_V1\n")?;
            file.sync_all()
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
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

fn delete_recovery_error(error: impl std::fmt::Display) -> ThreadStoreError {
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
                (name == "indexed" || name == "unindexed" || name == "committed") && kind.is_dir()
            };
            if !known {
                return Err(delete_recovery_error(
                    "unknown staged deletion layout; files retained",
                ));
            }
        }
        let mut decisions = Vec::new();
        for kind in ["indexed", "unindexed", "committed"] {
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
                let restore = match kind {
                    "indexed" => {
                        let db = store.state_db.as_ref().ok_or_else(|| delete_recovery_error(
                            "indexed deletion recovery requires its state database; staged files retained"))?;
                        db.get_thread(id)
                            .await
                            .map_err(delete_recovery_error)?
                            .is_some()
                    }
                    "unindexed" => !store
                        .config
                        .codex_home
                        .join("tool-history")
                        .join(format!("{id}.retired"))
                        .try_exists()
                        .map_err(delete_recovery_error)?,
                    _ => false,
                };
                decisions.push((entry.path(), id, restore));
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
                            std::fs::remove_file(&path).map_err(delete_recovery_error)?;
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
                    std::fs::remove_dir(directory).map_err(delete_recovery_error)?;
                }
            }
            // Remove only directories proven empty. Never recursively erase
            // unknown evidence or files added while recovery was in progress.
            for kind in ["indexed", "unindexed", "committed"] {
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
/// Dropping an uncommitted guard restores every staged rollout. Committing keeps the rollout paths
/// undiscoverable, then performs compatibility-index and recorder cleanup on a best-effort basis.
#[derive(Debug)]
pub struct StagedThreadDelete<'a> {
    store: &'a LocalThreadStore,
    thread_ids: Vec<codex_protocol::ThreadId>,
    found_thread_ids: Vec<codex_protocol::ThreadId>,
    files: StagedRolloutFiles,
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
    recorder_pauses: Vec<codex_rollout::RolloutMaintenanceGuard>,
    _rollout_locks: Vec<std::fs::File>,
    retirement_pending: bool,
    _moves: Option<std::fs::File>,
}

impl StagedThreadDelete<'_> {
    pub fn found_thread(&self, thread_id: codex_protocol::ThreadId) -> bool {
        self.found_thread_ids.contains(&thread_id)
    }

    pub async fn commit(mut self) {
        // SQLite is the commit witness for indexed threads. For SQLite-less
        // files, move the existing staging subtree to its committed name.
        if let Some(directory) = self.files.staging_dir.as_ref() {
            let pending = directory.path().join("unindexed");
            if pending.exists()
                && let Err(err) = std::fs::rename(&pending, directory.path().join("committed"))
            {
                tracing::warn!(%err, "failed to mark SQLite-less staged deletion committed");
            }
        }
        self.files.committed = true;
        // Keep the queue if cancellation drops this observer. The worker owns
        // each nonblocking auxiliary lease through slow storage and unlink;
        // it never blocks a Tokio worker waiting for filesystem completion.
        self.files.retirement_pending = true;
        let home = self.store.config.codex_home.clone();
        let ids = self.thread_ids.clone();
        let staging = self
            .files
            .staging_dir
            .as_ref()
            .map(|directory| directory.path().to_path_buf());
        match tokio::task::spawn_blocking(move || {
            let mut incomplete = false;
            for id in ids {
                let result = mark_storage_retired(&home, id)
                    .and_then(|()| retire_auxiliary_storage(&home, id)).and_then(|()| {
                    if let Some(directory) = &staging {
                        for kind in ["indexed", "unindexed", "committed"] {
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
            incomplete
        }).await {
            Ok(incomplete) => self.files.retirement_pending = incomplete,
            Err(error) => tracing::warn!(%error, "auxiliary retirement worker incomplete; recovery queue retained"),
        }
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
        let home = self.store.config.codex_home.clone();
        // Release the deletion/publication exclusion before optional collection.
        drop(self);
        if let Err(error) = codex_rollout::reclaim_payload_artifacts(&home).await {
            tracing::warn!(%error, "payload reclamation incomplete; unproven blobs retained");
        }
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
        loop {
            match stage_thread_deletes(&store, &[thread_id]).await {
                Ok(staged) => return commit_thread_delete(staged, thread_id).await,
                Err(ThreadStoreError::Conflict { .. })
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep_until(
                        deadline.min(
                            tokio::time::Instant::now() + std::time::Duration::from_millis(25),
                        ),
                    )
                    .await;
                }
                Err(error) => return Err(error),
            }
        }
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
    if let Some(state_db) = staged.store.state_db.as_ref() {
        state_db
            .delete_thread(thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to delete state for thread {thread_id}: {err}"),
            })?;
    }
    staged.commit().await;
    Ok(())
}

pub(super) async fn stage_thread_deletes<'a>(
    store: &'a LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<StagedThreadDelete<'a>> {
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
    let mut writer_leases = Vec::new();
    let mut recorder_pauses = Vec::new();
    for id in ordered_ids {
        // Never hold the registry mutex or moves lock while waiting for a writer.
        // Sorted IDs also serialize overlapping multi-thread deletions consistently.
        if let Ok(recorder) = store.live_recorder(id).await {
            let pause = recorder
                .pause_for_maintenance()
                .await
                .map_err(delete_recovery_error)?;
            if let Some(lease) = recorder.writer_lease() {
                writer_leases.push(lease);
            } else {
                writer_leases.push(store.acquire_writer_lease(id, false).await?);
            }
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
    let mut found_thread_ids = Vec::new();
    let mut candidates = Vec::new();
    let mut auxiliary_threads = Vec::new();
    let mut inventory = codex_rollout::ThreadRolloutInventory::scan(
        &store.config.codex_home, &requested_ids,
    ).await;
    for thread_id in &requested_ids {
        let paths = inventory.take_paths(*thread_id).await.map_err(delete_recovery_error)?;
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
            auxiliary_threads.push((*thread_id, indexed));
        }
        candidates.extend(paths.into_iter().map(|path| (*thread_id, indexed, path)));
    }
    let codex_home = store.config.codex_home.clone();
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
                recorder_pauses,
                _rollout_locks: Vec::new(),
                retirement_pending: false,
                _moves: Some(moves),
            };
            let mut paths: Vec<_> = candidates
                .iter()
                .map(|(_, _, path)| codex_rollout::plain_rollout_path(path))
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
            for (thread_id, indexed, rollout_path) in candidates {
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
                        .any(|(_, _, path)| path == &checked_path)
                    {
                        original_paths.push((thread_id, indexed, checked_path));
                    }
                }
            }
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
                for (id, indexed) in auxiliary_threads {
                    let pending = directory
                        .path()
                        .join(if indexed { "indexed" } else { "unindexed" })
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
            for (thread_id, indexed, original_path) in original_paths {
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
                    .join(if indexed { "indexed" } else { "unindexed" })
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
            Ok::<_, ThreadStoreError>(files)
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

