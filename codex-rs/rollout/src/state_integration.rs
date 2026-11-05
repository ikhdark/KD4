//! Integration between JSONL rollouts and the SQLite state runtime.
//!
//! `codex-state` owns the SQLite schema and runtime. This module owns the
//! rollout-specific initialization, backfill, reconciliation, and fallback
//! behavior that cannot live in `codex-state` without introducing a crate
//! dependency cycle.

use crate::config::RolloutConfig;
use crate::config::RolloutConfigView;
use crate::list::Cursor;
use crate::list::SortDirection;
use crate::list::ThreadSortKey;
use crate::metadata;
use crate::sqlite_metrics;
use anyhow::Context;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
pub use codex_state::LogEntry;
use codex_state::ThreadMetadataBuilder;
use codex_utils_absolute_path::normalize_for_path_comparison;
use futures::StreamExt;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Core-facing handle to the SQLite-backed state runtime.
pub type StateDbHandle = Arc<codex_state::StateRuntime>;

/// Reconciliation cadence once coverage is settled.
const BACKFILL_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Pause before a process's first pass, so it never competes with the first model
/// request, and before resuming a pass that had more work left.
const BACKFILL_RESUME_DELAY: Duration = Duration::from_secs(1);
/// One pass, after which it hands the claim back and resumes from its checkpoint.
const BACKFILL_PASS_BOUND: Duration = Duration::from_secs(20);
const MAX_CONCURRENT_ROLLOUT_PATH_VALIDATIONS: usize = 8;
const _: () = assert!(
    metadata::BACKFILL_LEASE_SECONDS as u64 > BACKFILL_PASS_BOUND.as_secs(),
    "a backfill lease must outlive the bound that limits each pass"
);

/// Initialize the state runtime for thread state persistence.
///
/// This is the process entry point for local state: it opens the SQLite-backed
/// runtime, starts the rollout metadata owner, and returns the initialized handle.
pub async fn init(config: &impl RolloutConfigView) -> Option<StateDbHandle> {
    let config = RolloutConfig::from_view(config);
    match try_init_with_roots(
        config.codex_home,
        config.sqlite_home,
        config.model_provider_id,
    )
    .await
    {
        Ok(runtime) => Some(runtime),
        Err(err) => {
            emit_startup_warning(&format!("failed to initialize state runtime: {err:#}"));
            None
        }
    }
}

/// Initialize the state runtime and return any initialization error to the caller.
///
/// Prefer [`init`] unless the caller needs to surface the exact failure after
/// tracing or UI setup has completed.
pub async fn try_init(config: &impl RolloutConfigView) -> anyhow::Result<StateDbHandle> {
    let config = RolloutConfig::from_view(config);
    try_init_with_roots(
        config.codex_home,
        config.sqlite_home,
        config.model_provider_id,
    )
    .await
}

/// Startup opens the schema and never waits for historical rollouts: serving is separate
/// from certification. Default listings and thread lookups are filesystem-backed, and readers
/// for which a missing row means absence wait for coverage themselves
/// ([`codex_state::StateRuntime::await_thread_index`]). The background owner builds the index.
async fn try_init_with_roots(
    codex_home: PathBuf,
    sqlite_home: PathBuf,
    default_model_provider_id: String,
) -> anyhow::Result<StateDbHandle> {
    let runtime =
        codex_state::StateRuntime::init(sqlite_home.clone(), default_model_provider_id.clone())
            .await
            .with_context(|| {
                format!(
                    "failed to initialize state runtime at {}",
                    sqlite_home.display()
                )
            })?;
    let backfill_state = match runtime.get_backfill_state().await {
        Ok(state) => state,
        Err(err) => {
            runtime.close().await;
            anyhow::bail!(
                "failed to read backfill state at {}: {err}",
                codex_home.display()
            );
        }
    };
    let initial_delay = if backfill_state.coverage_certified() {
        BACKFILL_REFRESH_INTERVAL
    } else {
        BACKFILL_RESUME_DELAY
    };
    spawn_backfill_refresh(
        &runtime,
        codex_home,
        default_model_provider_id,
        metadata::BackfillMemo::default(),
        initial_delay,
    );
    Ok(runtime)
}

fn spawn_backfill_refresh(
    runtime: &StateDbHandle,
    codex_home: PathBuf,
    default_model_provider_id: String,
    mut backfill_memo: metadata::BackfillMemo,
    initial_delay: Duration,
) {
    let weak = std::sync::Arc::downgrade(runtime);
    tokio::spawn(async move {
        let mut delay = initial_delay;
        loop {
            tokio::time::sleep(delay).await;
            let Some(runtime) = weak.upgrade() else {
                break;
            };
            refresh_backfill_once(
                &runtime,
                &codex_home,
                &default_model_provider_id,
                &mut backfill_memo,
            )
            .await;
            delay = match backfill_memo.last_pass_remainder() {
                // A fairness slice is not a failed traversal; it released its lease already.
                Some(metadata::PassRemainder::DiscoveryPending) => Duration::ZERO,
                Some(metadata::PassRemainder::Stopped) => BACKFILL_RESUME_DELAY,
                Some(metadata::PassRemainder::Settled | metadata::PassRemainder::Unresolved) => {
                    BACKFILL_REFRESH_INTERVAL
                }
                // No claim: another worker holds it, or a killed one's lease is running out.
                None if matches!(
                    runtime.get_backfill_state().await,
                    Ok(state) if state.status == codex_state::BackfillStatus::Running
                ) =>
                {
                    BACKFILL_RESUME_DELAY
                }
                None => BACKFILL_REFRESH_INTERVAL,
            };
        }
    });
}

/// One periodic reconciliation pass: reopen coverage completed at least one
/// interval ago, then reconcile within a bound. `memo` holds this runtime's
/// earlier outcomes, so only rollouts whose inputs changed are read again.
async fn refresh_backfill_once(
    runtime: &codex_state::StateRuntime,
    codex_home: &Path,
    default_model_provider_id: &str,
    memo: &mut metadata::BackfillMemo,
) -> Option<metadata::BackfillPassReport> {
    if let Err(error) = runtime.request_backfill_refresh(60).await {
        warn!(%error, "rollout metadata reconciliation remains pending");
        return None;
    }
    let stop_at = tokio::time::Instant::now() + BACKFILL_PASS_BOUND;
    Some(
        metadata::backfill_sessions_until(
            runtime,
            codex_home,
            default_model_provider_id,
            metadata::BACKFILL_LEASE_SECONDS,
            memo,
            || tokio::time::Instant::now() >= stop_at,
        )
        .await,
    )
}

fn emit_startup_warning(message: &str) {
    warn!("{message}");
    if !tracing::dispatcher::has_been_set() {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("{message}");
        }
    }
}

/// Open the DB if it exists and its startup backfill has already completed.
///
/// Unlike [`init`], this helper does not run rollout backfill. It is for
/// optional local reads from non-owning contexts such as remote app-server mode.
pub async fn get_state_db(config: &impl RolloutConfigView) -> Option<StateDbHandle> {
    let state_path = codex_state::state_db_path(config.sqlite_home());
    if !tokio::fs::try_exists(&state_path).await.unwrap_or(false) {
        codex_state::record_fallback(
            "get_state_db",
            "db_unavailable",
            /*telemetry_override*/ None,
        );
        return None;
    }
    let runtime = match codex_state::StateRuntime::init_for_read(
        config.sqlite_home().to_path_buf(),
        config.model_provider_id().to_string(),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(_) => {
            codex_state::record_fallback(
                "get_state_db",
                "db_error",
                /*telemetry_override*/ None,
            );
            return None;
        }
    };
    require_backfill_complete(runtime, config.sqlite_home()).await
}

/// Build a SQLite telemetry recorder backed by an OTEL metrics client.
pub fn sqlite_telemetry_recorder(
    metrics: codex_otel::MetricsClient,
    originator: &str,
) -> codex_state::DbTelemetryHandle {
    sqlite_metrics::recorder(metrics, originator)
}

/// Periodic refresh keeps a valid certificate while reconciling newer rows, but
/// a known session-metadata gap revokes it: missing lineage is not absence.
/// Readers still fall back to rollout headers for fields not yet indexed.
async fn require_backfill_complete(
    runtime: StateDbHandle,
    codex_home: &Path,
) -> Option<StateDbHandle> {
    match runtime.get_backfill_state().await {
        Ok(state) if state.coverage_certified() => Some(runtime),
        Ok(state) => {
            warn!(
                "state db backfill not complete at {} (status: {})",
                codex_home.display(),
                state.status.as_str()
            );
            codex_state::record_fallback(
                "get_state_db",
                "backfill_incomplete",
                /*telemetry_override*/ None,
            );
            None
        }
        Err(err) => {
            warn!(
                "failed to read backfill state at {}: {err}",
                codex_home.display()
            );
            codex_state::record_fallback(
                "get_state_db",
                "db_error",
                /*telemetry_override*/ None,
            );
            None
        }
    }
}

fn cursor_to_anchor(cursor: Option<&Cursor>) -> Option<codex_state::Anchor> {
    let cursor = cursor?;
    let millis = cursor.timestamp().unix_timestamp_nanos() / 1_000_000;
    let millis = i64::try_from(millis).ok()?;
    let ts = chrono::DateTime::<Utc>::from_timestamp_millis(millis)?;
    Some(codex_state::Anchor {
        ts,
        id: cursor.thread_id(),
    })
}

pub fn normalize_cwd_for_state_db(cwd: &Path) -> PathBuf {
    normalize_for_path_comparison(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// List thread ids from SQLite for parity checks without rollout scanning.
#[allow(clippy::too_many_arguments)]
pub async fn list_thread_ids_db(
    context: Option<&codex_state::StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    archived_only: bool,
    stage: &str,
) -> Option<Vec<ThreadId>> {
    let ctx = context?;
    if ctx.codex_home() != codex_home {
        warn!(
            "state db codex_home mismatch: expected {}, got {}",
            ctx.codex_home().display(),
            codex_home.display()
        );
    }

    let anchor = cursor_to_anchor(cursor);
    let allowed_sources: Vec<String> = allowed_sources
        .iter()
        .map(|value| match serde_json::to_value(value) {
            Ok(Value::String(s)) => s,
            Ok(other) => other.to_string(),
            Err(_) => String::new(),
        })
        .collect();
    let model_providers = model_providers.map(<[String]>::to_vec);
    match ctx
        .list_thread_ids(
            page_size,
            anchor.as_ref(),
            sort_key,
            allowed_sources.as_slice(),
            model_providers.as_deref(),
            archived_only,
        )
        .await
    {
        Ok(ids) => Some(ids),
        Err(err) => {
            warn!("state db list_thread_ids failed during {stage}: {err}");
            None
        }
    }
}

/// List thread metadata from SQLite without rollout directory traversal.
#[allow(clippy::too_many_arguments)]
pub async fn list_threads_db(
    context: Option<&codex_state::StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    sort_direction: SortDirection,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    cwd_filters: Option<&[PathBuf]>,
    relation_filter: Option<codex_state::ThreadRelationFilter>,
    archived: bool,
    project_id: Option<Option<&str>>,
    search_term: Option<&str>,
) -> Option<codex_state::ThreadsPage> {
    let ctx = context?;
    if ctx.codex_home() != codex_home {
        warn!(
            "state db codex_home mismatch: expected {}, got {}",
            ctx.codex_home().display(),
            codex_home.display()
        );
    }

    let anchor = cursor_to_anchor(cursor);
    let allowed_sources: Vec<String> = allowed_sources
        .iter()
        .map(|value| match serde_json::to_value(value) {
            Ok(Value::String(s)) => s,
            Ok(other) => other.to_string(),
            Err(_) => String::new(),
        })
        .collect();
    let model_providers = model_providers.map(<[String]>::to_vec);
    let normalized_cwd_filters = cwd_filters.map(|filters| {
        filters
            .iter()
            .map(|cwd| normalize_cwd_for_state_db(cwd))
            .collect::<Vec<_>>()
    });
    let filters = codex_state::ThreadFilterOptions {
        project_id,
        archived_only: archived,
        allowed_sources: allowed_sources.as_slice(),
        model_providers: model_providers.as_deref(),
        cwd_filters: normalized_cwd_filters.as_deref(),
        anchor: anchor.as_ref(),
        sort_key,
        sort_direction,
        search_term,
    };
    let page = match relation_filter {
        Some(relation_filter) => {
            ctx.list_threads_by_relation(page_size, relation_filter, filters)
                .await
        }
        None => ctx.list_threads(page_size, filters).await,
    };
    match page {
        Ok(mut page) => {
            // Relationship-filtered listings intentionally treat persisted state as authoritative.
            if relation_filter.is_some() {
                return Some(page);
            }
            let items = page.items;
            let mut valid_items = Vec::with_capacity(items.len());
            let mut path_validations = futures::stream::iter(items)
                .map(|item| async move {
                    let existing_path =
                        crate::compression::existing_rollout_path(item.rollout_path.as_path())
                            .await;
                    let existing_path = match existing_path {
                        Some(path)
                            if crate::list::read_session_meta_line(&path)
                                .await
                                .is_ok_and(|header| header.meta.id != item.id) =>
                        {
                            None
                        }
                        other => other,
                    };
                    (item, existing_path)
                })
                .buffered(MAX_CONCURRENT_ROLLOUT_PATH_VALIDATIONS);
            while let Some((mut item, existing_path)) = path_validations.next().await {
                if let Some(existing_path) = existing_path {
                    item.rollout_path = existing_path;
                    valid_items.push(item);
                } else {
                    // A rollout may have moved between active/archive directories
                    // without the state row following it. Repair only a verified
                    // file; absence is not permission to delete thread metadata.
                    let id = item.id.to_string();
                    let active =
                        crate::list::find_thread_path_by_id_str(codex_home, &id, None).await;
                    let relocated = match active {
                        Ok(Some(path)) => Some((path, false)),
                        Ok(None) => {
                            crate::list::find_archived_thread_path_by_id_str(codex_home, &id, None)
                                .await
                                .ok()
                                .flatten()
                                .map(|path| (path, true))
                        }
                        Err(_) => None,
                    };
                    if let Some((path, is_archived)) = relocated
                        && crate::list::read_session_meta_line(&path)
                            .await
                            .is_ok_and(|meta| meta.meta.id == item.id)
                    {
                        read_repair_rollout_path(
                            Some(ctx),
                            Some(item.id),
                            Some(is_archived),
                            &path,
                        )
                        .await;
                        if is_archived == archived {
                            item.rollout_path = path;
                            valid_items.push(item);
                        }
                        continue;
                    }
                    warn!(
                        "state db list_threads returned stale rollout path for thread {}: {}",
                        item.id,
                        item.rollout_path.display()
                    );
                    warn!("state db discrepancy during list_threads_db: stale_db_path_dropped");
                }
            }
            page.items = valid_items;
            Some(page)
        }
        Err(err) => {
            warn!("state db list_threads failed: {err}");
            None
        }
    }
}

/// Look up the rollout path for a thread id using SQLite.
pub async fn find_rollout_path_by_id(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    archived_only: Option<bool>,
    stage: &str,
) -> Option<PathBuf> {
    let ctx = context?;
    ctx.find_rollout_path_by_id(thread_id, archived_only)
        .await
        .unwrap_or_else(|err| {
            warn!("state db find_rollout_path_by_id failed during {stage}: {err}");
            None
        })
}

/// Reconcile rollout items into SQLite, falling back to scanning the rollout file.
pub async fn reconcile_rollout(
    context: Option<&codex_state::StateRuntime>,
    rollout_path: &Path,
    default_provider: &str,
    builder: Option<&ThreadMetadataBuilder>,
    items: &[RolloutItem],
    archived_only: Option<bool>,
) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    if builder.is_some() || !items.is_empty() {
        return apply_rollout_items(
            Some(ctx),
            rollout_path,
            default_provider,
            builder,
            items,
            "reconcile_rollout",
            /*updated_at_override*/ None,
        )
        .await;
    }
    let (Ok(expected), Ok(source_version)) = (
        metadata::expected_thread_metadata_for_rollout(ctx, rollout_path).await,
        metadata::rollout_source_version(rollout_path).await,
    ) else {
        return false;
    };
    let outcome =
        match metadata::extract_metadata_from_rollout(rollout_path, default_provider).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    "state db reconcile_rollout extraction failed {}: {err}",
                    rollout_path.display()
                );
                return false;
            }
        };
    let parent_thread_id = outcome.parent_thread_id;
    let mut metadata = outcome.metadata;
    metadata.cwd = normalize_cwd_for_state_db(&metadata.cwd);
    if let Ok(Some(existing_metadata)) = ctx.get_thread(metadata.id).await {
        metadata.prefer_existing_git_info(&existing_metadata);
        metadata.prefer_existing_explicit_title(&existing_metadata);
    }
    match archived_only {
        Some(true) if metadata.archived_at.is_none() => {
            metadata.archived_at = Some(metadata.updated_at);
        }
        Some(false) => {
            metadata.archived_at = None;
        }
        Some(true) | None => {}
    }
    if let Err(err) = ctx
        .reconcile_thread_metadata(&metadata, expected.as_ref(), &source_version)
        .await
    {
        warn!(
            "state db reconcile_rollout upsert failed {}: {err}",
            rollout_path.display()
        );
        return false;
    }
    if let Err(error) = ctx
        .patch_existing_thread_metadata(metadata.id, |current| {
            if !rollout_path.exists() && !rollout_path.with_extension("jsonl.zst").exists() {
                return;
            }
            current.rollout_path = rollout_path.to_path_buf();
            match archived_only {
                Some(true) if current.archived_at.is_none() => {
                    current.archived_at = Some(metadata.updated_at)
                }
                Some(false) => current.archived_at = None,
                _ => {}
            }
        })
        .await
    {
        warn!(%error, "rollout location reconciliation remains pending");
        return false;
    }
    if let Some(parent_thread_id) = parent_thread_id
        && let Err(err) = ctx
            .infer_thread_spawn_edge_if_absent(parent_thread_id, metadata.id)
            .await
    {
        warn!(
            "state db reconcile_rollout parent edge update failed {}: {err}",
            rollout_path.display()
        );
        return false;
    }
    true
}

/// Repair a thread's rollout path after filesystem fallback succeeds.
pub async fn read_repair_rollout_path(
    context: Option<&codex_state::StateRuntime>,
    thread_id: Option<ThreadId>,
    archived_only: Option<bool>,
    rollout_path: &Path,
) {
    let Some(ctx) = context else {
        return;
    };

    // Fast path: update an existing metadata row in place, but avoid writes when
    // read-repair computes no effective change.
    let mut saw_existing_metadata = false;
    if let Some(thread_id) = thread_id
        && let Ok(Some(metadata)) = ctx.get_thread(thread_id).await
    {
        saw_existing_metadata = true;
        let mut repaired = metadata.clone();
        repaired.rollout_path = rollout_path.to_path_buf();
        repaired.cwd = normalize_cwd_for_state_db(&repaired.cwd);
        match archived_only {
            Some(true) if repaired.archived_at.is_none() => {
                repaired.archived_at = Some(repaired.updated_at);
            }
            Some(false) => {
                repaired.archived_at = None;
            }
            Some(true) | None => {}
        }
        if repaired == metadata {
            return;
        }
        warn!("state db discrepancy during read_repair_rollout_path: upsert_needed (fast path)");
        if let Err(err) = ctx
            .patch_existing_thread_metadata(thread_id, |current| {
                if !rollout_path.exists() && !rollout_path.with_extension("jsonl.zst").exists() {
                    return;
                }
                current.rollout_path = rollout_path.to_path_buf();
                current.cwd = normalize_cwd_for_state_db(&current.cwd);
                match archived_only {
                    Some(true) if current.archived_at.is_none() => {
                        current.archived_at = Some(current.updated_at)
                    }
                    Some(false) => current.archived_at = None,
                    _ => {}
                }
            })
            .await
        {
            warn!(
                "state db read-repair upsert failed for {}: {err}",
                rollout_path.display()
            );
        } else {
            return;
        }
    }

    // Slow path: when the row is missing/unreadable (or direct upsert failed),
    // rebuild metadata from rollout contents and reconcile it into SQLite.
    if !saw_existing_metadata {
        warn!("state db discrepancy during read_repair_rollout_path: upsert_needed (slow path)");
    }
    // Full extraction below reads SessionMeta and owns provider resolution; a
    // separate metadata-line read would traverse the rollout twice.
    reconcile_rollout(
        Some(ctx),
        rollout_path,
        /*default_provider*/ "",
        /*builder*/ None,
        &[],
        archived_only,
    )
    .await;
}

/// Apply rollout items incrementally to SQLite.
#[allow(clippy::too_many_arguments)]
pub async fn apply_rollout_items(
    context: Option<&codex_state::StateRuntime>,
    rollout_path: &Path,
    default_provider: &str,
    builder: Option<&ThreadMetadataBuilder>,
    items: &[RolloutItem],
    stage: &str,
    updated_at_override: Option<DateTime<Utc>>,
) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    let mut builder = match builder {
        Some(builder) => builder.clone(),
        None => match metadata::builder_from_items(items, rollout_path) {
            Some(builder) => builder,
            None => {
                warn!(
                    "state db apply_rollout_items missing builder during {stage}: {}",
                    rollout_path.display()
                );
                warn!("state db discrepancy during apply_rollout_items: {stage}, missing_builder");
                return false;
            }
        },
    };
    if builder.model_provider.is_none() {
        builder.model_provider = Some(default_provider.to_string());
    }
    builder.rollout_path = rollout_path.to_path_buf();
    builder.cwd = normalize_cwd_for_state_db(&builder.cwd);
    if let Err(err) = ctx
        .apply_rollout_items(&builder, items, updated_at_override)
        .await
    {
        warn!(
            "state db apply_rollout_items failed during {stage} for {}: {err}",
            rollout_path.display()
        );
        return false;
    }
    true
}

pub async fn touch_thread_updated_at(
    context: Option<&codex_state::StateRuntime>,
    thread_id: Option<ThreadId>,
    updated_at: DateTime<Utc>,
    stage: &str,
) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    let Some(thread_id) = thread_id else {
        return false;
    };
    ctx.touch_thread_updated_at(thread_id, updated_at)
        .await
        .unwrap_or_else(|err| {
            warn!("state db touch_thread_updated_at failed during {stage} for {thread_id}: {err}");
            false
        })
}

