use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

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
    let mut observation =
        tracing::enabled!(target: "codex_storage_search_phases", tracing::Level::TRACE)
            .then(SearchObservation::new);
    let result = search_threads_inner(store, params, &mut observation).await;
    if let Some(observation) = observation {
        observation.emit(&result);
    }
    result
}

struct SearchObservation {
    started: Instant,
    phase_started: Option<Instant>,
    phases_us: [u64; 4],
    matched_rollouts: u64,
    reconciled_rollouts: u64,
}

impl SearchObservation {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            phase_started: None,
            phases_us: [0; 4],
            matched_rollouts: 0,
            reconciled_rollouts: 0,
        }
    }

    fn advance(observation: &mut Option<Self>, completed_phase: Option<usize>) {
        if let Some(observation) = observation {
            let now = Instant::now();
            if let Some(index) = completed_phase
                && let Some(started) = observation.phase_started
            {
                observation.phases_us[index] = now.duration_since(started).as_micros() as u64;
            }
            observation.phase_started = Some(now);
        }
    }

    fn emit(self, result: &ThreadStoreResult<ThreadSearchPage>) {
        let [content_search_us, reconciliation_us, selection_us, hydration_us] = self.phases_us;
        tracing::trace!(target: "codex_storage_search_phases",
            stage = "local_thread_search", success = result.is_ok(),
            elapsed_us = self.started.elapsed().as_micros() as u64,
            content_search_us, reconciliation_us, selection_us, hydration_us,
            matched_rollouts = self.matched_rollouts, reconciled_rollouts = self.reconciled_rollouts,
            selected_rows = result.as_ref().map_or(0, |page| page.items.len()) as u64);
    }
}

async fn search_threads_inner(
    store: &LocalThreadStore,
    params: SearchThreadsParams,
    observation: &mut Option<SearchObservation>,
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
    SearchObservation::advance(observation, /*completed_phase*/ None);
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
    SearchObservation::advance(observation, Some(0));
    if let Some(observation) = observation.as_mut() {
        observation.matched_rollouts = matching_rollouts.len() as u64;
    }
    if matching_rollouts.is_empty() {
        return Ok(ThreadSearchPage {
            items: Vec::new(),
            next_cursor: None,
        });
    }
    if state_db.is_none() {
        // Validate in place rather than clone every path before the async reads.
        // The producer only emits nonempty snippets, so an empty snippet marks
        // source exclusions without conflating them with metadata-only hits.
        for (path, snippet) in &mut matching_rollouts {
            if let Some(item) = codex_rollout::read_thread_item_from_rollout(path.clone()).await {
                if !params.allowed_sources.is_empty()
                    && item
                        .source
                        .as_ref()
                        .is_none_or(|source| !params.allowed_sources.contains(source))
                {
                    *snippet = Some(String::new());
                }
            } else {
                if let Ok(header) = codex_rollout::read_session_meta_line(path).await {
                    if !params.allowed_sources.is_empty()
                        && !params.allowed_sources.contains(&header.meta.source)
                    {
                        *snippet = Some(String::new());
                        continue;
                    }
                    // Valid empty-preview archives are listable; snippetless hits
                    // have no visible result for either discovery policy.
                    if params.archived || snippet.is_none() {
                        continue;
                    }
                }
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "search catalog cannot represent a confirmed visible match at {}",
                        path.display()
                    ),
                });
            }
        }
        if !params.allowed_sources.is_empty() {
            matching_rollouts.retain(|_, snippet| snippet.as_deref() != Some(""));
        }
    }
    if let Some(runtime) = state_db.as_deref() {
        // These paths were already discovered by content search. Missing or
        // stale index rows must not silently exclude canonical matches.
        for (path, snippet) in &matching_rollouts {
            if let Some(observation) = observation.as_mut() {
                observation.reconciled_rollouts += 1;
            }
            let (reconciled, row_observation) = codex_rollout::state_integration::reconcile_search_rollout(
                runtime,
                path,
                &store.config.default_model_provider_id,
                params.archived,
                observation.is_some(),
            )
            .await;
            if !reconciled {
                if let Some(row_observation) = row_observation {
                    row_observation.emit(false, /*thread_id*/ None);
                }
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "search index reconciliation is incomplete for {}; retry after storage recovers",
                        path.display()
                    ),
                });
            }
            let identity = codex_rollout::verified_rollout_thread_id(path).await;
            if let Some(row_observation) = row_observation {
                row_observation.emit(true, identity.as_ref().ok().copied());
            }
            let id = identity.map_err(|error| ThreadStoreError::Internal {
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
            // Archived discovery accepts empty previews. A metadata-only hit has no
            // visible result to represent, even under the active preview policy.
            if eligible
                && !params.archived
                && snippet.is_some()
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
    SearchObservation::advance(observation, Some(1));
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

    // Unconsumed candidates are no longer needed while hydrating this page.
    drop(remaining_rollouts);

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
    SearchObservation::advance(observation, Some(2));

    let (names, resolved_title_ids) =
        thread_item_titles(matching_items.iter().map(|item| &item.item));
    let mut items = Vec::with_capacity(matching_items.len());
    for item in matching_items {
        let backwards_cursor = cursor_from_thread_search_item(&item, params.sort_key, storage_path)
            .and_then(|cursor| serde_json::to_value(cursor).ok())
            .and_then(|value| value.as_str().map(str::to_owned))
            .and_then(|position| chrono::DateTime::parse_from_rfc3339(position.split('|').next()?).ok())
            .and_then(|timestamp| super::list_threads::backwards_cursor_position(
                timestamp.with_timezone(&chrono::Utc), params.sort_direction))
            .map(|position| super::list_threads::bind_cursor(storage_path, &position));
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
            backwards_cursor,
        });
    }
    set_thread_search_result_names(store, &mut items, names, resolved_title_ids).await?;
    SearchObservation::advance(observation, Some(3));

    Ok(ThreadSearchPage { items, next_cursor })
}

fn cursor_from_thread_search_item(
    item: &ThreadSearchItem,
    sort_key: ThreadSortKey,
    storage_path: crate::types::ThreadListStoragePath,
) -> Option<codex_rollout::Cursor> {
    if storage_path == crate::types::ThreadListStoragePath::StateDb
        && let Some(metadata) = &item.metadata
    {
        let timestamp = match sort_key {
            ThreadSortKey::CreatedAt => metadata.created_at,
            ThreadSortKey::UpdatedAt => metadata.updated_at,
            ThreadSortKey::RecencyAt => metadata.recency_at,
        };
        return parse_cursor(&format!("{}|{}", timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), metadata.id));
    }
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
) -> ThreadStoreResult<()> {
    let thread_ids = items
        .iter()
        .map(|item| item.thread.thread_id)
        .collect::<HashSet<_>>();
    let mut unresolved_thread_ids = thread_ids
        .difference(&resolved_title_ids)
        .copied()
        .collect::<HashSet<_>>();
    if !thread_ids.is_empty()
        && let Some(state_db_ctx) = store.state_db().await
    {
        let titles = state_db_ctx
            .get_thread_titles(&thread_ids.iter().copied().collect::<Vec<_>>())
            .await
            .map_err(|error| ThreadStoreError::Internal {
                message: format!("failed to read canonical search names: {error}"),
            })?;
        for (thread_id, title, first_user_message, title_is_explicit) in titles {
            if title_is_explicit {
                unresolved_thread_ids.remove(&thread_id);
                names.remove(&thread_id);
                if !title.trim().is_empty() {
                    names.insert(thread_id, title.trim().to_owned());
                }
                continue;
            }
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
        if !unresolved_thread_ids.contains(&item.thread.thread_id) {
            // Resolved includes authoritative clears: remove a name observed in
            // an older projection instead of only overwriting nonempty names.
            item.thread.name = names.get(&item.thread.thread_id).cloned();
        } else if let Some(title) = names.get(&item.thread.thread_id).cloned() {
            set_thread_name_from_title(&mut item.thread, title);
        }
    }
    Ok(())
}
