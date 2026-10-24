use std::path::PathBuf;
use std::sync::Arc;

use codex_git_utils::RepositoryContext;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
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

/// Keep startup custody until the observer actually receives the initialized
/// writer. Dropping an unreceived result cleans up only this append owner.
struct InitializationGuard {
    store: LocalThreadStore,
    thread_id: ThreadId,
    recorder: Option<RolloutRecorder>,
}

impl Drop for InitializationGuard {
    fn drop(&mut self) {
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        let store = self.store.clone();
        let thread_id = self.thread_id;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            warn!(%thread_id, "cannot discard unclaimed recorder without a Tokio runtime");
            return;
        };
        runtime.spawn(async move {
            if let Err(error) = recorder.discard_owned().await {
                warn!(%thread_id, %error, "failed to discard unclaimed recorder initialization");
                return;
            }
            remove_current_recorder(&store, thread_id, &recorder).await;
        });
    }
}

pub(super) async fn initialize<F, Fut>(
    store: LocalThreadStore,
    thread_id: ThreadId,
    start: F,
) -> ThreadStoreResult<()>
where
    F: FnOnce(LocalThreadStore) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ThreadStoreResult<Option<Arc<Vec<RolloutItem>>>>>
        + Send
        + 'static,
{
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = async {
            let history = start(store.clone()).await?;
            let guard = InitializationGuard {
                recorder: Some(store.live_recorder(thread_id).await?),
                store: store.clone(),
                thread_id,
            };
            super::projection::initialize_deferred(&store, thread_id, history).await?;
            Ok::<_, ThreadStoreError>(guard)
        }
        .await;
        // Failed send drops the guard too; a detached successful worker cannot
        // leave a registered writer without an initialization owner.
        let _ = send.send(result);
    });
    let mut guard = receive.await.map_err(|error| ThreadStoreError::Internal {
        message: format!("recorder initialization failed: {error}"),
    })??;
    guard.recorder.take();
    Ok(())
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "Writer and projection maps settle together in live-recorders -> projections order; eviction only try-locks the writer map"
)]
async fn remove_current_recorder(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    recorder: &RolloutRecorder,
) {
    let mut entries = store.live_recorders.lock().await;
    if entries
        .get(&thread_id)
        .is_some_and(|entry| entry.recorder.same_writer(recorder))
    {
        // No replacement can register in this store until both maps settle.
        super::projection::remove(store, thread_id).await;
        entries.remove(&thread_id);
    }
}

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
    resume_thread_owned(store, params).await
}

async fn resume_thread_owned(
    store: &LocalThreadStore,
    params: ResumeThreadParams,
) -> ThreadStoreResult<Arc<Vec<RolloutItem>>> {
    store.ensure_live_recorder_absent(params.thread_id).await?;
    let lease = store.acquire_writer_lease(params.thread_id, false).await?;
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
            false,
        )
        .await?
    } else {
        super::read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: false,
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
    };
    let validate_history = |history: &[RolloutItem]| -> ThreadStoreResult<()> {
        reject_paginated_history_mode(canonical_history_mode_from_rollout_items(history))?;
        if let Some(RolloutItem::SessionMeta(meta)) = history
            .iter()
            .find(|item| matches!(item, RolloutItem::SessionMeta(_)))
            && meta.meta.id != params.thread_id
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "resume history belongs to a different thread".to_string(),
            });
        }
        Ok(())
    };
    let (recorder, history) = match params.history {
        Some(history) => {
            // Reject supplied history before opening or materializing a writer.
            validate_history(&history)?;
            let recorder = RolloutRecorder::new_with_writer_lease(
                &config,
                RolloutRecorderParams::resume(rollout_path),
                None,
                lease,
            )
            .await
            .map_err(thread_store_io_error)?;
            (recorder, history)
        }
        None => {
            let (recorder, history) = RolloutRecorder::resume_and_load_with_writer_lease(
                &config,
                rollout_path,
                params.thread_id,
                Some(lease),
            )
            .await
            .map_err(thread_store_io_error)?;
            validate_history(&history)?;
            (recorder, Arc::new(history))
        }
    };
    repair_resume_discovery_metadata(store, params.thread_id, recorder.rollout_path()).await;
    store
        .insert_live_recorder(params.thread_id, recorder, history_mode)
        .await?;
    Ok(history)
}

/// Canonical history is the restart receipt for deferred index facts. Repair
/// only missing discovery facts on resume; never overwrite current settings,
/// assignments or an explicitly cleared/custom title with an old observation.
async fn repair_resume_discovery_metadata(
    store: &LocalThreadStore,
    id: ThreadId,
    path: &std::path::Path,
) {
    let Some(db) = store.state_db.as_deref() else {
        return;
    };
    let result = async {
        let current = db
            .get_thread(id)
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: error.to_string(),
            })?;
        let Some(current) = current else {
            // Resuming does not unarchive: a missing row for an archived
            // rollout is rebuilt archived, as a metadata update rebuilds it.
            let archived =
                super::helpers::rollout_path_is_archived(store.config.codex_home.as_path(), path);
            if !codex_rollout::state_integration::reconcile_rollout(
                Some(db),
                path,
                &store.config.default_model_provider_id,
                None,
                &[],
                archived.then_some(true),
            )
            .await
            {
                return Err(thread_store_io_error(std::io::Error::other(
                    "resume metadata reconciliation remains deferred",
                )));
            }
            return Ok::<_, ThreadStoreError>(());
        };
        if current.first_user_message.is_some()
            && current
                .preview
                .as_ref()
                .is_some_and(|text| !text.is_empty())
        {
            return Ok(());
        }
        // A caller's cached history is not authority for a missing fact.
        let (history, actual, _) = RolloutRecorder::load_rollout_items(path)
            .await
            .map_err(thread_store_io_error)?;
        if actual != Some(id) {
            return Err(thread_store_io_error(std::io::Error::other(
                "resume metadata identity mismatch",
            )));
        }
        let Some(builder) = codex_rollout::builder_from_items(&history, path) else {
            return Ok(());
        };
        let mut facts = builder.build(&store.config.default_model_provider_id);
        codex_state::apply_rollout_items(
            &mut facts,
            &history,
            &store.config.default_model_provider_id,
        );
        db.patch_existing_thread_metadata(id, |row| {
            if row.first_user_message.is_none() {
                row.first_user_message = facts.first_user_message;
            }
            if row
                .preview
                .as_ref()
                .is_none_or(std::string::String::is_empty)
            {
                row.preview = facts.preview;
            }
        })
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        warn!(%id, %error, "resume discovery repair deferred");
    }
}

pub(super) async fn append_persisted_items_ordered(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    items: &[RolloutItem],
) -> ThreadStoreResult<()> {
    if items.is_empty() {
        return Ok(());
    }
    let recorder = store.live_recorder(thread_id).await?;
    recorder
        .record_canonical_items_ordered(items)
        .await
        .map_err(thread_store_io_error)
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
    durable: bool,
) -> ThreadStoreResult<()> {
    let recorder = store.live_recorder(thread_id).await?;
    if durable {
        recorder.flush_durable().await
    } else {
        recorder.flush().await
    }
    .map_err(thread_store_io_error)?;
    sync_materialized_rollout_path(store, thread_id).await
}

pub(super) async fn shutdown_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let store = store.clone();
    tokio::spawn(async move { shutdown_thread_owned(&store, thread_id).await })
        .await
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?
}

async fn shutdown_thread_owned(
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
    remove_current_recorder(store, thread_id, &recorder).await;
    Ok(())
}

pub(super) async fn discard_thread(
    store: &LocalThreadStore,
    thread_id: ThreadId,
) -> ThreadStoreResult<()> {
    let store = store.clone();
    let completion = tokio::spawn(async move {
        let recorder = store.live_recorder(thread_id).await?;
        recorder
            .discard_owned()
            .await
            .map_err(thread_store_io_error)?;
        remove_current_recorder(&store, thread_id, &recorder).await;
        Ok(())
    });
    // Timing out only the observer keeps eventual terminal/map settlement owned.
    tokio::time::timeout(std::time::Duration::from_secs(30), completion)
        .await
        .map_err(|_| ThreadStoreError::Internal {
            message: "timed out waiting for rollout discard settlement".into(),
        })?
        .map_err(|error| ThreadStoreError::Internal {
            message: error.to_string(),
        })?
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
        if state_db.thread_rollout_path_matches(thread_id, &rollout_path).await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to update thread metadata for {thread_id}: {err}"),
            })?
        {
            return Ok(());
        }
        state_db
            .patch_existing_thread_metadata(thread_id, |current| {
                if !rollout_path.exists() && !rollout_path.with_extension("jsonl.zst").exists() {
                    return;
                }
                current.rollout_path = rollout_path;
            })
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to update thread metadata for {thread_id}: {err}"),
            })?;
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
