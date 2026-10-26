use std::collections::HashMap;
use std::collections::HashSet;

use chrono::Duration as ChronoDuration;
use chrono::SecondsFormat;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::find_thread_names_by_ids;
use codex_rollout::parse_cursor;

use super::LocalThreadStore;
use super::helpers::distinct_thread_metadata_title;
use super::helpers::set_thread_name_from_title;
use super::helpers::stored_thread_from_rollout_item;
use super::helpers::thread_item_titles;
use super::read_thread::stored_thread_from_sqlite_metadata_with_name;
use crate::ListThreadsParams;
use crate::SortDirection;
use crate::StoredThread;
use crate::ThreadListStorageMode;
use crate::ThreadPage;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::types::ThreadListStoragePath;

const THREAD_LIST_CURSOR_PREFIX: &str = "thread-list-v1:";
const STATE_DB_CURSOR_PREFIX: &str = "state-db:";
const SCAN_AND_REPAIR_CURSOR_PREFIX: &str = "scan-and-repair:";

pub(super) struct BoundThreadListCursor {
    pub(super) storage_path: ThreadListStoragePath,
    pub(super) position: codex_rollout::Cursor,
}

pub(super) struct SelectedRolloutThreads {
    pub(super) page: codex_rollout::ThreadsPage,
    storage_path: ThreadListStoragePath,
    pub(super) state_metadata: HashMap<codex_protocol::ThreadId, codex_state::ThreadMetadata>,
    state_db_reads: usize,
}

pub(super) async fn list_threads(
    store: &LocalThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<ThreadPage> {
    let (page, _) = list_threads_with_state_db_read_count(store, params).await?;
    Ok(page)
}

async fn list_threads_with_state_db_read_count(
    store: &LocalThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<(ThreadPage, usize)> {
    let cursor = params
        .cursor
        .as_deref()
        .map(parse_bound_cursor)
        .transpose()?;
    let state_db = store.state_db().await;
    let hydration_db = state_db.clone();
    let rollout_config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite_home: store.config.sqlite_home.clone(),
        cwd: store.config.codex_home.clone(),
        model_provider_id: store.config.default_model_provider_id.clone(),
    };
    let SelectedRolloutThreads {
        page,
        storage_path,
        mut state_metadata,
        mut state_db_reads,
    } = select_rollout_threads(
        state_db.clone(),
        &rollout_config,
        store.config.default_model_provider_id.as_str(),
        &params,
        cursor.as_ref(),
    )
    .await?;

    let next_cursor = page
        .next_cursor
        .as_ref()
        .map(|cursor| encode_rollout_cursor(storage_path, cursor))
        .transpose()?;
    let (mut names, mut resolved_title_ids) = thread_item_titles(&page.items);
    let scan_backwards_timestamp = if storage_path == ThreadListStoragePath::ScanAndRepair {
        page.items
            .first()
            .and_then(|item| match params.sort_key {
                ThreadSortKey::CreatedAt => codex_rollout::rollout_created_at_key(&item.path),
                ThreadSortKey::UpdatedAt => item.updated_at.clone(),
                ThreadSortKey::RecencyAt => {
                    item.recency_at.clone().or_else(|| item.updated_at.clone())
                }
            })
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(&timestamp).ok())
            .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
    } else {
        None
    };
    if storage_path != ThreadListStoragePath::StateDb
        && let Some(state_db_ctx) = hydration_db.as_deref()
    {
        let ids = page.items.iter().filter_map(|item| {
            item.thread_id.or_else(|| super::helpers::thread_id_from_rollout_path(&item.path))
        }).collect::<Vec<_>>();
        if !ids.is_empty() {
            state_db_reads += 1;
            if let Ok(metadata) = state_db_ctx.get_threads_for_listing(&ids).await {
                state_metadata = metadata.into_iter().map(|metadata| (metadata.id, metadata)).collect();
            }
        }
    }
    let mut items = Vec::with_capacity(page.items.len());
    for item in page.items {
        let Some(mut thread) = stored_thread_from_rollout_item(
            item,
            params.archived,
            store.config.default_model_provider_id.as_str(),
        ) else {
            continue;
        };
        let relation_parent_thread_id = thread.parent_thread_id;
        let metadata = state_metadata.remove(&thread.thread_id);
        if let Some(metadata) = metadata {
            let name = distinct_thread_metadata_title(&metadata);
            if let Some(title) = name.as_ref() {
                resolved_title_ids.insert(thread.thread_id);
                names
                    .entry(thread.thread_id)
                    .or_insert_with(|| title.clone());
            }
            if let Ok(authoritative_thread) =
                stored_thread_from_sqlite_metadata_with_name(store, metadata, name).await
            {
                if storage_path == ThreadListStoragePath::StateDb {
                    thread = authoritative_thread;
                } else {
                    // The recorder has already reconciled the scan and only overlaid
                    // SQLite fields when repair succeeded. Do not replace its filesystem
                    // evidence with a readable but stale row after a failed repair.
                    thread.project_id = authoritative_thread.project_id;
                    thread.parent_thread_id = authoritative_thread.parent_thread_id;
                    thread.session_id = authoritative_thread.session_id;
                    thread.extra_config = authoritative_thread.extra_config;
                    thread.forked_from_id = authoritative_thread.forked_from_id;
                    thread.model = authoritative_thread.model;
                    thread.reasoning_effort = authoritative_thread.reasoning_effort;
                    thread.thread_source = authoritative_thread.thread_source;
                    thread.agent_path = authoritative_thread.agent_path;
                    thread.approval_mode = authoritative_thread.approval_mode;
                    thread.permission_profile = authoritative_thread.permission_profile;
                    thread.token_usage = authoritative_thread.token_usage;
                }
                if params.relation_filter.is_some() {
                    thread.parent_thread_id = relation_parent_thread_id;
                }
            }
        }
        if thread.session_id.is_none()
            && let Some(path) = thread.rollout_path.as_deref()
            && let Ok(header) = codex_rollout::read_session_meta_line(path).await
            && header.meta.id == thread.thread_id
        {
            thread.session_id = Some(header.meta.session_id);
            thread.forked_from_id = header.meta.forked_from_id;
            thread.thread_source = header.meta.thread_source;
        }
        items.push(thread);
    }

    let thread_ids = items
        .iter()
        .map(|thread| thread.thread_id)
        .collect::<HashSet<_>>();
    let unresolved_thread_ids = thread_ids
        .difference(&resolved_title_ids)
        .copied()
        .collect::<HashSet<_>>();
    if !unresolved_thread_ids.is_empty()
        && let Ok(legacy_names) =
            find_thread_names_by_ids(store.config.codex_home.as_path(), &unresolved_thread_ids)
                .await
    {
        for (thread_id, title) in legacy_names {
            names.entry(thread_id).or_insert(title);
        }
    }
    for thread in &mut items {
        if let Some(title) = names.get(&thread.thread_id).cloned() {
            set_thread_name_from_title(thread, title);
        }
    }

    let backwards_cursor = items
        .first()
        .and_then(|thread| {
            let mut anchor = thread.clone();
            if let Some(timestamp) = scan_backwards_timestamp {
                match params.sort_key {
                    ThreadSortKey::CreatedAt => anchor.created_at = timestamp,
                    ThreadSortKey::UpdatedAt => anchor.updated_at = timestamp,
                    ThreadSortKey::RecencyAt => anchor.recency_at = timestamp,
                }
            }
            backwards_cursor_position(&anchor, params.sort_key, params.sort_direction)
        })
        .map(|position| bind_cursor(storage_path, &position));

    Ok((
        ThreadPage {
            items,
            next_cursor,
            backwards_cursor,
        },
        state_db_reads,
    ))
}

pub(super) async fn select_rollout_threads(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&BoundThreadListCursor>,
) -> ThreadStoreResult<SelectedRolloutThreads> {
    let selected_path = storage_path_for_request(params, cursor)?;
    if selected_path == Some(ThreadListStoragePath::StateDb) {
        return list_state_threads_for_storage(
            state_db,
            config,
            params,
            cursor.map(|cursor| &cursor.position),
        )
        .await;
    }
    if selected_path == Some(ThreadListStoragePath::ScanAndRepair) {
        let page = list_rollout_threads_for_storage(
            state_db,
            config,
            default_model_provider_id,
            params,
            cursor.map(|cursor| &cursor.position),
            ThreadListStoragePath::ScanAndRepair,
        )
        .await?;
        return Ok(SelectedRolloutThreads {
            page,
            storage_path: ThreadListStoragePath::ScanAndRepair,
            state_metadata: HashMap::new(),
            state_db_reads: 0,
        });
    }

    match list_state_threads_for_storage(state_db.clone(), config, params, /*cursor*/ None).await {
        Ok(selected) => Ok(selected),
        Err(ThreadStoreError::Internal { .. }) => {
            let page = list_rollout_threads_for_storage(
                state_db,
                config,
                default_model_provider_id,
                params,
                /*cursor*/ None,
                ThreadListStoragePath::ScanAndRepair,
            )
            .await?;
            Ok(SelectedRolloutThreads {
                page,
                storage_path: ThreadListStoragePath::ScanAndRepair,
                state_metadata: HashMap::new(),
                state_db_reads: 0,
            })
        }
        Err(err) => Err(err),
    }
}

async fn list_state_threads_for_storage(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
) -> ThreadStoreResult<SelectedRolloutThreads> {
    if params
        .cwd_filters
        .as_deref()
        .is_some_and(<[std::path::PathBuf]>::is_empty)
    {
        return Ok(SelectedRolloutThreads {
            page: codex_rollout::ThreadsPage::default(),
            storage_path: ThreadListStoragePath::StateDb,
            state_metadata: HashMap::new(),
            state_db_reads: 0,
        });
    }

    let state_page = codex_rollout::state_integration::list_threads_db(
        state_db.as_deref(),
        config.codex_home.as_path(),
        params.page_size,
        cursor,
        params.sort_key,
        params.sort_direction,
        params.allowed_sources.as_slice(),
        params.model_providers.as_deref(),
        params.cwd_filters.as_deref(),
        params.relation_filter,
        params.archived,
        params.project_id.as_ref().map(Option::as_deref),
        params.search_term.as_deref(),
    )
    .await
    .ok_or_else(|| ThreadStoreError::Internal {
        message: "state DB unavailable for thread listing".to_string(),
    })?;
    let state_metadata = state_page
        .items
        .iter()
        .map(|metadata| (metadata.id, metadata.clone()))
        .collect();

    Ok(SelectedRolloutThreads {
        page: state_page.into(),
        storage_path: ThreadListStoragePath::StateDb,
        state_metadata,
        state_db_reads: 1,
    })
}

pub(super) async fn list_rollout_threads_for_storage(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    storage_path: ThreadListStoragePath,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    let sort_key = params.sort_key;
    let sort_direction = params.sort_direction;
    if let Some(relation_filter) = params.relation_filter {
        if storage_path != ThreadListStoragePath::StateDb {
            return Err(ThreadStoreError::InvalidRequest {
                message: "relationship-filtered thread listing requires state DB storage"
                    .to_string(),
            });
        }
        let page = codex_rollout::state_integration::list_threads_db(
            state_db.as_deref(),
            config.codex_home.as_path(),
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            Some(relation_filter),
            params.archived,
            params.project_id.as_ref().map(Option::as_deref),
            params.search_term.as_deref(),
        )
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "state DB unavailable for relationship-filtered thread listing".to_string(),
        })?;
        return Ok(page.into());
    }

    let page = if storage_path == ThreadListStoragePath::StateDb && params.archived {
        RolloutRecorder::list_archived_threads_from_state_db(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else if storage_path == ThreadListStoragePath::StateDb {
        RolloutRecorder::list_threads_from_state_db(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else if params.archived {
        RolloutRecorder::list_archived_threads(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else {
        RolloutRecorder::list_threads(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    };
    page.map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to list threads: {err}"),
    })
}

fn storage_path_for_request(
    params: &ListThreadsParams,
    cursor: Option<&BoundThreadListCursor>,
) -> ThreadStoreResult<Option<ThreadListStoragePath>> {
    if params.project_id.is_some() {
        if params.storage_mode == ThreadListStorageMode::ScanAndRepair {
            return Err(ThreadStoreError::InvalidRequest {
                message: "project-filtered thread listing requires state DB storage".to_string(),
            });
        }
        if cursor.is_some_and(|cursor| cursor.storage_path != ThreadListStoragePath::StateDb) {
            return Err(ThreadStoreError::InvalidRequest {
                message: "project-filtered thread listing requires a state DB cursor".to_string(),
            });
        }
    }

    if params.relation_filter.is_some()
        && params.storage_mode == ThreadListStorageMode::ScanAndRepair
    {
        return Err(ThreadStoreError::InvalidRequest {
            message:
                "relationship-filtered thread listing does not support scan-and-repair storage"
                    .to_string(),
        });
    }

    if let Some(cursor) = cursor {
        if params.relation_filter.is_some() && cursor.storage_path != ThreadListStoragePath::StateDb
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "relationship-filtered thread listing requires a state DB cursor"
                    .to_string(),
            });
        }
        let explicitly_requested_path = match params.storage_mode {
            ThreadListStorageMode::PreferStateDb => None,
            ThreadListStorageMode::StateDbOnly => Some(ThreadListStoragePath::StateDb),
            ThreadListStorageMode::ScanAndRepair => Some(ThreadListStoragePath::ScanAndRepair),
        };
        if explicitly_requested_path.is_some_and(|path| path != cursor.storage_path) {
            return Err(ThreadStoreError::InvalidRequest {
                message: "thread-list cursor storage does not match the requested storage mode"
                    .to_string(),
            });
        }
        return Ok(Some(cursor.storage_path));
    }

    if params.relation_filter.is_some() || params.project_id.is_some() {
        return Ok(Some(ThreadListStoragePath::StateDb));
    }
    Ok(match params.storage_mode {
        // A completed one-time backfill is not a current root-coverage proof:
        // another writer can publish or move a rollout without updating SQL.
        // Reuse canonical scan/repair until the index has a durable freshness
        // contract. Explicit DB views and already-bound cursors stay DB-only.
        ThreadListStorageMode::PreferStateDb => Some(ThreadListStoragePath::ScanAndRepair),
        ThreadListStorageMode::StateDbOnly => Some(ThreadListStoragePath::StateDb),
        ThreadListStorageMode::ScanAndRepair => Some(ThreadListStoragePath::ScanAndRepair),
    })
}

pub(super) fn parse_bound_cursor(token: &str) -> ThreadStoreResult<BoundThreadListCursor> {
    let payload = token
        .strip_prefix(THREAD_LIST_CURSOR_PREFIX)
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: format!("invalid cursor: {token}"),
        })?;
    let (storage_path, position) =
        if let Some(position) = payload.strip_prefix(STATE_DB_CURSOR_PREFIX) {
            (ThreadListStoragePath::StateDb, position)
        } else if let Some(position) = payload.strip_prefix(SCAN_AND_REPAIR_CURSOR_PREFIX) {
            (ThreadListStoragePath::ScanAndRepair, position)
        } else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("invalid cursor: {token}"),
            });
        };
    let position = parse_cursor(position).ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("invalid cursor: {token}"),
    })?;
    Ok(BoundThreadListCursor {
        storage_path,
        position,
    })
}

pub(super) fn encode_rollout_cursor(
    storage_path: ThreadListStoragePath,
    cursor: &codex_rollout::Cursor,
) -> ThreadStoreResult<String> {
    let position = serde_json::to_value(cursor)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "failed to serialize thread-list cursor".to_string(),
        })?;
    Ok(bind_cursor(storage_path, &position))
}

fn bind_cursor(storage_path: ThreadListStoragePath, position: &str) -> String {
    let storage_prefix = match storage_path {
        ThreadListStoragePath::StateDb => STATE_DB_CURSOR_PREFIX,
        ThreadListStoragePath::ScanAndRepair => SCAN_AND_REPAIR_CURSOR_PREFIX,
    };
    format!("{THREAD_LIST_CURSOR_PREFIX}{storage_prefix}{position}")
}

fn backwards_cursor_position(
    thread: &StoredThread,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
) -> Option<String> {
    let timestamp = match sort_key {
        ThreadSortKey::CreatedAt => thread.created_at,
        ThreadSortKey::UpdatedAt => thread.updated_at,
        ThreadSortKey::RecencyAt => thread.recency_at,
    };
    let timestamp = match sort_direction {
        SortDirection::Asc => timestamp.checked_add_signed(ChronoDuration::milliseconds(1))?,
        SortDirection::Desc => timestamp.checked_sub_signed(ChronoDuration::milliseconds(1))?,
    };
    Some(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

