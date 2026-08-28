use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::read_thread_item_from_rollout;
use codex_rollout::rollout_date_parts;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::rollout_lookup_error;
use super::helpers::scoped_rollout_path;
use super::helpers::stored_thread_from_rollout_item;
use super::helpers::touch_modified_time;
use crate::ArchiveThreadParams;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn unarchive_thread(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<StoredThread> {
    unarchive_thread_with_touch(store, params, touch_modified_time).await
}

async fn unarchive_thread_with_touch<F>(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
    touch: F,
) -> ThreadStoreResult<StoredThread>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<()>,
{
    let thread_id = params.thread_id;
    let state_db_ctx = store.state_db().await;
    let archived_path = find_archived_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        &thread_id.to_string(),
        state_db_ctx.as_deref(),
    )
    .await
    .map_err(|err| rollout_lookup_error(thread_id, /*archived*/ true, err))?
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;

    let canonical_archived_path = scoped_rollout_path(
        store
            .config
            .codex_home
            .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
        archived_path.as_path(),
        "archived",
    )?;
    let file_name = matching_rollout_file_name(
        canonical_archived_path.as_path(),
        thread_id,
        archived_path.as_path(),
    )?;
    let Some((year, month, day)) = rollout_date_parts(&file_name) else {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout path `{}` missing filename timestamp",
                archived_path.display()
            ),
        });
    };

    let dest_dir = store
        .config
        .codex_home
        .join(codex_rollout::SESSIONS_SUBDIR)
        .join(year)
        .join(month)
        .join(day);
    let restored_path = dest_dir.join(&file_name);

    let item = read_thread_item_from_rollout(canonical_archived_path.clone())
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!(
                "failed to read archived thread {}",
                canonical_archived_path.display()
            ),
        })?;
    let mut thread = stored_thread_from_rollout_item(
        item,
        /*archived*/ false,
        store.config.default_model_provider_id.as_str(),
    )
    .ok_or_else(|| ThreadStoreError::Internal {
        message: format!(
            "failed to read archived thread id from {}",
            canonical_archived_path.display()
        ),
    })?;
    thread.rollout_path = Some(codex_rollout::plain_rollout_path(restored_path.as_path()));

    std::fs::create_dir_all(&dest_dir).map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to unarchive thread: {err}"),
    })?;
    std::fs::rename(&canonical_archived_path, &restored_path).map_err(|err| {
        ThreadStoreError::Internal {
            message: format!("failed to unarchive thread: {err}"),
        }
    })?;

    if let Some(ctx) = state_db_ctx
        && let Err(err) = ctx
            .mark_unarchived(thread_id, restored_path.as_path())
            .await
    {
        tracing::warn!(
            "failed to update unarchived thread metadata after moving the rollout; \
             filesystem state remains authoritative until reconciliation: {err}"
        );
    }

    if let Err(err) = touch(restored_path.as_path()) {
        tracing::warn!(
            "failed to update unarchived thread timestamp after moving the rollout; \
             the unarchive remains committed: {err}"
        );
    }

    Ok(thread)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionSource;
    use pretty_assertions::assert_eq;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::ThreadStore;
    use crate::local::LocalThreadStore;
    use crate::local::test_support::test_config;
    use crate::local::test_support::write_archived_session_file;

    #[tokio::test]
    async fn unarchive_thread_restores_rollout_and_returns_updated_thread() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(203);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let archived_path = write_archived_session_file(home.path(), "2025-01-03T13-00-00", uuid)
            .expect("archived session file");

        let thread = store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect("unarchive thread");

        assert!(!archived_path.exists());
        let restored_path = home
            .path()
            .join("sessions/2025/01/03")
            .join(archived_path.file_name().expect("file name"));
        assert!(restored_path.exists());
        assert_eq!(thread.thread_id, thread_id);
        assert_eq!(thread.rollout_path, Some(restored_path));
        assert_eq!(thread.archived_at, None);
        assert_eq!(thread.preview, "Archived user message");
        assert_eq!(
            thread.first_user_message.as_deref(),
            Some("Archived user message")
        );
    }

    #[tokio::test]
    async fn unarchive_thread_does_not_report_failure_after_timestamp_touch_fails() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(207);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let archived_path = write_archived_session_file(home.path(), "2025-01-03T15-00-00", uuid)
            .expect("archived session file");

        let thread =
            unarchive_thread_with_touch(&store, ArchiveThreadParams { thread_id }, |_path| {
                Err(std::io::Error::other("forced timestamp failure"))
            })
            .await
            .expect("rollout move is the unarchive commit point");

        let restored_path = home
            .path()
            .join("sessions/2025/01/03")
            .join(archived_path.file_name().expect("file name"));
        assert!(!archived_path.exists());
        assert!(restored_path.exists());
        assert_eq!(thread.rollout_path, Some(restored_path));
    }

    #[tokio::test]
    async fn unarchive_thread_reports_typed_not_found() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let thread_id = ThreadId::new();

        let error = store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect_err("missing archived thread should fail");

        assert!(matches!(
            error,
            ThreadStoreError::ThreadNotFound {
                thread_id: missing_thread_id
            } if missing_thread_id == thread_id
        ));
    }

    #[tokio::test]
    async fn unarchive_thread_keeps_archive_when_rollout_cannot_be_read() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let uuid = Uuid::from_u128(205);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let archived_path = write_archived_session_file(home.path(), "2025-01-03T14-00-00", uuid)
            .expect("archived session file");
        let valid_rollout = std::fs::read(&archived_path).expect("read archived session");
        std::fs::write(&archived_path, b"not valid rollout json\n")
            .expect("corrupt archived session");
        let old_modified_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        {
            let archive = std::fs::OpenOptions::new()
                .write(true)
                .open(&archived_path)
                .expect("open archived session");
            archive
                .set_times(std::fs::FileTimes::new().set_modified(old_modified_time))
                .expect("set archived session mtime");
        }
        let modified_time_before = std::fs::metadata(&archived_path)
            .expect("archived session metadata")
            .modified()
            .expect("archived session mtime");
        let restored_path = home
            .path()
            .join("sessions/2025/01/03")
            .join(archived_path.file_name().expect("file name"));

        store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect_err("invalid rollout should fail");

        assert!(archived_path.exists());
        assert!(!restored_path.exists());
        assert_eq!(
            std::fs::metadata(&archived_path)
                .expect("archived session metadata")
                .modified()
                .expect("archived session mtime"),
            modified_time_before
        );

        std::fs::write(&archived_path, valid_rollout).expect("restore archived session");
        store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect("retry unarchive thread");

        assert!(!archived_path.exists());
        assert!(restored_path.exists());
    }

    #[tokio::test]
    async fn unarchive_thread_updates_sqlite_metadata_when_present() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let uuid = Uuid::from_u128(204);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let archived_path = write_archived_session_file(home.path(), "2025-01-03T13-00-00", uuid)
            .expect("archived session file");
        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        let mut builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            archived_path.clone(),
            Utc::now(),
            SessionSource::Cli,
        );
        builder.model_provider = Some(config.default_model_provider_id.clone());
        builder.cwd = home.path().to_path_buf();
        builder.cli_version = Some("test_version".to_string());
        let mut metadata = builder.build(config.default_model_provider_id.as_str());
        metadata.archived_at = Some(metadata.updated_at);
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("state db upsert should succeed");

        store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect("unarchive thread");

        let restored_path = home
            .path()
            .join("sessions/2025/01/03")
            .join(archived_path.file_name().expect("file name"));
        let updated = runtime
            .get_thread(thread_id)
            .await
            .expect("state db read should succeed")
            .expect("thread metadata should exist");
        assert_eq!(updated.rollout_path, restored_path);
        assert_eq!(updated.archived_at, None);
        assert_eq!(updated.recency_at, metadata.recency_at);
    }

    #[tokio::test]
    async fn unarchive_thread_keeps_rollout_active_when_sqlite_metadata_update_fails() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let uuid = Uuid::from_u128(206);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let archived_path = write_archived_session_file(home.path(), "2025-01-03T14-30-00", uuid)
            .expect("archived session file");
        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
        let mut builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            archived_path.clone(),
            Utc::now(),
            SessionSource::Cli,
        );
        builder.model_provider = Some(config.default_model_provider_id.clone());
        builder.cwd = home.path().to_path_buf();
        let mut metadata = builder.build(config.default_model_provider_id.as_str());
        metadata.archived_at = Some(metadata.updated_at);
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("state db upsert should succeed");
        install_archival_update_failure(&runtime).await;

        let thread = store
            .unarchive_thread(ArchiveThreadParams { thread_id })
            .await
            .expect("rollout move should remain successful");

        let restored_path = home
            .path()
            .join("sessions/2025/01/03")
            .join(archived_path.file_name().expect("file name"));
        assert!(!archived_path.exists());
        assert!(restored_path.exists());
        assert_eq!(thread.rollout_path, Some(restored_path));
        let unchanged = runtime
            .get_thread(thread_id)
            .await
            .expect("state db read should succeed")
            .expect("thread metadata should exist");
        assert_eq!(unchanged.rollout_path, archived_path);
        assert!(unchanged.archived_at.is_some());
    }

    async fn install_archival_update_failure(runtime: &codex_state::StateRuntime) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(runtime.codex_home().join(codex_state::STATE_DB_FILENAME))
                    .create_if_missing(false),
            )
            .await
            .expect("open state db");
        sqlx::query(
            r#"
CREATE TRIGGER fail_archival_update
BEFORE UPDATE ON threads
BEGIN
    SELECT RAISE(FAIL, 'forced archival update failure');
END
            "#,
        )
        .execute(&pool)
        .await
        .expect("install archival update failure");
        pool.close().await;
    }
}
