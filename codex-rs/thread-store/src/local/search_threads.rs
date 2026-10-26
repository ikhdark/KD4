use std::collections::HashMap;
use std::collections::HashSet;

use codex_install_context::InstallContext;
use codex_protocol::ThreadId;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutSearchQuery;
use codex_rollout::find_thread_names_by_ids;
use codex_rollout::parse_cursor;

use super::LocalThreadStore;
use super::helpers::distinct_thread_title;
use super::helpers::set_thread_name_from_title;
use super::helpers::thread_item_titles;
use super::list_threads::BoundThreadListCursor;
use super::list_threads::encode_rollout_cursor;
use super::list_threads::parse_bound_cursor;
use super::list_threads::select_rollout_threads;
use crate::ListThreadsParams;
use crate::SearchThreadsParams;
use crate::StoredThreadSearchResult;
use crate::ThreadSearchPage;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;


struct ThreadSearchItem {
    item: codex_rollout::ThreadItem,
    snippet: String,
    metadata: Option<codex_state::ThreadMetadata>,
}

pub(super) async fn search_threads(
    store: &LocalThreadStore,
    params: SearchThreadsParams,
) -> ThreadStoreResult<ThreadSearchPage> {
    if params.page_size == 0 {
        return Err(ThreadStoreError::InvalidRequest {
            message: "page size must be greater than zero".to_string(),
        });
    }
    let search_term = params.search_term.as_str();
    if search_term.is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread/search requires search_term".to_string(),
        });
    }
    let state_db = store.state_db().await;
    let mut storage_path = if state_db.is_some() {
        crate::types::ThreadListStoragePath::StateDb
    } else {
        crate::types::ThreadListStoragePath::ScanAndRepair
    };
    let cursor = params
        .cursor
        .as_deref()
        .map(|cursor| {
            if cursor.starts_with("thread-list-v1:") {
                let bound = parse_bound_cursor(cursor)?;
                storage_path = bound.storage_path;
                return Ok(bound.position);
            }
            parse_cursor(cursor).ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("invalid cursor: {cursor}"),
            })
        })
        .transpose()?;
    let rollout_config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite_home: store.config.sqlite_home.clone(),
        cwd: store.config.codex_home.clone(),
        model_provider_id: store.config.default_model_provider_id.clone(),
    };
    let rg_command = InstallContext::current().rg_command();
    let query = RolloutSearchQuery::new(search_term).map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to prepare rollout search: {err}"),
    })?;
    let mut matching_rollouts = query
        .search_matches(
            rg_command.as_path(),
            store.config.codex_home.as_path(),
            params.archived,
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to search rollout contents: {err}"),
        })?;
    if matching_rollouts.is_empty() {
        return Ok(ThreadSearchPage {
            items: Vec::new(),
            next_cursor: None,
        });
    }
    let candidates: Vec<_> = matching_rollouts.keys().cloned().collect();
    for path in &candidates {
        if state_db.is_none() {
            if let Some(item) = codex_rollout::read_thread_item_from_rollout(path.clone()).await {
                if !params.allowed_sources.is_empty()
                    && item
                        .source
                        .as_ref()
                        .is_none_or(|source| !params.allowed_sources.contains(source))
                {
                    matching_rollouts.remove(path);
                }
            } else {
                if let Ok(header) = codex_rollout::read_session_meta_line(path).await
                    && !params.allowed_sources.is_empty()
                    && !params.allowed_sources.contains(&header.meta.source)
                {
                    matching_rollouts.remove(path);
                    continue;
                }
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "search catalog cannot represent a confirmed visible match at {}",
                        path.display()
                    ),
                });
            }
        }
    }
    if let Some(runtime) = state_db.as_deref() {
        // These paths were already discovered by content search. Missing or
        // stale index rows must not silently exclude canonical matches.
        for path in matching_rollouts.keys() {
            if !codex_rollout::state_integration::reconcile_rollout(
                Some(runtime),
                path,
                &store.config.default_model_provider_id,
                None,
                &[],
                Some(params.archived),
            )
            .await
            {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "search index reconciliation is incomplete for {}; retry after storage recovers",
                        path.display()
                    ),
                });
            }
            let id = codex_rollout::verified_rollout_thread_id(path)
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: error.to_string(),
                })?;
            let row = runtime
                .get_thread(id)
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: error.to_string(),
                })?;
            let eligible = row.as_ref().is_none_or(|row| {
                params.allowed_sources.is_empty()
                    || serde_json::from_value::<codex_protocol::protocol::SessionSource>(
                        serde_json::Value::String(row.source.clone()),
                    )
                    .is_ok_and(|source| params.allowed_sources.contains(&source))
            });
            if eligible
                && row.as_ref().is_none_or(|row| {
                    row.preview
                        .as_ref()
                        .or(row.first_user_message.as_ref())
                        .is_none_or(std::string::String::is_empty)
                })
            {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "search index cannot represent a confirmed visible match at {}",
                        path.display()
                    ),
                });
            }
        }
    }
    let mut matching_items = Vec::new();
    let mut page_cursor = cursor;
    let scan_page_size = params.page_size.saturating_mul(8).clamp(256, 2048);
    let scan_params = ListThreadsParams {
        project_id: None,
        page_size: scan_page_size,
        cursor: None,
        sort_key: params.sort_key,
        sort_direction: params.sort_direction,
        allowed_sources: params.allowed_sources.clone(),
        model_providers: None,
        cwd_filters: None,
        archived: params.archived,
        search_term: None,
        relation_filter: None,
        storage_mode: if storage_path == crate::types::ThreadListStoragePath::StateDb {
            crate::ThreadListStorageMode::StateDbOnly
        } else {
            crate::ThreadListStorageMode::ScanAndRepair
        },
    };
    let mut remaining_rollouts = matching_rollouts;

    loop {
        let bound_cursor = page_cursor.map(|position| BoundThreadListCursor {
            storage_path,
            position,
        });
        let mut selected = select_rollout_threads(
            state_db.clone(),
            &rollout_config,
            store.config.default_model_provider_id.as_str(),
            &scan_params,
            bound_cursor.as_ref(),
        )
        .await?;
        let page = selected.page;
        for item in page.items {
            let logical_path = codex_rollout::plain_rollout_path(item.path.as_path());
            // Content search already inspected each snippetless candidate to its
            // end: the match is metadata-only, and rereading cannot find one.
            let Some(snippet) = remaining_rollouts
                .remove(logical_path.as_path())
                .flatten()
            else {
                continue;
            };
            let metadata = item
                .thread_id
                .and_then(|id| selected.state_metadata.remove(&id));
            matching_items.push(ThreadSearchItem {
                item,
                snippet,
                metadata,
            });
            if matching_items.len() > params.page_size {
                break;
            }
        }
        page_cursor = page.next_cursor;
        if matching_items.len() > params.page_size
            || remaining_rollouts.is_empty()
            || page_cursor.is_none()
        {
            break;
        }
    }

    let more_matches_available = matching_items.len() > params.page_size;
    matching_items.truncate(params.page_size);
    let next_cursor = if more_matches_available {
        Some(
            matching_items
                .last()
                .and_then(|item| {
                    cursor_from_thread_search_item(item, params.sort_key, storage_path)
                })
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "failed to construct required thread search cursor".to_string(),
                })?,
        )
    } else {
        None
    }
    .as_ref()
    .map(|cursor| encode_rollout_cursor(storage_path, cursor))
    .transpose()?;

    let (names, resolved_title_ids) =
        thread_item_titles(matching_items.iter().map(|item| &item.item));
    let mut items = Vec::with_capacity(matching_items.len());
    for item in matching_items {
        let thread = if let Some(metadata) = item.metadata {
            super::read_thread::stored_thread_from_sqlite_metadata_with_name(store, metadata, None)
                .await?
        } else {
            super::read_thread::read_thread_by_rollout_path(store, item.item.path, true, false)
                .await?
        };
        items.push(StoredThreadSearchResult {
            thread,
            snippet: item.snippet,
        });
    }
    set_thread_search_result_names(store, &mut items, names, resolved_title_ids).await;

    Ok(ThreadSearchPage { items, next_cursor })
}

fn cursor_from_thread_search_item(
    item: &ThreadSearchItem,
    sort_key: ThreadSortKey,
    storage_path: crate::types::ThreadListStoragePath,
) -> Option<codex_rollout::Cursor> {
    let physical_created_at = (storage_path == crate::types::ThreadListStoragePath::ScanAndRepair)
        .then(|| codex_rollout::rollout_created_at_key(&item.item.path))
        .flatten();
    let timestamp = match sort_key {
        ThreadSortKey::CreatedAt => physical_created_at
            .as_deref()
            .or(item.item.created_at.as_deref())?,
        ThreadSortKey::UpdatedAt => item
            .item
            .updated_at
            .as_deref()
            .or(item.item.created_at.as_deref())?,
        ThreadSortKey::RecencyAt => item
            .item
            .recency_at
            .as_deref()
            .or(item.item.updated_at.as_deref())
            .or(item.item.created_at.as_deref())?,
    };
    parse_cursor(&format!("{timestamp}|{}", item.item.thread_id?))
}

async fn set_thread_search_result_names(
    store: &LocalThreadStore,
    items: &mut [StoredThreadSearchResult],
    mut names: HashMap<ThreadId, String>,
    resolved_title_ids: HashSet<ThreadId>,
) {
    let thread_ids = items
        .iter()
        .map(|item| item.thread.thread_id)
        .collect::<HashSet<_>>();
    let mut unresolved_thread_ids = thread_ids
        .difference(&resolved_title_ids)
        .copied()
        .collect::<HashSet<_>>();
    if !unresolved_thread_ids.is_empty()
        && let Some(state_db_ctx) = store.state_db().await
        && let Ok(titles) = state_db_ctx
            .get_thread_titles(&unresolved_thread_ids.iter().copied().collect::<Vec<_>>())
            .await
    {
        for (thread_id, title, first_user_message) in titles {
            if let Some(title) = distinct_thread_title(&title, first_user_message.as_deref()) {
                unresolved_thread_ids.remove(&thread_id);
                names.insert(thread_id, title);
            }
        }
    }
    if !unresolved_thread_ids.is_empty()
        && let Ok(legacy_names) =
            find_thread_names_by_ids(store.config.codex_home.as_path(), &unresolved_thread_ids)
                .await
    {
        for (thread_id, title) in legacy_names {
            names.entry(thread_id).or_insert(title);
        }
    }
    for item in items {
        if let Some(title) = names.get(&item.thread.thread_id).cloned() {
            set_thread_name_from_title(&mut item.thread, title);
        }
    }
}
