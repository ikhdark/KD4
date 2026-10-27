use crate::ARCHIVED_SESSIONS_SUBDIR;
use crate::SESSIONS_SUBDIR;
use crate::compression;
use crate::list::parse_timestamp_uuid_from_filename;
use crate::payload_artifact::PayloadDependencyVersion;
use crate::recorder::RolloutRecorder;
use crate::state_integration::normalize_cwd_for_state_db;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_state::BackfillState;
use codex_state::BackfillStats;
use codex_state::BackfillStatus;
use codex_state::DB_ERROR_METRIC;
use codex_state::DB_METRIC_BACKFILL;
use codex_state::DB_METRIC_BACKFILL_DURATION_MS;
use codex_state::ExtractionOutcome;
use codex_state::ThreadMetadataBuilder;
use codex_state::ThreadMetadataRolloutReducer;
use codex_utils_absolute_path::normalize_for_native_workdir;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;
use tracing::info;
use tracing::warn;

const BACKFILL_BATCH_SIZE: usize = 200;
const EXTRACTION_CACHE_CAPACITY: usize = 256;
/// A claim must outlive one bounded startup run so live workers never overlap, yet expire
/// soon after a worker is killed so later startups can resume instead of timing out behind it.
pub(crate) const BACKFILL_LEASE_SECONDS: i64 = 60;

pub(crate) fn builder_from_session_meta(
    session_meta: &SessionMetaLine,
    rollout_path: &Path,
) -> Option<ThreadMetadataBuilder> {
    let created_at = parse_timestamp_to_utc(session_meta.meta.timestamp.as_str())?;
    let mut builder = ThreadMetadataBuilder::new(
        session_meta.meta.id,
        rollout_path.to_path_buf(),
        created_at,
        session_meta.meta.source.clone(),
    );
    builder.history_mode = session_meta.meta.history_mode;
    builder.session_id = Some(session_meta.meta.session_id);
    builder.parent_thread_id = session_meta.meta.parent_thread_id;
    builder.model_provider = session_meta.meta.model_provider.clone();
    builder.agent_nickname = session_meta.meta.agent_nickname.clone();
    builder.agent_role = session_meta.meta.agent_role.clone();
    builder.agent_path = session_meta.meta.agent_path.clone();
    builder.cwd = session_meta.meta.cwd.clone();
    builder.cli_version = Some(session_meta.meta.cli_version.clone());
    builder.sandbox_policy = SandboxPolicy::new_read_only_policy();
    builder.approval_mode = AskForApproval::OnRequest;
    if let Some(git) = session_meta.git.as_ref() {
        builder.git_sha = git.commit_hash.as_ref().map(|sha| sha.0.clone());
        builder.git_branch = git.branch.clone();
        builder.git_origin_url = git.repository_url.clone();
    }
    Some(builder)
}

pub fn builder_from_items(
    items: &[RolloutItem],
    rollout_path: &Path,
) -> Option<ThreadMetadataBuilder> {
    if let Some(session_meta) = items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line),
        RolloutItem::ResponseItem(_)
        | RolloutItem::ToolManifest(_)
        | RolloutItem::SamplingBoundary(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::AdditionalContextState { .. }
        | RolloutItem::EventMsg(_) => None,
    }) && let Some(builder) = builder_from_session_meta(session_meta, rollout_path)
    {
        return Some(builder);
    }

    let file_name = rollout_path.file_name()?.to_str()?;
    let file_name = compression::parse_rollout_file_name(file_name)?;
    let (created_ts, uuid) = parse_timestamp_uuid_from_filename(file_name)?;
    let created_at =
        DateTime::<Utc>::from_timestamp(created_ts.unix_timestamp(), 0)?.with_nanosecond(0)?;
    let id = ThreadId::from_string(&uuid.to_string()).ok()?;
    Some(ThreadMetadataBuilder::new(
        id,
        rollout_path.to_path_buf(),
        created_at,
        SessionSource::default(),
    ))
}

pub async fn extract_metadata_from_rollout(
    rollout_path: &Path,
    default_provider: &str,
) -> anyhow::Result<ExtractionOutcome> {
    extract_metadata_from_rollout_with_cache_status(rollout_path, default_provider)
        .await
        .map(|(outcome, _cache_hit)| outcome)
}

pub(crate) async fn rollout_source_version(
    path: &Path,
) -> anyhow::Result<(PathBuf, u64, Option<SystemTime>)> {
    let path = crate::existing_rollout_path(path)
        .await
        .ok_or_else(|| anyhow::anyhow!("rollout is missing"))?;
    let metadata = tokio::fs::metadata(&path).await?;
    Ok((path, metadata.len(), metadata.modified().ok()))
}

/// Pre-extraction authority snapshot for a rollout. The index lookup matches
/// the stored path spelling exactly, while reconciliation compares locations
/// as paths. A row indexed under another safe spelling of this same file (such
/// as a Windows verbatim prefix) is its existing authority, not a concurrent insert.
pub(crate) async fn expected_thread_metadata_for_rollout(
    runtime: &codex_state::StateRuntime,
    rollout_path: &Path,
) -> anyhow::Result<Option<codex_state::ThreadMetadata>> {
    if let Some(metadata) = runtime
        .thread_metadata_for_rollout_path(rollout_path)
        .await?
    {
        return Ok(Some(metadata));
    }
    let Some((_, uuid)) = rollout_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(parse_timestamp_uuid_from_filename)
    else {
        return Ok(None);
    };
    let thread_id = ThreadId::from_string(&uuid.to_string())?;
    let rollout_path = normalize_for_native_workdir(rollout_path);
    Ok(runtime.get_thread(thread_id).await?.filter(|metadata| {
        // Reuse the platform's safe namespace simplification only: do not fold
        // case or treat a genuinely moved rollout as the observed authority.
        let stored_path = normalize_for_native_workdir(&metadata.rollout_path);
        stored_path == rollout_path
            || stored_path == rollout_path.with_extension("jsonl.zst")
    }))
}

pub(crate) async fn extract_metadata_from_rollout_with_cache_status(
    rollout_path: &Path,
    default_provider: &str,
) -> anyhow::Result<(ExtractionOutcome, bool)> {
    if let Some(outcome) = cached_extraction_outcome(rollout_path, default_provider).await {
        return Ok((outcome, true));
    }
    let mut accumulator = RolloutMetadataAccumulator::default();
    let (_thread_id, parse_errors, _, unresolved_payloads) =
        RolloutRecorder::for_each_rollout_item_with_integrity(rollout_path, false, |item, _| {
            accumulator.push(item, rollout_path, default_provider);
        })
        .await?;
    let mut outcome = accumulator
        .finish(rollout_path, default_provider, parse_errors)
        .await
        .map_err(|error| {
            if unresolved_payloads != 0 {
                anyhow::Error::from(std::io::Error::other(RepairablePayloadGap(format!(
                    "rollout metadata has repairable payload gaps: {error}"
                ))))
            } else {
                error
            }
        })?;
    outcome.unresolved_payloads = unresolved_payloads;
    Ok((outcome, false))
}

/// Extraction failed only after referenced payload artifacts were unreadable.
#[derive(Debug)]
struct RepairablePayloadGap(String);

impl std::fmt::Display for RepairablePayloadGap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RepairablePayloadGap {}

fn is_repairable_payload_gap(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
        .is_some_and(
            <dyn std::error::Error + std::marker::Send + std::marker::Sync + 'static>::is::<
                RepairablePayloadGap,
            >,
        )
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ExtractionCacheKey {
    path: PathBuf,
    physical_path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    default_provider: String,
}

#[derive(Default)]
struct ExtractionCache {
    entries: HashMap<ExtractionCacheKey, (ExtractionOutcome, Vec<PayloadDependencyVersion>)>,
    summaries: HashMap<ExtractionCacheKey, crate::list::HeadTailSummary>,
    order: VecDeque<ExtractionCacheKey>,
}

fn extraction_cache() -> &'static Mutex<ExtractionCache> {
    static CACHE: OnceLock<Mutex<ExtractionCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ExtractionCache::default()))
}

/// Snapshot only existing summary paths; callers must still perform normal
/// rollout-key and payload-version validation before reusing a value.
pub(crate) fn cached_summary_paths(default_provider: &str) -> HashSet<PathBuf> {
    extraction_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .summaries
        .keys()
        .filter(|key| key.default_provider == default_provider)
        .map(|key| key.path.clone())
        .collect()
}

async fn extraction_cache_key(
    rollout_path: &Path,
    default_provider: &str,
) -> Option<ExtractionCacheKey> {
    let physical_path = compression::existing_rollout_path(rollout_path).await?;
    let metadata = tokio::fs::metadata(&physical_path).await.ok()?;
    Some(ExtractionCacheKey {
        path: rollout_path.to_path_buf(),
        physical_path,
        len: metadata.len(),
        modified: Some(metadata.modified().ok()?),
        default_provider: default_provider.to_string(),
    })
}

pub(crate) struct ExtractionCacheGuard {
    key: Option<ExtractionCacheKey>,
}

pub(crate) async fn extraction_cache_guard(
    rollout_path: &Path,
    default_provider: &str,
) -> ExtractionCacheGuard {
    ExtractionCacheGuard {
        key: extraction_cache_key(rollout_path, default_provider).await,
    }
}

pub(crate) async fn cache_extraction_outcome(
    guard: ExtractionCacheGuard,
    outcome: ExtractionOutcome,
    summary: Option<crate::list::HeadTailSummary>,
    payload_versions: Vec<PayloadDependencyVersion>,
) {
    let Some(key) = guard.key else {
        return;
    };
    let Some(current_key) = extraction_cache_key(&key.path, &key.default_provider).await else {
        return;
    };
    if current_key != key {
        return;
    }
    // Sidecars are independent inputs. Check the streaming reader's pre-read
    // receipts without holding the cache mutex across filesystem work.
    let payloads_unchanged = payload_versions.is_empty()
        || crate::payload_artifact::payload_versions_match(&payload_versions).await;
    let mut cache = extraction_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.entries.retain(|candidate, _| {
        candidate.path != key.path || candidate.default_provider != key.default_provider
    });
    cache.summaries.retain(|candidate, _| {
        candidate.path != key.path || candidate.default_provider != key.default_provider
    });
    cache.order.retain(|candidate| {
        candidate.path != key.path || candidate.default_provider != key.default_provider
    });
    // Payload artifacts can be repaired without changing the rollout cache key.
    // Retire any earlier snapshot, but do not retain this incomplete projection.
    if outcome.parse_errors != 0 || outcome.unresolved_payloads != 0 || !payloads_unchanged {
        return;
    }
    cache.order.push_back(key.clone());
    if let Some(summary) = summary {
        cache.summaries.insert(key.clone(), summary);
    }
    cache.entries.insert(key, (outcome, payload_versions));
    while cache.order.len() > EXTRACTION_CACHE_CAPACITY {
        if let Some(evicted) = cache.order.pop_front() {
            cache.entries.remove(&evicted);
            cache.summaries.remove(&evicted);
        }
    }
}

pub(crate) async fn cached_head_summary(
    rollout_path: &Path,
    default_provider: &str,
) -> Option<crate::list::HeadTailSummary> {
    let key = extraction_cache_key(rollout_path, default_provider).await?;
    let (summary, dependencies) = {
        let cache = extraction_cache().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (cache.summaries.get(&key)?.clone(), cache.entries.get(&key)?.1.clone())
    };
    (dependencies.is_empty() || crate::payload_artifact::payload_versions_match(&dependencies).await)
        .then_some(summary)
}

async fn cached_extraction_outcome(
    rollout_path: &Path,
    default_provider: &str,
) -> Option<ExtractionOutcome> {
    let key = extraction_cache_key(rollout_path, default_provider).await?;
    let (outcome, dependencies) = {
        let cache = extraction_cache().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.entries.get(&key)?.clone()
    };
    (dependencies.is_empty() || crate::payload_artifact::payload_versions_match(&dependencies).await)
        .then_some(outcome)
}

#[derive(Default)]
pub(crate) struct RolloutMetadataAccumulator {
    metadata: Option<codex_state::ThreadMetadata>,
    reducer: Option<ThreadMetadataRolloutReducer>,
    has_metadata_builder: bool,
    parent_thread_id: Option<ThreadId>,
    saw_item: bool,
    saw_first_session_meta: bool,
    persisted_recency_at: bool,
}

impl RolloutMetadataAccumulator {
    pub(crate) fn push(&mut self, item: RolloutItem, rollout_path: &Path, default_provider: &str) {
        self.saw_item = true;
        if self.metadata.is_none() {
            let builder = builder_from_items(&[], rollout_path);
            self.has_metadata_builder = builder.is_some();
            // A late SessionMeta may supply the identity for a nonstandard filename.
            // Until then, fold events into a provisional projection instead of
            // retaining arbitrarily many user messages and settings snapshots.
            let builder = builder.unwrap_or_else(|| {
                ThreadMetadataBuilder::new(
                    ThreadId::new(),
                    rollout_path.to_path_buf(),
                    DateTime::<Utc>::UNIX_EPOCH,
                    SessionSource::default(),
                )
            });
            self.metadata = Some(builder.build(default_provider));
            self.reducer = Some(ThreadMetadataRolloutReducer::default());
        }
        if let RolloutItem::SessionMeta(meta_line) = &item
            && !self.saw_first_session_meta
        {
            self.saw_first_session_meta = true;
            if let Some(builder) = builder_from_session_meta(meta_line, rollout_path) {
                self.parent_thread_id = builder.parent_thread_id;
                let mut canonical = builder.build(default_provider);
                if let Some(projected) = self.metadata.take() {
                    // Before the first SessionMeta, only these non-setting
                    // fields can change. The reducer retains settings in order.
                    canonical.title = projected.title;
                    canonical.preview = projected.preview;
                    canonical.first_user_message = projected.first_user_message;
                    canonical.tokens_used = projected.tokens_used;
                    if self.persisted_recency_at {
                        canonical.recency_at = projected.recency_at;
                    }
                }
                self.metadata = Some(canonical);
                self.has_metadata_builder = true;
            }
        }
        if codex_state::latest_rollout_recency_at(std::slice::from_ref(&item)).is_some() {
            self.persisted_recency_at = true;
        }
        if let (Some(projected), Some(streaming_reducer)) =
            (self.metadata.as_mut(), self.reducer.as_mut())
        {
            streaming_reducer.apply_item(projected, &item);
        }
    }

    pub(crate) async fn finish(
        self,
        rollout_path: &Path,
        default_provider: &str,
        parse_errors: usize,
    ) -> anyhow::Result<ExtractionOutcome> {
        if !self.saw_item {
            return Err(anyhow::anyhow!(
                "empty session file: {}",
                rollout_path.display()
            ));
        }

        let mut metadata = self
            .metadata
            .filter(|_| self.has_metadata_builder)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "rollout missing metadata builder: {}",
                    rollout_path.display()
                )
            })?;
        let reducer = self.reducer.ok_or_else(|| {
            anyhow::anyhow!(
                "rollout metadata projection is missing its reducer: {}",
                rollout_path.display()
            )
        })?;
        reducer.finish(&mut metadata, default_provider);
        if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
            metadata.updated_at = updated_at;
            if !self.persisted_recency_at {
                metadata.recency_at = updated_at;
            }
        }
        Ok(ExtractionOutcome {
            metadata,
            parent_thread_id: self.parent_thread_id,
            parse_errors,
            unresolved_payloads: 0,
        })
    }
}



/// Rollout version stamped before a read: physical path, length and mtime.
type RolloutSourceVersion = (PathBuf, u64, Option<SystemTime>);

/// Outcomes one runtime already reconciled, keyed by discovered rollout path.
///
/// A later pass reuses an entry only while the rollout's physical path, length
/// and modification time are unchanged and, for a payload gap, every payload
/// it names is too. Appends, rewrites, moves, compression and payload repair
/// are therefore read again. Failures a retry may resolve are never kept.
#[derive(Default)]
pub(crate) struct BackfillMemo {
    entries: HashMap<PathBuf, BackfillMemoEntry>,
    last_pass: Option<PassRemainder>,
    discovery: Option<BackfillDiscovery>,
}

struct BackfillDiscovery {
    home: PathBuf,
    pending: Vec<(PathBuf, bool)>,
    current: Option<(tokio::fs::ReadDir, bool)>,
    entry: Option<(tokio::fs::DirEntry, bool)>,
    paths: std::collections::BTreeMap<String, BackfillRolloutPath>,
    failed: bool,
}

/// What a pass that claimed the backfill lease left unsettled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PassRemainder {
    /// Every rollout after the checkpoint settled.
    Settled,
    /// Only payload gaps remain, each read again at the rollout version of its
    /// previous read with the payloads it names stamped before that read, or
    /// reused while all of those stay unchanged.
    ConfirmedPayloadGaps(usize),
    /// Some payload gap was read only once at its current rollout version.
    UnconfirmedPayloadGaps,
    /// Discovery retained a traversal cursor after a bounded slice. The caller
    /// may continue with this memo while its overall deadline permits.
    DiscoveryPending,
    /// The pass stopped early, or a failure a retry may resolve remains.
    Unresolved,
}

struct BackfillMemoEntry {
    source: RolloutSourceVersion,
    outcome: BackfillMemoOutcome,
    observed_at: std::time::Instant,
}

enum BackfillMemoOutcome {
    /// Reconciled, or content that no retry can repair.
    Settled,
    /// Indexed provisionally with unreadable payloads. It stays unsettled and
    /// keeps the checkpoint before it. Payload versions stamped before the
    /// read that confirmed the gap decide when to read it again; `None` until
    /// such a read.
    PayloadGap(Option<Vec<PayloadDependencyVersion>>),
}

impl BackfillMemo {
    /// `Some(settled)` while nothing the last reconciliation read has changed.
    async fn reusable_outcome(&self, path: &Path, source: &RolloutSourceVersion) -> Option<bool> {
        let entry = self
            .entries
            .get(path)
            .filter(|entry| entry.source == *source)?;
        match &entry.outcome {
            BackfillMemoOutcome::Settled => Some(true),
            BackfillMemoOutcome::PayloadGap(Some(dependencies)) => {
                if entry.observed_at.elapsed() >= std::time::Duration::from_secs(30) {
                    return None;
                }
                crate::payload_artifact::payload_versions_match(dependencies)
                    .await
                    .then_some(false)
            }
            BackfillMemoOutcome::PayloadGap(None) => None,
        }
    }

    /// A gap already seen at this exact rollout version: the rollout has stopped
    /// changing, so stamping its payloads now lets the next read confirm the gap.
    fn payload_gap_at(&self, path: &Path, source: &RolloutSourceVersion) -> bool {
        self.entries.get(path).is_some_and(|entry| {
            entry.source == *source && matches!(entry.outcome, BackfillMemoOutcome::PayloadGap(_))
        })
    }

    /// What the latest [`backfill_sessions_until`] call left unsettled; `None`
    /// when that call did not claim the lease and run a pass.
    pub(crate) fn last_pass_remainder(&self) -> Option<PassRemainder> {
        self.last_pass
    }
}

/// Work done by one backfill pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackfillPassReport {
    /// Rollouts whose history this pass read.
    pub(crate) extracted: usize,
    /// Rollouts whose earlier outcome was kept because nothing it read changed.
    pub(crate) unchanged: usize,
}

/// Backfill rollout metadata into SQLite under the shared worker lease.
///
/// `should_stop` is checked between rollouts. A stopped run checkpoints its contiguous
/// progress and releases its claim, so a caller with a deadline never abandons a held lease
/// that would block every later worker until it expires. `memo` carries this runtime's
/// earlier outcomes so a periodic pass reads only rollouts whose inputs changed.
pub(crate) async fn backfill_sessions_until(
    runtime: &codex_state::StateRuntime,
    codex_home: &Path,
    default_provider: &str,
    backfill_lease_seconds: i64,
    memo: &mut BackfillMemo,
    mut should_stop: impl FnMut() -> bool,
) -> BackfillPassReport {
    memo.last_pass = None;
    let metric_client = codex_otel::global();
    let timer = metric_client
        .as_ref()
        .and_then(|otel| otel.start_timer(DB_METRIC_BACKFILL_DURATION_MS, &[]).ok());
    let backfill_state = match runtime.get_backfill_state().await {
        Ok(state) => state,
        Err(err) => {
            warn!(
                "failed to read backfill state at {}: {err}",
                codex_home.display()
            );
            BackfillState::default()
        }
    };
    if backfill_state.status == BackfillStatus::Complete {
        return BackfillPassReport::default();
    }
    let claim = match runtime.claim_backfill(backfill_lease_seconds).await {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            info!("state db backfill already running at {}; skipping duplicate worker", codex_home.display());
            return BackfillPassReport::default();
        }
        Err(err) => {
            warn!(
                "failed to claim backfill worker at {}: {err}",
                codex_home.display()
            );
            return BackfillPassReport::default();
        }
    };
    let backfill_state = BackfillState {
        status: BackfillStatus::Running,
        last_watermark: claim.last_watermark.clone(),
        ..Default::default()
    };

    if memo.discovery.as_ref().is_none_or(|discovery| discovery.home != codex_home) {
        memo.discovery = Some(BackfillDiscovery::new(codex_home));
    }
    if !memo.discovery.as_mut().expect("discovery initialized").advance(&mut should_stop).await {
        // A partial tree cannot certify a contiguous durable watermark. Retain
        // the in-memory traversal but release the cross-process claim promptly.
        memo.last_pass = Some(PassRemainder::DiscoveryPending);
        if let Err(error) = runtime.release_backfill_claim(&claim).await {
            warn!(%error, "failed to release paused rollout discovery claim");
        }
        return BackfillPassReport::default();
    }
    let discovery = memo.discovery.take().expect("completed discovery");
    let mut unresolved_failures = discovery.failed;
    let mut rollout_paths = discovery.paths.into_values().collect::<Vec<_>>();
    if !unresolved_failures {
        // Rollouts that moved or disappeared are read again under their new paths.
        let discovered = rollout_paths
            .iter()
            .map(|rollout| rollout.path.as_path())
            .collect::<HashSet<_>>();
        memo.entries
            .retain(|path, _| discovered.contains(path.as_path()));
    }
    if let Some(last_watermark) = backfill_state.last_watermark.as_deref() {
        rollout_paths.retain(|entry| entry.watermark.as_str() > last_watermark);
    }

    let mut stats = BackfillStats {
        scanned: 0,
        upserted: 0,
        failed: 0,
    };
    let mut contiguous_watermark = backfill_state.last_watermark.clone();
    let mut checkpointed_watermark = backfill_state.last_watermark.clone();
    let mut stopped = false;
    let mut report = BackfillPassReport::default();
    // Separate payload gaps from failures a retry may resolve. Only discovery can
    // have failed so far, and a rollout it missed is unread.
    let mut transient_failures = unresolved_failures;
    let mut confirmed_gaps = 0usize;
    let mut unconfirmed_gaps = 0usize;
    'batches: for batch in rollout_paths.chunks(BACKFILL_BATCH_SIZE) {
        for rollout in batch {
            if should_stop() {
                stopped = true;
                break 'batches;
            }
            stats.scanned = stats.scanned.saturating_add(1);
            let Ok(source_version) = rollout_source_version(&rollout.path).await else {
                memo.entries.remove(&rollout.path);
                stats.failed = stats.failed.saturating_add(1);
                unresolved_failures = true;
                transient_failures = true;
                continue;
            };
            // Re-reading unchanged inputs reproduces the same outcome, so keep it.
            // A payload gap stays unsettled and keeps the checkpoint before it.
            if let Some(settled) = memo.reusable_outcome(&rollout.path, &source_version).await {
                report.unchanged = report.unchanged.saturating_add(1);
                if settled {
                    if !unresolved_failures {
                        contiguous_watermark = Some(rollout.watermark.clone());
                    }
                } else {
                    // Only a gap whose named payloads were stamped is reused.
                    confirmed_gaps = confirmed_gaps.saturating_add(1);
                    stats.failed = stats.failed.saturating_add(1);
                    unresolved_failures = true;
                }
                continue;
            }
            // Stamp a known gap's payloads before reading it again, so a payload that
            // appears during or after this read is still a change for the next pass.
            let gap_dependencies = if memo.payload_gap_at(&rollout.path, &source_version) {
                crate::payload_artifact::named_payload_versions(&source_version.0)
                    .await
                    .ok()
            } else {
                None
            };
            let Ok(expected) = expected_thread_metadata_for_rollout(runtime, &rollout.path).await
            else {
                memo.entries.remove(&rollout.path);
                stats.failed = stats.failed.saturating_add(1);
                unresolved_failures = true;
                transient_failures = true;
                continue;
            };
            report.extracted = report.extracted.saturating_add(1);
            let (settled, remembered) =
                match extract_metadata_from_rollout(&rollout.path, default_provider).await {
                    Ok(outcome) => {
                        if outcome.parse_errors > 0
                            && let Some(ref metric_client) = metric_client
                        {
                            let metric_client = metric_client.clone();
                            let parse_errors = outcome.parse_errors as i64;
                            let _ = tokio::task::spawn_blocking(move || {
                                let _ = metric_client.counter(
                                    DB_ERROR_METRIC,
                                    parse_errors,
                                    &[("stage", "backfill_sessions")],
                                );
                            })
                            .await;
                        }
                        let recoverable_gap = outcome.unresolved_payloads != 0;
                        let parent_thread_id = outcome.parent_thread_id;
                        let mut metadata = outcome.metadata;
                        metadata.cwd = normalize_cwd_for_state_db(&metadata.cwd);
                        if let Ok(Some(existing_metadata)) = runtime.get_thread(metadata.id).await {
                            metadata.prefer_existing_git_info(&existing_metadata);
                            metadata.prefer_existing_explicit_title(&existing_metadata);
                        }
                        if rollout.archived && metadata.archived_at.is_none() {
                            let fallback_archived_at = metadata.updated_at;
                            metadata.archived_at = file_modified_time_utc(&rollout.path)
                                .await
                                .or(Some(fallback_archived_at));
                        }
                        if let Err(err) = runtime
                            .reconcile_thread_metadata(
                                &metadata,
                                expected.as_ref(),
                                &source_version,
                            )
                            .await
                        {
                            warn!("failed to upsert rollout {}: {err}", rollout.path.display());
                            (false, None)
                        } else if let Some(parent_thread_id) = parent_thread_id
                            && let Err(err) = runtime
                                .infer_thread_spawn_edge_if_absent(parent_thread_id, metadata.id)
                                .await
                        {
                            warn!(
                                "failed to restore parent edge for {}: {err}",
                                rollout.path.display()
                            );
                            (false, None)
                        } else {
                            stats.upserted = stats.upserted.saturating_add(1);
                            // Provisional rows remain useful, but may not certify lineage.
                            // Retry on a later bounded pass, with the checkpoint before this gap.
                            if recoverable_gap {
                                (
                                    false,
                                    Some(BackfillMemoOutcome::PayloadGap(gap_dependencies)),
                                )
                            } else {
                                (true, Some(BackfillMemoOutcome::Settled))
                            }
                        }
                    }
                    Err(err) => {
                        warn!(
                            "failed to extract rollout {}: {err}",
                            rollout.path.display()
                        );
                        // Invalid content cannot be repaired by retrying this pass, but exhausted
                        // I/O retries do not prove a rollout is permanently unreadable. Keep the
                        // contiguous checkpoint before it so a later worker can recover it.
                        let retryable_io = err
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|err| err.kind() != std::io::ErrorKind::InvalidData);
                        if !retryable_io {
                            stats.failed = stats.failed.saturating_add(1);
                            (true, Some(BackfillMemoOutcome::Settled))
                        } else if is_repairable_payload_gap(&err) {
                            (
                                false,
                                Some(BackfillMemoOutcome::PayloadGap(gap_dependencies)),
                            )
                        } else {
                            (false, None)
                        }
                    }
                };
            if !settled {
                match &remembered {
                    Some(BackfillMemoOutcome::PayloadGap(Some(_))) => {
                        confirmed_gaps = confirmed_gaps.saturating_add(1);
                    }
                    Some(BackfillMemoOutcome::PayloadGap(None)) => {
                        unconfirmed_gaps = unconfirmed_gaps.saturating_add(1);
                    }
                    Some(BackfillMemoOutcome::Settled) | None => transient_failures = true,
                }
            }
            match remembered {
                Some(outcome) => {
                    memo.entries.insert(
                        rollout.path.clone(),
                        BackfillMemoEntry {
                            source: source_version,
                            outcome,
                            observed_at: std::time::Instant::now(),
                        },
                    );
                }
                None => {
                    memo.entries.remove(&rollout.path);
                }
            }
            if settled {
                if !unresolved_failures {
                    contiguous_watermark = Some(rollout.watermark.clone());
                }
            } else {
                stats.failed = stats.failed.saturating_add(1);
                unresolved_failures = true;
            }
        }

        if contiguous_watermark != checkpointed_watermark
            && let Some(watermark) = contiguous_watermark.as_deref()
        {
            if let Err(err) = runtime.checkpoint_backfill_claim(&claim, watermark).await {
                unresolved_failures = true;
                transient_failures = true;
                warn!(
                    "failed to checkpoint backfill at {}: {err}",
                    codex_home.display()
                );
                break;
            } else {
                checkpointed_watermark = contiguous_watermark.clone();
            }
        }
    }
    // A stopped run skipped its batch checkpoint; save the contiguous progress so the next
    // claim resumes after it instead of rescanning the batch.
    if stopped
        && contiguous_watermark != checkpointed_watermark
        && let Some(watermark) = contiguous_watermark.as_deref()
        && let Err(err) = runtime.checkpoint_backfill_claim(&claim, watermark).await
    {
        warn!(
            "failed to checkpoint stopped backfill at {}: {err}",
            codex_home.display()
        );
    }
    if unresolved_failures || stopped {
        if stopped {
            info!(
                "state db backfill paused at {}; a later worker resumes from its checkpoint",
                codex_home.display()
            );
        }
        if let Err(err) = runtime.release_backfill_claim(&claim).await {
            warn!(
                "failed to release incomplete backfill at {}: {err}",
                codex_home.display()
            );
        }
    } else if let Err(err) = runtime
        .complete_backfill_claim(&claim, contiguous_watermark.as_deref())
        .await
    {
        transient_failures = true;
        warn!(
            "failed to mark backfill complete at {}: {err}",
            codex_home.display()
        );
        if let Err(pending_err) = runtime.release_backfill_claim(&claim).await {
            warn!(
                "failed to release backfill after completion error at {}: {pending_err}",
                codex_home.display()
            );
        }
    }
    memo.last_pass = Some(if stopped || transient_failures {
        PassRemainder::Unresolved
    } else if unconfirmed_gaps > 0 {
        PassRemainder::UnconfirmedPayloadGaps
    } else if confirmed_gaps > 0 {
        PassRemainder::ConfirmedPayloadGaps(confirmed_gaps)
    } else {
        PassRemainder::Settled
    });

    info!(
        "state db backfill scanned={}, extracted={}, unchanged={}, upserted={}, failed={}",
        stats.scanned, report.extracted, report.unchanged, stats.upserted, stats.failed
    );
    if metric_client.is_some() || timer.is_some() {
        // SDK instrument creation and measurement may take shared SDK locks or
        // invoke configured tracing subscribers. Keep these synchronous operations
        // off the executor that drives embedded archive/startup backfill.
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(metric_client) = metric_client {
                let _ = metric_client.counter(
                    DB_METRIC_BACKFILL,
                    stats.upserted as i64,
                    &[("status", "upserted")],
                );
                let _ = metric_client.counter(
                    DB_METRIC_BACKFILL,
                    stats.failed as i64,
                    &[("status", "failed")],
                );
            }
            if let Some(timer) = timer.as_ref() {
                let status = if stats.failed == 0 {
                    "success"
                } else if stats.upserted == 0 {
                    "failed"
                } else {
                    "partial_failure"
                };
                let _ = timer.record(&[("status", status)]);
            }
        })
        .await;
    }
    report
}

#[derive(Debug, Clone)]
struct BackfillRolloutPath {
    watermark: String,
    path: PathBuf,
    archived: bool,
}

fn backfill_watermark_for_path(codex_home: &Path, path: &Path) -> String {
    path.strip_prefix(codex_home)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

async fn file_modified_time_utc(path: &Path) -> Option<DateTime<Utc>> {
    let modified = compression::file_modified_time(path).await.ok()??;
    DateTime::<Utc>::from_timestamp(modified.unix_timestamp(), modified.nanosecond())
}

fn parse_timestamp_to_utc(ts: &str) -> Option<DateTime<Utc>> {
    const FILENAME_TS_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";
    if let Ok(naive) = NaiveDateTime::parse_from_str(ts, FILENAME_TS_FORMAT) {
        let dt = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        return dt.with_nanosecond(0);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&Utc));
    }
    None
}

impl BackfillDiscovery {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            pending: vec![(home.join(SESSIONS_SUBDIR), false), (home.join(ARCHIVED_SESSIONS_SUBDIR), true)],
            current: None,
            entry: None,
            paths: std::collections::BTreeMap::new(),
            failed: false,
        }
    }

    async fn advance(&mut self, should_stop: &mut impl FnMut() -> bool) -> bool {
        // Retain traversal and in-flight entry ownership at every await so a
        // cancelled pass cannot silently omit a directory or consumed entry.
        for _ in 0..4096 {
            if should_stop() { return false; }
            if self.entry.is_none() {
                if self.current.is_none() {
                    let Some((dir, archived)) = self.pending.last().cloned() else { return true; };
                    let opened = tokio::fs::read_dir(&dir).await;
                    self.pending.pop();
                    match opened {
                        Ok(reader) => self.current = Some((reader, archived)),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            self.failed = true;
                            warn!(path = %dir.display(), %error, "rollout discovery remains incomplete");
                            continue;
                        }
                    }
                }
                let (reader, archived) = self.current.as_mut().expect("discovery owns a directory");
                let archived = *archived;
                match reader.next_entry().await {
                    Ok(Some(entry)) => self.entry = Some((entry, archived)),
                    Ok(None) => { self.current = None; continue; }
                    Err(error) => {
                        self.failed = true;
                        self.current = None;
                        warn!(%error, "rollout directory enumeration remains incomplete");
                        continue;
                    }
                }
            }
            let (entry, archived) = self.entry.as_ref().expect("discovery owns an entry");
            let archived = *archived;
            let path = entry.path();
            let inspected = entry.file_type().await;
            self.entry.take();
            let file_type = match inspected {
                Ok(file_type) => file_type,
                Err(error) => {
                    self.failed = true;
                    warn!(path = %path.display(), %error, "rollout entry inspection remains incomplete");
                    continue;
                }
            };
            if file_type.is_dir() {
                self.pending.push((path, archived));
            } else if file_type.is_file()
                && let Some(rollout_file) = compression::RolloutFile::from_path(path)
            {
                let path = rollout_file.into_path();
                let watermark = backfill_watermark_for_path(&self.home, &path);
                self.paths.insert(watermark.clone(), BackfillRolloutPath { watermark, path, archived });
            }
        }
        false
    }
}

