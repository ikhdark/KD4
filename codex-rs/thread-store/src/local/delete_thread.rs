//! Local hard-delete support for persisted threads.
//!
//! Local rollout removal can be staged so callers that also own SQLite state have one logical
//! commit point. Staged files are restored on drop until the caller commits the deletion.

#[cfg(test)]
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use codex_rollout::ARCHIVED_SESSIONS_SUBDIR;
use codex_rollout::SESSIONS_SUBDIR;
use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::find_thread_path_by_id_str;
use codex_rollout::remove_thread_name_entries;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::rollout_lookup_error;
use super::helpers::scoped_rollout_path;
use crate::DeleteThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

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
    staged_files: Vec<StagedRolloutFile>,
    staging_dir: Option<tempfile::TempDir>,
    committed: bool,
}

impl StagedThreadDelete<'_> {
    pub fn found_thread(&self, thread_id: codex_protocol::ThreadId) -> bool {
        self.found_thread_ids.contains(&thread_id)
    }

    pub async fn commit(mut self) {
        self.committed = true;
        for thread_id in &self.thread_ids {
            if let Err(err) =
                remove_thread_name_entries(self.store.config.codex_home.as_path(), *thread_id).await
            {
                tracing::warn!(
                    "failed to delete thread name index entries for {thread_id} after committing thread deletion: {err}"
                );
            }
        }

        {
            let mut live_recorders = self.store.live_recorders.lock().await;
            for thread_id in &self.thread_ids {
                live_recorders.remove(thread_id);
            }
        }

        let mut projections = self.store.projections.lock().await;
        for thread_id in &self.thread_ids {
            projections.remove(thread_id);
        }
    }

    fn restore(&mut self) {
        for staged in self.staged_files.iter().rev() {
            if !staged.staged_path.exists() {
                continue;
            }
            if let Err(err) = std::fs::rename(&staged.staged_path, &staged.original_path) {
                tracing::error!(
                    "failed to restore staged rollout `{}` to `{}`: {err}",
                    staged.staged_path.display(),
                    staged.original_path.display()
                );
            }
        }
    }
}

impl Drop for StagedThreadDelete<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.restore();
        }
    }
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
    staged.commit().await;
    Ok(())
}

pub(super) async fn stage_thread_deletes<'a>(
    store: &'a LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<StagedThreadDelete<'a>> {
    let mut found_thread_ids = Vec::new();
    let mut original_paths = Vec::new();

    for thread_id in thread_ids {
        let paths = rollout_paths(store, *thread_id).await?;
        if !paths.is_empty() {
            found_thread_ids.push(*thread_id);
        }
        for rollout_path in paths {
            let plain_path = codex_rollout::plain_rollout_path(&rollout_path);
            for path in [plain_path.clone(), plain_path.with_extension("jsonl.zst")] {
                if !path
                    .try_exists()
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to inspect rollout file `{}` before staging deletion: {err}",
                            path.display()
                        ),
                    })?
                {
                    continue;
                }
                let checked_path = checked_rollout_path(store, path.as_path(), *thread_id)?;
                if !original_paths.contains(&checked_path) {
                    original_paths.push(checked_path);
                }
            }
        }
    }

    let staging_dir = if original_paths.is_empty() {
        None
    } else {
        Some(
            tempfile::Builder::new()
                .prefix("thread-delete-")
                .tempdir_in(store.config.codex_home.as_path())
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to create thread deletion staging directory: {err}"),
                })?,
        )
    };
    let mut staged = StagedThreadDelete {
        store,
        thread_ids: thread_ids.to_vec(),
        found_thread_ids,
        staged_files: Vec::new(),
        staging_dir,
        committed: false,
    };
    let staging_path = staged
        .staging_dir
        .as_ref()
        .map(|staging_dir| staging_dir.path().to_path_buf());

    for (index, original_path) in original_paths.into_iter().enumerate() {
        let staged_path = staging_path
            .as_ref()
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "thread deletion staging directory is missing".to_string(),
            })?
            .join(format!("rollout-{index}"));
        std::fs::rename(&original_path, &staged_path).map_err(|err| {
            ThreadStoreError::Internal {
                message: format!(
                    "failed to stage rollout file `{}` for deletion: {err}",
                    original_path.display()
                ),
            }
        })?;
        staged.staged_files.push(StagedRolloutFile {
            original_path,
            staged_path,
        });
    }

    Ok(staged)
}

pub(super) async fn preflight_delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let rollout_paths = rollout_paths(store, thread_id).await?;
    if rollout_paths.is_empty() {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }
    for rollout_path in rollout_paths {
        preflight_rollout_file(store, rollout_path.as_path(), thread_id)?;
    }
    Ok(())
}

async fn rollout_paths(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<Vec<PathBuf>> {
    let thread_id_str = thread_id.to_string();
    let state_db_ctx = store.state_db().await;
    let mut rollout_paths = Vec::new();

    match find_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) => rollout_paths.push(path),
        Ok(None) => {}
        Err(err) => {
            return Err(rollout_lookup_error(
                thread_id, /*archived*/ false, err,
            ));
        }
    }

    match find_archived_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) if !rollout_paths.contains(&path) => rollout_paths.push(path),
        Ok(Some(_)) | Ok(None) => {}
        Err(err) => {
            return Err(rollout_lookup_error(thread_id, /*archived*/ true, err));
        }
    }

    Ok(rollout_paths)
}

fn preflight_rollout_file(
    store: &LocalThreadStore,
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
        let checked_path = checked_rollout_path(store, path.as_path(), thread_id)?;
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

#[cfg(test)]
fn delete_rollout_file(
    store: &LocalThreadStore,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let plain_path = codex_rollout::plain_rollout_path(rollout_path);
    let compressed_path = plain_path.with_extension("jsonl.zst");
    let deleted_plain = delete_rollout_path(store, plain_path.as_path(), thread_id)?;
    let deleted_compressed = delete_rollout_path(store, compressed_path.as_path(), thread_id)?;
    Ok(deleted_plain || deleted_compressed)
}

#[cfg(test)]
fn delete_rollout_path(
    store: &LocalThreadStore,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<bool> {
    let canonical_rollout_path = checked_rollout_path(store, rollout_path, thread_id)?;
    match std::fs::remove_file(&canonical_rollout_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!(
                "failed to delete rollout file `{}`: {err}",
                canonical_rollout_path.display()
            ),
        }),
    }
}

fn checked_rollout_path(
    store: &LocalThreadStore,
    rollout_path: &Path,
    thread_id: codex_protocol::ThreadId,
) -> ThreadStoreResult<PathBuf> {
    let canonical_rollout_path = scoped_rollout_path(
        store.config.codex_home.join(SESSIONS_SUBDIR),
        rollout_path,
        "sessions",
    )
    .or_else(|_| {
        scoped_rollout_path(
            store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
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

#[cfg(test)]
mod tests {
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::ThreadStore;
    use crate::local::LocalThreadStore;
    use crate::local::test_support::test_config;
    use crate::local::test_support::write_archived_session_file;
    use crate::local::test_support::write_session_file;

    #[tokio::test]
    async fn delete_thread_removes_active_and_archived_rollouts() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let active_path =
            write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(301))
                .expect("session file");
        let compressed_path = active_path.with_extension("jsonl.zst");
        std::fs::write(&compressed_path, b"compressed sibling").expect("compressed sibling");
        let cases = [
            (Uuid::from_u128(301), active_path),
            (
                Uuid::from_u128(302),
                write_archived_session_file(
                    home.path(),
                    "2025-01-03T12-00-00",
                    Uuid::from_u128(302),
                )
                .expect("archived session file"),
            ),
        ];

        for (uuid, path) in cases {
            let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
            store
                .delete_thread(DeleteThreadParams { thread_id })
                .await
                .expect("delete thread");

            assert!(!path.exists());
        }
        assert!(!compressed_path.exists());
    }

    #[tokio::test]
    async fn staged_thread_delete_restores_rollout_when_not_committed() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(307);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-30-00", uuid).expect("session file");

        let staged = store
            .stage_thread_deletes(&[thread_id])
            .await
            .expect("stage delete");
        assert!(!path.exists());
        drop(staged);

        assert!(path.exists());
    }

    #[tokio::test]
    async fn delete_rollout_file_treats_vanished_path_as_already_deleted() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(305);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        std::fs::remove_file(&path).expect("remove session file");

        assert!(!delete_rollout_file(&store, path.as_path(), thread_id).expect("delete rollout"));
    }

    #[tokio::test]
    async fn delete_thread_reports_missing_thread() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000304").expect("valid thread id");

        let err = store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect_err("missing thread should fail");
        assert_eq!(
            err.to_string(),
            "thread 00000000-0000-0000-0000-000000000304 not found"
        );
    }

    #[tokio::test]
    async fn preflight_delete_rejects_rollout_locked_against_deletion() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(306);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002)
            .open(&path)
            .expect("lock rollout without delete sharing");

        store
            .preflight_delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect_err("locked rollout must fail before deletion begins");
        assert!(path.exists());
    }
}
