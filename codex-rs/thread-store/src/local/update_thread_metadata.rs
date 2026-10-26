use std::path::Path;
use std::path::PathBuf;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::append_rollout_item_to_path;
use codex_rollout::append_thread_name;
use codex_rollout::read_session_meta_line;
use codex_state::ThreadMetadataBuilder;
use tracing::warn;

use super::LocalThreadStore;
use super::helpers::ResolvedRolloutPath;
use super::helpers::distinct_thread_metadata_title;
use super::helpers::git_info_from_parts;
use super::helpers::permission_profile_to_metadata_value;
use super::helpers::resolve_rollout_path;
use super::helpers::rollout_path_is_archived;
use super::live_writer;
use crate::GitInfoPatch;
use crate::ReadThreadParams;
use crate::StoredThread;
use crate::ThreadMetadataPatch;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;
use crate::local::read_thread;


pub(super) async fn update_thread_metadata_ack(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<()> {
    update_thread_metadata_applied(store, params)
        .await
        .map(|_| ())
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

    let require_sqlite_write = sqlite_write_failure_should_block(&params.patch);
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
        return update_thread_metadata_owned(store, params).await;
    }
    let permit = std::sync::Arc::clone(&store.metadata_update_slots)
        .try_acquire_owned()
        .map_err(|_| ThreadStoreError::Conflict {
            message: "metadata update capacity reached; retry after pending updates settle".into(),
        })?;
    let thread_id = params.thread_id;
    // A live recorder shares its file lease with metadata maintenance. It does
    // not serialize two logical updates, so retain a per-thread ordering owner
    // through both projections. Weak entries cannot retain abandoned threads.
    let order = {
        let mut orders = store
            .metadata_update_order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        orders.retain(|_, order| order.strong_count() > 0);
        match orders.get(&thread_id).and_then(std::sync::Weak::upgrade) {
            Some(order) => order,
            None => {
                let order = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                orders.insert(thread_id, std::sync::Arc::downgrade(&order));
                order
            }
        }
    };
    let store = store.clone();
    // The logical owner, not just the physical append, survives observer loss.
    // It retains the writer lease through the SQLite/name-index finalizers.
    tokio::spawn(async move {
        let _permit = permit;
        let result = tokio::spawn(async move {
            let _order = order.lock_owned().await;
            update_thread_metadata_owned(&store, params).await
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
    if patch.is_empty() {
        return read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id,
                include_archived: params.include_archived,
                include_history: false,
            },
        )
        .await;
    }

    let needs_rollout_compat = needs_rollout_compatibility_update(&patch);
    let _lease = store.acquire_writer_lease(thread_id, true).await?;
    if needs_rollout_compat {
        // These explicit patches still write legacy rollout/name-index state after the
        // SQLite update. Paginated threads must fail before either side is mutated.
        let thread = read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id,
                include_archived: params.include_archived,
                include_history: false,
            },
        )
        .await?;
        reject_paginated_history_mode(thread.history_mode)?;
    }
    let require_sqlite_write = sqlite_write_failure_should_block(&patch);
    if !needs_rollout_compat {
        let updated = apply_metadata_update(
            store,
            thread_id,
            patch.clone(),
            params.include_archived,
            require_sqlite_write,
        )
        .await?;
        if let Some(metadata) = updated {
            return read_thread::stored_thread_from_sqlite_metadata(store, metadata).await;
        }
        return read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id,
                include_archived: params.include_archived,
                include_history: false,
            },
        )
        .await;
    }

    if live_writer::rollout_path(store, thread_id).await.is_ok() {
        live_writer::persist_thread(store, thread_id).await?;
    }
    let mut resolved_rollout_path = resolve_rollout_path(
        store,
        thread_id,
        params.include_archived,
        /*require_materialized*/ false,
    )
    .await?
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let name = patch.name.clone();
    let git_info = patch.git_info.clone();

    let state_db_ctx = store.state_db().await;
    // Reconciliation establishes only a missing baseline. Never reload older
    // rollout facts over an existing explicit SQLite patch.
    if let Some(state_db) = state_db_ctx.as_ref()
        && state_db
            .get_thread(thread_id)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to read thread metadata for {thread_id}: {err}"),
            })?
            .is_none()
    {
        codex_rollout::state_integration::reconcile_rollout(
            Some(state_db.as_ref()),
            resolved_rollout_path.path.as_path(),
            store.config.default_model_provider_id.as_str(),
            None,
            &[],
            resolved_rollout_path.archived.then_some(true),
        )
        .await;
    }

    let resolved_git_info = match git_info {
        Some(git_info) => {
            let Some(state_db) = store.state_db().await else {
                return Err(ThreadStoreError::Internal {
                    message: format!("sqlite state db unavailable for thread {thread_id}"),
                });
            };
            let metadata =
                state_db
                    .get_thread(thread_id)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to read git metadata for thread {thread_id}: {err}"
                        ),
                    })?;
            let Some(metadata) = metadata else {
                return Err(ThreadStoreError::Internal {
                    message: format!("thread metadata unavailable before git update: {thread_id}"),
                });
            };
            let existing = (
                metadata.git_sha.clone(),
                metadata.git_branch.clone(),
                metadata.git_origin_url.clone(),
            );
            let existing_git_info = git_info_from_parts(
                metadata.git_sha,
                metadata.git_branch,
                metadata.git_origin_url,
            );
            let resolved = resolve_git_info_patch(existing_git_info, git_info);
            // Clients resend unchanged git info every turn, and each rollout write
            // appends a full session metadata copy. SQLite is updated only after
            // the rollout, so matching SQLite means the rollout already has it.
            (resolved != existing).then_some(resolved)
        }
        None => None,
    };
    if let Some((sha, branch, origin_url)) = resolved_git_info.as_ref() {
        apply_thread_git_info_to_rollout(
            resolved_rollout_path.path.as_path(),
            thread_id,
            sha,
            branch,
            origin_url,
        )
        .await?;
        refresh_resolved_rollout_path(&mut resolved_rollout_path).await;
    }

    let updated = apply_metadata_update(
        store,
        thread_id,
        patch.clone(),
        params.include_archived,
        require_sqlite_write,
    )
    .await?;
    if let Some(name) = name {
        apply_thread_name(
            store,
            thread_id,
            name.unwrap_or_default(),
            updated.as_ref().and_then(distinct_thread_metadata_title).is_some(),
        )
        .await?;
    }
    // Prefer the committed projection; reading compatibility state must not
    // reapply an older rollout snapshot after the explicit patch.
    if let Some(metadata) = updated {
        return read_thread::stored_thread_from_sqlite_metadata(store, metadata).await;
    }
    let mut thread = match read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived: params.include_archived,
            include_history: false,
        },
    )
    .await
    {
        Ok(thread) => thread,
        Err(_) => {
            read_thread::read_thread_by_rollout_path(
                store,
                resolved_rollout_path.path,
                params.include_archived,
                /*include_history*/ false,
            )
            .await?
        }
    };
    if let Some((sha, branch, origin_url)) = resolved_git_info {
        thread.git_info = git_info_from_parts(sha, branch, origin_url);
    }
    Ok(thread)
}

async fn refresh_resolved_rollout_path(resolved: &mut ResolvedRolloutPath) {
    if let Some(path) = codex_rollout::existing_rollout_path(resolved.path.as_path()).await {
        resolved.path = path;
    }
}

async fn apply_metadata_update(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
    require_sqlite_write: bool,
) -> ThreadStoreResult<Option<codex_state::ThreadMetadata>> {
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
            let metadata = state_db
                .patch_thread_metadata_with_recency(&fallback,
                    advance_recency_at.filter(|_| existing.is_some()), |metadata| {
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
                    }
                    if let Some(title) = patch.title {
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
                    if existing.is_none()
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
                        let existing_git_info = git_info_from_parts(
                            metadata.git_sha.clone(),
                            metadata.git_branch.clone(),
                            metadata.git_origin_url.clone(),
                        );
                        let (sha, branch, origin_url) =
                            resolve_git_info_patch(existing_git_info, git_info);
                        metadata.git_sha = sha;
                        metadata.git_branch = branch;
                        metadata.git_origin_url = origin_url;
                    }
                    if let Some(project_id) = patch.project_id {
                        metadata.project_id = project_id;
                    }

                    Ok(())
                })
                .await
                .map_err(|err| {
                    let message = err.to_string();
                    if message.contains("project not found") || message.contains("is archived") {
                        ThreadStoreError::InvalidRequest { message }
                    } else if name_update {
                        ThreadStoreError::Internal {
                            message: format!("failed to set thread name: {message}"),
                        }
                    } else {
                        ThreadStoreError::Internal { message }
                    }
                })?;
            Ok(Some(metadata))
        }
        .await
    } else {
        Ok(None)
    };
    let updated_metadata = match (state_db.is_some(), sqlite_write_result) {
        (true, Ok(metadata)) => metadata,
        (true, Err(err)) if require_sqlite_write || !sqlite_write_error_is_best_effort(&err) => {
            return Err(err);
        }
        (true, Err(err)) => {
            warn!("state db update_thread_metadata failed for {thread_id}: {err}");
            None
        }
        (false, Ok(metadata)) => metadata,
        (false, Err(err)) if require_sqlite_write || !sqlite_write_error_is_best_effort(&err) => {
            return Err(err);
        }
        (false, Err(err)) => {
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
    if patch.name.is_some() {
        return true;
    }
    if patch.git_info.is_none() {
        return false;
    }
    !has_observed_metadata_facts(patch)
}

fn sqlite_write_failure_should_block(patch: &ThreadMetadataPatch) -> bool {
    // Before live metadata sync moved above the rollout writer, SQLite sync failures for
    // transcript-derived metadata were log-only. Keep that
    // failure isolation so a corrupted optional state DB does not make JSONL transcript durability
    // look broken. Explicit git-only updates still require SQLite because partial git patches need
    // the existing SQLite value to preserve unspecified fields. Name compatibility writes still
    // require their targeted SQLite update to succeed before writing the legacy name index.
    patch.name.is_some()
        || patch.project_id.is_some()
        || (patch.git_info.is_some() && !has_observed_metadata_facts(patch))
}

fn sqlite_write_error_is_best_effort(err: &ThreadStoreError) -> bool {
    matches!(err, ThreadStoreError::Internal { .. })
}

fn has_observed_metadata_facts(patch: &ThreadMetadataPatch) -> bool {
    patch.rollout_path.is_some()
        || patch.preview.is_some()
        || patch.title.is_some()
        || patch.model_provider.is_some()
        || patch.model.is_some()
        || patch.reasoning_effort.is_some()
        || patch.created_at.is_some()
        || patch.source.is_some()
        || patch.thread_source.is_some()
        || patch.agent_nickname.is_some()
        || patch.agent_role.is_some()
        || patch.agent_path.is_some()
        || patch.cwd.is_some()
        || patch.cli_version.is_some()
        || patch.approval_mode.is_some()
        || patch.permission_profile.is_some()
        || patch.token_usage.is_some()
        || patch.first_user_message.is_some()
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

fn resolve_git_info_patch(
    existing: Option<GitInfo>,
    git_info: GitInfoPatch,
) -> (Option<String>, Option<String>, Option<String>) {
    let (existing_sha, existing_branch, existing_origin_url) = match existing {
        Some(info) => (
            info.commit_hash.map(|sha| sha.0),
            info.branch,
            info.repository_url,
        ),
        None => (None, None, None),
    };
    let sha = git_info.sha.unwrap_or(existing_sha);
    let branch = git_info.branch.unwrap_or(existing_branch);
    let origin_url = git_info.origin_url.unwrap_or(existing_origin_url);
    (sha, branch, origin_url)
}

async fn apply_thread_git_info_to_rollout(
    rollout_path: &Path,
    thread_id: ThreadId,
    sha: &Option<String>,
    branch: &Option<String>,
    origin_url: &Option<String>,
) -> ThreadStoreResult<()> {
    let mut session_meta =
        read_session_meta_line(rollout_path)
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to set thread git metadata: {err}"),
            })?;
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "failed to set thread git metadata: rollout session metadata id mismatch: expected {thread_id}, found {}",
                session_meta.meta.id
            ),
        });
    }

    session_meta.git = Some(GitInfo {
        commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
        branch: branch.clone(),
        repository_url: origin_url.clone(),
    });
    append_rollout_item_to_path(rollout_path, &RolloutItem::SessionMeta(session_meta))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to set thread git metadata: {err}"),
        })
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

