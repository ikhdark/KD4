use std::path::Path;
use std::path::PathBuf;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::DurableRolloutAppendOutcome;
use codex_rollout::RolloutAppendCheckpoint;
use codex_rollout::StateDbHandle;
use codex_rollout::append_checkpointed_session_meta;
use codex_rollout::append_thread_name;
use codex_rollout::read_session_meta_line;
use codex_rollout::replay_checkpointed_session_meta;
use codex_state::ExplicitThreadGitFields;
use codex_state::THREAD_GIT_COMPATIBILITY_RECORD_VERSION;
use codex_state::ThreadGitCompatibilityIntent;
use codex_state::ThreadGitCompatibilityPrepare;
use codex_state::ThreadGitCompatibilityRecord;
use codex_state::ThreadMetadata;
use codex_state::ThreadMetadataBuilder;
use tracing::warn;

use super::LocalThreadStore;
use super::helpers::ResolvedRolloutPath;
use super::helpers::permission_profile_to_metadata_value;
use super::helpers::resolve_git_compatibility_rollout_path;
use super::helpers::resolve_rollout_path;
use super::helpers::rollout_path_is_archived;
use super::live_writer;
use crate::ReadThreadParams;
use crate::StoredThread;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;
use crate::local::read_thread;
use crate::types::ThreadGitInfoOrigin;


pub(super) async fn update_thread_metadata_ack(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<()> {
    update_thread_metadata_applied(store, params).await.map(|_| ())
}

pub(super) async fn update_thread_metadata_applied(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<bool> {
    if params.patch.is_empty()
        || needs_rollout_compatibility_update(&params.patch)
        || (params.patch.project_id.is_some() && !store.supports_projects())
    {
        return update_thread_metadata(store, params).await.map(|_| true);
    }

    let require_sqlite_write = params.patch.project_id.is_some();
    let updated = apply_metadata_update(
        store,
        params.thread_id,
        params.patch,
        params.include_archived,
        require_sqlite_write,
    )
    .await?;
    if updated.is_some() {
        // The commit is complete. Live callers discard the summary, so do not reread the
        // rollout or legacy name index solely to construct it while holding their mutex.
        return Ok(true);
    }

    // No SQLite row was committed (including best-effort failures). Preserve the returning
    // API's canonical fallback and its errors rather than acknowledging an unchecked update.
    read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id: params.thread_id,
            include_archived: params.include_archived,
            include_history: false,
        },
    )
    .await
    .map(|_| store.state_db.is_none())
}

pub(super) async fn update_thread_metadata(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<StoredThread> {
    if params.patch.is_empty() {
        return read_metadata_summary(store, params.thread_id, params.include_archived).await;
    }
    let thread_id = params.thread_id;
    run_owned_metadata_update(store, thread_id, move |store| async move {
        update_thread_metadata_owned(&store, params).await
    })
    .await
}

pub(super) async fn commit_imported_session_metadata(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
    content_sha256: &str,
) -> ThreadStoreResult<()> {
    // This path is for the import patch, not updates with an additional
    // compatibility publication or recency side effect.
    if params.patch.is_empty()
        || needs_rollout_compatibility_update(&params.patch)
        || params.patch.advance_recency_at.is_some()
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "invalid imported session metadata patch".into(),
        });
    }
    let thread_id = params.thread_id;
    let content_sha256 = content_sha256.to_owned();
    run_owned_metadata_update(store, thread_id, move |store| async move {
        let _lease = store.acquire_writer_lease(thread_id, true).await?;
        apply_metadata_update_with_compatibility(
            &store,
            thread_id,
            params.patch,
            params.include_archived,
            /*require_sqlite_write*/ true,
            None,
            Some(&content_sha256),
        )
        .await?
        .ok_or(ThreadStoreError::Unsupported {
            operation: "durable_session_import",
        })?;
        Ok(())
    })
    .await
}

async fn run_owned_metadata_update<T, F>(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    update: impl FnOnce(LocalThreadStore) -> F + Send + 'static,
) -> ThreadStoreResult<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = ThreadStoreResult<T>> + Send + 'static,
{
    let permit = std::sync::Arc::clone(&store.metadata_update_slots)
        .try_acquire_owned()
        .map_err(|_| ThreadStoreError::Conflict {
            message: "metadata update capacity reached; retry after pending updates settle".into(),
        })?;
    let store = store.clone();
    // The logical owner retains the order and writer lease through SQLite,
    // the exact rollout operation and finalizers, even if the observer drops.
    tokio::spawn(async move {
        let _permit = permit;
        let result = tokio::spawn(async move {
            let _order = store.lock_rollout_publication(thread_id).await;
            update(store).await
        })
        .await
        .unwrap_or_else(|error| {
            Err(ThreadStoreError::Internal {
                message: format!(
                    "metadata update owner failed for {thread_id}: {error}; reconciliation required"
                ),
            })
        });
        if let Err(error) = &result {
            warn!(%thread_id, %error, "metadata update did not settle successfully");
        }
        result
    })
    .await
    .map_err(|error| ThreadStoreError::Internal {
        message: format!("metadata update owner failed for {thread_id}: {error}"),
    })?
}

async fn update_thread_metadata_owned(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<StoredThread> {
    let thread_id = params.thread_id;
    let patch = params.patch;
    if patch.project_id.is_some() && !store.supports_projects() {
        return Err(ThreadStoreError::Unsupported {
            operation: "projects",
        });
    }
    let needs_rollout_compat = needs_rollout_compatibility_update(&patch);
    let _lease = store.acquire_writer_lease(thread_id, true).await?;
    if needs_rollout_compat {
        // Reject unsupported history before persistence, the metadata patch,
        // or any compatibility append.
        let thread = read_metadata_summary(store, thread_id, params.include_archived).await?;
        reject_paginated_history_mode(thread.history_mode)?;
    }
    let require_sqlite_write = needs_rollout_compat || patch.project_id.is_some();
    if !needs_rollout_compat {
        let updated = apply_metadata_update(
            store,
            thread_id,
            patch,
            params.include_archived,
            require_sqlite_write,
        )
        .await?;
        if let Some(metadata) = updated {
            return read_thread::stored_thread_from_sqlite_metadata(store, metadata).await;
        }
        return read_metadata_summary(store, thread_id, params.include_archived).await;
    }

    let state_db_ctx = store.state_db().await;
    let live_rollout_path = live_writer::rollout_path(store, thread_id).await.ok();
    let explicit_git = explicit_git_fields(&patch);
    if live_rollout_path.is_some() {
        // A prior intent already fenced/drained the live queue. Do not flush
        // across its reserved position while preparing a later metadata update.
        live_writer::persist_thread_for_metadata(store, thread_id).await?;
    }

    let existing = match state_db_ctx.as_ref() {
        Some(state_db) => state_db
            .get_thread(thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to read thread metadata for {thread_id}: {err}"),
            })?,
        None => None,
    };
    let resolved_rollout_path = if explicit_git.is_some() && let Some(metadata) = existing.as_ref() {
        ResolvedRolloutPath {
            path: metadata.rollout_path.clone(),
            archived: metadata.archived_at.is_some(),
        }
    } else {
        if explicit_git.is_some() && state_db_ctx.is_some() && live_rollout_path.is_none() {
            let copies = codex_rollout::find_thread_rollout_paths(&store.config.codex_home, thread_id)
                .await
                .map_err(git_preflight_error)?;
            let logical_paths: std::collections::HashSet<_> = copies
                .iter()
                .map(|path| codex_rollout::plain_rollout_path(path))
                .collect();
            if logical_paths.len() > 1 {
                return Err(ThreadStoreError::Conflict {
                    message: format!("thread {thread_id} has multiple rollout copies"),
                });
            }
        }
        resolve_rollout_path(
            store,
            thread_id,
            params.include_archived,
            /*require_materialized*/ false,
        )
        .await?
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?
    };

    // Establish only a missing baseline. Never reload older rollout facts
    // over an existing explicit SQLite patch.
    if existing.is_none() && let Some(state_db) = state_db_ctx.as_ref() {
        codex_rollout::state_integration::reconcile_rollout(
            Some(state_db.as_ref()),
            &resolved_rollout_path.path,
            store.config.default_model_provider_id.as_str(),
            None,
            &[],
            resolved_rollout_path.archived.then_some(true),
        )
        .await;
    }

    let name = patch.name.clone();
    let updated = if let Some(explicit_fields) = explicit_git {
        let Some(state_db) = state_db_ctx.as_ref() else {
            // Explicit partial Git updates have always required SQLite's
            // authoritative base. Name-only updates work without it.
            return Err(ThreadStoreError::Internal {
                message: format!("sqlite state db unavailable for thread {thread_id}"),
            });
        };
        // Only a changed explicit field or newly established clear needs a new record.
        // Preserve an unchanged
        // request's ability to drain a prior exact head when preflight fails.
        let preflight = async {
            let rollout_path = resolve_git_compatibility_rollout_path(
                &store.config.codex_home,
                &resolved_rollout_path.path,
                &resolved_rollout_path.path,
                live_rollout_path.as_deref(),
            )
            .await?;
            let session_meta = read_session_meta_line(&rollout_path).await?;
            Ok::<_, std::io::Error>(GitCompatibilityPreflight { session_meta, rollout_path })
        }
        .await;
        // Arm the fence before the transaction: an uncertain DB receipt must
        // not let an ordinary append run ahead of a possible committed intent.
        store.mark_git_compatibility_check(thread_id, true).await;
        let applied = apply_metadata_update_with_compatibility(
            store,
            thread_id,
            patch,
            params.include_archived,
            require_sqlite_write,
            Some((explicit_fields, preflight)),
            None,
        )
        .await?
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!("no state DB result for explicit Git update: {thread_id}"),
        })?;
        let pending = if let Some(intent_id) = applied.git_intent_id {
            match drain_git_compatibility(
                store, state_db, thread_id, live_rollout_path.as_deref(), Some(intent_id),
            )
            .await
            {
                Ok(()) => {
                    store.mark_git_compatibility_check(thread_id, false).await;
                    None
                }
                Err(failure) => {
                    warn!(%thread_id, ?failure, "Git compatibility publication remains pending");
                    // A failed acknowledgement does not undo an exact append.
                    // A blocked predecessor has not attempted our target record.
                    let target_failure = failure.sequence == Some(intent_id);
                    (!(target_failure && failure.confirmed)).then_some(
                        ThreadStoreError::GitCompatibilityPending {
                            thread_id,
                            intent_id,
                            outcome_unknown: target_failure && failure.unknown,
                        },
                    )
                }
            }
        } else {
            store.mark_git_compatibility_check(thread_id, false).await;
            None
        };
        // Keep lineage failure independent. If both fail, the committed-patch
        // signal must take precedence so callers do not repeat the mutation.
        if let Some(lineage_error) = applied.lineage_error {
            if let Some(pending) = pending {
                warn!(%thread_id, %lineage_error, "committed Git patch also failed lineage finalization");
                return Err(pending);
            }
            return Err(lineage_error);
        }
        if let Some(name) = name {
            let name_result = apply_thread_name(
                store, thread_id, name.unwrap_or_default(), applied.metadata.title_is_explicit,
            )
            .await;
            if let Err(error) = name_result {
                if let Some(pending) = pending {
                    warn!(%thread_id, %error, "committed Git patch also failed name publication");
                    return Err(pending);
                }
                return Err(error);
            }
        }
        if let Some(pending) = pending {
            return Err(pending);
        }
        Some(applied.metadata)
    } else {
        let updated = apply_metadata_update(
            store,
            thread_id,
            patch,
            params.include_archived,
            require_sqlite_write,
        )
        .await?;
        if let Some(name) = name {
            apply_thread_name(
                store,
                thread_id,
                name.unwrap_or_default(),
                updated.as_ref().is_some_and(|metadata| metadata.title_is_explicit),
            )
            .await?;
        }
        updated
    };

    // Prefer the committed row; compatibility reads must not reapply older
    // rollout snapshots after the explicit patch.
    if let Some(metadata) = updated {
        return read_thread::stored_thread_from_sqlite_metadata(store, metadata).await;
    }
    match read_metadata_summary(store, thread_id, params.include_archived).await {
        Ok(thread) => Ok(thread),
        Err(_) => read_thread::read_thread_by_rollout_path(
            store,
            resolved_rollout_path.path,
            params.include_archived,
            /*include_history*/ false,
        )
        .await,
    }
}

async fn read_metadata_summary(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<StoredThread> {
    read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        },
    )
    .await
}

struct GitCompatibilityPreflight {
    session_meta: SessionMetaLine,
    rollout_path: PathBuf,
}

struct AppliedMetadata {
    metadata: ThreadMetadata,
    git_intent_id: Option<i64>,
    lineage_error: Option<ThreadStoreError>,
}

fn git_preflight_error(error: std::io::Error) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("failed to set thread git metadata: {error}"),
    }
}

async fn apply_metadata_update(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
    require_sqlite_write: bool,
) -> ThreadStoreResult<Option<ThreadMetadata>> {
    apply_metadata_update_with_compatibility(
        store, thread_id, patch, include_archived, require_sqlite_write, None, None,
    )
    .await
    .map(|updated| updated.map(|applied| applied.metadata))
}

async fn apply_metadata_update_with_compatibility(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
    require_sqlite_write: bool,
    git_compatibility: Option<(ExplicitThreadGitFields, std::io::Result<GitCompatibilityPreflight>)>,
    import_content_sha256: Option<&str>,
) -> ThreadStoreResult<Option<AppliedMetadata>> {
    let live_rollout_path = live_writer::rollout_path(store, thread_id).await.ok();
    let mut rollout_path = patch.rollout_path.clone().or(live_rollout_path);
    let mut rollout_path_archived = rollout_path
        .as_deref()
        .is_some_and(|path| rollout_path_is_archived(store.config.codex_home.as_path(), path));
    let state_db = store.state_db().await;
    let sqlite_write_result = if let Some(state_db) = state_db.as_ref() {
        let patch = patch.clone();
        async {
            let existing =
                state_db
                    .get_thread(thread_id)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!("failed to read thread metadata for {thread_id}: {err}"),
                    })?;
            let advance_recency_at = patch.advance_recency_at;
            if existing.is_none() && rollout_path.is_none() {
                let resolved = resolve_rollout_path(
                    store,
                    thread_id,
                    include_archived,
                    /*require_materialized*/ false,
                )
                .await?
                .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
                rollout_path_archived = resolved.archived;
                rollout_path = Some(resolved.path);
            }
            let mut metadata = match existing.clone() {
                Some(metadata) => metadata,
                None => {
                    let rollout_path =
                        rollout_path
                            .as_deref()
                            .ok_or_else(|| ThreadStoreError::Internal {
                                message: format!(
                                    "thread metadata missing rollout path for {thread_id}"
                                ),
                            })?;
                    metadata_for_missing_sqlite_row(
                        store,
                        thread_id,
                        rollout_path,
                        rollout_path_archived,
                        &patch,
                    )
                    .await?
                }
            };
            read_thread::resolve_sqlite_rollout_location(store, &mut metadata).await?;
            if !include_archived
                && (metadata.archived_at.is_some()
                    || rollout_path_is_archived(&store.config.codex_home, &metadata.rollout_path)
                    || rollout_path_archived)
            {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {thread_id} is archived"),
                });
            }
            let fallback = metadata;
            let name_update = patch.name.is_some();
            let compatibility_required = git_compatibility.is_some();
            let git_origins = patch.git_info_origin;
            let has_existing = existing.is_some();
            let apply_patch = |metadata: &mut ThreadMetadata| {
                if compatibility_required && metadata.history_mode == ThreadHistoryMode::Paginated {
                    return Err("paginated_threads".to_owned());
                }
                if !include_archived && metadata.archived_at.is_some() {
                    return Err(format!("thread {thread_id} is archived"));
                }
                if let Some(rollout_path) = rollout_path {
                    metadata.rollout_path = rollout_path;
                }
                if let Some(preview) = patch.preview {
                    metadata.preview = Some(preview);
                }
                if let Some(name) = patch.name {
                    metadata.title = name.unwrap_or_default();
                    metadata.title_is_explicit = true;
                }
                if let Some(title) = patch.title
                    && !metadata.title_is_explicit
                {
                    metadata.title = title;
                }
                if let Some(model_provider) = patch.model_provider {
                    metadata.model_provider = model_provider;
                }
                if let Some(model) = patch.model {
                    metadata.model = Some(model);
                }
                if let Some(reasoning_effort) = patch.reasoning_effort {
                    metadata.reasoning_effort = reasoning_effort;
                }
                if let Some(created_at) = patch.created_at {
                    metadata.created_at = created_at;
                }
                if let Some(updated_at) = patch.updated_at {
                    metadata.updated_at = updated_at;
                }
                if !has_existing
                    && let Some(recency_at) = advance_recency_at
                {
                    metadata.recency_at = recency_at;
                }
                if let Some(source) = patch.source {
                    metadata.source = enum_to_string(&source);
                }
                if let Some(thread_source) = patch.thread_source {
                    metadata.thread_source = thread_source;
                }
                if let Some(agent_nickname) = patch.agent_nickname {
                    metadata.agent_nickname = agent_nickname;
                }
                if let Some(agent_role) = patch.agent_role {
                    metadata.agent_role = agent_role;
                }
                if let Some(agent_path) = patch.agent_path {
                    metadata.agent_path = agent_path;
                }
                if let Some(cwd) = patch.cwd {
                    metadata.cwd = normalize_cwd(cwd);
                }
                if let Some(cli_version) = patch.cli_version {
                    metadata.cli_version = cli_version;
                }
                if let Some(approval_mode) = patch.approval_mode {
                    metadata.approval_mode = enum_to_string(&approval_mode);
                }
                if let Some(permission_profile) = patch.permission_profile {
                    metadata.sandbox_policy =
                        permission_profile_to_metadata_value(&permission_profile);
                }
                if let Some(token_usage) = patch.token_usage {
                    metadata.tokens_used = token_usage.total_tokens.max(0);
                }
                if let Some(first_user_message) = patch.first_user_message {
                    metadata.first_user_message = Some(first_user_message);
                }
                if let Some(git_info) = patch.git_info {
                    apply_git_field(&mut metadata.git_sha, git_info.sha, git_origins.sha);
                    apply_git_field(&mut metadata.git_branch, git_info.branch, git_origins.branch);
                    apply_git_field(&mut metadata.git_origin_url, git_info.origin_url, git_origins.origin_url);
                }
                if let Some(project_id) = patch.project_id {
                    metadata.project_id = project_id;
                }

                Ok(())
            };
            let recency = advance_recency_at.filter(|_| has_existing);
            let map_sqlite_error = |message: String| {
                if message == "paginated_threads" {
                    ThreadStoreError::Unsupported { operation: "paginated_threads" }
                } else if message.contains("project not found") || message.contains("is archived") {
                    ThreadStoreError::InvalidRequest { message }
                } else if name_update {
                    ThreadStoreError::Internal {
                        message: format!("failed to set thread name: {message}"),
                    }
                } else {
                    ThreadStoreError::Internal { message }
                }
            };
            if let Some((explicit_fields, preflight)) = git_compatibility {
                let committed = state_db
                    .patch_thread_metadata_with_git_compatibility(
                        thread_id,
                        recency,
                        explicit_fields,
                        apply_patch,
                        |before, after| {
                            let GitCompatibilityPreflight { mut session_meta, rollout_path } = preflight?;
                            if session_meta.meta.id != thread_id
                                || before.session_id.is_some_and(|id| id != session_meta.meta.session_id)
                            {
                                return Err(std::io::Error::other("rollout session metadata identity mismatch").into());
                            }
                            if session_meta.meta.history_mode == ThreadHistoryMode::Paginated {
                                return Err(std::io::Error::other("paginated_threads").into());
                            }
                            session_meta.git = Some(GitInfo {
                                commit_hash: after.git_sha.as_deref().map(codex_git_utils::GitSha::new),
                                branch: after.git_branch.clone(),
                                repository_url: after.git_origin_url.clone(),
                            });
                            let session_id = session_meta.meta.session_id;
                            let mut bytes = serde_json::to_vec(&RolloutLine {
                                timestamp: after.updated_at.to_rfc3339(),
                                item: RolloutItem::SessionMeta(session_meta),
                            })?;
                            bytes.push(b'\n');
                            Ok(ThreadGitCompatibilityRecord {
                                rollout_path,
                                session_id,
                                bytes,
                            })
                        },
                    )
                    .await
                    .map_err(|error| map_sqlite_error(error.to_string()))?;
                Ok(Some(AppliedMetadata {
                    metadata: committed.metadata,
                    git_intent_id: committed.git_compatibility_sequence,
                    lineage_error: committed.lineage_result.err().map(|err| map_sqlite_error(err.to_string())),
                }))
            } else {
                let metadata = if let Some(content_sha256) = import_content_sha256 {
                    state_db
                        .patch_imported_thread_metadata(&fallback, content_sha256, apply_patch)
                        .await
                } else {
                    state_db
                        .patch_thread_metadata_with_recency(&fallback, recency, apply_patch)
                        .await
                }
                .map_err(|error| map_sqlite_error(error.to_string()))?;
                Ok(Some(AppliedMetadata {
                    metadata,
                    git_intent_id: None,
                    lineage_error: None,
                }))
            }
        }
        .await
    } else {
        Ok(None)
    };
    let updated_metadata = match sqlite_write_result {
        Ok(metadata) => metadata,
        Err(err) if require_sqlite_write || !sqlite_write_error_is_best_effort(&err) => {
            return Err(err);
        }
        Err(err) => {
            warn!("state db update_thread_metadata failed for {thread_id}: {err}");
            None
        }
    };
    Ok(updated_metadata)
}

async fn metadata_for_missing_sqlite_row(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
    rollout_path_archived: bool,
    patch: &ThreadMetadataPatch,
) -> ThreadStoreResult<codex_state::ThreadMetadata> {
    if let Ok(session_meta) = read_session_meta_line(rollout_path).await {
        if session_meta.meta.id != thread_id {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "rollout session metadata id mismatch: expected {thread_id}, found {}",
                    session_meta.meta.id
                ),
            });
        }
        let history_mode = session_meta.meta.history_mode;
        let item = RolloutItem::SessionMeta(session_meta);
        let mut builder =
            codex_rollout::builder_from_items(std::slice::from_ref(&item), rollout_path)
                .unwrap_or_else(|| {
                    ThreadMetadataBuilder::new(
                        thread_id,
                        rollout_path.to_path_buf(),
                        patch
                            .created_at
                            .or(patch.updated_at)
                            .unwrap_or_else(Utc::now),
                        SessionSource::Unknown,
                    )
                });
        builder.id = thread_id;
        builder.history_mode = history_mode;
        let mut metadata = builder.build(&store.config.default_model_provider_id);
        codex_state::apply_rollout_item(
            &mut metadata,
            &item,
            &store.config.default_model_provider_id,
        );
        metadata.cwd = normalize_cwd(metadata.cwd);
        if rollout_path_archived {
            metadata.archived_at = Some(metadata.updated_at);
        }
        return Ok(metadata);
    }
    let created_at = patch
        .created_at
        .or(patch.updated_at)
        .unwrap_or_else(Utc::now);
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.to_path_buf(),
        created_at,
        patch.source.clone().unwrap_or(SessionSource::Unknown),
    );
    builder.model_provider = patch.model_provider.clone();
    builder.history_mode = canonical_history_mode(store, thread_id, rollout_path).await?;
    builder.thread_source = patch.thread_source.clone().flatten();
    builder.agent_nickname = patch.agent_nickname.clone().flatten();
    builder.agent_role = patch.agent_role.clone().flatten();
    builder.agent_path = patch.agent_path.clone().flatten();
    builder.cwd = patch.cwd.clone().map(normalize_cwd).unwrap_or_default();
    builder.cli_version = patch.cli_version.clone();
    let mut metadata = builder.build(store.config.default_model_provider_id.as_str());
    if rollout_path_archived {
        metadata.archived_at = Some(metadata.updated_at);
    }
    Ok(metadata)
}

async fn canonical_history_mode(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<ThreadHistoryMode> {
    let session_meta = match read_session_meta_line(rollout_path).await {
        Ok(session_meta) => session_meta,
        Err(err) => {
            if codex_rollout::existing_rollout_path(rollout_path)
                .await
                .is_none()
                && let Some(history_mode) = store
                    .live_recorders
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|entry| entry.history_mode)
            {
                // The live writer retains the canonical mode selected before its deferred
                // SessionMeta reaches JSONL.
                return Ok(history_mode);
            }
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to read canonical session metadata for {thread_id}: {err}"
                ),
            });
        }
    };
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "failed to rebuild thread metadata: rollout session metadata id mismatch: expected {thread_id}, found {}",
                session_meta.meta.id
            ),
        });
    }
    Ok(session_meta.meta.history_mode)
}

fn needs_rollout_compatibility_update(patch: &ThreadMetadataPatch) -> bool {
    patch.name.is_some() || explicit_git_fields(patch).is_some()
}

fn explicit_git_fields(patch: &ThreadMetadataPatch) -> Option<ExplicitThreadGitFields> {
    let git = patch.git_info.as_ref()?;
    let origins = patch.git_info_origin;
    let fields = ExplicitThreadGitFields {
        sha: git.sha.is_some() && origins.sha == ThreadGitInfoOrigin::Explicit,
        branch: git.branch.is_some() && origins.branch == ThreadGitInfoOrigin::Explicit,
        origin_url: git.origin_url.is_some() && origins.origin_url == ThreadGitInfoOrigin::Explicit,
    };
    let empty_git_request = git.sha.is_none() && git.branch.is_none() && git.origin_url.is_none();
    (fields.sha || fields.branch || fields.origin_url || empty_git_request).then_some(fields)
}

fn sqlite_write_error_is_best_effort(err: &ThreadStoreError) -> bool {
    matches!(err, ThreadStoreError::Internal { .. })
}

fn enum_to_string<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(value)) => value,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn normalize_cwd(cwd: PathBuf) -> PathBuf {
    codex_utils_absolute_path::normalize_for_path_comparison(cwd.as_path()).unwrap_or(cwd)
}

fn apply_git_field(
    current: &mut Option<String>,
    value: Option<Option<String>>,
    origin: ThreadGitInfoOrigin,
) {
    // Decide against the transaction's row, never a previously read snapshot.
    // State's explicit-clear mask separately fences historical/live refill.
    if let Some(value) = value
        && (origin != ThreadGitInfoOrigin::HistoricalFill || current.is_none())
    {
        *current = value;
    }
}

const GIT_COMPATIBILITY_IDLE_BATCH: usize = 32;
const GIT_COMPATIBILITY_THREAD_BATCH: usize = 8;

pub(super) async fn recover_idle_git_compatibility(
    store: &LocalThreadStore,
    state_db: &StateDbHandle,
) {
    let rows = match state_db
        .list_pending_thread_git_compatibility_intent_ids(0, GIT_COMPATIBILITY_IDLE_BATCH)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            warn!(%error, "could not inspect pending Git compatibility at startup");
            return;
        }
    };
    let mut seen = std::collections::HashSet::new();
    for (_, thread_id) in rows {
        if !seen.insert(thread_id) {
            continue;
        }
        // No live recorder is paused or borrowed by opportunistic startup work.
        // The exact idle writer lease is held until publication/ack settles.
        let Ok(_lease) = store.acquire_writer_lease(thread_id, false).await else {
            continue;
        };
        if let Err(error) = drain_git_compatibility(store, state_db, thread_id, None, None).await {
            warn!(%thread_id, ?error, "startup left Git compatibility pending");
        }
    }
}

/// Under the caller's exact idle/live writer lease and, for an existing live
/// recorder, its local publication order. Never reenter LocalThreadStore::state_db.
pub(super) async fn recover_git_compatibility_before_write(
    store: &LocalThreadStore,
    state_db: &StateDbHandle,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    drain_git_compatibility(store, state_db, thread_id, Some(rollout_path), None)
        .await
        .map_err(|failure| ThreadStoreError::Conflict {
            message: format!(
                "thread {thread_id} cannot append while Git compatibility is pending: {}",
                failure.error
            ),
        })?;
    store.mark_git_compatibility_check(thread_id, false).await;
    Ok(())
}

#[derive(Debug)]
struct GitCompatibilityDrainFailure {
    sequence: Option<i64>,
    unknown: bool,
    confirmed: bool,
    error: std::io::Error,
}

async fn drain_git_compatibility(
    store: &LocalThreadStore,
    state_db: &StateDbHandle,
    thread_id: ThreadId,
    live_path: Option<&Path>,
    through_sequence: Option<i64>,
) -> Result<(), GitCompatibilityDrainFailure> {
    for index in 0..=GIT_COMPATIBILITY_THREAD_BATCH {
        let intent = state_db
            .next_thread_git_compatibility_intent(thread_id)
            .await
            .map_err(|err| GitCompatibilityDrainFailure {
                sequence: None,
                unknown: false,
                confirmed: false,
                error: std::io::Error::other(err),
            })?;
        let Some(intent) = intent else {
            return if through_sequence.is_none() {
                Ok(())
            } else {
                Err(GitCompatibilityDrainFailure {
                    sequence: through_sequence,
                    unknown: true,
                    confirmed: false,
                    error: std::io::Error::other("submitted Git intent disappeared before settlement"),
                })
            };
        };
        let sequence = intent.sequence;
        if index == GIT_COMPATIBILITY_THREAD_BATCH
            || through_sequence.is_some_and(|target| sequence > target)
        {
            return Err(GitCompatibilityDrainFailure {
                sequence: None,
                unknown: false,
                confirmed: false,
                error: std::io::Error::other("Git compatibility drain did not reach the requested boundary"),
            });
        }
        let checkpointed = intent.publication_checkpoint.is_some();
        let fail = |error, unknown, confirmed| GitCompatibilityDrainFailure {
            sequence: Some(sequence),
            unknown,
            confirmed,
            error,
        };
        // One owner fences archive and staged deletion from target selection
        // through exact publication and acknowledgement. No SQLite patch
        // transaction is held while waiting for the physical append owner.
        let _moves = codex_rollout::lock_rollout_moves(&store.config.codex_home)
            .await
            .map_err(|error| fail(error, checkpointed, false))?;
        match publish_git_compatibility(store, state_db, intent, live_path)
            .await
            .map_err(|error| fail(error, checkpointed, false))?
        {
            DurableRolloutAppendOutcome::Appended | DurableRolloutAppendOutcome::AlreadyPresent => {
                match state_db.acknowledge_thread_git_compatibility_intent(thread_id, sequence).await {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(fail(std::io::Error::other("Git intent is not the prepared head"), false, true));
                    }
                    Err(error) => return Err(fail(std::io::Error::other(error), false, true)),
                }
            }
            DurableRolloutAppendOutcome::Retryable(error) | DurableRolloutAppendOutcome::Rejected(error) => {
                return Err(fail(error, false, false));
            }
            DurableRolloutAppendOutcome::Unknown(error) => return Err(fail(error, true, false)),
        }
        if through_sequence == Some(sequence) {
            return Ok(());
        }
    }
    unreachable!("bounded drain returns on its final iteration")
}

async fn publish_git_compatibility(
    store: &LocalThreadStore,
    state_db: &StateDbHandle,
    intent: ThreadGitCompatibilityIntent,
    live_path: Option<&Path>,
) -> std::io::Result<DurableRolloutAppendOutcome> {
    use std::io::Error;

    if intent.record_version != THREAD_GIT_COMPATIBILITY_RECORD_VERSION {
        return Err(Error::other("unsupported Git compatibility record version"));
    }
    let line: RolloutLine = serde_json::from_slice(&intent.record)?;
    let RolloutItem::SessionMeta(session_meta) = line.item else {
        return Err(Error::other("Git compatibility record is not SessionMeta"));
    };
    if session_meta.meta.id != intent.thread_id
        || session_meta.meta.session_id != intent.session_id
        || session_meta.meta.history_mode != ThreadHistoryMode::Legacy
    {
        return Err(Error::other("Git compatibility record has unexpected rollout provenance"));
    }

    let metadata = state_db
        .get_thread(intent.thread_id)
        .await
        .map_err(Error::other)?
        .ok_or_else(|| Error::other("Git compatibility thread no longer exists"))?;
    if metadata.session_id.is_some_and(|id| id != intent.session_id)
        || metadata.history_mode != ThreadHistoryMode::Legacy
    {
        return Err(Error::other("Git compatibility thread has changed identity or history mode"));
    }
    let path = resolve_git_compatibility_rollout_path(
        &store.config.codex_home,
        &intent.rollout_path,
        &metadata.rollout_path,
        live_path,
    )
    .await?;
    if let Some(checkpoint) = intent.publication_checkpoint {
        let checkpoint: RolloutAppendCheckpoint = serde_json::from_slice(&checkpoint)?;
        return Ok(replay_checkpointed_session_meta(&path, session_meta, checkpoint).await);
    }

    let state_db = state_db.clone();
    let thread_id = intent.thread_id;
    let sequence = intent.sequence;
    Ok(append_checkpointed_session_meta(&path, session_meta, move |checkpoint| async move {
        let bytes = serde_json::to_vec(&checkpoint)?;
        match state_db
            .prepare_thread_git_compatibility_intent(thread_id, sequence, &bytes)
            .await
            .map_err(Error::other)?
        {
            ThreadGitCompatibilityPrepare::Prepared | ThreadGitCompatibilityPrepare::AlreadyPrepared => {
                Ok(())
            }
            ThreadGitCompatibilityPrepare::Conflict | ThreadGitCompatibilityPrepare::NotPendingHead => {
                Err(Error::other("Git intent is not the current exact append owner"))
            }
        }
    }).await)
}

async fn apply_thread_name(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    name: String,
    committed: bool,
) -> ThreadStoreResult<()> {
    match append_thread_name(store.config.codex_home.as_path(), thread_id, &name).await {
        Ok(()) => Ok(()),
        Err(err) if committed => {
            warn!(%thread_id, %err, "thread name committed; compatibility name index update failed; no retry required");
            Ok(())
        }
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!("failed to index thread name: {err}"),
        }),
    }
}

