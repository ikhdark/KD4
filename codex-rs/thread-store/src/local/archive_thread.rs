use chrono::Utc;
use codex_rollout::find_thread_path_by_id_str;

use super::LocalThreadStore;
use super::helpers::matching_rollout_file_name;
use super::helpers::rollout_lookup_error;
use super::helpers::scoped_rollout_path_async;
use crate::ArchiveThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn archive_thread(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<()> {
    let store = store.clone();
    // The relocation and its index settlement retain the logical lease even
    // when the caller stops observing the blocking filesystem operation.
    tokio::spawn(async move { archive_thread_owned(&store, params).await })
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?
}

async fn archive_thread_owned(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    if super::live_writer::rollout_path(store, thread_id)
        .await
        .is_ok()
    {
        super::live_writer::shutdown_thread(store, thread_id).await?;
    }
    let _lease = store.acquire_writer_lease(thread_id, false).await?;
    let state_db_ctx = store.state_db().await;
    let moves = codex_rollout::lock_rollout_moves(&store.config.codex_home)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?;
    let copies = codex_rollout::find_thread_rollout_paths(&store.config.codex_home, thread_id)
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?;
    let logical_copies: std::collections::HashSet<_> = copies
        .iter()
        .map(|path| codex_rollout::plain_rollout_path(path))
        .collect();
    if logical_copies.len() > 1 {
        return Err(ThreadStoreError::Conflict {
            message: format!(
                "thread {thread_id} has multiple canonical rollouts; archive left every copy unchanged"
            ),
        });
    }
    let rollout_path = find_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        &thread_id.to_string(),
        state_db_ctx.as_deref(),
    )
    .await
    .map_err(|err| rollout_lookup_error(thread_id, /*archived*/ false, err))?
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;

    let canonical_rollout_path = scoped_rollout_path_async(
        store.config.codex_home.join(codex_rollout::SESSIONS_SUBDIR),
        rollout_path.clone(),
        "sessions",
    )
    .await?;
    matching_rollout_file_name(
        canonical_rollout_path.as_path(),
        thread_id,
        rollout_path.as_path(),
    )?;

    let archive_folder = store
        .config
        .codex_home
        .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR);
    let archived_path = codex_rollout::move_rollout_to_directory_with_owner(
        &canonical_rollout_path,
        &archive_folder,
        Some(moves),
    )
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to archive thread: {err}"),
    })?;

    if let Some(ctx) = state_db_ctx
        && let Err(err) = ctx
            .mark_archived(thread_id, archived_path.as_path(), Utc::now())
            .await
    {
        tracing::warn!(
            "failed to update archived thread metadata after moving the rollout; \
             filesystem state remains authoritative until reconciliation: {err}"
        );
    }
    Ok(())
}

