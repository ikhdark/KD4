use chrono::DateTime;
use chrono::Utc;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_rollout::RolloutRecorder;
use codex_rollout::find_thread_name_by_id;
use codex_rollout::read_session_meta_line;
use codex_rollout::read_thread_item_from_rollout;
use codex_rollout::read_thread_preview_from_rollout;
use codex_state::ThreadMetadata;

use super::LocalThreadStore;
use super::helpers::distinct_thread_metadata_title;
use super::helpers::git_info_from_parts;
use super::helpers::permission_profile_from_metadata_value;
use super::helpers::resolve_rollout_path;
use super::helpers::rollout_path_is_archived;
use super::helpers::set_thread_name_from_title;
use super::helpers::stored_thread_from_rollout_item;
use crate::ReadThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::error::reject_paginated_history_mode;

pub(super) async fn read_thread(
    store: &LocalThreadStore,
    params: ReadThreadParams,
) -> ThreadStoreResult<StoredThread> {
    read_thread_with_payload_versions(store, params, None).await
}

pub(super) async fn read_thread_with_payload_versions(
    store: &LocalThreadStore,
    params: ReadThreadParams,
    mut payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
) -> ThreadStoreResult<StoredThread> {
    let thread_id = params.thread_id;
    if let Some((metadata, header)) =
        visible_sqlite_metadata(store, thread_id, params.include_archived).await?
    {
        let rollout_path = metadata.rollout_path.clone();
        let name = sqlite_metadata_name(store, &metadata).await;
        let mut thread = stored_thread_from_resolved_sqlite_metadata(store, metadata, name, header);
        reject_paginated_history(&thread, params.include_history)?;
        let preloaded_history = if params.include_history {
            load_history_items_for_thread_with_versions(
                &rollout_path,
                thread_id,
                payload_versions.as_deref_mut(),
            )
            .await
        } else {
            None
        };
        if params.include_history && preloaded_history.is_none() {
            // SQLite metadata can outlive a moved/recreated rollout path. Fall through to the
            // canonical path resolver when the single history read does not match this thread.
        } else {
            if !params.include_history
                && let Some(rollout_path) = thread.rollout_path.clone()
                && let Ok(Some((preview_thread_id, preview))) =
                    read_thread_preview_from_rollout(&rollout_path).await
                && preview_thread_id == thread_id
                && !preview.is_empty()
            {
                // Preview extraction can be newer than the last SQLite flush, but the SQLite-backed
                // object remains authoritative for every persisted metadata field.
                thread.preview = preview;
            }
            if let Some(items) = preloaded_history {
                thread.history = Some(StoredThreadHistory { thread_id, items });
            }
            return Ok(thread);
        }
    }

    read_thread_from_resolved_rollout(store, params, payload_versions).await
}

/// Returns the rollout path a summary read would report, with the same visibility and identity
/// checks, without building the summary's name, preview, and session-metadata fields.
pub(super) async fn read_thread_rollout_path(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<Option<std::path::PathBuf>> {
    if let Some((metadata, header)) = visible_sqlite_metadata(store, thread_id, include_archived).await? {
        let foreign = header.as_ref().is_some_and(|line| line.meta.id != metadata.id);
        return Ok((!foreign).then(|| codex_rollout::plain_rollout_path(&metadata.rollout_path)));
    }
    read_thread_from_resolved_rollout(
        store,
        ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        },
        None,
    )
    .await
    .map(|thread| thread.rollout_path)
}

/// SQLite metadata for a thread, when present and visible under `include_archived`.
async fn visible_sqlite_metadata(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    include_archived: bool,
) -> ThreadStoreResult<Option<(ThreadMetadata, Option<SessionMetaLine>)>> {
    let Some(mut metadata) = read_sqlite_metadata(store, thread_id).await else {
        return Ok(None);
    };
    let header = resolve_sqlite_rollout_location(store, &mut metadata).await?;
    let visible = include_archived
        || (metadata.archived_at.is_none()
            && !rollout_path_is_archived(
                store.config.codex_home.as_path(),
                metadata.rollout_path.as_path(),
            ));
    Ok(visible.then_some((metadata, header)))
}

async fn read_thread_from_resolved_rollout(
    store: &LocalThreadStore,
    params: ReadThreadParams,
    payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
) -> ThreadStoreResult<StoredThread> {
    let thread_id = params.thread_id;
    let path = resolve_rollout_path(
        store,
        thread_id,
        params.include_archived,
        /*require_materialized*/ true,
    )
    .await?
    .map(|resolved| resolved.path)
    .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;

    let mut thread = read_thread_from_rollout_path(store, path).await?;
    if thread.thread_id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "rollout session metadata id mismatch: expected {thread_id}, found {}",
                thread.thread_id
            ),
        });
    }
    if !params.include_archived && thread.archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", thread.thread_id),
        });
    }
    reject_paginated_history(&thread, params.include_history)?;
    attach_history_if_requested(&mut thread, params.include_history, payload_versions).await?;
    Ok(thread)
}

async fn load_history_items_for_thread(
    path: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
) -> Option<Vec<codex_protocol::protocol::RolloutItem>> {
    codex_rollout::existing_rollout_path(path).await?;
    let (items, loaded_thread_id, _) = RolloutRecorder::load_rollout_items(path).await.ok()?;
    (loaded_thread_id == Some(thread_id)).then_some(items)
}

async fn load_history_items_for_thread_with_versions(
    path: &std::path::Path,
    thread_id: codex_protocol::ThreadId,
    payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
) -> Option<Vec<codex_protocol::protocol::RolloutItem>> {
    if let Some(versions) = payload_versions {
        *versions = None;
        codex_rollout::existing_rollout_path(path).await?;
        let (items, loaded_thread_id, _, observed) =
            RolloutRecorder::load_rollout_items_with_payload_versions(path).await.ok()?;
        if loaded_thread_id != Some(thread_id) {
            return None;
        }
        *versions = Some(observed);
        return Some(items);
    }
    load_history_items_for_thread(path, thread_id).await
}

pub(super) async fn read_thread_by_rollout_path(
    store: &LocalThreadStore,
    rollout_path: std::path::PathBuf,
    include_archived: bool,
    include_history: bool,
) -> ThreadStoreResult<StoredThread> {
    read_thread_by_rollout_path_with_payload_versions(
        store,
        rollout_path,
        include_archived,
        include_history,
        None,
    )
    .await
}

pub(super) async fn read_thread_by_rollout_path_with_payload_versions(
    store: &LocalThreadStore,
    rollout_path: std::path::PathBuf,
    include_archived: bool,
    include_history: bool,
    payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
) -> ThreadStoreResult<StoredThread> {
    let path = resolve_requested_rollout_path(store, rollout_path).await?;
    let mut thread = read_thread_from_rollout_path(store, path).await?;
    if !include_archived && thread.archived_at.is_some() {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!("thread {} is archived", thread.thread_id),
        });
    }
    if let Some(mut metadata) = read_sqlite_metadata(store, thread.thread_id).await {
        // The requested, verified physical location owns archive/path state;
        // the current row owns explicit metadata (including Git null clears).
        metadata.rollout_path = thread.rollout_path.clone().unwrap_or(metadata.rollout_path);
        metadata.archived_at = thread.archived_at;
        thread = stored_thread_from_sqlite_metadata(store, metadata).await?;
    }
    reject_paginated_history(&thread, include_history)?;
    attach_history_if_requested(&mut thread, include_history, payload_versions).await?;
    Ok(thread)
}

fn reject_paginated_history(thread: &StoredThread, include_history: bool) -> ThreadStoreResult<()> {
    if include_history {
        reject_paginated_history_mode(thread.history_mode)?;
    }
    Ok(())
}

async fn resolve_requested_rollout_path(
    store: &LocalThreadStore,
    rollout_path: std::path::PathBuf,
) -> ThreadStoreResult<std::path::PathBuf> {
    let path = if rollout_path.is_relative() {
        store.config.codex_home.join(rollout_path)
    } else {
        rollout_path
    };
    match tokio::fs::metadata(path.as_path()).await {
        Ok(metadata) if metadata.is_dir() => {
            return Err(ThreadStoreError::RolloutNotMaterialized {
                path,
                reason: "path is a directory".to_string(),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(ThreadStoreError::RolloutNotMaterialized {
                path,
                reason: "path is not a file".to_string(),
            });
        }
        _ => {}
    }
    let mut retries = 3;
    loop {
        let Some(resolved_path) = codex_rollout::existing_rollout_path(path.as_path()).await else {
            return Err(ThreadStoreError::RolloutNotMaterialized {
                path,
                reason: "file does not exist".to_string(),
            });
        };
        match tokio::fs::canonicalize(resolved_path.as_path()).await {
            Ok(path) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && retries > 0 => {
                // Compression/materialization can replace the physical spelling after lookup.
                // Resolve the logical rollout again instead of retrying the vanished file.
                retries -= 1;
            }
            Err(err) => {
                return Err(ThreadStoreError::RolloutNotMaterialized {
                    path: resolved_path,
                    reason: err.to_string(),
                });
            }
        }
    }
}

async fn attach_history_if_requested(
    thread: &mut StoredThread,
    include_history: bool,
    payload_versions: Option<&mut Option<Vec<codex_rollout::PayloadDependencyVersion>>>,
) -> ThreadStoreResult<()> {
    if !include_history {
        return Ok(());
    }
    let thread_id = thread.thread_id;
    let Some(path) = thread.rollout_path.clone() else {
        return Err(ThreadStoreError::Internal {
            message: format!("failed to load thread history for thread {thread_id}"),
        });
    };
    let items = load_history_items_for_thread_with_versions(&path, thread_id, payload_versions)
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!(
                "failed to load matching thread history {} for {thread_id}",
                path.display()
            ),
        })?;
    thread.history = Some(StoredThreadHistory { thread_id, items });
    Ok(())
}

async fn read_thread_from_rollout_path(
    store: &LocalThreadStore,
    path: std::path::PathBuf,
) -> ThreadStoreResult<StoredThread> {
    let Some(item) = read_thread_item_from_rollout(path.clone()).await else {
        return stored_thread_from_session_meta(store, path).await;
    };
    let archived = rollout_path_is_archived(store.config.codex_home.as_path(), path.as_path());
    let mut thread = stored_thread_from_rollout_item(
        item,
        archived,
        store.config.default_model_provider_id.as_str(),
    )
    .ok_or_else(|| ThreadStoreError::Internal {
        message: format!("failed to read thread id from {}", path.display()),
    })?;
    thread.rollout_path = Some(codex_rollout::plain_rollout_path(path.as_path()));
    match read_session_meta_line(path.as_path()).await {
        Ok(meta_line) => {
            if meta_line.meta.id != thread.thread_id {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("rollout session metadata id mismatch in {}", path.display()),
                });
            }
            thread.forked_from_id = meta_line.meta.forked_from_id;
            thread.session_id = Some(meta_line.meta.session_id);
            thread.parent_thread_id = meta_line.meta.parent_thread_id;
            thread.history_mode = meta_line.meta.history_mode;
        }
        // The rollout summary recovered the filename identity. Preserve the
        // readable suffix without inventing missing creation-only fields.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to read session metadata {}: {error}",
                    path.display()
                ),
            });
        }
    }
    // The rollout summary already applies later persisted settings to the provider.
    // SessionMeta supplies creation-only fields, not the current provider.
    if let Ok(Some(title)) =
        find_thread_name_by_id(store.config.codex_home.as_path(), &thread.thread_id).await
    {
        set_thread_name_from_title(&mut thread, title);
    }
    Ok(thread)
}

// Resolve only missing indexed paths. Lazily materialized rows remain readable when no
// rollout exists, while completed archive moves take effect even if index maintenance failed.
pub(super) async fn resolve_sqlite_rollout_location(
    store: &LocalThreadStore,
    metadata: &mut ThreadMetadata,
) -> ThreadStoreResult<Option<SessionMetaLine>> {
    let mut header = read_session_meta_line(&metadata.rollout_path).await.ok();
    let foreign = header.as_ref().is_some_and(|line| line.meta.id != metadata.id);
    if (foreign
        || codex_rollout::existing_rollout_path(&metadata.rollout_path)
            .await
            .is_none())
        && let Some(resolved) = resolve_rollout_path(store, metadata.id, true, true).await?
    {
        metadata.rollout_path = resolved.path;
        header = read_session_meta_line(&metadata.rollout_path).await.ok();
        metadata.archived_at = if resolved.archived {
            Some(metadata.archived_at.unwrap_or(metadata.updated_at))
        } else {
            None
        };
    }
    Ok(header)
}

async fn read_sqlite_metadata(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
) -> Option<ThreadMetadata> {
    let runtime = store.state_db().await?;
    runtime.get_thread(thread_id).await.ok().flatten()
}

pub(super) async fn stored_thread_from_sqlite_metadata(
    store: &LocalThreadStore,
    metadata: ThreadMetadata,
) -> ThreadStoreResult<StoredThread> {
    let name = sqlite_metadata_name(store, &metadata).await;
    stored_thread_from_sqlite_metadata_with_name(store, metadata, name).await
}

async fn sqlite_metadata_name(store: &LocalThreadStore, metadata: &ThreadMetadata) -> Option<String> {
    match distinct_thread_metadata_title(metadata) {
        Some(title) => Some(title),
        None => find_thread_name_by_id(store.config.codex_home.as_path(), &metadata.id)
            .await
            .ok()
            .flatten()
            .filter(|title| !title.trim().is_empty()),
    }
}

pub(super) async fn stored_thread_from_sqlite_metadata_with_name(
    store: &LocalThreadStore,
    mut metadata: ThreadMetadata,
    name: Option<String>,
) -> ThreadStoreResult<StoredThread> {
    let header = resolve_sqlite_rollout_location(store, &mut metadata).await?;
    Ok(stored_thread_from_resolved_sqlite_metadata(store, metadata, name, header))
}

fn stored_thread_from_resolved_sqlite_metadata(
    store: &LocalThreadStore,
    metadata: ThreadMetadata,
    name: Option<String>,
    header: Option<SessionMetaLine>,
) -> StoredThread {
    // SQLite owns the persisted projection. Session metadata only supplements fields that are
    // not stored in the row, so an unavailable or malformed rollout must not discard the row.
    let header = header.map(|meta_line| meta_line.meta);
    let foreign = header.as_ref().is_some_and(|meta| meta.id != metadata.id);
    let session_meta = header.filter(|meta| meta.id == metadata.id);
    let rollout_path = codex_rollout::plain_rollout_path(metadata.rollout_path.as_path());
    let forked_from_id = session_meta.as_ref().and_then(|meta| meta.forked_from_id);
    let parent_thread_id = metadata
        .current_parent_thread_id
        .or_else(|| session_meta.as_ref().and_then(|meta| meta.parent_thread_id));
    let mut source = parse_session_source(&metadata.source);
    if let Some(current_parent) = metadata.current_parent_thread_id
        && let SessionSource::SubAgent(codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id,
            ..
        }) = &mut source
    {
        *parent_thread_id = current_parent;
    }
    let history_mode = session_meta
        .as_ref()
        .map(|meta| meta.history_mode)
        .unwrap_or(metadata.history_mode);
    let preview = metadata
        .preview
        .clone()
        .or_else(|| metadata.first_user_message.clone())
        .unwrap_or_default();
    let permission_profile =
        permission_profile_from_metadata_value(&metadata.sandbox_policy, metadata.cwd.as_path());
    StoredThread {
        project_id: metadata.project_id,
        thread_id: metadata.id,
        session_id: metadata
            .session_id
            .or_else(|| session_meta.as_ref().map(|meta| meta.session_id)),
        extra_config: None,
        rollout_path: (!foreign).then_some(rollout_path),
        forked_from_id,
        parent_thread_id,
        preview,
        name,
        model_provider: if metadata.model_provider.is_empty() {
            store.config.default_model_provider_id.clone()
        } else {
            metadata.model_provider
        },
        model: metadata.model,
        reasoning_effort: metadata.reasoning_effort,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
        recency_at: metadata.recency_at,
        archived_at: metadata.archived_at,
        cwd: metadata.cwd,
        cli_version: metadata.cli_version,
        source,
        history_mode,
        thread_source: metadata.thread_source,
        agent_nickname: metadata.agent_nickname,
        agent_role: metadata.agent_role,
        agent_path: metadata.agent_path,
        git_info: git_info_from_parts(
            metadata.git_sha,
            metadata.git_branch,
            metadata.git_origin_url,
        ),
        approval_mode: parse_or_default(&metadata.approval_mode, AskForApproval::OnRequest),
        permission_profile,
        token_usage: None,
        first_user_message: metadata.first_user_message,
        history: None,
    }
}

async fn stored_thread_from_session_meta(
    store: &LocalThreadStore,
    path: std::path::PathBuf,
) -> ThreadStoreResult<StoredThread> {
    let meta_line = read_required_session_meta_line(path.as_path()).await?;
    let archived = rollout_path_is_archived(store.config.codex_home.as_path(), path.as_path());
    Ok(stored_thread_from_meta_line(store, meta_line, path, archived).await)
}

async fn read_required_session_meta_line(
    path: &std::path::Path,
) -> ThreadStoreResult<SessionMetaLine> {
    read_session_meta_line(path)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to read session metadata {}: {err}", path.display()),
        })
}

async fn stored_thread_from_meta_line(
    store: &LocalThreadStore,
    meta_line: SessionMetaLine,
    path: std::path::PathBuf,
    archived: bool,
) -> StoredThread {
    let created_at = parse_rfc3339_non_optional(&meta_line.meta.timestamp).unwrap_or_else(Utc::now);
    let updated_at = tokio::fs::metadata(path.as_path())
        .await
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or(created_at);
    let rollout_path = codex_rollout::plain_rollout_path(path.as_path());
    StoredThread {
        project_id: None,
        thread_id: meta_line.meta.id,
        session_id: Some(meta_line.meta.session_id),
        extra_config: None,
        rollout_path: Some(rollout_path),
        forked_from_id: meta_line.meta.forked_from_id,
        parent_thread_id: meta_line.meta.parent_thread_id,
        preview: String::new(),
        name: None,
        model_provider: meta_line
            .meta
            .model_provider
            .filter(|provider| !provider.is_empty())
            .unwrap_or_else(|| store.config.default_model_provider_id.clone()),
        model: None,
        reasoning_effort: None,
        created_at,
        updated_at,
        recency_at: updated_at,
        archived_at: archived.then_some(updated_at),
        cwd: meta_line.meta.cwd,
        cli_version: meta_line.meta.cli_version,
        source: meta_line.meta.source,
        history_mode: meta_line.meta.history_mode,
        thread_source: meta_line.meta.thread_source,
        agent_nickname: meta_line.meta.agent_nickname,
        agent_role: meta_line.meta.agent_role,
        agent_path: meta_line.meta.agent_path,
        git_info: meta_line.git,
        approval_mode: AskForApproval::OnRequest,
        permission_profile: PermissionProfile::read_only(),
        token_usage: None,
        first_user_message: None,
        history: None,
    }
}

fn parse_session_source(source: &str) -> SessionSource {
    serde_json::from_str(source)
        .or_else(|_| serde_json::from_value(serde_json::Value::String(source.to_string())))
        .unwrap_or(SessionSource::Unknown)
}

fn parse_or_default<T>(value: &str, default: T) -> T
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(value)
        .or_else(|_| serde_json::from_value(serde_json::Value::String(value.to_string())))
        .unwrap_or(default)
}

fn parse_rfc3339_non_optional(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

