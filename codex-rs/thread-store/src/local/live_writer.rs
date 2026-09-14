use std::path::PathBuf;
use std::sync::Arc;

use codex_git_utils::RepositoryContext;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use tracing::warn;

use super::LocalThreadStore;
use super::create_thread;
use crate::CreateThreadParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::error::reject_paginated_history_mode;
use crate::types::canonical_history_mode_from_rollout_items;

const ROLLOUT_SIZE_BYTES_METRIC: &str = "codex.rollout.size_bytes";

pub(super) async fn create_thread(
    store: &LocalThreadStore,
    params: CreateThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let history_mode = params.history_mode;
    store.ensure_live_recorder_absent(thread_id).await?;
    let recorder = create_thread::create_thread(store, params).await?;
    store
        .insert_live_recorder(thread_id, recorder, history_mode)
        .await
}

pub(super) async fn create_thread_with_repository_context(
    store: &LocalThreadStore,
    params: CreateThreadParams,
    repository_context: Option<RepositoryContext>,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let history_mode = params.history_mode;
    store.ensure_live_recorder_absent(thread_id).await?;
    let recorder =
        create_thread::create_thread_with_repository_context(store, params, repository_context)
            .await?;
    store
        .insert_live_recorder(thread_id, recorder, history_mode)
        .await
}

pub(super) async fn resume_thread(
    store: &LocalThreadStore,
    params: ResumeThreadParams,
) -> ThreadStoreResult<Arc<Vec<RolloutItem>>> {
    store.ensure_live_recorder_absent(params.thread_id).await?;
    let cwd = params
        .metadata
        .cwd
        .clone()
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: "local thread store requires a cwd".to_string(),
        })?;
    let thread = if let Some(rollout_path) = params.rollout_path {
        super::read_thread::read_thread_by_rollout_path(
            store,
            rollout_path,
            params.include_archived,
            params.history.is_none(),
        )
        .await?
    } else {
        super::read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: params.history.is_none(),
            },
        )
        .await?
    };
    if thread.thread_id != params.thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: "resume rollout belongs to a different thread".to_string(),
        });
    }
    let history_mode = thread.history_mode;
    reject_paginated_history_mode(history_mode)?;
    let history = match params.history {
        Some(history) => history,
        None => Arc::new(
            thread
                .history
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: format!("failed to load history for thread {}", params.thread_id),
                })?
                .items,
        ),
    };
    reject_paginated_history_mode(canonical_history_mode_from_rollout_items(&history))?;
    if let Some(RolloutItem::SessionMeta(meta)) = history
        .iter()
        .find(|item| matches!(item, RolloutItem::SessionMeta(_)))
        && meta.meta.id != params.thread_id
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "resume history belongs to a different thread".to_string(),
        });
    }
    let rollout_path = thread
        .rollout_path
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!("thread {} does not have a rollout path", params.thread_id),
        })?;
    let config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite_home: store.config.sqlite_home.clone(),
        cwd,
        model_provider_id: params.metadata.model_provider.clone(),
        generate_memories: matches!(params.metadata.memory_mode, ThreadMemoryMode::Enabled),
    };
    let recorder = RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to resume local thread recorder: {err}"),
        })?;
    store
        .insert_live_recorder(params.thread_id, recorder, history_mode)
        .await?;
    Ok(history)
}

pub(super) async fn append_persisted_items(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    append_persisted_items_with_flush(store, thread_id, items, true).await
}

pub(super) async fn append_persisted_items_ordered(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    append_persisted_items_with_flush(store, thread_id, items, false).await
}

async fn append_persisted_items_with_flush(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    persisted_items: &[RolloutItem],
    flush: bool,
) -> ThreadStoreResult<()> {
    if persisted_items.is_empty() {
        return Ok(());
    }
    let recorder = store.live_recorder(thread_id).await?;
    if flush {
        recorder
            .record_canonical_items(persisted_items)
            .await
            .map_err(thread_store_io_error)?;
    } else {
        recorder
            .record_canonical_items_ordered(persisted_items)
            .await
            .map_err(thread_store_io_error)?;
    }
    // LiveThread applies metadata immediately after append_items returns. Wait for the local
    // writer so SQLite never gets ahead of JSONL for accepted live appends.
    if flush {
        recorder.flush().await.map_err(thread_store_io_error)
    } else {
        Ok(())
    }
}

pub(super) async fn persist_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    store
        .live_recorder(thread_id)
        .await?
        .persist()
        .await
        .map_err(thread_store_io_error)?;
    sync_materialized_rollout_path(store, thread_id).await
}

pub(super) async fn flush_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    store
        .live_recorder(thread_id)
        .await?
        .flush()
        .await
        .map_err(thread_store_io_error)?;
    sync_materialized_rollout_path(store, thread_id).await
}

pub(super) async fn shutdown_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let recorder = store.live_recorder(thread_id).await?;
    let rollout_path = recorder.rollout_path().to_path_buf();
    recorder.shutdown().await.map_err(thread_store_io_error)?;
    sync_materialized_rollout_path(store, thread_id).await?;
    if let Some(metrics) = codex_otel::global()
        && let Ok(metadata) = tokio::fs::metadata(rollout_path).await
    {
        let size_bytes = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        let _ = metrics.histogram(ROLLOUT_SIZE_BYTES_METRIC, size_bytes, &[]);
    }
    store.live_recorders.lock().await.remove(&thread_id);
    Ok(())
}

pub(super) async fn discard_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    store
        .live_recorders
        .lock()
        .await
        .remove(&thread_id)
        .map(|_| ())
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })
}

pub(super) async fn rollout_path(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<PathBuf> {
    Ok(store
        .live_recorders
        .lock()
        .await
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?
        .recorder
        .rollout_path()
        .to_path_buf())
}

async fn sync_materialized_rollout_path(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let rollout_path = rollout_path(store, thread_id).await?;
    if codex_rollout::existing_rollout_path(rollout_path.as_path())
        .await
        .is_none()
    {
        return Ok(());
    }
    let Some(state_db) = store.state_db().await else {
        return Ok(());
    };
    let result: ThreadStoreResult<()> = async {
        let Some(mut metadata) =
            state_db
                .get_thread(thread_id)
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to read thread metadata for {thread_id}: {err}"),
                })?
        else {
            return Ok(());
        };
        if metadata.rollout_path != rollout_path {
            metadata.rollout_path = rollout_path;
            state_db
                .upsert_thread(&metadata)
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to update thread metadata for {thread_id}: {err}"),
                })?;
        }
        Ok(())
    }
    .await;
    if let Err(err) = result {
        warn!("failed to sync materialized rollout path for thread {thread_id}: {err}");
    }
    Ok(())
}

fn thread_store_io_error(err: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: err.to_string(),
    }
}
