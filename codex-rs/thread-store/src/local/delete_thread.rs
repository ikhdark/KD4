//! Local hard-delete support for persisted threads.
//!
//! Existing rollout files are deleted before this operation reports success. A rollout file that
//! vanishes after discovery counts as already deleted. SQLite cleanup happens at the app-server
//! layer after every associated rollout has been removed so failed deletes can be retried.

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

pub(super) async fn delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let rollout_paths = rollout_paths(store, thread_id).await?;

    let found_rollout_path = !rollout_paths.is_empty();
    for rollout_path in rollout_paths {
        delete_rollout_file(store, rollout_path.as_path(), thread_id)?;
    }
    remove_thread_name_entries(store.config.codex_home.as_path(), thread_id)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to delete thread name index entries for {thread_id}: {err}"),
        })?;

    if !found_rollout_path {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }

    store.live_recorders.lock().await.remove(&thread_id);

    Ok(())
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
