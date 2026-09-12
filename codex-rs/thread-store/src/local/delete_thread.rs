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

#[cfg(test)]
tokio::task_local! {
    static TEST_STAGE_PAUSE: std::sync::Arc<TestStagePause>;
}

#[cfg(test)]
struct TestStagePause {
    reached: tokio::sync::Notify,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
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
    committed: bool,
}

impl StagedThreadDelete<'_> {
    pub fn found_thread(&self, thread_id: codex_protocol::ThreadId) -> bool {
        self.found_thread_ids.contains(&thread_id)
    }

    pub async fn commit(mut self) {
        self.files.committed = true;
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
}

impl StagedRolloutFiles {
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

impl Drop for StagedRolloutFiles {
    fn drop(&mut self) {
        if !self.committed {
            self.restore();
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
        let staged = stage_thread_deletes(&store, &[thread_id]).await?;
        if let Some(state_db) = store.state_db().await {
            state_db
                .delete_thread(thread_id)
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!(
                        "failed to delete state for rolled-back thread {thread_id}: {err}"
                    ),
                })?;
        }
        staged.commit().await;
        Ok(())
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
    staged.commit().await;
    Ok(())
}

pub(super) async fn stage_thread_deletes<'a>(
    store: &'a LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<StagedThreadDelete<'a>> {
    let mut found_thread_ids = Vec::new();
    let mut candidates = Vec::new();
    for thread_id in thread_ids {
        let paths = rollout_paths(store, *thread_id).await?;
        if !paths.is_empty() {
            found_thread_ids.push(*thread_id);
        }
        candidates.extend(paths.into_iter().map(|path| (*thread_id, path)));
    }
    let codex_home = store.config.codex_home.clone();
    #[cfg(test)]
    let pause = TEST_STAGE_PAUSE.try_with(std::sync::Arc::clone).ok();
    let files =
        tokio::task::spawn_blocking(move || {
            let mut original_paths = Vec::new();
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
                    if !original_paths.contains(&checked_path) {
                        original_paths.push(checked_path);
                    }
                }
            }
            let staging_dir = if original_paths.is_empty() {
                None
            } else {
                Some(
                    tempfile::Builder::new()
                        .prefix("thread-delete-")
                        .tempdir_in(&codex_home)
                        .map_err(|err| ThreadStoreError::Internal {
                            message: format!(
                                "failed to create thread deletion staging directory: {err}"
                            ),
                        })?,
                )
            };
            let mut files = StagedRolloutFiles {
                staged_files: Vec::new(),
                staging_dir,
                committed: false,
            };
            for (index, original_path) in original_paths.into_iter().enumerate() {
                let staged_path = files
                    .staging_dir
                    .as_ref()
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: "thread deletion staging directory is missing".to_string(),
                    })?
                    .path()
                    .join(format!("rollout-{index}"));
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
                #[cfg(test)]
                if let Some(pause) = &pause
                    && let Some(release) = pause.release.lock().expect("pause lock").take()
                {
                    pause.reached.notify_one();
                    let _ = release.recv();
                }
            }
            Ok::<_, ThreadStoreError>(files)
        })
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("thread deletion staging worker failed: {err}"),
        })??;
    Ok(StagedThreadDelete {
        store,
        thread_ids: thread_ids.to_vec(),
        found_thread_ids,
        files,
    })
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
    let canonical_rollout_path =
        checked_rollout_path(&store.config.codex_home, rollout_path, thread_id)?;
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

    #[tokio::test]
    async fn delete_thread_restores_all_files_when_a_sibling_cannot_be_staged() {
        use std::os::windows::fs::OpenOptionsExt;

        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), None);
        let uuid = Uuid::from_u128(308);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        let original = std::fs::read(&path).expect("original rollout");
        let compressed = path.with_extension("jsonl.zst");
        std::fs::write(&compressed, b"locked sibling").expect("compressed sibling");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001 | 0x0000_0002)
            .open(&compressed)
            .expect("deny deletion of sibling");

        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect_err("locked sibling must reject the entire deletion");

        assert_eq!(std::fs::read(&path).expect("restored rollout"), original);
        assert_eq!(
            std::fs::read(&compressed).expect("retained sibling"),
            b"locked sibling"
        );
        drop(lock);
        store
            .preflight_delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("restored thread remains discoverable and deletable");
        store
            .delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("retry deletion");
        assert!(!path.exists());
        assert!(!compressed.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_thread_cancellation_restores_files_owned_by_the_running_worker() {
        let home = TempDir::new().expect("temp dir");
        let store = std::sync::Arc::new(LocalThreadStore::new(test_config(home.path()), None));
        let uuid = Uuid::from_u128(309);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");
        let original = std::fs::read(&path).expect("original rollout");
        let (release, released) = std::sync::mpsc::channel();
        let pause = std::sync::Arc::new(TestStagePause {
            reached: tokio::sync::Notify::new(),
            release: std::sync::Mutex::new(Some(released)),
        });
        let deleting = std::sync::Arc::clone(&store);
        let operation = tokio::spawn(TEST_STAGE_PAUSE.scope(
            std::sync::Arc::clone(&pause),
            async move {
                deleting
                    .delete_thread(DeleteThreadParams { thread_id })
                    .await
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), pause.reached.notified())
            .await
            .expect("worker staged the real rollout without blocking the runtime");
        assert!(!path.exists());
        operation.abort();
        assert!(
            operation
                .await
                .expect_err("delete task cancelled")
                .is_cancelled()
        );
        drop(release);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if tokio::fs::read(&path).await.ok().as_deref() == Some(original.as_slice()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached worker restored the cancelled deletion");
        store
            .preflight_delete_thread(DeleteThreadParams { thread_id })
            .await
            .expect("cancelled deletion leaves the thread discoverable");
    }

    async fn rollback_fixture() -> (
        TempDir,
        LocalThreadStore,
        std::sync::Arc<codex_state::StateRuntime>,
        ThreadId,
        PathBuf,
    ) {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let state = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("real state databases");
        state.mark_backfill_complete(None).await.unwrap();
        let uuid = Uuid::new_v4();
        let thread_id = ThreadId::from_string(&uuid.to_string()).unwrap();
        let path = write_session_file(home.path(), "2025-01-03T12-30-00", uuid).unwrap();
        let builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            path.clone(),
            chrono::Utc::now(),
            codex_protocol::protocol::SessionSource::Cli,
        );
        state
            .upsert_thread_preserving_timestamps(&builder.build(&config.default_model_provider_id))
            .await
            .unwrap();
        codex_rollout::append_thread_name(home.path(), thread_id, "rolled-back child")
            .await
            .unwrap();
        let store = LocalThreadStore::new(config, Some(std::sync::Arc::clone(&state)));
        (home, store, state, thread_id, path)
    }

    #[tokio::test]
    async fn rollback_created_thread_removes_files_state_and_name_index() {
        for materialized in [true, false] {
            let (home, store, state, thread_id, path) = rollback_fixture().await;
            let compressed = path.with_extension("jsonl.zst");
            if materialized {
                std::fs::write(&compressed, b"compressed sibling").unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            store
                .rollback_created_thread(thread_id)
                .await
                .expect("complete local rollback");
            assert!(!path.exists());
            assert!(!compressed.exists());
            assert!(state.get_thread(thread_id).await.unwrap().is_none());
            assert!(
                codex_rollout::find_thread_name_by_id(home.path(), &thread_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                store
                    .read_thread(crate::ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: true,
                    })
                    .await,
                Err(ThreadStoreError::ThreadNotFound { .. })
            ));
            store
                .rollback_created_thread(thread_id)
                .await
                .expect("rollback is idempotent for missing persistence");
            state.close().await;
        }
    }

    #[tokio::test]
    async fn rollback_created_thread_restores_files_and_state_on_primary_failure() {
        use sqlx::Connection;
        let (home, store, state, thread_id, path) = rollback_fixture().await;
        let original = std::fs::read(&path).unwrap();
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(codex_state::state_db_path(home.path())),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER reject_rollback BEFORE DELETE ON threads BEGIN SELECT RAISE(ABORT, 'blocked primary delete'); END")
            .execute(&mut connection).await.unwrap();
        let error = store
            .rollback_created_thread(thread_id)
            .await
            .expect_err("real primary transaction must fail");
        assert!(error.to_string().contains("blocked primary delete"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(state.get_thread(thread_id).await.unwrap().is_some());
        assert_eq!(
            codex_rollout::find_thread_name_by_id(home.path(), &thread_id)
                .await
                .unwrap()
                .as_deref(),
            Some("rolled-back child")
        );
        store
            .read_thread(crate::ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await
            .expect("failed rollback remains normally readable");
        sqlx::query("DROP TRIGGER reject_rollback")
            .execute(&mut connection)
            .await
            .unwrap();
        store
            .rollback_created_thread(thread_id)
            .await
            .expect("retry completes both persistence surfaces");
        assert!(!path.exists());
        assert!(state.get_thread(thread_id).await.unwrap().is_none());
        connection.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test]
    async fn rollback_created_thread_completion_survives_cancellation_after_primary_commit() {
        use sqlx::Connection;
        use std::time::Duration;
        let (home, store, state, thread_id, path) = rollback_fixture().await;
        let mut logs_lock = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(codex_state::logs_db_path(home.path())),
        )
        .await
        .unwrap();
        // Block the real auxiliary DELETE after StateRuntime's primary transaction
        // commits, while the original caller would still own a restorable file guard.
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut logs_lock)
            .await
            .unwrap();
        let deleting = store.clone();
        let operation =
            tokio::spawn(async move { deleting.rollback_created_thread(thread_id).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while state.get_thread(thread_id).await.unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("primary SQLite delete commits while auxiliary database is locked");
        assert!(
            !path.exists(),
            "physical rollout is staged out of discovery"
        );
        assert!(
            !operation.is_finished(),
            "auxiliary lock holds the real delete operation"
        );
        assert_eq!(
            codex_rollout::find_thread_name_by_id(home.path(), &thread_id)
                .await
                .unwrap()
                .as_deref(),
            Some("rolled-back child")
        );
        operation.abort();
        assert!(
            operation
                .await
                .expect_err("caller cancelled")
                .is_cancelled()
        );
        assert!(
            !path.exists(),
            "cancelled waiter cannot restore a rollout whose primary row was deleted"
        );
        sqlx::query("ROLLBACK")
            .execute(&mut logs_lock)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let name_gone = codex_rollout::find_thread_name_by_id(home.path(), &thread_id)
                    .await
                    .unwrap()
                    .is_none();
                let staging_gone = std::fs::read_dir(home.path()).unwrap().all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("thread-delete-")
                });
                if name_gone && staging_gone {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned rollback finishes compatibility cleanup and releases staged files");
        assert!(!path.exists());
        assert!(state.get_thread(thread_id).await.unwrap().is_none());
        assert!(matches!(
            store
                .read_thread(crate::ReadThreadParams {
                    thread_id,
                    include_archived: true,
                    include_history: true,
                })
                .await,
            Err(ThreadStoreError::ThreadNotFound { .. })
        ));
        logs_lock.close().await.unwrap();
        state.close().await;
    }
}
