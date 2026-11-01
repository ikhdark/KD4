//! Thread catalog selection and filesystem/SQLite projection for the recorder.

use super::RolloutRecorder;
use crate::ARCHIVED_SESSIONS_SUBDIR;
use crate::config::RolloutConfigView;
use crate::list::Cursor;
use crate::list::SortDirection;
use crate::list::ThreadItem;
use crate::list::ThreadListConfig;
use crate::list::ThreadListLayout;
use crate::list::ThreadSortKey;
use crate::list::ThreadsPage;
use crate::list::get_threads;
use crate::list::get_threads_ascending;
use crate::list::get_threads_in_root;
use crate::list::get_threads_in_root_ascending;
use crate::list::thread_item_sort_key;
use crate::session_index::find_thread_names_by_ids;
use crate::state_integration;
use crate::state_integration::StateDbHandle;
use chrono::SecondsFormat;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_state::StateRuntime;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

#[derive(Clone, Copy)]
enum ThreadListArchiveFilter {
    Active,
    Archived,
}

#[derive(Clone, Copy)]
enum ThreadListRepairMode {
    ScanAndRepair,
    StateDbOnly,
}

impl RolloutRecorder {
    /// List threads (rollout files) under the provided Codex home directory.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_threads(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Active,
            ThreadListRepairMode::ScanAndRepair,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_threads_from_state_db(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Active,
            ThreadListRepairMode::StateDbOnly,
            search_term,
        )
        .await
    }

    /// List archived threads (rollout files) under the archived sessions directory.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_archived_threads(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Archived,
            ThreadListRepairMode::ScanAndRepair,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_archived_threads_from_state_db(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            state_db_ctx,
            config,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
            ThreadListArchiveFilter::Archived,
            ThreadListRepairMode::StateDbOnly,
            search_term,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_threads_with_db_fallback(
        state_db_ctx: Option<StateDbHandle>,
        config: &impl RolloutConfigView,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        sort_direction: SortDirection,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        cwd_filters: Option<&[PathBuf]>,
        default_provider: &str,
        archive_filter: ThreadListArchiveFilter,
        repair_mode: ThreadListRepairMode,
        search_term: Option<&str>,
    ) -> std::io::Result<ThreadsPage> {
        let codex_home = config.codex_home();
        let archived = match archive_filter {
            ThreadListArchiveFilter::Active => false,
            ThreadListArchiveFilter::Archived => true,
        };
        if cwd_filters.is_some_and(<[std::path::PathBuf]>::is_empty) {
            return Ok(ThreadsPage::default());
        }

        if matches!(repair_mode, ThreadListRepairMode::StateDbOnly) {
            let page = state_integration::list_threads_db(
                state_db_ctx.as_deref(),
                codex_home,
                page_size,
                cursor,
                sort_key,
                sort_direction,
                allowed_sources,
                model_providers,
                cwd_filters,
                /*relation_filter*/ None,
                archived,
                /*project_id*/ None,
                search_term,
            )
            .await
            .ok_or_else(|| std::io::Error::other("state DB unavailable for thread listing"))?;
            return Ok(page.into());
        }

        let listing_has_metadata_filters = !allowed_sources.is_empty()
            || model_providers.is_some()
            || cwd_filters.is_some()
            || search_term.is_some();
        // Filesystem-first listing intentionally overfetches so we can repair stale/missing
        // SQLite rows while keeping the returned page and cursor filesystem-backed.
        let fs_page = match sort_direction {
            SortDirection::Asc => {
                list_threads_from_files_asc(
                    state_db_ctx.as_deref(),
                    codex_home,
                    page_size,
                    cursor,
                    sort_key,
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    archived,
                    search_term,
                )
                .await?
            }
            SortDirection::Desc => {
                list_threads_from_files_desc(
                    state_db_ctx.as_deref(),
                    codex_home,
                    if state_db_ctx.is_some() {
                        page_size.saturating_mul(2)
                    } else {
                        page_size
                    },
                    cursor,
                    sort_key,
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    archived,
                    search_term,
                )
                .await?
            }
        };

        if state_db_ctx.is_none() {
            // Keep legacy behavior when SQLite is unavailable: return filesystem results
            // at the requested page size.
            codex_state::record_fallback(
                "list_threads",
                "db_unavailable",
                /*telemetry_override*/ None,
            );
            return Ok(page_from_filesystem_scan(
                fs_page,
                sort_direction,
                page_size,
                sort_key,
            ));
        }

        // Track filesystem IDs so the later DB comparison only triggers full reconciliation for
        // DB-only hits.
        let fs_page_thread_ids = fs_page
            .items
            .iter()
            .filter_map(|item| item.thread_id)
            .collect::<HashSet<_>>();

        // Reconcile each filesystem hit once, then use SQLite as the authoritative projection.
        // Filesystem filtering already reduced the complete persisted-settings history, so the
        // same current provider/cwd facts drive both candidate selection and the database row.
        let mut reconciled_ids = HashSet::new();
        for item in &fs_page.items {
            if state_integration::reconcile_rollout(
                state_db_ctx.as_deref(),
                item.path.as_path(),
                default_provider,
                /*builder*/ None,
                &[],
                Some(archived),
            )
            .await
            {
                reconciled_ids.extend(item.thread_id);
            }
        }

        if !listing_has_metadata_filters {
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            return Ok(overlay_thread_item_metadata_from_state_db(
                state_db_ctx.as_deref(),
                page,
                &reconciled_ids,
            )
            .await);
        }

        let db_page = state_integration::list_threads_db(
            state_db_ctx.as_deref(),
            codex_home,
            page_size,
            cursor,
            sort_key,
            sort_direction,
            allowed_sources,
            model_providers,
            cwd_filters,
            /*relation_filter*/ None,
            archived,
            /*project_id*/ None,
            search_term,
        )
        .await;
        if let Some(db_page) = db_page {
            // Filesystem hits were reconciled above. DB-only hits can still be
            // stale metadata matches and need the same repair in every branch.
            for item in &db_page.items {
                if !Self::db_hit_needs_reconciliation(&fs_page_thread_ids, item.id) {
                    continue;
                }
                state_integration::reconcile_rollout(
                    state_db_ctx.as_deref(),
                    item.rollout_path.as_path(),
                    default_provider,
                    /*builder*/ None,
                    &[],
                    Some(archived),
                )
                .await;
            }
            if !(search_term.is_some() && (!db_page.items.is_empty() || cursor.is_some()))
                && sort_key != ThreadSortKey::RecencyAt
            {
                codex_state::record_fallback(
                    "list_threads",
                    "metadata_filter",
                    /*telemetry_override*/ None,
                );
            }
            let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
            return Ok(overlay_thread_item_metadata_from_state_db(
                state_db_ctx.as_deref(),
                page,
                &reconciled_ids,
            )
            .await);
        }
        // SQLite listing failed and list_threads_db already logged why. Return the
        // filesystem page rather than failing the list.
        let page = page_from_filesystem_scan(fs_page, sort_direction, page_size, sort_key);
        codex_state::record_fallback("list_threads", "db_error", /*telemetry_override*/ None);
        Ok(overlay_thread_item_metadata_from_state_db(
            state_db_ctx.as_deref(),
            page,
            &reconciled_ids,
        )
        .await)
    }

    fn db_hit_needs_reconciliation(
        filesystem_thread_ids: &HashSet<ThreadId>,
        db_thread_id: ThreadId,
    ) -> bool {
        !filesystem_thread_ids.contains(&db_thread_id)
    }
}

fn truncate_fs_page(
    mut page: ThreadsPage,
    page_size: usize,
    sort_key: ThreadSortKey,
) -> ThreadsPage {
    if page.items.len() <= page_size {
        return page;
    }
    page.items.truncate(page_size);
    page.next_cursor = page
        .items
        .last()
        .and_then(|item| cursor_from_thread_item(item, sort_key));
    page
}

fn page_from_filesystem_scan(
    page: ThreadsPage,
    sort_direction: SortDirection,
    page_size: usize,
    sort_key: ThreadSortKey,
) -> ThreadsPage {
    match sort_direction {
        SortDirection::Asc => page,
        SortDirection::Desc => truncate_fs_page(page, page_size, sort_key),
    }
}

async fn overlay_thread_item_metadata_from_state_db(
    state_db_ctx: Option<&StateRuntime>,
    mut page: ThreadsPage,
    reconciled_ids: &HashSet<ThreadId>,
) -> ThreadsPage {
    let Some(state_db_ctx) = state_db_ctx else {
        return page;
    };

    for item in &mut page.items {
        let Some(thread_id) = item.thread_id else {
            continue;
        };
        let metadata = match state_db_ctx.get_thread(thread_id).await {
            Ok(Some(metadata)) => metadata,
            Ok(None) => continue,
            Err(err) => {
                warn!(
                    "state db get_thread failed while overlaying filesystem scan thread metadata: {err}"
                );
                continue;
            }
        };
        if !reconciled_ids.contains(&thread_id) {
            if metadata.title_is_explicit {
                item.title = metadata.thread_name().map(str::to_owned);
            } else if !metadata.title.trim().is_empty()
                && metadata.first_user_message.as_deref().map(str::trim)
                    != Some(metadata.title.trim())
            {
                item.title = Some(metadata.title);
            }
            continue;
        }
        overlay_thread_item_metadata(
            item,
            thread_item_from_state_metadata(metadata, /*parent_thread_id*/ None),
        );
    }

    page
}

fn overlay_thread_item_metadata(item: &mut ThreadItem, state_item: ThreadItem) {
    let ThreadItem {
        path,
        thread_id: _state_thread_id,
        first_user_message,
        title,
        preview,
        cwd,
        git_branch,
        git_sha,
        git_origin_url,
        source,
        history_mode,
        parent_thread_id,
        agent_nickname,
        agent_role,
        model_provider,
        cli_version,
        created_at,
        updated_at,
        recency_at,
    } = state_item;

    item.path = path;
    item.first_user_message = first_user_message;
    item.title = title;
    item.preview = preview;
    item.cwd = cwd;
    item.git_branch = git_branch;
    item.git_sha = git_sha;
    item.git_origin_url = git_origin_url;
    item.source = source;
    item.history_mode = history_mode;
    if item.parent_thread_id.is_none() {
        item.parent_thread_id = parent_thread_id;
    }
    item.agent_nickname = agent_nickname;
    item.agent_role = agent_role;
    item.model_provider = model_provider;
    item.cli_version = cli_version;
    item.created_at = created_at;
    item.updated_at = updated_at;
    item.recency_at = recency_at;
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_desc(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
    search_term: Option<&str>,
) -> std::io::Result<ThreadsPage> {
    if let Some(search_term) = search_term {
        let mut matching_items = Vec::new();
        let mut scanned_files = 0usize;
        let mut reached_scan_cap = false;
        let mut page_cursor = cursor.cloned();
        let scan_page_size = page_size.saturating_mul(8).clamp(256, 2048);

        loop {
            let mut page = list_threads_from_files_desc_unfiltered(
                codex_home,
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
                archived,
            )
            .await?;
            scanned_files = scanned_files.saturating_add(page.num_scanned_files);
            reached_scan_cap |= page.reached_scan_cap;
            filter_thread_items_by_search_term(
                state_db_ctx,
                codex_home,
                &mut page.items,
                Some(search_term),
            )
            .await?;
            matching_items.extend(page.items);
            page_cursor = page.next_cursor;
            if matching_items.len() > page_size || page_cursor.is_none() {
                break;
            }
        }

        let more_matches_available =
            matching_items.len() > page_size || page_cursor.is_some() || reached_scan_cap;
        matching_items.truncate(page_size);
        let next_cursor = if more_matches_available {
            matching_items
                .last()
                .and_then(|item| cursor_from_thread_item(item, sort_key))
        } else {
            None
        };

        return Ok(ThreadsPage {
            items: matching_items,
            next_cursor,
            num_scanned_files: scanned_files,
            reached_scan_cap,
        });
    }

    list_threads_from_files_desc_unfiltered(
        codex_home,
        page_size,
        cursor,
        sort_key,
        allowed_sources,
        model_providers,
        cwd_filters,
        default_provider,
        archived,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_desc_unfiltered(
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
) -> std::io::Result<ThreadsPage> {
    if archived {
        let root = codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
        get_threads_in_root(
            root,
            page_size,
            cursor,
            sort_key,
            ThreadListConfig {
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
                include_empty_preview: true,
                layout: ThreadListLayout::Flat,
            },
        )
        .await
    } else {
        get_threads(
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            cwd_filters,
            default_provider,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn list_threads_from_files_asc(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    default_provider: &str,
    archived: bool,
    search_term: Option<&str>,
) -> std::io::Result<ThreadsPage> {
    let scan_page_size =
        search_term.map_or(page_size, |_| page_size.saturating_mul(8).clamp(256, 2048));
    let mut page = ThreadsPage::default();
    let mut page_cursor = cursor.cloned();
    loop {
        let mut scanned = if archived {
            get_threads_in_root_ascending(
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                ThreadListConfig {
                    allowed_sources,
                    model_providers,
                    cwd_filters,
                    default_provider,
                    include_empty_preview: true,
                    layout: ThreadListLayout::Flat,
                },
            )
            .await?
        } else {
            get_threads_ascending(
                codex_home,
                scan_page_size,
                page_cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                cwd_filters,
                default_provider,
            )
            .await?
        };
        page.num_scanned_files = page
            .num_scanned_files
            .saturating_add(scanned.num_scanned_files);
        page.reached_scan_cap |= scanned.reached_scan_cap;
        filter_thread_items_by_search_term(
            state_db_ctx,
            codex_home,
            &mut scanned.items,
            search_term,
        )
        .await?;
        page.items.extend(scanned.items);
        page_cursor = scanned.next_cursor;
        if search_term.is_none() || page.items.len() > page_size || page_cursor.is_none() {
            break;
        }
    }

    let more_matches_available =
        page_cursor.is_some() || page.items.len() > page_size || page.reached_scan_cap;
    page.items.truncate(page_size);
    page.next_cursor = if more_matches_available {
        page.items
            .last()
            .and_then(|item| cursor_from_thread_item(item, sort_key))
    } else {
        None
    };
    Ok(page)
}

async fn filter_thread_items_by_search_term(
    state_db_ctx: Option<&StateRuntime>,
    codex_home: &Path,
    items: &mut Vec<ThreadItem>,
    search_term: Option<&str>,
) -> std::io::Result<()> {
    let Some(search_term) = search_term else {
        return Ok(());
    };

    // Match the title the listing displays: SQLite owns explicit titles, and the sidecar index
    // is the compatibility fallback (it can lack a committed rename). Previews come from the
    // rollout summary. Match SQLite's title-or-preview substring predicate in both sort orders.
    let thread_ids = items
        .iter()
        .filter_map(|item| item.thread_id)
        .collect::<HashSet<_>>();
    let mut thread_names = HashMap::with_capacity(thread_ids.len());
    let mut resolved_thread_ids = HashSet::with_capacity(thread_ids.len());
    if let Some(state_db_ctx) = state_db_ctx
        && !thread_ids.is_empty()
    {
        let ids = thread_ids.iter().copied().collect::<Vec<_>>();
        match state_db_ctx.get_thread_titles(&ids).await {
            Ok(titles) => {
                for (thread_id, title, first_user_message, title_is_explicit) in titles {
                    let title = title.trim();
                    if title_is_explicit {
                        resolved_thread_ids.insert(thread_id);
                    }
                    if !title.is_empty()
                        && (title_is_explicit
                            || first_user_message.as_deref().map(str::trim) != Some(title))
                    {
                        resolved_thread_ids.insert(thread_id);
                        thread_names.insert(thread_id, title.to_string());
                    }
                }
            }
            Err(err) => {
                return Err(IoError::other(format!(
                    "failed to read canonical names while filtering thread search: {err}"
                )));
            }
        }
    }
    let unresolved_thread_ids = thread_ids
        .into_iter()
        .filter(|thread_id| !resolved_thread_ids.contains(thread_id))
        .collect::<HashSet<_>>();
    for (thread_id, name) in find_thread_names_by_ids(codex_home, &unresolved_thread_ids).await? {
        thread_names.entry(thread_id).or_insert(name);
    }
    items.retain(|item| {
        item.thread_id
            .and_then(|thread_id| thread_names.get(&thread_id))
            .is_some_and(|title| title.contains(search_term))
            || item
                .preview
                .as_deref()
                .is_some_and(|preview| preview.contains(search_term))
    });
    Ok(())
}

fn cursor_from_thread_item(item: &ThreadItem, sort_key: ThreadSortKey) -> Option<Cursor> {
    let (timestamp, id) = thread_item_sort_key(item, sort_key)?;
    Some(Cursor::with_thread_id(
        timestamp,
        ThreadId::from_string(&id.to_string()).ok()?,
    ))
}

impl From<codex_state::ThreadsPage> for ThreadsPage {
    fn from(db_page: codex_state::ThreadsPage) -> Self {
        let codex_state::ThreadsPage {
            items,
            parent_thread_ids,
            next_anchor,
            num_scanned_rows,
        } = db_page;
        let items = items
            .into_iter()
            .map(|item| {
                let parent_thread_id = parent_thread_ids.get(&item.id).copied();
                thread_item_from_state_metadata(item, parent_thread_id)
            })
            .collect();
        Self {
            items,
            next_cursor: next_anchor.map(Into::into),
            num_scanned_files: num_scanned_rows,
            reached_scan_cap: false,
        }
    }
}

fn thread_item_from_state_metadata(
    item: codex_state::ThreadMetadata,
    parent_thread_id: Option<ThreadId>,
) -> ThreadItem {
    ThreadItem {
        path: item.rollout_path,
        thread_id: Some(item.id),
        first_user_message: item.first_user_message,
        title: Some(item.title),
        preview: item.preview,
        cwd: Some(item.cwd),
        git_branch: item.git_branch,
        git_sha: item.git_sha,
        git_origin_url: item.git_origin_url,
        source: Some(
            serde_json::from_str(item.source.as_str())
                .or_else(|_| serde_json::from_value(Value::String(item.source)))
                .unwrap_or(SessionSource::Unknown),
        ),
        history_mode: item.history_mode,
        parent_thread_id,
        agent_nickname: item.agent_nickname,
        agent_role: item.agent_role,
        model_provider: Some(item.model_provider),
        cli_version: Some(item.cli_version),
        created_at: Some(item.created_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        updated_at: Some(item.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
        recency_at: Some(item.recency_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
    }
}
