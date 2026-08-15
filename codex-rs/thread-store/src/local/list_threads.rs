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
use crate::ListThreadsParams;
use crate::SortDirection;
use crate::StoredThread;
use crate::ThreadListStorageMode;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::types::ThreadListStoragePath;

const THREAD_LIST_CURSOR_PREFIX: &str = "thread-list-v1:";
const STATE_DB_CURSOR_PREFIX: &str = "state-db:";
const SCAN_AND_REPAIR_CURSOR_PREFIX: &str = "scan-and-repair:";

struct BoundThreadListCursor {
    storage_path: ThreadListStoragePath,
    position: codex_rollout::Cursor,
}

pub(super) async fn list_threads(
    store: &LocalThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<ThreadPage> {
    let cursor = params
        .cursor
        .as_deref()
        .map(parse_bound_cursor)
        .transpose()?;
    let state_db = store.state_db().await;
    let rollout_config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite_home: store.config.sqlite_home.clone(),
        cwd: store.config.codex_home.clone(),
        model_provider_id: store.config.default_model_provider_id.clone(),
        generate_memories: false,
    };
    let (page, storage_path) = select_rollout_threads(
        state_db,
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
    let (mut names, resolved_title_ids) = thread_item_titles(&page.items);
    let mut items = page
        .items
        .into_iter()
        .filter_map(|item| {
            stored_thread_from_rollout_item(
                item,
                params.archived,
                store.config.default_model_provider_id.as_str(),
            )
        })
        .collect::<Vec<_>>();

    let thread_ids = items
        .iter()
        .map(|thread| thread.thread_id)
        .collect::<HashSet<_>>();
    let mut unresolved_thread_ids = thread_ids
        .difference(&resolved_title_ids)
        .copied()
        .collect::<HashSet<_>>();
    if let Some(state_db_ctx) = store.state_db().await {
        for thread_id in unresolved_thread_ids.clone() {
            let Ok(Some(metadata)) = state_db_ctx.get_thread(thread_id).await else {
                continue;
            };
            if let Some(title) = distinct_thread_metadata_title(&metadata) {
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
    for thread in &mut items {
        if let Some(title) = names.get(&thread.thread_id).cloned() {
            set_thread_name_from_title(thread, title);
        }
    }

    let backwards_cursor = items
        .first()
        .and_then(|thread| {
            backwards_cursor_position(thread, params.sort_key, params.sort_direction)
        })
        .map(|position| bind_cursor(storage_path, &position));

    Ok(ThreadPage {
        items,
        next_cursor,
        backwards_cursor,
    })
}

async fn select_rollout_threads(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&BoundThreadListCursor>,
) -> ThreadStoreResult<(codex_rollout::ThreadsPage, ThreadListStoragePath)> {
    let selected_path = storage_path_for_request(params, cursor)?;
    if selected_path == Some(ThreadListStoragePath::StateDb) {
        let page = list_rollout_threads_for_storage(
            state_db,
            config,
            default_model_provider_id,
            params,
            cursor.map(|cursor| &cursor.position),
            ThreadListStoragePath::StateDb,
        )
        .await?;
        return Ok((page, ThreadListStoragePath::StateDb));
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
        return Ok((page, ThreadListStoragePath::ScanAndRepair));
    }

    match list_rollout_threads_for_storage(
        state_db.clone(),
        config,
        default_model_provider_id,
        params,
        /*cursor*/ None,
        ThreadListStoragePath::StateDb,
    )
    .await
    {
        Ok(page) => Ok((page, ThreadListStoragePath::StateDb)),
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
            Ok((page, ThreadListStoragePath::ScanAndRepair))
        }
        Err(err) => Err(err),
    }
}

pub(super) async fn list_rollout_threads_for_storage(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    storage_path: ThreadListStoragePath,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    let sort_key = rollout_sort_key(params.sort_key);
    let sort_direction = rollout_sort_direction(params.sort_direction);
    if let Some(relation_filter) = params.relation_filter {
        if storage_path != ThreadListStoragePath::StateDb {
            return Err(ThreadStoreError::InvalidRequest {
                message: "relationship-filtered thread listing requires state DB storage"
                    .to_string(),
            });
        }
        let relation_filter = match relation_filter {
            ThreadRelationFilter::DirectChildrenOf(parent_thread_id) => {
                codex_state::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)
            }
            ThreadRelationFilter::DescendantsOf(ancestor_thread_id) => {
                codex_state::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)
            }
        };
        let page = codex_rollout::state_db::list_threads_db(
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

fn rollout_sort_key(sort_key: ThreadSortKey) -> codex_rollout::ThreadSortKey {
    match sort_key {
        ThreadSortKey::CreatedAt => codex_rollout::ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt => codex_rollout::ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt => codex_rollout::ThreadSortKey::RecencyAt,
    }
}

fn rollout_sort_direction(sort_direction: SortDirection) -> codex_rollout::SortDirection {
    match sort_direction {
        SortDirection::Asc => codex_rollout::SortDirection::Asc,
        SortDirection::Desc => codex_rollout::SortDirection::Desc,
    }
}

fn storage_path_for_request(
    params: &ListThreadsParams,
    cursor: Option<&BoundThreadListCursor>,
) -> ThreadStoreResult<Option<ThreadListStoragePath>> {
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

    if params.relation_filter.is_some() {
        return Ok(Some(ThreadListStoragePath::StateDb));
    }
    Ok(match params.storage_mode {
        ThreadListStorageMode::PreferStateDb => None,
        ThreadListStorageMode::StateDbOnly => Some(ThreadListStoragePath::StateDb),
        ThreadListStorageMode::ScanAndRepair => Some(ThreadListStoragePath::ScanAndRepair),
    })
}

fn parse_bound_cursor(token: &str) -> ThreadStoreResult<BoundThreadListCursor> {
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

fn encode_rollout_cursor(
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadHistoryMode;
    use pretty_assertions::assert_eq;
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::ThreadStore;
    use crate::local::LocalThreadStore;
    use crate::local::test_support::test_config;
    use crate::local::test_support::write_archived_session_file;
    use crate::local::test_support::write_session_file;
    use crate::local::test_support::write_session_file_with;

    fn list_params(
        page_size: usize,
        cursor: Option<String>,
        storage_mode: ThreadListStorageMode,
    ) -> ListThreadsParams {
        ListThreadsParams {
            page_size,
            cursor,
            sort_key: ThreadSortKey::CreatedAt,
            sort_direction: SortDirection::Desc,
            allowed_sources: Vec::new(),
            model_providers: None,
            cwd_filters: None,
            archived: false,
            search_term: None,
            relation_filter: None,
            storage_mode,
        }
    }

    async fn backfilled_runtime(
        home: &TempDir,
        config: &crate::LocalThreadStoreConfig,
    ) -> codex_rollout::StateDbHandle {
        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        let rollout_config = RolloutConfig {
            codex_home: config.codex_home.clone(),
            sqlite_home: config.sqlite_home.clone(),
            cwd: config.codex_home.clone(),
            model_provider_id: config.default_model_provider_id.clone(),
            generate_memories: false,
        };
        RolloutRecorder::list_threads(
            Some(runtime.clone()),
            &rollout_config,
            /*page_size*/ 100,
            /*cursor*/ None,
            codex_rollout::ThreadSortKey::CreatedAt,
            codex_rollout::SortDirection::Desc,
            &[],
            /*model_providers*/ None,
            /*cwd_filters*/ None,
            config.default_model_provider_id.as_str(),
            /*search_term*/ None,
        )
        .await
        .expect("rollout scan should backfill state db");
        runtime
    }

    #[tokio::test]
    async fn prefer_state_db_binds_and_resumes_sqlite_pagination() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(201))
            .expect("newest session file");
        write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(202))
            .expect("older session file");
        let runtime = backfilled_runtime(&home, &config).await;
        let store = LocalThreadStore::new(config, Some(runtime));

        let first = store
            .list_threads(list_params(1, None, ThreadListStorageMode::PreferStateDb))
            .await
            .expect("first DB page");
        let cursor = first.next_cursor.clone().expect("DB cursor");
        assert!(cursor.starts_with("thread-list-v1:state-db:"));
        assert!(
            first
                .backwards_cursor
                .as_deref()
                .is_some_and(|cursor| cursor.starts_with("thread-list-v1:state-db:"))
        );

        let second = store
            .list_threads(list_params(
                1,
                Some(cursor),
                ThreadListStorageMode::PreferStateDb,
            ))
            .await
            .expect("second DB page");
        assert_ne!(first.items[0].thread_id, second.items[0].thread_id);
        assert!(
            second
                .backwards_cursor
                .as_deref()
                .is_some_and(|cursor| cursor.starts_with("thread-list-v1:state-db:"))
        );
    }

    #[tokio::test]
    async fn sqlite_cursor_does_not_fallback_after_db_failure() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(211))
            .expect("newest session file");
        write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(212))
            .expect("older session file");
        let runtime = backfilled_runtime(&home, &config).await;
        let store = LocalThreadStore::new(config, Some(runtime.clone()));
        let first = store
            .list_threads(list_params(1, None, ThreadListStorageMode::PreferStateDb))
            .await
            .expect("first DB page");
        let cursor = first.next_cursor.expect("DB cursor");

        runtime.close().await;
        let err = store
            .list_threads(list_params(
                1,
                Some(cursor),
                ThreadListStorageMode::PreferStateDb,
            ))
            .await
            .expect_err("closed DB must not fall back to JSONL");
        assert!(matches!(err, ThreadStoreError::Internal { .. }));
    }

    #[tokio::test]
    async fn unavailable_initial_db_falls_back_and_scan_cursor_stays_on_scan_path() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(221))
            .expect("newest session file");
        write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(222))
            .expect("older session file");
        let scan_store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
        let first = scan_store
            .list_threads(list_params(1, None, ThreadListStorageMode::PreferStateDb))
            .await
            .expect("initial scan fallback");
        let cursor = first.next_cursor.expect("scan cursor");
        assert!(cursor.starts_with("thread-list-v1:scan-and-repair:"));
        assert!(
            first
                .backwards_cursor
                .as_deref()
                .is_some_and(|cursor| cursor.starts_with("thread-list-v1:scan-and-repair:"))
        );

        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        runtime.close().await;
        let store_with_unavailable_db = LocalThreadStore::new(config, Some(runtime));
        let second = store_with_unavailable_db
            .list_threads(list_params(
                1,
                Some(cursor),
                ThreadListStorageMode::PreferStateDb,
            ))
            .await
            .expect("scan cursor should remain scan-backed");
        assert_eq!(second.items.len(), 1);
    }

    #[tokio::test]
    async fn successful_empty_db_page_does_not_trigger_scan_fallback() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(231))
            .expect("session file visible only to scan");
        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        let store = LocalThreadStore::new(config, Some(runtime));

        let page = store
            .list_threads(list_params(10, None, ThreadListStorageMode::PreferStateDb))
            .await
            .expect("empty DB listing");
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn explicit_modes_and_cursor_conflicts_are_enforced() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        write_session_file(home.path(), "2025-01-03T12-00-00", Uuid::from_u128(241))
            .expect("newest session file");
        write_session_file(home.path(), "2025-01-02T12-00-00", Uuid::from_u128(242))
            .expect("older session file");
        let scan_store = LocalThreadStore::new(config.clone(), /*state_db*/ None);
        let scan_page = scan_store
            .list_threads(list_params(1, None, ThreadListStorageMode::ScanAndRepair))
            .await
            .expect("explicit scan listing");
        let scan_cursor = scan_page.next_cursor.expect("scan cursor");
        let err = scan_store
            .list_threads(list_params(
                1,
                Some(scan_cursor),
                ThreadListStorageMode::StateDbOnly,
            ))
            .await
            .expect_err("scan cursor must reject explicit DB mode");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));

        let err = scan_store
            .list_threads(list_params(1, None, ThreadListStorageMode::StateDbOnly))
            .await
            .expect_err("explicit DB mode must surface unavailable DB");
        assert!(matches!(err, ThreadStoreError::Internal { .. }));

        let runtime = backfilled_runtime(&home, &config).await;
        let db_store = LocalThreadStore::new(config, Some(runtime));
        let db_page = db_store
            .list_threads(list_params(1, None, ThreadListStorageMode::StateDbOnly))
            .await
            .expect("explicit DB listing");
        let db_cursor = db_page.next_cursor.expect("DB cursor");
        let err = db_store
            .list_threads(list_params(
                1,
                Some(db_cursor),
                ThreadListStorageMode::ScanAndRepair,
            ))
            .await
            .expect_err("DB cursor must reject explicit scan mode");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn relationship_filters_reject_scan_storage_and_require_db() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let parent_thread_id = ThreadId::new();

        let mut params = list_params(10, None, ThreadListStorageMode::ScanAndRepair);
        params.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id));
        let err = store
            .list_threads(params)
            .await
            .expect_err("relationship filter must reject explicit scan mode");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));

        let mut params = list_params(
            10,
            Some("thread-list-v1:scan-and-repair:2025-01-03T12:00:00Z".to_string()),
            ThreadListStorageMode::PreferStateDb,
        );
        params.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id));
        let err = store
            .list_threads(params)
            .await
            .expect_err("relationship filter must reject scan cursor");
        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));

        let mut params = list_params(10, None, ThreadListStorageMode::PreferStateDb);
        params.relation_filter = Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id));
        let err = store
            .list_threads(params)
            .await
            .expect_err("relationship filter must surface unavailable DB");
        assert!(matches!(err, ThreadStoreError::Internal { .. }));
    }

    #[tokio::test]
    async fn list_threads_uses_default_provider_when_rollout_omits_provider() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        write_session_file_with(
            home.path(),
            home.path().join("sessions/2025/01/03"),
            "2025-01-03T12-00-00",
            Uuid::from_u128(102),
            "Hello from user",
            /*model_provider*/ None,
            ThreadHistoryMode::Legacy,
        )
        .expect("session file");

        let page = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::ScanAndRepair,
            })
            .await
            .expect("thread listing");

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].model_provider, "test-provider");
    }

    #[tokio::test]
    async fn list_threads_preserves_sqlite_title_search_results() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let uuid = Uuid::from_u128(103);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path = home.path().join("rollout-title-search.jsonl");
        fs::write(&rollout_path, "").expect("placeholder rollout file");

        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        let store = LocalThreadStore::new(config.clone(), Some(runtime.clone()));
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        let created_at = Utc::now();
        let mut builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            rollout_path,
            created_at,
            SessionSource::Cli,
        );
        builder.model_provider = Some(config.default_model_provider_id.clone());
        builder.cwd = home.path().to_path_buf();
        builder.cli_version = Some("test_version".to_string());
        let mut metadata = builder.build(config.default_model_provider_id.as_str());
        metadata.title = "needle title".to_string();
        metadata.first_user_message = Some("plain preview".to_string());
        metadata.preview = metadata.first_user_message.clone();
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("state db upsert should succeed");

        let page = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: Some("needle".to_string()),
                relation_filter: None,
                storage_mode: ThreadListStorageMode::StateDbOnly,
            })
            .await
            .expect("thread listing");

        let ids = page
            .items
            .iter()
            .map(|item| item.thread_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![thread_id]);
        assert_eq!(
            page.items[0].first_user_message.as_deref(),
            Some("plain preview")
        );
        assert_eq!(page.items[0].name.as_deref(), Some("needle title"));
    }

    #[tokio::test]
    async fn list_threads_falls_back_to_legacy_name_for_default_sqlite_title() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let uuid = Uuid::from_u128(104);
        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        let rollout_path =
            write_session_file(home.path(), "2025-01-03T12-30-00", uuid).expect("session file");
        codex_rollout::append_thread_name(home.path(), thread_id, "Legacy chosen name")
            .await
            .expect("write legacy name");

        let runtime = codex_state::StateRuntime::init(
            home.path().to_path_buf(),
            config.default_model_provider_id.clone(),
        )
        .await
        .expect("state db should initialize");
        runtime
            .mark_backfill_complete(/*last_watermark*/ None)
            .await
            .expect("backfill should be complete");
        let mut builder = codex_state::ThreadMetadataBuilder::new(
            thread_id,
            rollout_path,
            Utc::now(),
            SessionSource::Cli,
        );
        builder.model_provider = Some(config.default_model_provider_id.clone());
        builder.cwd = home.path().to_path_buf();
        builder.cli_version = Some("test_version".to_string());
        let mut metadata = builder.build(config.default_model_provider_id.as_str());
        metadata.title = "Hello from user".to_string();
        metadata.first_user_message = Some("Hello from user".to_string());
        metadata.preview = metadata.first_user_message.clone();
        runtime
            .upsert_thread(&metadata)
            .await
            .expect("state db upsert should succeed");
        let store = LocalThreadStore::new(config, Some(runtime));

        let page = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::StateDbOnly,
            })
            .await
            .expect("thread listing");

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].thread_id, thread_id);
        assert_eq!(page.items[0].name.as_deref(), Some("Legacy chosen name"));
    }

    #[tokio::test]
    async fn list_threads_selects_active_or_archived_collection() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);
        let active_uuid = Uuid::from_u128(105);
        let archived_uuid = Uuid::from_u128(106);
        write_session_file(home.path(), "2025-01-03T12-00-00", active_uuid)
            .expect("active session file");
        write_archived_session_file(home.path(), "2025-01-03T13-00-00", archived_uuid)
            .expect("archived session file");

        let active = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::ScanAndRepair,
            })
            .await
            .expect("active listing");
        let archived = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: true,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::ScanAndRepair,
            })
            .await
            .expect("archived listing");

        let active_id = ThreadId::from_string(&active_uuid.to_string()).expect("valid thread id");
        let archived_id =
            ThreadId::from_string(&archived_uuid.to_string()).expect("valid thread id");
        assert_eq!(
            active
                .items
                .iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![active_id]
        );
        assert_eq!(
            archived
                .items
                .iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![archived_id]
        );
        assert_eq!(active.items[0].archived_at, None);
        assert_eq!(
            archived.items[0].archived_at,
            Some(archived.items[0].updated_at)
        );
    }

    #[tokio::test]
    async fn list_threads_returns_local_rollout_summary() {
        let home = TempDir::new().expect("temp dir");
        let config = test_config(home.path());
        let store = LocalThreadStore::new(config, /*state_db*/ None);
        let uuid = Uuid::from_u128(101);
        let path =
            write_session_file(home.path(), "2025-01-03T12-00-00", uuid).expect("session file");

        let page = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: vec![SessionSource::Cli],
                model_providers: Some(vec!["test-provider".to_string()]),
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::ScanAndRepair,
            })
            .await
            .expect("thread listing");

        let thread_id = ThreadId::from_string(&uuid.to_string()).expect("valid thread id");
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].thread_id, thread_id);
        assert_eq!(page.items[0].rollout_path, Some(path));
        assert_eq!(page.items[0].preview, "Hello from user");
        assert_eq!(
            page.items[0].first_user_message.as_deref(),
            Some("Hello from user")
        );
        assert_eq!(page.items[0].model_provider, "test-provider");
        assert_eq!(page.items[0].cli_version, "test_version");
        assert_eq!(page.items[0].source, SessionSource::Cli);
    }

    #[tokio::test]
    async fn list_threads_rejects_invalid_cursor() {
        let home = TempDir::new().expect("temp dir");
        let store = LocalThreadStore::new(test_config(home.path()), /*state_db*/ None);

        let err = store
            .list_threads(ListThreadsParams {
                page_size: 10,
                cursor: Some("not-a-cursor".to_string()),
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                storage_mode: ThreadListStorageMode::ScanAndRepair,
            })
            .await
            .expect_err("invalid cursor should fail");

        assert!(matches!(err, ThreadStoreError::InvalidRequest { .. }));
    }
}
