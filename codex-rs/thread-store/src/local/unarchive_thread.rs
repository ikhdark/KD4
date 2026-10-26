use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::rollout_date_parts;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::rollout_lookup_error;
use super::helpers::scoped_rollout_path_async;
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
    F: FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
{
    let store = store.clone();
    tokio::spawn(async move { unarchive_thread_owned(&store, params, touch).await })
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?
}

async fn unarchive_thread_owned<F>(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
    touch: F,
) -> ThreadStoreResult<StoredThread>
where
    F: FnOnce(&std::path::Path) -> std::io::Result<()> + Send + 'static,
{
    let thread_id = params.thread_id;
    let _lease = store.acquire_writer_lease(thread_id, false).await?;
    let state_db_ctx = store.state_db().await;
    let archived_path = find_archived_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        &thread_id.to_string(),
        state_db_ctx.as_deref(),
    )
    .await
    .map_err(|err| rollout_lookup_error(thread_id, /*archived*/ true, err))?
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;

    let canonical_archived_path = scoped_rollout_path_async(
        store
            .config
            .codex_home
            .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
        archived_path.clone(),
        "archived",
    )
    .await?;
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

    let mut thread = super::read_thread::read_thread_by_rollout_path(
        store,
        canonical_archived_path.clone(),
        true,
        false,
    )
    .await?;
    if thread.thread_id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: "archived rollout belongs to a different thread".to_string(),
        });
    }
    let restored_path =
        codex_rollout::move_rollout_to_directory(&canonical_archived_path, &dest_dir)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to unarchive thread: {err}"),
            })?;
    thread.rollout_path = Some(codex_rollout::plain_rollout_path(restored_path.as_path()));
    thread.archived_at = None;

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

    let touch_result = tokio::task::spawn_blocking(move || {
        touch(restored_path.as_path())?;
        std::fs::metadata(&restored_path)?.modified()
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(std::convert::identity);
    if let Ok(modified) = touch_result.as_ref() {
        thread.updated_at = (*modified).into();
    }
    if let Err(err) = touch_result {
        tracing::warn!(
            "failed to update unarchived thread timestamp after moving the rollout; \
             the unarchive remains committed: {err}"
        );
    }

    Ok(thread)
}


