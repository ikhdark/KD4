#[path = "raw_output_artifact_task.rs"]
pub(crate) mod capture;

#[cfg(windows)]
#[path = "artifact_snapshot_cache.rs"]
mod immutable_snapshot_cache;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_file_system::AtomicWriteLock;
use codex_file_system::acquire_atomic_write_lock;
use codex_file_system::write_bytes_atomically;
use codex_tools::CanonicalByteRange;
use codex_tools::CanonicalJsonPointer;
use codex_tools::CanonicalToolResult;
use codex_tools::CanonicalToolResultKind;
use codex_tools::ToolProjectionSection;
use codex_utils_string::TokenCountEstimate;
use codex_utils_string::approx_token_count_exceeds;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const ARTIFACT_EXPIRED_MESSAGE: &str = "artifact expired or does not belong to this thread. The producing operation already ran; a missing artifact does not mean it failed or had no effects. Use output already in context or inspect the state it changed; rerun it only if it is read-only or safely repeatable.";
// A command's output writer holds its artifact until the process exits; a
// yielded poll does not release it.
const ARTIFACT_WRITING_MESSAGE: &str = "artifact is still being written; a running command's output artifact becomes readable after the command exits, not when it yields. For a live command, resume it with write_stdin using its original session_id and incarnation; do not poll this artifact or rerun the producer. Other artifact writes finish shortly";
const ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION: &str = "active-tool-history";
const ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES: &[u8] = b"KD4_ACTIVE_TOOL_HISTORY_ARTIFACT_V1\n";
pub(crate) const MAX_RAW_OUTPUT_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES: usize = 64 * 1024;
const STREAMING_ARTIFACT_BUFFER_BYTES: usize = 64 * 1024;
const MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD: u64 = 256 * 1024 * 1024;
// Both byte budgets apply to evictable cache bytes, like the artifact-count
// limits. Durable protected history must not exhaust admission for its own
// thread's recovery directory or for other resumable threads.
const MAX_RETAINED_ARTIFACT_BYTES_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_RETENTION_INDEX_ROOTS: usize = 4;
// Artifacts referenced by durable thread history stay protected, so a root
// holds far more records than the unprotected retention limit. Past this cap
// every commit falls back to full-root scans (~0.3s each on Windows at 11K
// records), so size it for protected growth; memory tracks actual records
// (~0.8 KB each).
const MAX_RETENTION_INDEX_RECORDS: usize = 262_144;
const RETENTION_RECONCILIATION_INTERVAL: u64 = 128;
const EXTERNAL_RETENTION_RECONCILIATION_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);
const RETENTION_BYTE_GUARD_BAND: u64 = MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64;
pub(crate) const RECOVERY_AGGREGATE_TOKEN_CEILING: usize = 10_000;
const RECOVERY_FRAGMENT_TOKEN_CEILING: usize = 1_000;
pub(crate) const RECOVERY_RETRY_AVOIDANCE_TOKEN_MARGIN: usize = 128;
const MAX_AUTOMATIC_SUBDIVISIONS: u64 = 64;
const MAX_AUTOMATIC_SUBDIVISION_BYTES: u64 = 64 * 1_024;
pub(crate) const ARTIFACT_SEARCH_DEFAULT_MAX_RESULTS: usize = 20;
pub(crate) const ARTIFACT_SEARCH_MAX_RESULTS: usize = 100;
pub(crate) const ARTIFACT_SEARCH_MAX_CONTEXT_LINES: usize = 20;
pub(crate) const ARTIFACT_SEARCH_MAX_QUERY_BYTES: usize = 1_024;
const LOGICAL_ARTIFACT_METADATA_VERSION: u8 = 1;


const LOGICAL_ARTIFACT_TRANSACTION_EXTENSION: &str = "transaction";
const LOGICAL_ARTIFACT_CREATE_TRANSACTION_BYTES: &[u8] =
    b"KD4_LOGICAL_ARTIFACT_CREATE_TRANSACTION_V1\n";
const LOGICAL_ARTIFACT_ATTACH_TRANSACTION_BYTES: &[u8] =
    b"KD4_LOGICAL_ARTIFACT_ATTACH_TRANSACTION_V1\n";
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ToolOutputSelector {
    Bytes {
        start: u64,
        end: u64,
    },
    Lines {
        start: usize,
        end: usize,
    },
    Section {
        id: String,
    },
    JsonPointer {
        pointer: String,
    },
    Search {
        query: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        enclosing: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        case_insensitive: bool,
        #[serde(default)]
        start_byte: u64,
        #[serde(default = "artifact_search_default_max_results")]
        max_results: usize,
        #[serde(default = "artifact_search_default_context_lines")]
        context_lines: usize,
    },
}

impl ToolOutputSelector {
    pub(crate) fn is_search(&self) -> bool {
        matches!(self, Self::Search { .. })
    }
}

const fn artifact_search_default_max_results() -> usize {
    ARTIFACT_SEARCH_DEFAULT_MAX_RESULTS
}

const fn artifact_search_default_context_lines() -> usize {
    crate::tools::shell_output_summary::FOCUS_CONTEXT_LINES
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolOutputSelectorStatus {
    Ok,
    SelectorTooLarge,
    AggregateOmitted,
    NotFound,
    Invalid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ByteSubdivisionPlan {
    pub range: CanonicalByteRange,
    pub chunk_bytes: u64,
    pub chunk_count: u64,
    pub selector_kind: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ToolOutputSelectorResult {
    pub selector: ToolOutputSelector,
    pub status: ToolOutputSelectorStatus,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_range: Option<CanonicalByteRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdivision_plan: Option<ByteSubdivisionPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_selectors: Vec<ToolOutputSelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolOutputSelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ToolOutputSelectorResult {
    fn state(selector: ToolOutputSelector, status: ToolOutputSelectorStatus) -> Self {
        let continuation =
            (status == ToolOutputSelectorStatus::AggregateOmitted).then(|| selector.clone());
        Self {
            selector,
            status,
            complete: false,
            exact_bytes: None,
            canonical_range: None,
            text: None,
            value: None,
            data_base64: None,
            subdivision_plan: None,
            child_selectors: Vec::new(),
            continuation,
            message: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct ReadToolOutputResult {
    pub artifact_id: String,
    pub canonical_sha256: String,
    pub canonical_bytes: u64,
    pub retained_bytes: u64,
    // Known complete producer capture, not merely all bytes in the retained log.
    #[serde(default, rename = "retained_artifact_complete")]
    pub capture_complete: bool,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_ranges: Vec<CanonicalByteRange>,
    pub results: Vec<ToolOutputSelectorResult>,
}

impl ReadToolOutputResult {
    /// Requested-page delivery, independent of transport and whole-file coverage.
    pub(crate) fn selection_status(&self) -> &'static str {
        if self.complete
            && !self.results.is_empty()
            && self
                .results
                .iter()
                .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete)
        {
            "complete"
        } else if self
            .results
            .iter()
            .any(|result| result.status == ToolOutputSelectorStatus::Ok)
        {
            "partial"
        } else {
            "failed"
        }
    }

    /// Exact bytes delivered in this response, including hydrated search context.
    /// Coordinates, recovery handles, and shared references alone are not delivery.
    pub(crate) fn delivered_ranges(&self) -> Vec<(u64, u64)> {
        self.payload_ranges(false)
    }

    /// Budget semantic values by their lexical source extent without claiming
    /// that their original spelling was delivered. Reconstruction and overlapping
    /// selectors must neither erase nor double-charge represented source bytes.
    pub(crate) fn represented_source_bytes(&self) -> u64 {
        self.payload_ranges(true)
            .iter()
            .map(|(start, end)| end - start)
            .sum()
    }

    fn payload_ranges(&self, include_semantic: bool) -> Vec<(u64, u64)> {
        let mut ranges = Vec::new();
        for result in &self.results {
            if result.status != ToolOutputSelectorStatus::Ok {
                continue;
            }
            if let Some(range) = result.canonical_range
                && (result.text.is_some()
                    || result.data_base64.is_some()
                    || (include_semantic && !result.selector.is_search() && result.value.is_some()))
            {
                ranges.push((range.start, range.end));
            }
            if result.selector.is_search()
                && let Some(hydrated) = result
                    .value
                    .as_ref()
                    .and_then(|value| value["hydrated_ranges"].as_array())
            {
                for value in hydrated {
                    if (value["text"].is_string() || value["data_base64"].is_string())
                        && let (Some(start), Some(end)) = (
                            value["canonical_range"]["start"].as_u64(),
                            value["canonical_range"]["end"].as_u64(),
                        )
                        && start <= end
                        && end <= self.canonical_bytes
                    {
                        ranges.push((start, end));
                    }
                }
            }
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        merged
    }

    /// Evidence describes delivered coverage, never requested or merely retained bytes.
    /// Equivalent line/byte selectors and overlapping ranges have the same identity.
    pub(crate) fn delivered_evidence(&self) -> Option<Value> {
        let ranges = self.delivered_ranges();
        let mut values = Vec::new();
        for result in &self.results {
            if result.status != ToolOutputSelectorStatus::Ok {
                continue;
            }
            if result.canonical_range.is_some()
                && (result.text.is_some() || result.data_base64.is_some())
            {
                continue;
            }
            if let Some(value) = &result.value {
                // Search values include the query and scanned coverage, including
                // successful negative searches. JSON selections retain their location.
                if let ToolOutputSelector::Search {
                    query,
                    case_insensitive,
                    start_byte,
                    ..
                } = &result.selector
                {
                    let proof = value.as_object().map_or_else(|| value.clone(), |fields| {
                        Value::Object(fields.iter()
                            .filter(|(key, _)| key.as_str() != "hydrated_ranges")
                            .map(|(key, value)| (key.clone(), value.clone())).collect())
                    });
                    values.push(serde_json::json!({
                        "matching": {"query": query, "case_insensitive": case_insensitive, "start_byte": start_byte},
                        "value": proof,
                    }));
                } else {
                    values.push(serde_json::json!({
                        "selector": result.selector,
                        "value": value,
                    }));
                }
            }
        }
        if ranges.is_empty() && values.is_empty() {
            return None;
        }
        values.sort_by_cached_key(Value::to_string);
        values.dedup();
        Some(serde_json::json!({
            "sha256": self.canonical_sha256,
            "ranges": ranges,
            "values": values,
        }))
    }
}

impl Serialize for ReadToolOutputResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut result = serializer.serialize_struct("ReadToolOutputResult", 10)?;
        result.serialize_field("artifact_id", &self.artifact_id)?;
        result.serialize_field("canonical_sha256", &self.canonical_sha256)?;
        result.serialize_field("canonical_bytes", &self.canonical_bytes)?;
        result.serialize_field("retained_bytes", &self.retained_bytes)?;
        // `complete` remains the legacy selection-completeness field.
        result.serialize_field("complete", &self.complete)?;
        result.serialize_field("selection_status", &self.selection_status())?;
        result.serialize_field("delivered_selection_complete", &self.complete)?;
        result.serialize_field(
            "retained_artifact_complete",
            &(self.capture_complete
                && self.retained_bytes == self.canonical_bytes
                && self.unavailable_ranges.is_empty()),
        )?;
        if !self.unavailable_ranges.is_empty() {
            result.serialize_field("unavailable_ranges", &self.unavailable_ranges)?;
        }
        result.serialize_field("results", &self.results)?;
        result.end()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LogicalArtifactSegment {
    index: u32,
    range: CanonicalByteRange,
}

fn known_capture_complete() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct LogicalArtifactMetadata {
    version: u8,
    artifact_id: String,
    canonical_kind: CanonicalToolResultKind,
    canonical_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_sha256: Option<String>,
    canonical_bytes: u64,
    retained_bytes: u64,
    // Persisted logical metadata authenticates canonical completeness; a raw
    // log without it cannot establish whether producer output was lost.
    #[serde(default = "known_capture_complete")]
    capture_complete: bool,
    complete: bool,
    unavailable_ranges: Vec<CanonicalByteRange>,
    json_pointers: BTreeMap<String, CanonicalJsonPointer>,
    sections: Vec<ToolProjectionSection>,
    line_starts: Vec<u64>,
    segments: Vec<LogicalArtifactSegment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanonicalOutputArtifact {
    pub(crate) id: Option<ToolOutputArtifactId>,
    pub(crate) retained_bytes: u64,
    pub(crate) complete: bool,
    pub(crate) unavailable_ranges: Vec<CanonicalByteRange>,
    pub(crate) error: Option<String>,
}

impl CanonicalOutputArtifact {
    pub(crate) fn artifact_id(&self) -> Option<String> {
        self.id.map(|id| id.to_string())
    }
}




#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionModeKind {
    Indexed,
    Dirty,
    Reconciling,
    ScanOnly,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionDeltaDisposition {
    ApplyIndexed,
    IgnoreScanOnly,
    RejectCurrent,
    RejectStale,
}

#[derive(Debug, Clone)]
struct ArtifactRetentionRecord {
    path: PathBuf,
    thread_directory: PathBuf,
    bytes: u64,
    modified: SystemTime,
    protected: bool,
}

#[derive(Debug, Clone, Default)]
struct ThreadRetentionIndex {
    artifacts: usize,
    unprotected_paths: BTreeSet<PathBuf>,
    bytes: u64,
    unprotected_bytes: u64,
    unprotected: usize,
}

impl ThreadRetentionIndex {
    fn is_near_limit(&self) -> bool {
        self.unprotected >= max_retained_artifacts_per_thread().saturating_sub(1)
            || self.unprotected_bytes >= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
                .saturating_sub(RETENTION_BYTE_GUARD_BAND)
    }
}

#[derive(Debug, Clone, Default)]
struct RetentionIndex {
    records: BTreeMap<PathBuf, ArtifactRetentionRecord>,
    threads: BTreeMap<PathBuf, ThreadRetentionIndex>,
    global_order: BTreeSet<(SystemTime, PathBuf)>,
    total_bytes: u64,
    unprotected_bytes: u64,
    unprotected: usize,
    near_limit_threads: usize,
    logical_mutations_since_reconciliation: u64,
    near_limit_reconciled: bool,
    near_limit_pending: bool,
    background_reconciliation_pending: bool,
}

impl RetentionIndex {
    // Callers normalize keys before taking the registry mutex. Index operations
    // must not resolve filesystem paths while that shared mutex is held.
    fn insert(&mut self, record: ArtifactRetentionRecord) -> bool {
        self.remove(&record.path);
        let Some(thread_bytes) = self
            .threads
            .get(&record.thread_directory)
            .map_or(Some(record.bytes), |thread| {
                thread.bytes.checked_add(record.bytes)
            })
        else {
            return false;
        };
        let Some(total_bytes) = self.total_bytes.checked_add(record.bytes) else {
            return false;
        };
        let thread = self
            .threads
            .entry(record.thread_directory.clone())
            .or_default();
        let was_near = thread.is_near_limit();
        thread.artifacts = thread.artifacts.saturating_add(1);
        thread.bytes = thread_bytes;
        if !record.protected {
            thread.unprotected_paths.insert(record.path.clone());
            thread.unprotected_bytes += record.bytes;
            thread.unprotected = thread.unprotected.saturating_add(1);
            self.unprotected = self.unprotected.saturating_add(1);
            // Bounded by the checked total_bytes above.
            self.unprotected_bytes += record.bytes;
        }
        self.near_limit_threads += usize::from(!was_near && thread.is_near_limit());
        self.total_bytes = total_bytes;
        if !record.protected {
            self.global_order.insert((record.modified, record.path.clone()));
        }
        self.records.insert(record.path.clone(), record);
        true
    }

    fn remove(&mut self, path: &Path) -> Option<ArtifactRetentionRecord> {
        let record = self.records.remove(path)?;
        self.global_order
            .remove(&(record.modified, record.path.clone()));
        self.total_bytes = self.total_bytes.saturating_sub(record.bytes);
        if !record.protected {
            self.unprotected = self.unprotected.saturating_sub(1);
            self.unprotected_bytes = self.unprotected_bytes.saturating_sub(record.bytes);
        }
        let remove_thread = if let Some(thread) = self.threads.get_mut(&record.thread_directory) {
            let was_near = thread.is_near_limit();
            thread.artifacts = thread.artifacts.saturating_sub(1);
            thread.unprotected_paths.remove(path);
            thread.bytes = thread.bytes.saturating_sub(record.bytes);
            if !record.protected {
                thread.unprotected_bytes = thread.unprotected_bytes.saturating_sub(record.bytes);
                thread.unprotected = thread.unprotected.saturating_sub(1);
            }
            self.near_limit_threads -= usize::from(was_near && !thread.is_near_limit());
            thread.artifacts == 0
        } else {
            false
        };
        if remove_thread {
            self.threads.remove(&record.thread_directory);
        }
        Some(record)
    }

    fn update_streaming_size(&mut self, path: &Path, bytes: u64, modified: SystemTime) -> bool {
        let Some(mut record) = self.remove(path) else {
            return false;
        };
        record.bytes = bytes;
        record.modified = modified;
        self.insert(record)
    }

    fn thread_cache_totals(&self, directory: &Path) -> (u64, usize) {
        self.threads.get(directory).map_or((0, 0), |thread| {
            (thread.unprotected_bytes, thread.unprotected)
        })
    }

    fn is_near_limit(&self) -> bool {
        let global_near = self.unprotected >= max_retained_artifacts_total().saturating_sub(1)
            || self.unprotected_bytes
                >= MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(RETENTION_BYTE_GUARD_BAND);
        global_near || self.near_limit_threads != 0
    }

    fn note_logical_mutation(&mut self) {
        self.logical_mutations_since_reconciliation = self
            .logical_mutations_since_reconciliation
            .saturating_add(1);
    }
}

#[derive(Debug)]
enum RetentionRootMode {
    Indexed(RetentionIndex),
    Dirty,
    Reconciling { invalidated: bool },
    ScanOnly { operations_since_probe: u64 },
}

#[derive(Debug)]
struct RetentionRootState {
    generation: u64,
    mode: RetentionRootMode,
    last_access: u64,
    /// When this process last scheduled a rescan for other processes' writes.
    /// Kept outside the index, which each rescan replaces.
    last_external_reconciliation: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
struct RetentionRegistry {
    roots: BTreeMap<PathBuf, RetentionRootState>,
    pending_canonical: BTreeSet<PathBuf>,
    failed_canonical: BTreeMap<PathBuf, String>,
    access_clock: u64,
    pending_sync: BTreeMap<PathBuf, u64>,
    next_sync: u64,
}

#[derive(Debug, Clone)]
struct RetentionIndexToken {
    root: PathBuf,
    generation: Option<u64>,
    starting_mode: RetentionModeKind,
}

#[derive(Debug, Clone, Copy)]
enum LogicalRetentionMutation {
    Create,
    Delete,
    AppendReplace,
    Protection,
    Cleanup,
}

#[derive(Debug)]
enum InactiveRemovalOutcome {
    RemovedOrAbsent,
    Active,
    Ambiguous(std::io::Error),
}

#[derive(Debug)]
enum RetentionScanCandidate {
    Indexed(RetentionIndex),
    Oversized,
}

#[derive(Debug, Clone, Copy, Default)]
struct RetentionUsage {
    thread_bytes: u64,
    global_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct RetentionScanProgress {
    complete: bool,
    directories_visited: u64,
    candidates_visited: u64,
}

pub(crate) fn max_retained_artifacts_per_thread() -> usize {
    128
}

/// The 2 GiB byte limit bounds disk use; at the ~195 KiB mean unprotected
/// artifact this count binds near 1.5 GiB. At 1,024 a shared root sat at the
/// count limit, so every commit evicted and near-limit rescans recurred.
fn max_retained_artifacts_total() -> usize {
    8_192
}

fn retention_registry() -> &'static StdMutex<RetentionRegistry> {
    static RETENTION_REGISTRY: OnceLock<StdMutex<RetentionRegistry>> = OnceLock::new();
    RETENTION_REGISTRY.get_or_init(|| StdMutex::new(RetentionRegistry::default()))
}

fn lock_retention_registry() -> std::sync::MutexGuard<'static, RetentionRegistry> {
    retention_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn indexing_disabled() -> &'static AtomicBool {
    static DISABLED: AtomicBool = AtomicBool::new(false);
    &DISABLED
}

fn next_index_generation() -> Option<u64> {
    static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
    if indexing_disabled().load(Ordering::Acquire) {
        return None;
    }
    match NEXT_GENERATION.try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
        generation.checked_add(1)
    }) {
        Ok(generation) => Some(generation),
        Err(_) => {
            indexing_disabled().store(true, Ordering::Release);
            None
        }
    }
}

fn normalized_tool_output_root(root: &Path) -> PathBuf {
    normalized_retention_path(root)
}

fn normalized_retention_path(path: &Path) -> PathBuf {
    if let Ok(path) = dunce::canonicalize(path) {
        return path;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = dunce::canonicalize(parent)
    {
        return parent.join(name);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn tool_output_root_for_directory(directory: &Path) -> PathBuf {
    normalized_tool_output_root(directory.parent().unwrap_or_else(|| Path::new(".")))
}

fn root_mode_kind(mode: &RetentionRootMode) -> RetentionModeKind {
    match mode {
        RetentionRootMode::Indexed(_) => RetentionModeKind::Indexed,
        RetentionRootMode::Dirty => RetentionModeKind::Dirty,
        RetentionRootMode::Reconciling { .. } => RetentionModeKind::Reconciling,
        RetentionRootMode::ScanOnly { .. } => RetentionModeKind::ScanOnly,
    }
}

fn evict_registry_root_if_needed(registry: &mut RetentionRegistry, keep_root: &Path) {
    if registry.roots.len() < MAX_RETENTION_INDEX_ROOTS || registry.roots.contains_key(keep_root) {
        return;
    }
    let oldest = registry
        .roots
        .iter()
        .min_by_key(|(_, state)| state.last_access)
        .map(|(root, _)| root.clone());
    if let Some(oldest) = oldest {
        registry.roots.remove(&oldest);
    }
}

fn insert_dirty_root(registry: &mut RetentionRegistry, root: PathBuf) -> Option<u64> {
    let generation = next_index_generation()?;
    evict_registry_root_if_needed(registry, &root);
    registry.access_clock = registry.access_clock.saturating_add(1);
    registry.roots.insert(
        root,
        RetentionRootState {
            generation,
            mode: RetentionRootMode::Dirty,
            last_access: registry.access_clock,
            last_external_reconciliation: None,
        },
    );
    Some(generation)
}

fn capture_retention_token(directory: &Path) -> RetentionIndexToken {
    let root = tool_output_root_for_directory(directory);
    if indexing_disabled().load(Ordering::Acquire) {
        return RetentionIndexToken {
            root,
            generation: None,
            starting_mode: RetentionModeKind::Disabled,
        };
    }
    let mut registry = lock_retention_registry();
    registry.access_clock = registry.access_clock.saturating_add(1);
    let access = registry.access_clock;
    if let Some(state) = registry.roots.get_mut(&root) {
        state.last_access = access;
        return RetentionIndexToken {
            root,
            generation: Some(state.generation),
            starting_mode: root_mode_kind(&state.mode),
        };
    }
    let generation = insert_dirty_root(&mut registry, root.clone());
    RetentionIndexToken {
        root,
        generation,
        starting_mode: if generation.is_some() {
            RetentionModeKind::Dirty
        } else {
            RetentionModeKind::Disabled
        },
    }
}

async fn capture_retention_token_async(directory: &Path) -> std::io::Result<RetentionIndexToken> {
    let directory = directory.to_path_buf();
    run_blocking_artifact_io(move || Ok(capture_retention_token(&directory))).await
}

fn transition_current_root_to_dirty(registry: &mut RetentionRegistry, root: &Path) {
    transition_root_to_dirty(registry, root, true);
}

fn transition_current_root_to_dirty_for_conflict(registry: &mut RetentionRegistry, root: &Path) {
    transition_root_to_dirty(registry, root, false);
}

fn transition_root_to_dirty(
    registry: &mut RetentionRegistry,
    root: &Path,
    #[allow(unused_variables)] stale_delta_rejected: bool,
) {
    let Some(state) = registry.roots.get_mut(root) else {
        let _ = insert_dirty_root(registry, root.to_path_buf());
        return;
    };
    match &mut state.mode {
        RetentionRootMode::Dirty => {
            if let Some(generation) = next_index_generation() {
                state.generation = generation;
            }
        }
        RetentionRootMode::Reconciling { invalidated } => {
            *invalidated = true;
        }
        RetentionRootMode::ScanOnly { .. } => {
            let Some(generation) = next_index_generation() else {
                indexing_disabled().store(true, Ordering::Release);
                return;
            };
            state.generation = generation;
            state.mode = RetentionRootMode::Dirty;
        }
        RetentionRootMode::Indexed(_) => {
            let Some(generation) = next_index_generation() else {
                indexing_disabled().store(true, Ordering::Release);
                return;
            };
            state.generation = generation;
            state.mode = RetentionRootMode::Dirty;
        }
    }
}

fn retention_delta_disposition(
    token: &RetentionIndexToken,
    state: &RetentionRootState,
) -> RetentionDeltaDisposition {
    if token.generation != Some(state.generation) {
        return RetentionDeltaDisposition::RejectStale;
    }
    match (token.starting_mode, root_mode_kind(&state.mode)) {
        (RetentionModeKind::Indexed, RetentionModeKind::Indexed) => {
            RetentionDeltaDisposition::ApplyIndexed
        }
        (RetentionModeKind::ScanOnly, RetentionModeKind::ScanOnly) => {
            RetentionDeltaDisposition::IgnoreScanOnly
        }
        _ => RetentionDeltaDisposition::RejectCurrent,
    }
}

fn reject_stale_delta(token: &RetentionIndexToken) {
    debug_assert_eq!(
        token.generation.is_none(),
        token.starting_mode == RetentionModeKind::Disabled
    );
    if token.generation.is_none() {
        return;
    }
    let mut registry = lock_retention_registry();
    transition_current_root_to_dirty(&mut registry, &token.root);
}

fn protection_marker_status(marker: &Path, expected: &[u8]) -> std::io::Result<bool> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let mut file = match options.open(marker) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() != expected.len() as u64
    {
        return Ok(false);
    }
    let mut bytes = vec![0_u8; expected.len()];
    match file.read_exact(&mut bytes) {
        Ok(()) if bytes == expected => {}
        Ok(()) => return Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(err) => return Err(err),
    }
    let mut trailing = [0_u8; 1];
    match file.read(&mut trailing) {
        Ok(0) => Ok(true),
        Ok(_) => Ok(false),
        Err(err) => Err(err),
    }
}

fn retention_protection_marker_status(marker: &Path, expected: &[u8]) -> std::io::Result<bool> {
    match protection_marker_status(marker, expected) {
        Ok(true) => Ok(true),
        Ok(false) => match std::fs::symlink_metadata(marker) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid retention protection marker `{}`", marker.display()),
            )),
            Err(err) => Err(err),
        },
        Err(err) => Err(err),
    }
}


fn artifact_retention_record_blocking(
    path: &Path,
) -> std::io::Result<Option<ArtifactRetentionRecord>> {
    let bytes = logical_artifact_disk_bytes(path)?;
    artifact_retention_record_with_bytes_blocking(path, bytes)
}

fn artifact_retention_record_with_bytes_blocking(
    path: &Path,
    bytes: u64,
) -> std::io::Result<Option<ArtifactRetentionRecord>> {
    let link_metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if link_metadata.file_type().is_symlink() || metadata_is_reparse_point(&link_metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is a link or reparse point",
        ));
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is not a regular file",
        ));
    }
    let tool_history_marker = active_tool_history_protection_path(path);
    let protected = retention_protection_marker_status(
        &tool_history_marker,
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
    )?;
    let final_link_metadata = std::fs::symlink_metadata(path)?;
    if final_link_metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&final_link_metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path changed to a link or reparse point during retention observation",
        ));
    }
    let final_metadata = std::fs::metadata(path)?;
    if final_metadata.len() != metadata.len()
        || final_metadata.modified()? != metadata.modified()?
        || !final_metadata.is_file()
        || metadata_is_reparse_point(&final_metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact metadata changed during retention observation",
        ));
    }
    let path = normalized_retention_path(path);
    Ok(Some(ArtifactRetentionRecord {
        path: path.clone(),
        thread_directory: path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        bytes,
        modified: metadata.modified()?,
        protected,
    }))
}

/// Retention record from directory-listing metadata. Reconciliation discards
/// a scan that raced an internal mutation, so the scan needs no per-file reopen.
fn listed_retention_record(
    path: &Path,
    bytes: u64,
    metadata: &std::fs::Metadata,
    marker: Option<&(PathBuf, std::fs::Metadata)>,
) -> std::io::Result<ArtifactRetentionRecord> {
    if metadata.file_type().is_symlink() || metadata_is_reparse_point(metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is a link or reparse point",
        ));
    }
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path is not a regular file",
        ));
    }
    let protected = match marker {
        None => false,
        Some((_, marker))
            if marker.is_file()
                && !metadata_is_reparse_point(marker)
                && marker.len() == ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES.len() as u64 =>
        {
            true
        }
        Some((marker_path, _)) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid retention protection marker `{}`",
                    marker_path.display()
                ),
            ));
        }
    };
    // The scan root is normalized and listing names are exact, so the joined
    // path is already normalized; canonicalizing would reopen every artifact.
    let path = path.to_path_buf();
    Ok(ArtifactRetentionRecord {
        thread_directory: path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        path,
        bytes,
        modified: metadata.modified()?,
        protected,
    })
}

fn logical_artifact_stem(name: &str) -> Option<&str> {
    if let Some(stem) = name.strip_suffix(".log") {
        return Some(stem);
    }
    if let Some(stem) = name.strip_suffix(".meta.json") {
        return Some(stem);
    }
    let (stem, segment) = name.rsplit_once(".segment-")?;
    (!stem.is_empty() && !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(stem)
}

/// One thread directory's retention records, read from its listing.
struct ThreadRetentionListing {
    records: Vec<ArtifactRetentionRecord>,
    /// Artifacts whose transaction markers were listed.
    transactions: Vec<PathBuf>,
    candidates: u64,
    /// More logs were listed than the record budget allowed.
    over_budget: bool,
}

/// Lists one thread directory. Listing metadata carries size, times, and link
/// attributes, so this opens no files; per-artifact opens took ~6 s at 14.5K
/// records. Records are collected only while at most `record_budget` logs have
/// been listed. Listings can run without the shared lock, so an entry may
/// vanish mid-listing. Returns `None` when the directory no longer exists.
fn list_thread_retention_directory_blocking(
    directory: &Path,
    record_budget: Option<usize>,
) -> std::io::Result<Option<ThreadRetentionListing>> {
    let mut entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut listing = ThreadRetentionListing {
        records: Vec::new(),
        transactions: Vec::new(),
        candidates: 0,
        over_budget: false,
    };
    let mut collect = record_budget.is_some();
    let mut artifacts_by_stem =
        BTreeMap::<String, (u64, Option<(PathBuf, std::fs::Metadata)>)>::new();
    let mut markers = BTreeMap::<String, (PathBuf, std::fs::Metadata)>::new();
    let mut has_staging_entries = false;
    while let Some(entry) = entries.next().transpose()? {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        has_staging_entries |= name.starts_with('.');
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension == Some(LOGICAL_ARTIFACT_TRANSACTION_EXTENSION) {
            listing.transactions.push(path.with_extension("log"));
            continue;
        }
        let is_log = extension == Some("log");
        if is_log {
            listing.candidates = listing.candidates.saturating_add(1);
            if collect && record_budget.is_some_and(|budget| listing.candidates as usize > budget) {
                collect = false;
                listing.over_budget = true;
                artifacts_by_stem.clear();
            }
        }
        if !collect {
            continue;
        }
        let marker_stem = name
            .strip_suffix(ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION)
            .and_then(|name| name.strip_suffix('.'));
        let Some(stem) = marker_stem.or_else(|| logical_artifact_stem(name)) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if marker_stem.is_some() {
            markers.insert(stem.to_string(), (path, metadata));
            continue;
        }
        let bytes = metadata.len();
        let (entry_bytes, log_path) = if let Some(entry) = artifacts_by_stem.get_mut(stem) {
            entry
        } else {
            artifacts_by_stem.entry(stem.to_string()).or_default()
        };
        *entry_bytes = entry_bytes.checked_add(bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "logical artifact byte total overflowed",
            )
        })?;
        if is_log {
            *log_path = Some((path, metadata));
        }
    }
    // Ordinary immutable families have no hidden staging files. Reuse this
    // listing to avoid two more full directory walks and a residency-lock
    // open for every inactive thread on a fresh process's first admission.
    // Staging cleanup touches only private pending files, not the committed
    // log/sidecar families collected above; its ownership locks stay intact.
    if has_staging_entries {
        recover_abandoned_stages(directory);
    }
    if collect {
        for (stem, (bytes, log)) in artifacts_by_stem {
            let Some((path, metadata)) = log else {
                continue;
            };
            listing.records.push(listed_retention_record(
                &path,
                bytes,
                &metadata,
                markers.get(&stem),
            )?);
        }
    }
    Ok(Some(listing))
}

/// The thread directories that a listing of `root` reaches.
fn retention_thread_directories_blocking(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => directories.push(entry.path()),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(directories)
}

/// Also returns the artifacts whose transaction markers were listed.
fn scan_retention_root_blocking(
    root: &Path,
    capacity: usize,
) -> std::io::Result<(RetentionScanCandidate, u64, u64, Vec<PathBuf>)> {

    let mut index = Some(RetentionIndex::default());
    let mut directories_visited = 0_u64;
    let mut candidates_visited = 0_u64;
    let mut transactions = Vec::new();
    for directory in retention_thread_directories_blocking(root)? {
        let record_budget = index
            .as_ref()
            .map(|_| capacity.saturating_sub(candidates_visited as usize));
        let Some(listing) = list_thread_retention_directory_blocking(&directory, record_budget)?
        else {
            continue;
        };
        directories_visited = directories_visited.saturating_add(1);
        candidates_visited = candidates_visited.saturating_add(listing.candidates);
        let room = capacity.saturating_sub(transactions.len());
        transactions.extend(listing.transactions.into_iter().take(room));
        if listing.over_budget {
            index = None;
        }
        let Some(candidate_index) = index.as_mut() else {
            continue;
        };
        for record in listing.records {
            if !candidate_index.insert(record) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "retention index byte totals overflowed",
                ));
            }
        }
    }
    let candidate = match index {
        Some(mut index) => {
            index.near_limit_reconciled = index.is_near_limit();
            RetentionScanCandidate::Indexed(index)
        }
        None => RetentionScanCandidate::Oversized,
    };
    Ok((
        candidate,
        directories_visited,
        candidates_visited,
        transactions,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolOutputArtifactId(uuid::Uuid);

impl ToolOutputArtifactId {
    pub(super) fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl fmt::Display for ToolOutputArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ToolOutputArtifactId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(value).map(Self)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum RawOutputArtifact {
    Pending {
        codex_home: PathBuf,
        thread_id: String,
    },
    Stored {
        id: ToolOutputArtifactId,
        path: PathBuf,
        bytes: u64,
        truncated: bool,
        handle: Arc<File>,
        digest: Sha256,
        finalized: bool,
    },
    Failed {
        id: Option<ToolOutputArtifactId>,
        message: String,
        owned_path: Option<PathBuf>,
        bytes: u64,
    },
}

impl PartialEq for RawOutputArtifact {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Pending {
                    codex_home: left_home,
                    thread_id: left_thread,
                },
                Self::Pending {
                    codex_home: right_home,
                    thread_id: right_thread,
                },
            ) => left_home == right_home && left_thread == right_thread,
            (
                Self::Stored {
                    id: left_id,
                    path: left_path,
                    bytes: left_bytes,
                    truncated: left_truncated,
                    ..
                },
                Self::Stored {
                    id: right_id,
                    path: right_path,
                    bytes: right_bytes,
                    truncated: right_truncated,
                    ..
                },
            ) => {
                left_id == right_id
                    && left_path == right_path
                    && left_bytes == right_bytes
                    && left_truncated == right_truncated
            }
            (
                Self::Failed {
                    id: left_id,
                    message: left_message,
                    owned_path: left_path,
                    bytes: left_bytes,
                },
                Self::Failed {
                    id: right_id,
                    message: right_message,
                    owned_path: right_path,
                    bytes: right_bytes,
                },
            ) => {
                left_id == right_id
                    && left_message == right_message
                    && left_path == right_path
                    && left_bytes == right_bytes
            }
            _ => false,
        }
    }
}

impl Eq for RawOutputArtifact {}




fn defer_artifact_sync(path: &Path) {
    let mut registry = lock_retention_registry();
    registry.next_sync = registry.next_sync.saturating_add(1);
    let sequence = registry.next_sync;
    registry.pending_sync.insert(path.to_path_buf(), sequence);
}

pub(crate) fn artifact_residency_file(
    home: &Path,
    thread_id: &str,
    exclusive: bool,
) -> std::io::Result<File> {
    let directory = home.join("tool-output-residency");
    std::fs::create_dir_all(&directory)?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("{thread_id}.lock")))?;
    if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    }
    .map_err(std::io::Error::from)?;
    Ok(file)
}

pub(crate) async fn acquire_artifact_residency(
    home: &Path,
    thread_id: &str,
) -> std::io::Result<Arc<File>> {
    let home = home.to_path_buf();
    let thread_id = thread_id.to_string();
    run_blocking_artifact_io(move || {
        artifact_residency_file(&home, &thread_id, false).map(Arc::new)
    })
    .await
}

pub(crate) async fn retain_external_rollout_ownership(
    home: &Path,
    thread_id: &str,
    path: &Path,
) -> std::io::Result<()> {
    if path.starts_with(home.join(codex_rollout::SESSIONS_SUBDIR))
        || path.starts_with(home.join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR))
    {
        return Ok(());
    }
    let marker = home
        .join("tool-output-residency")
        .join(format!("{thread_id}.external-rollout"));
    let bytes = serde_json::to_vec(path).map_err(std::io::Error::other)?;
    run_blocking_artifact_io(move || write_bytes_atomically(&marker, &bytes)).await
}

/// Flush artifacts before the rollout containing their references becomes
/// durable. Failed barriers retain their work; concurrent writes retain a newer
/// sequence and cannot be acknowledged by an older barrier.
pub(crate) async fn sync_tool_output_artifacts(
    codex_home: &Path,
    thread_id: &str,
) -> std::io::Result<()> {
    let directory = codex_home.join("tool-output").join(thread_id);
    // Preserve the full dirty-snapshot durability contract. The common empty
    // barrier needs no blocking worker; writes arriving after this observation
    // remain pending for the next barrier, just as with the worker snapshot.
    if !lock_retention_registry()
        .pending_sync
        .keys()
        .any(|path| path.starts_with(&directory))
    {
        return Ok(());
    }
    run_blocking_artifact_io(move || {
        let pending = lock_retention_registry()
            .pending_sync
            .iter()
            .filter(|(path, _)| path.starts_with(&directory))
            .map(|(path, sequence)| (path.clone(), *sequence))
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(());
        }
        // Recovery's Windows residency denies write opens, including fsync's
        // handle. Reuse the cache owner's bounded release/suppression guard;
        // immutable returned bytes survive, but later reads reauthenticate.
        #[cfg(windows)]
        let _cache_mutation = immutable_snapshot_cache::shared().mutating(&directory);
        sync_artifact_files(&pending, |path| {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                Ok(file) => file.sync_all(),
                // Only an owner-confirmed removal retires pending durability.
                // A missing file by itself may be lost referenced evidence.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !lock_retention_registry().pending_sync.contains_key(path) =>
                {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })?;
        sync_directory(&directory)?;
        let mut registry = lock_retention_registry();
        for (path, sequence) in pending {
            if registry.pending_sync.get(&path) == Some(&sequence) {
                registry.pending_sync.remove(&path);
            }
        }
        Ok(())
    })
    .await
}

/// Keep file fsyncs inside the blocking barrier, with bounded parallelism. The
/// scope joins every worker even on error, before directory sync or acknowledgement.
fn sync_artifact_files(
    pending: &[(PathBuf, u64)],
    sync_file: impl Fn(&Path) -> std::io::Result<()> + Sync,
) -> std::io::Result<()> {
    const MAX_CONCURRENT_SYNCS: usize = 8;
    if pending.len() <= 1 {
        return pending.iter().try_for_each(|(path, _)| sync_file(path));
    }
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for chunk in pending.chunks(pending.len().div_ceil(MAX_CONCURRENT_SYNCS)) {
            let sync_file = &sync_file;
            workers.push(std::thread::Builder::new().spawn_scoped(scope, move || {
                chunk.iter().try_for_each(|(path, _)| sync_file(path))
            })?);
        }
        workers.into_iter().try_for_each(|worker| {
            worker
                .join()
                .map_err(|_| std::io::Error::other("artifact sync worker panicked"))?
        })
    })
}






// A duplicated handle can share its lock with the retained artifact handle.
// Keep explicit unlock ownership until the async caller accepts the worker result.
struct LockedOutputFile(Option<File>);

impl LockedOutputFile {
    #[expect(
        clippy::expect_used,
        reason = "The owned handle is initialized once and taken only while consuming this wrapper"
    )]
    fn into_file(mut self) -> File {
        self.0.take().expect("locked output file ownership")
    }
}

impl Drop for LockedOutputFile {
    fn drop(&mut self) {
        if let Some(file) = self.0.as_ref() {
            let _ = file.unlock();
        }
    }
}

async fn lock_output_file(file: tokio::fs::File) -> std::io::Result<tokio::fs::File> {
    let file = file.into_std().await;
    let file = run_blocking_artifact_io(move || {
        file.try_lock()?;
        let file = LockedOutputFile(Some(file));
        Ok(file)
    })
    .await?;
    Ok(tokio::fs::File::from_std(file.into_file()))
}

#[expect(
    clippy::expect_used,
    reason = "The wrapper is initialized with this handle and consumed only after unlocking"
)]
async fn unlock_output_file(file: tokio::fs::File) -> std::io::Result<()> {
    let file = LockedOutputFile(Some(file.into_std().await));
    run_blocking_artifact_io(move || {
        let result = file
            .0
            .as_ref()
            .expect("locked output file ownership")
            .unlock();
        if result.is_ok() {
            drop(file.into_file());
        }
        result
    })
    .await
}

async fn lock_artifact_handle(handle: &Arc<File>, position: SeekFrom) -> std::io::Result<File> {
    let handle = Arc::clone(handle);
    run_blocking_artifact_io(move || {
        let mut file = handle.try_clone()?;
        file.seek(position)?;
        Ok(file)
    })
    .await
}


async fn unlock_created_output_file(file: File) -> std::io::Result<File> {
    run_blocking_artifact_io(move || {
        file.unlock()?;
        Ok(file)
    })
    .await
}

impl RawOutputArtifact {
    pub(crate) fn is_plain_command_log(&self) -> bool {
        matches!(self, Self::Stored { path, .. } | Self::Failed { owned_path: Some(path), .. }
            if path.parent().and_then(|p| p.parent()).and_then(|p| p.file_name())
                == Some(std::ffi::OsStr::new("command-output")))
    }

    pub(crate) fn capture_finalized_complete(&self) -> bool {
        matches!(
            self,
            Self::Stored {
                finalized: true,
                truncated: false,
                ..
            }
        )
    }


    pub(crate) fn pending(codex_home: &Path, thread_id: &str) -> Self {
        Self::Pending {
            codex_home: codex_home.to_path_buf(),
            thread_id: thread_id.to_string(),
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Failed {
            id: None,
            message: message.into(),
            owned_path: None,
            bytes: 0,
        }
    }

    pub(crate) fn render_for_model(&self) -> String {
        match self {
            Self::Pending { .. } => String::new(),
            Self::Stored {
                id,
                bytes,
                truncated,
                ..
            } => {
                let suffix = if *truncated {
                    ", incomplete capture"
                } else {
                    ""
                };
                format!("Raw output artifact: {id} ({bytes} bytes retained{suffix})")
            }
            Self::Failed {
                id: Some(id),
                bytes,
                ..
            } => format!("Raw output artifact {id} unavailable ({bytes} bytes retained)"),
            Self::Failed { .. } => "Raw output artifact unavailable".to_string(),
        }
    }

    pub(crate) fn model_projection(
        &self,
    ) -> (Option<ToolOutputArtifactId>, Option<u64>, Option<String>) {
        match self {
            Self::Pending { .. } => (None, None, None),
            Self::Stored { id, bytes, .. } => (Some(*id), Some(*bytes), None),
            Self::Failed { id, bytes, .. } if self.is_plain_command_log() => (
                *id, Some(*bytes), Some("command log capture is incomplete".to_string()),
            ),
            Self::Failed { .. } => (
                None,
                None,
                Some("raw output artifact storage failed".to_string()),
            ),
        }
    }

    pub(crate) fn reduction_notice(&self, source: &str, projection: &str) -> Option<String> {
        let Self::Stored { id, bytes, .. } = self else {
            return None;
        };
        if *bytes != source.len() as u64 {
            return None;
        }
        let (start, end) =
            codex_utils_output_truncation::first_omitted_line_range(source, projection)?;
        Some(format!(
            "[command output reduced; recover the first omitted lines with read_tool_output: {{\"artifact_id\":\"{id}\",\"selectors\":[{{\"kind\":\"lines\",\"start\":{start},\"end\":{end}}}]}}. Selection completeness does not mean full artifact delivery; do not rerun the producer.]"
        ))
    }

    pub(crate) fn retained_bytes(&self) -> Option<u64> {
        match self {
            Self::Pending { .. } => None,
            Self::Stored { bytes, .. } => Some(*bytes),
            Self::Failed { .. } => None,
        }
    }

    /// Exclusive end that recovery selectors over this capture's stream offsets
    /// may name. A live writer has not received the producer's coalesced bytes
    /// yet, and readers wait for finish, which appends them; only finalized
    /// lengths are final. Live retention never exceeds the raw cap.
    pub(crate) fn recovery_selector_bound(&self) -> Option<u64> {
        match self {
            Self::Stored {
                bytes,
                finalized: true,
                ..
            } => Some(*bytes),
            Self::Stored { bytes, .. } => Some((*bytes).max(MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64)),
            Self::Pending { .. } | Self::Failed { .. } => None,
        }
    }

    pub(crate) fn retention_limit_hit(&self) -> bool {
        !self.is_plain_command_log() && matches!(
            self,
            Self::Stored {
                truncated: true,
                ..
            }
        )
    }

    pub(crate) fn retention_limit_reason(&self) -> Option<&'static str> {
        self.retention_limit_hit()
            .then_some("per_artifact_safety_limit")
    }
}

pub(crate) async fn create_raw_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
) -> RawOutputArtifact {
    create_raw_output_artifact_inner(codex_home, thread_id, output, true).await
}

async fn create_raw_output_artifact_inner(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
    durable: bool,
) -> RawOutputArtifact {
    let directory = codex_home.join("tool-output").join(thread_id);
    let _residency = match acquire_artifact_residency(codex_home, thread_id).await {
        Ok(owner) => owner,
        Err(error) => {
            return RawOutputArtifact::unavailable(format!(
                "artifact residency is unavailable: {error}"
            ));
        }
    };
    if let Err(err) = tokio::fs::create_dir_all(&directory).await {
        return RawOutputArtifact::unavailable(format!(
            "failed to create `{}`: {err}",
            directory.display()
        ));
    }
    let retention_token = match capture_retention_token_async(&directory).await {
        Ok(token) => token,
        Err(error) => {
            return RawOutputArtifact::unavailable(format!(
                "failed to prepare `{}` for creation: {error}",
                directory.display()
            ));
        }
    };

    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    let retained = &output[..output.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
    let truncated = retained.len() != output.len();
    let mut digest = Sha256::new();
    digest.update(retained);
    match tokio::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .await
    {
        Ok(file) => {
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        0,
                        format!("failed to lock `{}` for creation: {err}", path.display()),
                        Some(&retention_token),
                    )
                    .await;
                }
            };
            if let Err(err) = file.write_all(retained).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to write `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            if durable
                && let Err(error) = write_raw_capture_metadata(
                    &path,
                    Some(id),
                    retained.len() as u64,
                    !truncated,
                    hex::encode(digest.clone().finalize()),
                )
                .await
            {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to persist raw capture status: {error}"),
                    Some(&retention_token),
                )
                .await;
            }
            let file = file.into_std().await;
            let file = match unlock_created_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        retained.len() as u64,
                        format!(
                            "failed to unlock `{}` after creation: {err}",
                            path.display()
                        ),
                        Some(&retention_token),
                    )
                    .await;
                }
            };
            let handle = Arc::new(file);
            // Lazy creation can be published before the writer's optional finish.
            defer_artifact_sync(&path);
            enforce_retention_after_upsert(
                &directory,
                &path,
                &retention_token,
                LogicalRetentionMutation::Create,
            )
            .await;
            RawOutputArtifact::Stored {
                id,
                path,
                bytes: retained.len() as u64,
                truncated,
                handle,
                digest,
                finalized: durable,
            }
        }
        Err(err) => {
            enforce_retention_after_observation(&directory, &path, &retention_token).await;
            RawOutputArtifact::unavailable(format!("failed to create `{}`: {err}", path.display()))
        }
    }
}

// A compact variant of the existing metadata sidecar. No stream reread/hash or
// second retention object is needed just to preserve final capture status.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawCaptureMetadata {
    raw_capture_version: u8,
    artifact_id: String,
    retained_bytes: u64,
    capture_complete: bool,
    #[serde(default)]
    retained_sha256: Option<String>,
}

async fn invalidate_raw_capture_metadata(path: &Path) -> std::io::Result<()> {
    // The directory entry is part of this data generation even if a later
    // replacement write fails before publishing a new status sidecar.
    defer_artifact_sync(path);
    let metadata_path = logical_metadata_path(path);
    match tokio::fs::remove_file(&metadata_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Callers hold the base log's exclusive lock, so this removal is
    // owner-confirmed: retire the obsolete sidecar's durability obligation
    // (the base log entry above covers the directory). A stale entry would
    // fail every later barrier with NotFound if no replacement is published.
    lock_retention_registry()
        .pending_sync
        .remove(&metadata_path);
    Ok(())
}

// Call only while holding the base log's exclusive lock. Invalidate before
// mutation, so cancellation or a partial status write can never retain true.
async fn write_raw_capture_metadata(
    path: &Path,
    id: Option<ToolOutputArtifactId>,
    retained_bytes: u64,
    capture_complete: bool,
    retained_sha256: String,
) -> std::io::Result<u64> {
    let id = id.ok_or_else(|| std::io::Error::other("raw capture has no artifact identity"))?;
    let bytes = serde_json::to_vec(&RawCaptureMetadata {
        raw_capture_version: 2,
        artifact_id: id.to_string(),
        retained_bytes,
        capture_complete,
        retained_sha256: Some(retained_sha256),
    })
    .map_err(std::io::Error::other)?;
    let metadata_path = logical_metadata_path(path);
    run_blocking_artifact_io(move || {
        codex_file_system::write_bytes_atomically_without_sync(&metadata_path, &bytes)?;
        defer_artifact_sync(&metadata_path);
        Ok(bytes.len() as u64)
    })
    .await
}

fn logical_metadata_path(path: &Path) -> PathBuf {
    path.with_extension("meta.json")
}

fn logical_segment_path(path: &Path, index: u32) -> PathBuf {
    if index == 0 {
        path.to_path_buf()
    } else {
        path.with_extension(format!("segment-{index:06}"))
    }
}

fn logical_transaction_path(path: &Path) -> PathBuf {
    path.with_extension(LOGICAL_ARTIFACT_TRANSACTION_EXTENSION)
}

fn begin_logical_artifact_transaction(
    path: &Path,
    preserve_base_segment: bool,
) -> std::io::Result<()> {
    reconcile_logical_artifact_transaction(path)?;
    let transaction_path = logical_transaction_path(path);
    let mut transaction = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&transaction_path)?;
    let write = transaction
        .write_all(if preserve_base_segment {
            LOGICAL_ARTIFACT_ATTACH_TRANSACTION_BYTES
        } else {
            LOGICAL_ARTIFACT_CREATE_TRANSACTION_BYTES
        })
        .and_then(|()| transaction.sync_all())
        .and_then(|()| sync_parent_directory(&transaction_path));
    if let Err(err) = write {
        drop(transaction);
        let _ = std::fs::remove_file(&transaction_path);
        let _ = sync_parent_directory(&transaction_path);
        return Err(err);
    }
    Ok(())
}

fn finish_logical_artifact_transaction(path: &Path) -> std::io::Result<()> {
    let transaction_path = logical_transaction_path(path);
    match std::fs::remove_file(&transaction_path) {
        Ok(()) => sync_parent_directory(&transaction_path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}


fn reconcile_logical_artifact_transaction(path: &Path) -> std::io::Result<()> {
    let transaction_path = logical_transaction_path(path);
    let invalid_marker = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid logical artifact transaction marker `{}`",
                transaction_path.display()
            ),
        )
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let file = match options.open(&transaction_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let metadata = file.metadata()?;
    let max_marker_bytes = LOGICAL_ARTIFACT_CREATE_TRANSACTION_BYTES
        .len()
        .max(LOGICAL_ARTIFACT_ATTACH_TRANSACTION_BYTES.len());
    if !metadata.is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() > max_marker_bytes as u64
    {
        return Err(invalid_marker());
    }
    // Validate and read the same object. Bound the read even if another writer
    // grows the marker after its handle metadata was observed.
    let mut transaction_bytes = Vec::with_capacity(max_marker_bytes + 1);
    file.take((max_marker_bytes + 1) as u64)
        .read_to_end(&mut transaction_bytes)?;
    let preserve_base_segment = transaction_bytes == LOGICAL_ARTIFACT_ATTACH_TRANSACTION_BYTES;
    if !preserve_base_segment && transaction_bytes != LOGICAL_ARTIFACT_CREATE_TRANSACTION_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "invalid logical artifact transaction marker `{}`",
                transaction_path.display()
            ),
        ));
    }

    let metadata_bytes = match read_artifact_metadata_bounded(&logical_metadata_path(path)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        // A temporarily unreadable committed metadata file is not evidence of
        // an uncommitted family. Preserve every segment for a later recovery.
        Err(error) => return Err(error),
    };
    let metadata_bytes =
        metadata_bytes.filter(|bytes| serde_json::from_slice::<RawCaptureMetadata>(bytes).is_err());
    let metadata = metadata_bytes
        .map(|bytes| -> std::io::Result<LogicalArtifactMetadata> {
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt artifact metadata; preserving family: {error}"),
                )
            })?;
            if value.get("version").and_then(serde_json::Value::as_u64)
                != Some(u64::from(LOGICAL_ARTIFACT_METADATA_VERSION))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "unsupported artifact metadata version; preserving family",
                ));
            }
            serde_json::from_value(value).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt artifact metadata; preserving family: {error}"),
                )
            })
        })
        .transpose()?;
    if let Some(metadata) = metadata {
        let retained = metadata
            .segments
            .iter()
            .map(|segment| logical_segment_path(path, segment.index))
            .collect::<BTreeSet<_>>();
        if let Some(directory) = path.parent() {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                let candidate = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let is_segment =
                    name == format!("{stem}.log") || name.starts_with(&format!("{stem}.segment-"));
                if is_segment && !retained.contains(&candidate) {
                    match std::fs::remove_file(candidate) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
        finish_logical_artifact_transaction(path)
    } else if preserve_base_segment {
        if let Some(directory) = path.parent() {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&format!("{stem}.segment-")) {
                    match std::fs::remove_file(entry.path()) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
        finish_logical_artifact_transaction(path)
    } else {
        remove_logical_artifact_files(path)
    }
}

fn reconcile_logical_artifact_transactions(directory: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str())
            != Some(LOGICAL_ARTIFACT_TRANSACTION_EXTENSION)
        {
            continue;
        }
        let Some(stem) = path.file_stem() else {
            continue;
        };
        reconcile_logical_artifact_transaction(&path.with_file_name(stem).with_extension("log"))?;
    }
    Ok(())
}

fn normalized_unavailable_ranges(
    canonical_bytes: u64,
    canonical_ranges: &[CanonicalByteRange],
    retained_bytes: u64,
) -> Vec<CanonicalByteRange> {
    let mut ranges = canonical_ranges
        .iter()
        .map(|range| {
            CanonicalByteRange::new(
                range.start.min(canonical_bytes),
                range.end.min(canonical_bytes),
            )
        })
        .filter(|range| !range.is_empty())
        .collect::<Vec<_>>();
    if retained_bytes < canonical_bytes {
        ranges.push(CanonicalByteRange::new(retained_bytes, canonical_bytes));
    }
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<CanonicalByteRange> = Vec::new();
    for range in ranges {
        if let Some(previous) = normalized.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        normalized.push(range);
    }
    normalized
}

struct StagedCanonicalOutput {
    directory: PathBuf,
    id: ToolOutputArtifactId,
    canonical: CanonicalToolResult,
    metadata_budget: CanonicalMetadataBudget,
    existing: Vec<u8>,
    segments: Vec<StagedLogicalSegment>,
    cleanup: StagedLogicalSegmentCleanup,
    base_lease: Option<File>,
    _residency: Arc<File>,
    _stage_owner: StageOwner,
}

const ARTIFACT_STAGE_WORKERS: usize = 4;
const ARTIFACT_STAGE_BYTE_CAPACITY: u32 = 1024 * 1024 * 1024;
// Well inside the default nested tool timeout; staging itself is short.
const ARTIFACT_STAGE_ADMISSION_WAIT: std::time::Duration = std::time::Duration::from_secs(10);


// Separate from retention admission: slow staging does not hold the global
// retention lock. Reserve both worker count and body/metadata workspace before cloning.
async fn admit_artifact_stage(
    canonical: &CanonicalToolResult,
) -> Result<((OwnedSemaphorePermit, OwnedSemaphorePermit), CanonicalMetadataBudget), String> {
    static COUNT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    static BYTES: OnceLock<Arc<Semaphore>> = OnceLock::new();
    admit_artifact_stage_from(
        COUNT.get_or_init(|| Arc::new(Semaphore::new(ARTIFACT_STAGE_WORKERS))),
        BYTES.get_or_init(|| Arc::new(Semaphore::new(ARTIFACT_STAGE_BYTE_CAPACITY as usize))),
        ARTIFACT_STAGE_BYTE_CAPACITY,
        canonical,
        ARTIFACT_STAGE_ADMISSION_WAIT,
    )
    .await
}

// Holders keep both permits until commit, so one parallel batch of reads can
// saturate admission briefly. Wait a bounded time for a release rather than
// discarding the evidence at once. A waiter holds no slot or file, and at most a
// worker permit while it waits for workspace; both waits share one deadline, so
// it cannot hold that permit past the bound. Dropping it (caller cancellation)
// releases anything acquired and leaves the queue intact; a holder stalled on
// storage still ends the wait at the bound with a named limit.
async fn admit_artifact_stage_from(
    count: &Arc<Semaphore>,
    bytes: &Arc<Semaphore>,
    byte_capacity: u32,
    canonical: &CanonicalToolResult,
    wait: std::time::Duration,
) -> Result<((OwnedSemaphorePermit, OwnedSemaphorePermit), CanonicalMetadataBudget), String> {
    let metadata_budget = CanonicalMetadataBudget::new(canonical)?;
    let metadata = metadata_budget.for_segments(
        canonical
            .bytes
            .len()
            .div_ceil(MAX_RAW_OUTPUT_ARTIFACT_BYTES),
    )?;
    let charge = (canonical.bytes.len() as u64)
        .saturating_mul(2)
        .saturating_add(metadata.saturating_mul(3))
        .max(64 * 1024);
    // No release can admit a charge above the whole workspace; do not wait for one.
    let charge = u32::try_from(charge)
        .ok()
        .filter(|charge| *charge <= byte_capacity)
        .ok_or_else(|| "artifact staging byte limit exceeded".to_string())?;
    let deadline = tokio::time::Instant::now() + wait;
    let count = tokio::time::timeout_at(deadline, Arc::clone(count).acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| {
            format!(
                "artifact staging worker limit reached; no worker was released within {}ms",
                wait.as_millis()
            )
        })?;
    let bytes = tokio::time::timeout_at(deadline, Arc::clone(bytes).acquire_many_owned(charge))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| {
            format!(
                "artifact staging byte limit reached; workspace was not released within {}ms",
                wait.as_millis()
            )
        })?;
    Ok(((count, bytes), metadata_budget))
}

// Four stable native lock files bound staging across every process sharing a
// home. Each slot records its generation before writing data; after a crash it
// cannot be reused until that exact generation's pending files are reclaimed.
// This is independent of the retention lock, which never covers staging IO.
#[derive(Serialize, Deserialize)]
struct StageSlotRecord {
    thread: String,
    id: String,
    tag: String,
}

struct NativeStageSlot {
    _lock: File,
    root: PathBuf,
    record_path: PathBuf,
    next_path: PathBuf,
}

impl NativeStageSlot {
    fn acquire(home: &Path) -> std::io::Result<Self> {
        let directory = home.join("tool-output-staging");
        match std::fs::create_dir(&directory) {
            Ok(()) => sync_parent_directory(&directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let mut last_error = None;
        for index in 0..4 {
            let path = directory.join(format!("slot-{index}.lock"));
            let open = |create| {
                let mut options = File::options();
                options.read(true).write(true).create_new(create);
                configure_no_follow_open(&mut options);
                options.open(&path)
            };
            let file = match open(true) {
                Ok(file) => {
                    file.sync_all()?;
                    sync_parent_directory(&path)?;
                    file
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => open(false)?,
                Err(error) => return Err(error),
            };
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other("invalid staging slot"));
            }
            if let Err(error) = file.try_lock() {
                last_error = Some(std::io::Error::from(error));
                continue;
            }
            let mut slot = Self {
                _lock: file,
                root: home.join("tool-output"),
                record_path: path.with_extension("json"),
                next_path: path.with_extension("next"),
            };
            match slot.recover_previous() {
                Ok(()) => return Ok(slot),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "shared artifact staging capacity exhausted",
            )
        }))
    }

    fn recover_previous(&mut self) -> std::io::Result<()> {
        let mut options = File::options();
        options.read(true);
        configure_no_follow_open(&mut options);
        let file = match options.open(&self.record_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return self.clear(),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > 64 * 1024 {
            return Err(std::io::Error::other(
                "invalid or oversized staging slot record",
            ));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::other(
                "staging slot record grew beyond its limit",
            ));
        }
        if !bytes.is_empty() {
            let record: StageSlotRecord =
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
            let thread = Path::new(&record.thread);
            if thread.components().count() != 1
                || !matches!(
                    thread.components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                return Err(std::io::Error::other("invalid staging owner namespace"));
            }
            let id = record
                .id
                .parse::<ToolOutputArtifactId>()
                .map_err(std::io::Error::other)?;
            let tag = uuid::Uuid::parse_str(&record.tag).map_err(std::io::Error::other)?;
            cleanup_stage_generation(&self.root.join(thread), id, tag)?;
        }
        self.clear()
    }

    fn record(
        &mut self,
        directory: &Path,
        id: ToolOutputArtifactId,
        tag: uuid::Uuid,
    ) -> std::io::Result<()> {
        if directory.parent() != Some(self.root.as_path()) {
            return Err(std::io::Error::other("invalid staging directory"));
        }
        let record = StageSlotRecord {
            thread: directory
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| std::io::Error::other("invalid staging thread"))?
                .to_owned(),
            id: id.to_string(),
            tag: tag.to_string(),
        };
        let bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        if bytes.len() > 64 * 1024 {
            return Err(std::io::Error::other(
                "staging slot record exceeds its limit",
            ));
        }
        // The lock inode never moves. A fixed-size, fixed-name next generation
        // makes short writes/crashes retryable without accumulating temp files.
        let mut options = File::options();
        options.write(true).create(true).truncate(true);
        configure_no_follow_open(&mut options);
        let mut file = options.open(&self.next_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.next_path, &self.record_path)?;
        sync_parent_directory(&self.record_path)
    }

    fn clear(&mut self) -> std::io::Result<()> {
        let mut changed = false;
        for path in [&self.record_path, &self.next_path] {
            match std::fs::remove_file(path) {
                Ok(()) => changed = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if changed {
            sync_parent_directory(&self.record_path)
        } else {
            Ok(())
        }
    }
}

async fn reserve_native_stage_slot(
    home: PathBuf,
    admission: (OwnedSemaphorePermit, OwnedSemaphorePermit),
) -> Result<
    (
        (OwnedSemaphorePermit, OwnedSemaphorePermit),
        NativeStageSlot,
    ),
    String,
> {
    // The worker, not its cancellable observer, owns admission while storage is blocked.
    tokio::task::spawn_blocking(move || {
        NativeStageSlot::acquire(&home).map(|slot| (admission, slot))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

fn cleanup_stage_generation(
    directory: &Path,
    id: ToolOutputArtifactId,
    tag: uuid::Uuid,
) -> std::io::Result<()> {
    let segments = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL)
        .div_ceil(MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64);
    for index in 0..=segments {
        match std::fs::remove_file(staged_segment_path_for_tag(
            directory,
            id,
            tag,
            index as u32,
        )) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    match std::fs::remove_file(directory.join(format!(".{id}.stage-{tag}.stage"))) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}


struct StageOwner {
    directory: PathBuf,
    id: ToolOutputArtifactId,
    tag: uuid::Uuid,
    _file: Option<File>,
    // Drop releases admission only after pending-file cleanup, even when a
    // stalled filesystem holds the commit worker inside this destructor.
    _admission: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
    _shared_slot: Option<NativeStageSlot>,
}

impl StageOwner {
    fn acquire(directory: &Path, id: ToolOutputArtifactId, create: bool) -> std::io::Result<Self> {
        let tag = uuid::Uuid::new_v4();
        Self::open(directory, id, tag, create)
    }

    fn open(
        directory: &Path,
        id: ToolOutputArtifactId,
        tag: uuid::Uuid,
        create: bool,
    ) -> std::io::Result<Self> {
        let path = directory.join(format!(".{id}.stage-{tag}.stage"));
        let mut options = File::options();
        options.read(true).write(true).create_new(create);
        configure_no_follow_open(&mut options);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("invalid artifact stage owner"));
        }
        file.try_lock().map_err(std::io::Error::from)?;
        Ok(Self {
            directory: directory.to_owned(),
            id,
            tag,
            _file: Some(file),
            _admission: None,
            _shared_slot: None,
        })
    }
}

impl Drop for StageOwner {
    fn drop(&mut self) {
        // The OS lock, not an age or PID guess, proves these files have no
        // other stage owner. Never touch installed canonical segments.
        if cleanup_stage_generation(&self.directory, self.id, self.tag).is_err() {
            return;
        }
        if let Some(slot) = self._shared_slot.as_mut()
            && let Err(error) = slot.clear()
        {
            tracing::warn!(%error, "artifact staging slot remains retryable");
        }
    }
}

fn recover_abandoned_stages(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some((id, tag)) = name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".stage"))
            .and_then(|name| name.split_once(".stage-"))
            .and_then(|(id, tag)| {
                Some((
                    id.parse::<ToolOutputArtifactId>().ok()?,
                    uuid::Uuid::parse_str(tag).ok()?,
                ))
            })
        {
            // A live stage keeps this exclusive lock through commit/rollback.
            if let Ok(owner) = StageOwner::open(directory, id, tag, false) {
                drop(owner);
            }
        }
    }
    // Upgrade cleanup for pre-manifest stages is allowed only when the whole
    // thread has no live reader/writer, including another process's session.
    let Some(home) = directory.parent().and_then(Path::parent) else {
        return;
    };
    let Some(thread) = directory.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(_exclusive) = artifact_residency_file(home, thread, true) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix('.')
            .and_then(|name| name.split_once(".segment-"))
            .is_some_and(|(owner, suffix)| {
                owner.split_once(".stage-").map_or_else(
                    || owner.parse::<ToolOutputArtifactId>().is_ok(),
                    |(id, tag)| {
                        id.parse::<ToolOutputArtifactId>().is_ok()
                            && uuid::Uuid::parse_str(tag).is_ok()
                    },
                ) && suffix.strip_suffix(".pending").is_some_and(|index| {
                    index.len() == 6 && index.bytes().all(|b| b.is_ascii_digit())
                })
            })
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

struct StagedLogicalSegment {
    index: u32,
    range: CanonicalByteRange,
    path: PathBuf,
    shared: bool,
}

struct StagedLogicalSegmentCleanup(Vec<PathBuf>);

impl StagedLogicalSegmentCleanup {
    fn new(segments: &[StagedLogicalSegment]) -> Self {
        Self(
            segments
                .iter()
                .map(|segment| segment.path.clone())
                .collect(),
        )
    }
}

impl Drop for StagedLogicalSegmentCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn staged_logical_segment_path(owner: &StageOwner, index: u32) -> PathBuf {
    staged_segment_path_for_tag(&owner.directory, owner.id, owner.tag, index)
}

fn staged_segment_path_for_tag(
    directory: &Path,
    id: ToolOutputArtifactId,
    tag: uuid::Uuid,
    index: u32,
) -> PathBuf {
    directory.join(format!(".{id}.stage-{tag}.segment-{index:06}.pending"))
}

fn remove_staged_logical_segments_blocking(segments: &[StagedLogicalSegment]) {
    for segment in segments {
        let _ = std::fs::remove_file(&segment.path);
    }
}

fn write_staged_logical_segment_blocking(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn stage_logical_segments_blocking(
    owner: &StageOwner,
    bytes: &[u8],
    canonical_start: u64,
    first_index: u32,
) -> Result<Vec<StagedLogicalSegment>, (u64, PathBuf, std::io::Error)> {
    let mut staged = Vec::new();
    for (offset, chunk) in bytes.chunks(MAX_RAW_OUTPUT_ARTIFACT_BYTES).enumerate() {
        let index = first_index + offset as u32;
        let start = canonical_start + offset as u64 * MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64;
        let path = staged_logical_segment_path(owner, index);
        if let Err(err) = write_staged_logical_segment_blocking(&path, chunk) {
            let _ = std::fs::remove_file(&path);
            remove_staged_logical_segments_blocking(&staged);
            return Err((canonical_start, path, err));
        }
        staged.push(StagedLogicalSegment {
            index,
            range: CanonicalByteRange::new(start, start + chunk.len() as u64),
            path,
            shared: false,
        });
    }
    if bytes.is_empty() && first_index == 0 {
        let path = staged_logical_segment_path(owner, 0);
        if let Err(err) = write_staged_logical_segment_blocking(&path, &[]) {
            let _ = std::fs::remove_file(&path);
            return Err((canonical_start, path, err));
        }
        staged.push(StagedLogicalSegment {
            index: 0,
            range: CanonicalByteRange::new(canonical_start, canonical_start),
            path,
            shared: false,
        });
    }
    Ok(staged)
}

fn install_staged_logical_segments_blocking(
    final_path: &Path,
    staged: Vec<StagedLogicalSegment>,
    retained_bytes: u64,
) -> Result<Vec<LogicalArtifactSegment>, (u64, PathBuf, std::io::Error)> {
    // Every installed segment is rolled back on failure. Only the prefix
    // preceding this operation remains recoverable (zero for a new artifact).
    let retained_prefix = staged
        .first()
        .map_or(retained_bytes, |segment| segment.range.start);
    let mut installed: Vec<LogicalArtifactSegment> = Vec::new();
    for segment in staged {
        if segment.range.start >= retained_bytes && !segment.range.is_empty() {
            let _ = std::fs::remove_file(&segment.path);
            continue;
        }
        let end = segment.range.end.min(retained_bytes);
        if end < segment.range.end && segment.shared {
            // Quota truncation must never mutate a shared immutable inode.
            // Detach only this invocation's staged link before writing its prefix.
            let detached = (|| -> std::io::Result<()> {
                let (file, _) = open_regular_artifact(&segment.path)
                    .map_err(|error| std::io::Error::other(error.for_model()))?;
                let mut bytes = Vec::new();
                file.take(end - segment.range.start).read_to_end(&mut bytes)?;
                std::fs::remove_file(&segment.path)?;
                write_staged_logical_segment_blocking(&segment.path, &bytes)
            })();
            if let Err(error) = detached {
                for installed_segment in &installed {
                    let _ = std::fs::remove_file(logical_segment_path(final_path, installed_segment.index));
                }
                return Err((retained_prefix, segment.path, error));
            }
        }
        if end < segment.range.end && !segment.shared {
            match std::fs::OpenOptions::new().write(true).open(&segment.path) {
                Ok(file) => {
                    if let Err(err) = file.set_len(end.saturating_sub(segment.range.start)) {
                        for installed_segment in &installed {
                            let _ = std::fs::remove_file(logical_segment_path(
                                final_path,
                                installed_segment.index,
                            ));
                        }
                        return Err((retained_prefix, segment.path, err));
                    }
                    if let Err(err) = file.sync_all() {
                        for installed_segment in &installed {
                            let _ = std::fs::remove_file(logical_segment_path(
                                final_path,
                                installed_segment.index,
                            ));
                        }
                        return Err((retained_prefix, segment.path, err));
                    }
                }
                Err(err) => {
                    for installed_segment in &installed {
                        let _ = std::fs::remove_file(logical_segment_path(
                            final_path,
                            installed_segment.index,
                        ));
                    }
                    return Err((retained_prefix, segment.path, err));
                }
            }
        }
        let destination = logical_segment_path(final_path, segment.index);
        if let Err(err) = std::fs::rename(&segment.path, &destination) {
            for installed_segment in &installed {
                let _ =
                    std::fs::remove_file(logical_segment_path(final_path, installed_segment.index));
            }
            return Err((retained_prefix, segment.path, err));
        }
        installed.push(LogicalArtifactSegment {
            index: segment.index,
            range: CanonicalByteRange::new(segment.range.start, end),
        });
    }
    if let Err(err) = sync_parent_directory(final_path) {
        for installed_segment in &installed {
            let _ = std::fs::remove_file(logical_segment_path(final_path, installed_segment.index));
        }
        return Err((retained_prefix, final_path.to_path_buf(), err));
    }
    Ok(installed)
}

fn logical_artifact_family_exists(path: &Path) -> std::io::Result<bool> {
    let Some(directory) = path.parent() else {
        return Ok(false);
    };
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with(&format!("{stem}.")) {
            return Ok(true);
        }
    }
    Ok(false)
}

// The legacy dense format is retained for compatibility, but it must never
// allocate an unbounded second representation of a newline-dense result.
const MAX_CANONICAL_LINE_OFFSETS: usize = 1024 * 1024;
const MAX_CANONICAL_METADATA_BYTES: u64 = 32 * 1024 * 1024;

fn canonical_line_count(bytes: &[u8]) -> Result<usize, ReadToolOutputError> {
    let count = 1 + bytes
        .iter()
        .take(bytes.len().saturating_sub(1))
        .filter(|byte| **byte == b'\n')
        .take(MAX_CANONICAL_LINE_OFFSETS)
        .count();
    if count > MAX_CANONICAL_LINE_OFFSETS {
        return Err(ReadToolOutputError::Io(
            "artifact line index exceeds its bounded representation; exact capture unavailable"
                .into(),
        ));
    }
    Ok(count)
}

// The detached storage owner needs canonical bytes and recovery addresses, not
// the producer's JSON DOM. Keep that DOM with the projection owner only.
fn canonical_storage_snapshot(canonical: &CanonicalToolResult) -> CanonicalToolResult {
    CanonicalToolResult {
        kind: canonical.kind,
        bytes: canonical.bytes.clone(),
        value: None,
        sha256: canonical.sha256.clone(),
        exact_bytes: canonical.exact_bytes,
        approximate_tokens: canonical.approximate_tokens,
        complete: canonical.complete,
        unavailable_ranges: canonical.unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
    }
}

#[derive(Debug)]
struct CanonicalMetadataBudget {
    invariant_bytes: u64,
    line_count: Option<usize>,
}

impl CanonicalMetadataBudget {
    fn new(canonical: &CanonicalToolResult) -> Result<Self, String> {
        let line_count = canonical_line_count(&canonical.bytes).ok();
        let invariant_bytes = canonical_invariant_metadata_bytes(canonical, line_count.unwrap_or(0))?;
        Ok(Self { invariant_bytes, line_count })
    }

    fn for_segments(&self, segments: usize) -> Result<u64, String> {
        let bytes = self.invariant_bytes.saturating_add((segments as u64).saturating_mul(256));
        if bytes > MAX_CANONICAL_METADATA_BYTES {
            return Err("artifact metadata exceeds its bounded representation".into());
        }
        Ok(bytes)
    }

    fn line_starts(&self, retained: &[u8], canonical_len: usize) -> Vec<u64> {
        // Every retained body is a prefix of the immutable admitted bytes, so
        // its line count cannot exceed the bounded count already computed.
        self.line_count.map_or_else(Vec::new, |count| {
            let capacity = if retained.len() == canonical_len { count } else { 1 };
            canonical_line_starts_with_capacity(retained, capacity)
        })
    }
}


fn canonical_invariant_metadata_bytes(
    canonical: &CanonicalToolResult,
    lines: usize,
) -> Result<u64, String> {
    struct Counter(u64);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            if self.0 > MAX_CANONICAL_METADATA_BYTES {
                return Err(std::io::Error::other(
                    "artifact metadata exceeds its bounded representation",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // A newline-dense result is stored without a dense line index, so only an
    // index that will be stored is budgeted; its bytes remain retainable.
    let mut bytes = Counter(4096 + 256 + lines as u64 * 21);
    serde_json::to_writer(&mut bytes, &canonical.json_pointers)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut bytes, &canonical.sections).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut bytes, &canonical.unavailable_ranges)
        .map_err(|error| error.to_string())?;
    Ok(bytes.0)
}

fn canonical_line_starts(bytes: &[u8]) -> Result<Vec<u64>, ReadToolOutputError> {
    Ok(canonical_line_starts_with_capacity(bytes, canonical_line_count(bytes)?))
}

fn canonical_line_starts_with_capacity(bytes: &[u8], capacity: usize) -> Vec<u64> {
    let mut starts = Vec::with_capacity(capacity);
    starts.push(0);
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index as u64 + 1))
            .filter(|offset| *offset < bytes.len() as u64),
    );
    starts
}


fn response_fits_recovery_token_ceiling(value: &impl Serialize, token_ceiling: usize) -> bool {
    serde_json::to_string(value)
        .is_ok_and(|rendered| !approx_token_count_exceeds(&rendered, token_ceiling))
}

fn response_fits_recovery_retry_avoidance_ceiling(
    value: &impl Serialize,
    token_ceiling: usize,
) -> bool {
    response_fits_recovery_token_ceiling(
        value,
        token_ceiling.saturating_add(RECOVERY_RETRY_AVOIDANCE_TOKEN_MARGIN),
    )
}

// Like recovery continuation accounting, combine serialized, independently
// delimited fragments before rounding. This state belongs only to one selection.
struct SelectorResponseCost {
    envelopes: [TokenCountEstimate; 2],
    results: TokenCountEstimate,
    parts: Vec<SelectorPartCost>,
}

#[derive(Clone, Copy)]
struct SelectorPartCost {
    serialized: TokenCountEstimate,
    successful: bool,
    complete: bool,
}

#[expect(
    clippy::expect_used,
    reason = "typed selector values have infallible JSON serialization"
)]
fn selector_serialized_cost(value: &ToolOutputSelectorResult) -> SelectorPartCost {
    // These typed JSON values contain no fallible map keys or custom payloads.
    SelectorPartCost {
        serialized: TokenCountEstimate::new(
            &serde_json::to_string(value).expect("selector value serializes"),
        ),
        successful: value.status == ToolOutputSelectorStatus::Ok,
        complete: value.complete,
    }
}

impl SelectorResponseCost {
    #[expect(
        clippy::expect_used,
        reason = "empty-result envelopes serialize only strings, integer ranges, booleans and a fixed status enum to an in-memory string; fallback costs could admit oversized output"
    )]
    fn new(metadata: &LogicalArtifactMetadata, previous: &[ToolOutputSelectorResult]) -> Self {
        let mut envelope = ReadToolOutputResult {
            artifact_id: metadata.artifact_id.clone(),
            canonical_sha256: metadata.canonical_sha256.clone(),
            canonical_bytes: metadata.canonical_bytes,
            retained_bytes: metadata.retained_bytes,
            capture_complete: metadata.capture_complete,
            complete: false,
            unavailable_ranges: metadata.unavailable_ranges.clone(),
            results: Vec::new(),
        };
        let incomplete = TokenCountEstimate::new(
            &serde_json::to_string(&envelope).expect("envelope serializes"),
        );
        envelope.complete = true;
        let mut cost = Self {
            envelopes: [
                incomplete,
                TokenCountEstimate::new(
                    &serde_json::to_string(&envelope).expect("envelope serializes"),
                ),
            ],
            results: TokenCountEstimate::default(),
            parts: Vec::with_capacity(previous.len()),
        };
        for result in previous {
            cost.push(selector_serialized_cost(result));
        }
        cost
    }

    fn size(&self, complete: bool) -> TokenCountEstimate {
        self.envelope(complete, None).add_delimited(self.results)
    }

    fn envelope(&self, complete: bool, next: Option<SelectorPartCost>) -> TokenCountEstimate {
        let mut parts = self.parts.iter().copied().chain(next);
        let any_success = parts.clone().any(|part| part.successful);
        let all_complete = parts.all(|part| part.successful && part.complete);
        let status = if complete && any_success && all_complete {
            "\"complete\""
        } else if any_success {
            "\"partial\""
        } else {
            "\"failed\""
        };
        // Empty-result envelopes serialize selection_status as "failed".
        self.envelopes[usize::from(complete)]
            .subtract_delimited(TokenCountEstimate::new("\"failed\""))
            .add_delimited(TokenCountEstimate::new(status))
    }

    fn with_next(&self, next: SelectorPartCost, complete: bool) -> TokenCountEstimate {
        self.envelope(complete, Some(next))
            .add_delimited(self.results)
            .add_delimited(next.serialized)
            .add_delimited(TokenCountEstimate::new(if self.parts.is_empty() {
                ""
            } else {
                ","
            }))
    }

    fn push(&mut self, next: SelectorPartCost) {
        if !self.parts.is_empty() {
            self.results = self.results.add_delimited(TokenCountEstimate::new(","));
        }
        self.results = self.results.add_delimited(next.serialized);
        self.parts.push(next);
    }

    fn replace(&mut self, index: usize, next: SelectorPartCost) {
        self.results = self
            .results
            .subtract_delimited(self.parts[index].serialized)
            .add_delimited(next.serialized);
        self.parts[index] = next;
    }
}

fn successful_byte_selector_result(
    range: CanonicalByteRange,
    bytes: &[u8],
) -> ToolOutputSelectorResult {
    let (text, data_base64) = match std::str::from_utf8(bytes) {
        Ok(text) => (Some(text.to_string()), None),
        Err(_) => (None, Some(BASE64_STANDARD.encode(bytes))),
    };
    ToolOutputSelectorResult {
        selector: ToolOutputSelector::Bytes {
            start: range.start,
            end: range.end,
        },
        status: ToolOutputSelectorStatus::Ok,
        complete: true,
        exact_bytes: Some(range.len()),
        canonical_range: Some(range),
        text,
        value: None,
        data_base64,
        subdivision_plan: None,
        child_selectors: Vec::new(),
        continuation: None,
        message: None,
    }
}

/// Largest prefix of `range` whose own response fits, measured on the bytes
/// that would actually be returned.
///
/// Sizing against a synthetic buffer cannot work: NUL renders as `\u0000`, so a
/// zero-filled probe overstates the cost about sixfold and advertises chunks a
/// fraction of the size that fits, while representative letters would
/// understate escape-heavy content and advertise chunks that do not. Only the
/// real prefix answers the question being asked.
///
/// The returned length is char-boundary aligned so the advertised continuation
/// is a readable fragment rather than a split code point.
fn largest_fitting_byte_prefix(
    artifact_id: &str,
    canonical_bytes: u64,
    range: CanonicalByteRange,
    snapshot: &[u8],
    token_ceiling: usize,
) -> u64 {
    if range.is_empty() {
        return 0;
    }
    // Binary snapshots may consist entirely of UTF-8 continuation-byte values.
    // Validate once, not once per sizing probe, before imposing text boundaries.
    let text = snapshot
        .get(range.start as usize..range.end as usize)
        .and_then(|bytes| std::str::from_utf8(bytes).ok());
    largest_fitting_byte_prefix_with_text(artifact_id, canonical_bytes, range, snapshot, token_ceiling, text)
}

fn largest_fitting_byte_prefix_with_text(
    artifact_id: &str,
    canonical_bytes: u64,
    range: CanonicalByteRange,
    snapshot: &[u8],
    token_ceiling: usize,
    text: Option<&str>,
) -> u64 {
    let align = |length: u64| match text {
        Some(text) => align_to_char_boundary(text, length),
        None => length,
    };
    let prefix_bytes = |candidate_bytes: u64| -> &[u8] {
        let start = range.start as usize;
        let end = start
            .saturating_add(candidate_bytes as usize)
            .min(snapshot.len());
        snapshot.get(start..end).unwrap_or_default()
    };
    let fits = |candidate_bytes: u64| {
        // Measure the same readable bytes we will deliver. A probe ending
        // inside a code point is serialized as base64, whose token cost can
        // differ from the text produced after boundary alignment.
        let candidate_bytes = align(candidate_bytes);
        let candidate_range = CanonicalByteRange::new(range.start, range.start + candidate_bytes);
        let result = ReadToolOutputResult {
            artifact_id: artifact_id.to_string(),
            canonical_sha256: "0".repeat(64),
            canonical_bytes,
            retained_bytes: canonical_bytes,
            capture_complete: true,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![successful_byte_selector_result(
                candidate_range,
                prefix_bytes(candidate_bytes),
            )],
        };
        response_fits_recovery_token_ceiling(&result, token_ceiling)
    };
    let mut best = 1_u64;
    let mut first_failure = range.len();
    while best < range.len() {
        let candidate = best.saturating_mul(2).min(range.len());
        if fits(candidate) {
            best = candidate;
            if candidate == range.len() {
                return align(candidate);
            }
        } else {
            first_failure = candidate;
            break;
        }
    }
    let mut low = best.saturating_add(1);
    let mut high = first_failure.saturating_sub(1);
    while low <= high {
        let middle = low + (high - low) / 2;
        if fits(middle) {
            best = middle;
            low = middle.saturating_add(1);
        } else {
            high = middle.saturating_sub(1);
        }
    }
    align(best)
}

/// Shrinks a validated text prefix until it ends on a UTF-8 char boundary.
fn align_to_char_boundary(text: &str, length: u64) -> u64 {
    let mut length = (length as usize).min(text.len());
    while length > 0 && !text.is_char_boundary(length) {
        length -= 1;
    }
    length as u64
}

/// Persists one canonical result as one logical artifact. Physical segment
/// files and the metadata sidecar are private implementation details and never
/// receive independent IDs.
pub(crate) async fn create_canonical_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    canonical: &CanonicalToolResult,
) -> CanonicalOutputArtifact {
    create_canonical_output_artifact_with_id(
        codex_home,
        thread_id,
        canonical,
        ToolOutputArtifactId::new(),
    )
    .await
}

pub(super) async fn create_canonical_output_artifact_with_id(
    codex_home: &Path,
    thread_id: &str,
    canonical: &CanonicalToolResult,
    id: ToolOutputArtifactId,
) -> CanonicalOutputArtifact {
    create_canonical_output_artifact_inner(codex_home, thread_id, canonical, id, false).await
}

/// The artifact owner, not a runtime cell, retains admission and failure state.
/// Reads stay thread-scoped and never wait for a stalled storage dependency.
pub(super) fn reserve_canonical_retention(
    home: &Path,
    thread_id: &str,
    id: ToolOutputArtifactId,
) -> PendingCanonicalRetention {
    let path = home.join("tool-output").join(thread_id).join(format!("{id}.log"));
    lock_retention_registry().pending_canonical.insert(path.clone());
    PendingCanonicalRetention { path, settled: false }
}

pub(super) struct PendingCanonicalRetention {
    path: PathBuf,
    settled: bool,
}

impl PendingCanonicalRetention {
    pub(super) fn settle(mut self, artifact: &CanonicalOutputArtifact) {
        let mut registry = lock_retention_registry();
        registry.pending_canonical.remove(&self.path);
        if !artifact.complete {
            registry.failed_canonical.insert(self.path.clone(),
                artifact.error.clone().unwrap_or_else(|| "canonical retention incomplete".into()));
        }
        // Failed capabilities have bounded retention, like stored artifacts.
        while registry.failed_canonical.len() > 4_096 {
            registry.failed_canonical.pop_first();
        }
        self.settled = true;
    }
}

impl Drop for PendingCanonicalRetention {
    fn drop(&mut self) {
        if !self.settled {
            let mut registry = lock_retention_registry();
            registry.pending_canonical.remove(&self.path);
            registry.failed_canonical.insert(self.path.clone(),
                "canonical retention owner ended before settlement; do not replay effects".into());
            while registry.failed_canonical.len() > 4_096 {
                registry.failed_canonical.pop_first();
            }
        }
    }
}

/// New history artifacts are protected in their creation transaction. Existing
/// artifacts still require independent on-disk verification before protection.
pub(crate) async fn create_tool_history_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    canonical: &CanonicalToolResult,
) -> CanonicalOutputArtifact {
    create_canonical_output_artifact_inner(
        codex_home,
        thread_id,
        canonical,
        ToolOutputArtifactId::new(),
        true,
    )
    .await
}

async fn create_canonical_output_artifact_inner(
    codex_home: &Path,
    thread_id: &str,
    canonical: &CanonicalToolResult,
    id: ToolOutputArtifactId,
    protect_history: bool,
) -> CanonicalOutputArtifact {
    create_canonical_output_artifact_with_backing(codex_home, thread_id, canonical, id, protect_history, None, std::future::ready).await
}


pub(crate) async fn create_source_snapshot_artifact_with_completion<F, Fut>(
    codex_home: &Path, thread_id: &str, canonical: &CanonicalToolResult,
    backing: Option<String>, completion: F,
) -> CanonicalOutputArtifact
where
    F: FnOnce(CanonicalOutputArtifact) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = CanonicalOutputArtifact> + Send + 'static,
{
    create_canonical_output_artifact_with_backing(codex_home, thread_id, canonical,
        ToolOutputArtifactId::new(), true, backing, completion).await
}

async fn create_canonical_output_artifact_with_backing<F, Fut>(
    codex_home: &Path, thread_id: &str, canonical: &CanonicalToolResult,
    id: ToolOutputArtifactId, protect_history: bool, backing: Option<String>,
    completion: F,
) -> CanonicalOutputArtifact
where
    F: FnOnce(CanonicalOutputArtifact) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = CanonicalOutputArtifact> + Send + 'static,
{
    let codex_home = codex_home.to_path_buf();
    let thread_id = thread_id.to_string();
    let exact_bytes = canonical.exact_bytes;
    let (stage_admission, metadata_budget) = match admit_artifact_stage(canonical).await {
        Ok(permit) => permit,
        Err(error) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
                error: Some(error),
            };
        }
    };
    let (stage_admission, shared_slot) =
        match reserve_native_stage_slot(codex_home.clone(), stage_admission).await {
            Ok(admission) => admission,
            Err(error) => {
                return CanonicalOutputArtifact {
                    id: Some(id),
                    retained_bytes: 0,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
                    error: Some(error),
                };
            }
        };
    let canonical = canonical_storage_snapshot(canonical);
    // The coordinator only transfers ownership between flat filesystem workers.
    // Each worker owns pending-path cleanup and, during commit, the retention permit.
    // Caller cancellation detaches the coordinator; runtime shutdown cannot drop a
    // permit or pending-path guard while its filesystem operation is still running.
    tokio::spawn(async move {
        let staged = tokio::task::spawn_blocking(move || {
            let mut staged = stage_create_canonical_output_artifact(
                codex_home,
                thread_id,
                canonical,
                metadata_budget,
                id,
                shared_slot,
                backing,
            )?;
            staged._stage_owner._admission = Some(stage_admission);
            Ok(staged)
        })
        .await?;
        let staged = match staged {
            Ok(staged) => staged,
            Err(result) => return Ok(result),
        };
        let (root, semaphore) = retention_sweep_admission_for_directory(&staged.directory).await?;
        let process_permit = match semaphore.acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                record_retention_sweep_permit_failure(&root, &error);
                return Ok(CanonicalOutputArtifact {
                    id: Some(staged.id),
                    retained_bytes: 0,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(
                        0,
                        staged.canonical.exact_bytes,
                    )],
                    error: Some(format!("failed to admit canonical artifact: {error}")),
                });
            }
        };
        let (artifact, admission) = tokio::task::spawn_blocking(move || {
            let mut staged = staged;
            // Keep existing stage admission through the owned completion. A
            // stalled registration cannot grow an unbounded queue of owners.
            let admission = staged._stage_owner._admission.take();
            let artifact = commit_create_canonical_output_artifact(staged, process_permit, protect_history);
            (artifact, admission)
        })
        .await?;
        let artifact = completion(artifact).await;
        drop(admission);
        Ok(artifact)
    })
    .await
    .and_then(std::convert::identity)
    .unwrap_or_else(|error| CanonicalOutputArtifact {
        id: Some(id),
        retained_bytes: 0,
        complete: false,
        unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
        error: Some(format!("canonical artifact worker failed: {error}")),
    })
}

fn stage_create_canonical_output_artifact(
    codex_home: PathBuf,
    thread_id: String,
    canonical: CanonicalToolResult,
    metadata_budget: CanonicalMetadataBudget,
    id: ToolOutputArtifactId,
    mut shared_slot: NativeStageSlot,
    backing: Option<String>,
) -> Result<StagedCanonicalOutput, CanonicalOutputArtifact> {
    let residency = artifact_residency_file(&codex_home, &thread_id, false)
        .map(Arc::new)
        .map_err(|error| CanonicalOutputArtifact {
            id: None,
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    metadata_budget.for_segments(0).map_err(|error| CanonicalOutputArtifact {
        id: None,
        retained_bytes: 0,
        complete: false,
        unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
        error: Some(error),
    })?;
    let directory = codex_home.join("tool-output").join(thread_id);
    if let Err(err) = std::fs::create_dir_all(&directory) {
        return Err(CanonicalOutputArtifact {
            id: None,
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(format!("failed to create `{}`: {err}", directory.display())),
        });
    }
    let max_staged_bytes = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL)
        .min(canonical.exact_bytes) as usize;
    let staged_bytes = &canonical.bytes[..canonical.bytes.len().min(max_staged_bytes)];
    let mut stage_owner =
        StageOwner::acquire(&directory, id, true).map_err(|error| CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    shared_slot
        .record(&directory, id, stage_owner.tag)
        .map_err(|error| CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    stage_owner._shared_slot = Some(shared_slot);
    let shared = backing.as_deref().and_then(|backing| stage_shared_source_backing(&stage_owner, backing, &canonical));
    let (staged_segments, base_lease) = if let Some((segments, lease)) = shared {
        (segments, Some(lease))
    } else { (match stage_logical_segments_blocking(&stage_owner, staged_bytes, 0, 0) {
        Ok(segments) => segments,
        Err((start, staged_path, err)) => {
            return Err(CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: start,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                error: Some(format!(
                    "failed to stage `{}`: {err}",
                    staged_path.display()
                )),
            });
        }
    }, None) };
    let cleanup = StagedLogicalSegmentCleanup::new(&staged_segments);

    Ok(StagedCanonicalOutput {
        directory,
        id,
        canonical,
        metadata_budget,
        existing: Vec::new(),
        segments: staged_segments,
        cleanup,
        base_lease,
        _residency: residency,
        _stage_owner: stage_owner,
    })
}

fn stage_shared_source_backing(owner: &StageOwner, backing: &str, canonical: &CanonicalToolResult) -> Option<(Vec<StagedLogicalSegment>, File)> {
    if !canonical.complete || canonical.bytes.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES { return None; }
    let id = backing.parse::<ToolOutputArtifactId>().ok()?;
    if id == owner.id { return None; }
    let source = owner.directory.join(format!("{id}.log"));
    let (metadata, bytes, lease) = load_logical_metadata_with_snapshot(&source, id).ok()?;
    if !metadata.capture_complete || !metadata.complete
        || metadata.canonical_bytes != canonical.exact_bytes
        || metadata.retained_bytes != canonical.exact_bytes
        || metadata.canonical_sha256 != canonical.sha256
        || !metadata.unavailable_ranges.is_empty()
        || metadata.segments.len() != 1 || metadata.segments[0].index != 0
        || metadata.segments[0].range != CanonicalByteRange::new(0, canonical.exact_bytes)
        || bytes.is_some()
    { return None; }
    let staged = staged_logical_segment_path(owner, 0);
    std::fs::hard_link(&source, &staged).ok()?;
    // Authenticate the installed link too: an external path replacement must
    // not substitute a different inode between validation and link creation.
    let valid = (|| {
        let (mut file, size) = open_regular_artifact(&staged).ok()?;
        if size != canonical.exact_bytes { return None; }
        // Compare/authenticate in a fixed slab, not two additional full blobs
        // outside the existing staging byte-admission budget.
        let mut buffer = [0u8; STREAMING_ARTIFACT_BUFFER_BYTES];
        let mut offset = 0usize;
        let mut digest = Sha256::new();
        loop {
            let count = file.read(&mut buffer).ok()?;
            if count == 0 { break; }
            if canonical.bytes.get(offset..offset.checked_add(count)?)? != &buffer[..count] { return None; }
            digest.update(&buffer[..count]); offset += count;
        }
        (offset == canonical.bytes.len() && hex::encode(digest.finalize()) == canonical.sha256).then_some(())
    })().is_some();
    if !valid { let _ = std::fs::remove_file(&staged); return None; }
    Some((vec![StagedLogicalSegment { index:0,
        range:CanonicalByteRange::new(0, canonical.exact_bytes), path:staged, shared:true }], lease))
}

fn commit_create_canonical_output_artifact(
    staged: StagedCanonicalOutput,
    process_permit: OwnedSemaphorePermit,
    protect_history: bool,
) -> CanonicalOutputArtifact {
    let StagedCanonicalOutput {
        directory,
        id,
        canonical,
        metadata_budget: canonical_metadata,
        existing: _,
        segments: staged_segments,
        cleanup: _staged_cleanup,
        base_lease: _base_lease,
        _residency,
        _stage_owner,
    } = staged;
    let root = tool_output_root_for_directory(&directory);
    let _retention_permit = match retention_sweep_permit_blocking(&root, process_permit) {
        Ok(permit) => permit,
        Err(error) => {
            record_retention_sweep_permit_failure(&root, &error);
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(format!(
                    "failed to acquire canonical artifact retention lock: {error}"
                )),
            };
        }
    };
    let staged_bytes = staged_segments
        .iter()
        .map(|segment| segment.range.end - segment.range.start)
        .sum::<u64>();
    let path = directory.join(format!("{id}.log"));
    if let Err(err) = reconcile_logical_artifact_transactions(&directory) {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(format!("failed to reconcile artifact transactions: {err}")),
        };
    }
    let metadata_budget = match canonical_metadata.for_segments(staged_segments.len()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(error),
            };
        }
    };
    enforce_retention_locked_blocking(
        &directory,
        &path,
        canonical.exact_bytes.saturating_add(metadata_budget),
        1,
    );
    let usage = retention_usage_locked_blocking(&directory);
    if !retention_shared_lock_held(&root) {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some("lost the canonical artifact retention lock".to_string()),
        };
    }
    let available = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .saturating_sub(usage.thread_bytes)
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(usage.global_bytes));
    if available < metadata_budget {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some("artifact metadata does not fit the retained-family budget".into()),
        };
    }
    let retained_bytes = canonical
        .exact_bytes
        .min(available.saturating_sub(metadata_budget))
        .min(staged_bytes);
    let retention_token = capture_retention_token(&directory);
    let complete = retained_bytes == canonical.exact_bytes && canonical.unavailable_ranges.is_empty();
    let unavailable_ranges = normalized_unavailable_ranges(
        canonical.exact_bytes,
        &canonical.unavailable_ranges,
        retained_bytes,
    );
    let retained = &canonical.bytes[..retained_bytes as usize];
    if let Err(err) = begin_logical_artifact_transaction(&path, false) {
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(format!("failed to begin artifact transaction: {err}")),
        };
    }
    let segments =
        match install_staged_logical_segments_blocking(&path, staged_segments, retained_bytes) {
            Ok(segments) => segments,
            Err((start, staged_path, err)) => {
                rollback_logical_artifact_creation(&retention_token, &path);
                return CanonicalOutputArtifact {
                    id: Some(id),
                    retained_bytes: start,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                    error: Some(format!(
                        "failed to install staged artifact segment `{}`: {err}",
                        staged_path.display()
                    )),
                };
            }
        };

    let line_starts = canonical_metadata.line_starts(retained, canonical.bytes.len());
    let metadata = LogicalArtifactMetadata {
        version: LOGICAL_ARTIFACT_METADATA_VERSION,
        artifact_id: id.to_string(),
        canonical_kind: canonical.kind,
        canonical_sha256: canonical.sha256.clone(),
        retained_sha256: Some(hex::encode(Sha256::digest(retained))),
        canonical_bytes: canonical.exact_bytes,
        retained_bytes,
        capture_complete: canonical.complete,
        complete,
        unavailable_ranges: unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
        line_starts,
        segments,
    };
    let metadata_bytes = match serde_json::to_vec(&metadata) {
        Ok(bytes) => bytes,
        Err(err) => {
            rollback_logical_artifact_creation(&retention_token, &path);
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(format!("failed to serialize artifact metadata: {err}")),
            };
        }
    };
    if let Err(err) = write_bytes_atomically(&logical_metadata_path(&path), &metadata_bytes) {
        rollback_logical_artifact_creation(&retention_token, &path);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(format!("failed to write artifact metadata: {err}")),
        };
    }
    // The installed segments are the bytes this worker just wrote and synced.
    // Do not reacquire the global lock and read/hash them again for protection.
    // Publish the marker only for a complete, internally consistent receipt.
    if protect_history {
        let protection = if complete
            && unavailable_ranges.is_empty()
            && metadata.retained_sha256.as_deref() == Some(canonical.sha256.as_str())
        {
            create_new_protection_marker(
                &active_tool_history_protection_path(&path),
                ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
            )
        } else {
            Err(std::io::Error::other(
                "history artifact is incomplete or has an invalid digest",
            ))
        };
        if let Err(err) = protection {
            rollback_logical_artifact_creation(&retention_token, &path);
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(format!("failed to protect new history artifact: {err}")),
            };
        }
    }
    if let Err(err) = finish_logical_artifact_transaction(&path) {
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes,
            complete: false,
            unavailable_ranges,
            error: Some(format!("failed to commit artifact transaction: {err}")),
        };
    }
    // Segment and sidecar sizes are known from this transaction; a directory
    // scan would only rediscover them while holding the machine-wide lock.
    let disk_bytes = retained_bytes.saturating_add(metadata_bytes.len() as u64);
    match artifact_retention_record_with_bytes_blocking(&path, disk_bytes) {
        Ok(Some(record)) => {
            publish_known_record(&retention_token, record, LogicalRetentionMutation::Create)
        }
        Ok(None) => publish_known_remove(
            &retention_token,
            &path,
            LogicalRetentionMutation::Create,
            false,
        ),
        Err(_) => reject_stale_delta(&retention_token),
    }
    CanonicalOutputArtifact {
        id: Some(id),
        retained_bytes,
        complete,
        unavailable_ranges,
        error: None,
    }
}

/// Upgrades an existing exact raw-output artifact into the canonical logical
/// artifact without allocating another ID. The first segment stays the legacy
/// `.log`; additional segments and metadata remain private to that ID.
pub(crate) async fn attach_canonical_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    canonical: &CanonicalToolResult,
) -> CanonicalOutputArtifact {
    let codex_home = codex_home.to_path_buf();
    let thread_id = thread_id.to_string();
    let artifact_id = artifact_id.to_string();
    let id = artifact_id.parse::<ToolOutputArtifactId>().ok();
    let exact_bytes = canonical.exact_bytes;
    let (stage_admission, metadata_budget) = match admit_artifact_stage(canonical).await {
        Ok(permit) => permit,
        Err(error) => {
            return CanonicalOutputArtifact {
                id,
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
                error: Some(error),
            };
        }
    };
    let (stage_admission, shared_slot) =
        match reserve_native_stage_slot(codex_home.clone(), stage_admission).await {
            Ok(admission) => admission,
            Err(error) => {
                return CanonicalOutputArtifact {
                    id,
                    retained_bytes: 0,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
                    error: Some(error),
                };
            }
        };
    let canonical = canonical_storage_snapshot(canonical);
    // The coordinator only transfers ownership between flat filesystem workers.
    // Each worker owns pending-path cleanup and, during commit, the retention permit.
    // Caller cancellation detaches the coordinator; runtime shutdown cannot drop a
    // permit or pending-path guard while its filesystem operation is still running.
    tokio::spawn(async move {
        let staged = tokio::task::spawn_blocking(move || {
            let mut staged = stage_attach_canonical_output_artifact(
                codex_home,
                thread_id,
                artifact_id,
                canonical,
                metadata_budget,
                shared_slot,
            )?;
            staged._stage_owner._admission = Some(stage_admission);
            Ok(staged)
        })
        .await?;
        let staged = match staged {
            Ok(staged) => staged,
            Err(result) => return Ok(result),
        };
        let (root, semaphore) = retention_sweep_admission_for_directory(&staged.directory).await?;
        let process_permit = match semaphore.acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                record_retention_sweep_permit_failure(&root, &error);
                return Ok(CanonicalOutputArtifact {
                    id: Some(staged.id),
                    retained_bytes: 0,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(
                        0,
                        staged.canonical.exact_bytes,
                    )],
                    error: Some(format!("failed to admit canonical artifact: {error}")),
                });
            }
        };
        tokio::task::spawn_blocking(move || {
            commit_attach_canonical_output_artifact(staged, process_permit)
        })
        .await
    })
    .await
    .and_then(std::convert::identity)
    .unwrap_or_else(|error| CanonicalOutputArtifact {
        id,
        retained_bytes: 0,
        complete: false,
        unavailable_ranges: vec![CanonicalByteRange::new(0, exact_bytes)],
        error: Some(format!(
            "canonical artifact attachment worker failed: {error}"
        )),
    })
}

fn stage_attach_canonical_output_artifact(
    codex_home: PathBuf,
    thread_id: String,
    artifact_id: String,
    canonical: CanonicalToolResult,
    metadata_budget: CanonicalMetadataBudget,
    mut shared_slot: NativeStageSlot,
) -> Result<StagedCanonicalOutput, CanonicalOutputArtifact> {
    let residency = artifact_residency_file(&codex_home, &thread_id, false)
        .map(Arc::new)
        .map_err(|error| CanonicalOutputArtifact {
            id: None,
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    let id = match artifact_id.parse::<ToolOutputArtifactId>() {
        Ok(id) if id.to_string() == artifact_id => id,
        _ => {
            return Err(CanonicalOutputArtifact {
                id: None,
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some("existing artifact ID is invalid".to_string()),
            });
        }
    };
    let directory = codex_home.join("tool-output").join(thread_id);
    let path = directory.join(format!("{id}.log"));
    let base_lease = open_regular_artifact(&path)
        .and_then(|(file, _)| {
            file.try_lock_shared()
                .map_err(|_| ReadToolOutputError::StillWriting)?;
            Ok(file)
        })
        .map_err(|error| CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.for_model()),
        })?;
    let mut existing = Vec::new();
    let existing = match (&base_lease)
        .take(MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut existing)
    {
        Ok(_) => existing,
        Err(err) => {
            return Err(CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(format!("failed to read existing artifact: {err}")),
            });
        }
    };
    let expected_prefix =
        &canonical.bytes[..canonical.bytes.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
    if existing != expected_prefix {
        return Err(CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(
                existing.len() as u64,
                canonical.exact_bytes,
            )],
            error: Some("existing artifact does not match the canonical byte prefix".to_string()),
        });
    }
    let max_staged_total = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL)
        .min(canonical.exact_bytes) as usize;
    let staged_end = canonical.bytes.len().min(max_staged_total);
    let staged_additional = canonical
        .bytes
        .get(existing.len()..staged_end)
        .unwrap_or_default();
    let mut stage_owner =
        StageOwner::acquire(&directory, id, true).map_err(|error| CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    shared_slot
        .record(&directory, id, stage_owner.tag)
        .map_err(|error| CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(error.to_string()),
        })?;
    stage_owner._shared_slot = Some(shared_slot);
    let staged_segments = match stage_logical_segments_blocking(
        &stage_owner,
        staged_additional,
        existing.len() as u64,
        1,
    ) {
        Ok(segments) => segments,
        Err((start, staged_path, err)) => {
            return Err(CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: start,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                error: Some(format!(
                    "failed to stage artifact segment `{}`: {err}",
                    staged_path.display()
                )),
            });
        }
    };
    let cleanup = StagedLogicalSegmentCleanup::new(&staged_segments);
    Ok(StagedCanonicalOutput {
        directory,
        id,
        canonical,
        metadata_budget,
        existing,
        segments: staged_segments,
        cleanup,
        base_lease: Some(base_lease),
        _residency: residency,
        _stage_owner: stage_owner,
    })
}

fn commit_attach_canonical_output_artifact(
    staged: StagedCanonicalOutput,
    process_permit: OwnedSemaphorePermit,
) -> CanonicalOutputArtifact {
    let StagedCanonicalOutput {
        directory,
        id,
        canonical,
        metadata_budget: canonical_metadata,
        existing,
        segments: staged_segments,
        cleanup: _staged_cleanup,
        base_lease,
        _residency,
        _stage_owner,
    } = staged;
    let root = tool_output_root_for_directory(&directory);
    let _retention_permit = match retention_sweep_permit_blocking(&root, process_permit) {
        Ok(permit) => permit,
        Err(error) => {
            record_retention_sweep_permit_failure(&root, &error);
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: existing.len() as u64,
                complete: false,
                unavailable_ranges: normalized_unavailable_ranges(
                    canonical.exact_bytes,
                    &canonical.unavailable_ranges,
                    existing.len() as u64,
                ),
                error: Some(format!(
                    "failed to acquire canonical artifact retention lock: {error}"
                )),
            };
        }
    };
    let path = directory.join(format!("{id}.log"));
    #[cfg(windows)]
    let _cache_mutation = immutable_snapshot_cache::shared().mutating(&path);
    let staged_additional_bytes = staged_segments
        .iter()
        .map(|segment| segment.range.end - segment.range.start)
        .sum::<u64>();
    // A hash match alone does not authorize replacing already-advertised
    // section or JSON-pointer addresses (for example with a later plain file read). Let the
    // producer allocate a new artifact rather than invalidate old recovery.
    if let Ok((metadata, _family_lease)) = load_logical_metadata(&path, id) {
        if (!metadata.sections.is_empty() && metadata.sections != canonical.sections)
            || metadata
                .json_pointers
                .iter()
                .any(|(pointer, address)| canonical.json_pointers.get(pointer) != Some(address))
        {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: metadata.retained_bytes,
                complete: false,
                unavailable_ranges: Vec::new(),
                error: Some("existing artifact has incompatible recovery addresses".to_string()),
            };
        }
        if metadata.capture_complete
            && metadata.complete
            && canonical.complete
            && metadata.canonical_bytes == canonical.exact_bytes
            && metadata.canonical_sha256 == canonical.sha256
            && metadata.canonical_kind == canonical.kind
            && metadata.sections == canonical.sections
            && metadata.json_pointers == canonical.json_pointers
        {
            // Authenticate under the existing retention lock. Unchanged bytes
            // and addresses need no transaction, index rebuild or metadata sync.
            let error = load_validated_logical_snapshot(&path, &metadata)
                .and_then(|bytes| {
                    if bytes == canonical.bytes {
                        Ok(())
                    } else {
                        Err(ReadToolOutputError::Io(
                            "existing artifact bytes changed".into(),
                        ))
                    }
                })
                .err()
                .map(|error| error.for_model());
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: metadata.retained_bytes,
                complete: error.is_none(),
                unavailable_ranges: metadata.unavailable_ranges,
                error,
            };
        }
    }
    if let Err(err) = reconcile_logical_artifact_transactions(&directory) {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: normalized_unavailable_ranges(
                canonical.exact_bytes,
                &canonical.unavailable_ranges,
                existing.len() as u64,
            ),
            error: Some(format!("failed to reconcile artifact transactions: {err}")),
        };
    }
    let metadata_budget = match canonical_metadata.for_segments(staged_segments.len()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(error),
            };
        }
    };
    enforce_retention_locked_blocking(
        &directory,
        &path,
        canonical
            .exact_bytes
            .saturating_sub(existing.len() as u64)
            .saturating_add(metadata_budget),
        0,
    );
    let usage = retention_usage_locked_blocking(&directory);
    if !retention_shared_lock_held(&root) {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: normalized_unavailable_ranges(
                canonical.exact_bytes,
                &canonical.unavailable_ranges,
                existing.len() as u64,
            ),
            error: Some("lost the canonical artifact retention lock".to_string()),
        };
    }
    let additional_available = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .saturating_sub(usage.thread_bytes)
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(usage.global_bytes));
    if additional_available < metadata_budget {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: normalized_unavailable_ranges(
                canonical.exact_bytes,
                &canonical.unavailable_ranges,
                existing.len() as u64,
            ),
            error: Some("artifact metadata does not fit the retained-family budget".into()),
        };
    }
    let retained_bytes = canonical
        .exact_bytes
        .min(existing.len() as u64 + additional_available.saturating_sub(metadata_budget))
        .min(existing.len() as u64 + staged_additional_bytes);
    let retention_token = capture_retention_token(&directory);
    let retained = &canonical.bytes[..retained_bytes as usize];
    let mut segments = vec![LogicalArtifactSegment {
        index: 0,
        range: CanonicalByteRange::new(0, existing.len() as u64),
    }];
    // Retention excludes GC during the nonblocking shared-to-exclusive handoff.
    // Readers own the base for the entire family, so attachment must defer while
    // any snapshot acquisition still needs the old segments or sidecar.
    drop(base_lease);
    let family_lease = open_regular_artifact(&path).and_then(|(file, _)| {
        file.try_lock()
            .map_err(|_| ReadToolOutputError::StillWriting)?;
        Ok(file)
    });
    let _family_lease = match family_lease {
        Ok(file) => file,
        Err(error) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: existing.len() as u64,
                complete: false,
                unavailable_ranges: normalized_unavailable_ranges(
                    canonical.exact_bytes,
                    &canonical.unavailable_ranges,
                    existing.len() as u64,
                ),
                error: Some(error.for_model()),
            };
        }
    };
    if let Err(err) = begin_logical_artifact_transaction(&path, true) {
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: normalized_unavailable_ranges(
                canonical.exact_bytes,
                &canonical.unavailable_ranges,
                existing.len() as u64,
            ),
            error: Some(format!("failed to begin artifact transaction: {err}")),
        };
    }
    let additional_segments =
        match install_staged_logical_segments_blocking(&path, staged_segments, retained_bytes) {
            Ok(additional_segments) => additional_segments,
            Err((start, staged_path, err)) => {
                reject_stale_delta(&retention_token);
                let _ = reconcile_logical_artifact_transaction(&path);
                return CanonicalOutputArtifact {
                    id: Some(id),
                    retained_bytes: start,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                    error: Some(format!(
                        "failed to install staged artifact segment `{}`: {err}",
                        staged_path.display()
                    )),
                };
            }
        };
    segments.extend(additional_segments.iter().cloned());
    let complete = retained_bytes == canonical.exact_bytes && canonical.unavailable_ranges.is_empty();
    let unavailable_ranges = normalized_unavailable_ranges(
        canonical.exact_bytes,
        &canonical.unavailable_ranges,
        retained_bytes,
    );
    let line_starts = canonical_metadata.line_starts(retained, canonical.bytes.len());
    let metadata = LogicalArtifactMetadata {
        version: LOGICAL_ARTIFACT_METADATA_VERSION,
        artifact_id: id.to_string(),
        canonical_kind: canonical.kind,
        canonical_sha256: canonical.sha256.clone(),
        retained_sha256: Some(hex::encode(Sha256::digest(
            &canonical.bytes[..retained_bytes as usize],
        ))),
        canonical_bytes: canonical.exact_bytes,
        retained_bytes,
        capture_complete: canonical.complete,
        complete,
        unavailable_ranges: unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
        line_starts,
        segments,
    };
    let write = serde_json::to_vec(&metadata)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_bytes_atomically(&logical_metadata_path(&path), &bytes));
    if let Err(err) = write {
        let _ = reconcile_logical_artifact_transaction(&path);
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: normalized_unavailable_ranges(
                canonical.exact_bytes,
                &canonical.unavailable_ranges,
                existing.len() as u64,
            ),
            error: Some(format!("failed to write artifact metadata: {err}")),
        };
    }
    if let Err(err) = finish_logical_artifact_transaction(&path) {
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes,
            complete: false,
            unavailable_ranges,
            error: Some(format!("failed to commit artifact transaction: {err}")),
        };
    }
    match artifact_retention_record_blocking(&path) {
        Ok(Some(record)) => publish_known_record(
            &retention_token,
            record,
            LogicalRetentionMutation::AppendReplace,
        ),
        Ok(None) => publish_known_remove(
            &retention_token,
            &path,
            LogicalRetentionMutation::AppendReplace,
            false,
        ),
        Err(_) => reject_stale_delta(&retention_token),
    }
    CanonicalOutputArtifact {
        id: Some(id),
        retained_bytes,
        complete,
        unavailable_ranges,
        error: None,
    }
}

/// Copies one exact, complete logical artifact into a new thread-owned artifact namespace.
///
/// Forks cannot reuse the parent's handle because `read_tool_output` is deliberately confined to
/// the current thread. Preserve the opaque ID inside the child namespace so already-persisted
/// bounded output does not expose a stale handle, while validating and copying the exact bytes.
/// Any failure is fail-open for callers: the fork keeps the bounded output in canonical history
/// and simply omits the receipt candidate.
pub(crate) async fn remint_tool_history_artifact_for_thread(
    codex_home: &Path,
    source_thread_id: &str,
    target_thread_id: &str,
    artifact_id: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<String, String> {
    if source_thread_id == target_thread_id {
        return Err("tool-history artifact remint requires a distinct target thread".to_string());
    }
    let source_id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| "invalid source tool-output artifact id".to_string())?;
    if source_id.to_string() != artifact_id {
        return Err("non-canonical source tool-output artifact id".to_string());
    }
    let source_path = codex_home
        .join("tool-output")
        .join(source_thread_id)
        .join(format!("{source_id}.log"));
    let expected_sha256 = expected_sha256.to_string();
    let expected_sha256_for_check = expected_sha256.clone();
    let (metadata, bytes) = tokio::task::spawn_blocking(move || {
        let (metadata, _family_lease) =
            load_logical_metadata(&source_path, source_id).map_err(|err| {
                format!(
                    "failed to load source artifact metadata: {}",
                    err.for_model()
                )
            })?;
        if !metadata.capture_complete
            || !metadata.complete
            || metadata.retained_bytes != expected_bytes
            || metadata.canonical_bytes != expected_bytes
            || !metadata.unavailable_ranges.is_empty()
            || metadata.canonical_sha256 != expected_sha256_for_check
        {
            return Err(
                "source artifact identity does not match tool-history metadata".to_string(),
            );
        }
        let bytes = read_logical_range(
            &source_path,
            &metadata,
            CanonicalByteRange::new(0, expected_bytes),
        )
        .map_err(|err| format!("failed to read source artifact: {}", err.for_model()))?;
        if hex::encode(Sha256::digest(&bytes)) != expected_sha256_for_check {
            return Err("source artifact digest does not match tool-history metadata".to_string());
        }
        Ok((metadata, bytes))
    })
    .await
    .map_err(|err| format!("artifact remint verification task failed: {err}"))??;

    let expected_target_metadata = metadata.clone();
    let mut canonical = CanonicalToolResult::bytes(bytes);
    canonical.kind = metadata.canonical_kind;
    canonical.json_pointers = metadata.json_pointers;
    canonical.sections = metadata.sections;

    let target_path = codex_home
        .join("tool-output")
        .join(target_thread_id)
        .join(format!("{source_id}.log"));
    let target_path_for_check = target_path.clone();
    let expected_sha256_for_target = expected_sha256.clone();
    let target_already_exists = tokio::task::spawn_blocking(move || {
        if !logical_artifact_family_exists(&target_path_for_check)
            .map_err(|err| format!("failed to inspect target artifact namespace: {err}"))?
        {
            return Ok(false);
        }
        let (target_metadata, _family_lease) =
            load_logical_metadata(&target_path_for_check, source_id)
                .map_err(|err| format!("target artifact collision: {}", err.for_model()))?;
        if target_metadata != expected_target_metadata {
            return Err("target artifact collision has different metadata".to_string());
        }
        let target_bytes = read_logical_range(
            &target_path_for_check,
            &target_metadata,
            CanonicalByteRange::new(0, expected_bytes),
        )
        .map_err(|err| format!("failed to verify target artifact: {}", err.for_model()))?;
        if hex::encode(Sha256::digest(&target_bytes)) != expected_sha256_for_target {
            return Err("target artifact collision has different bytes".to_string());
        }
        Ok(true)
    })
    .await
    .map_err(|err| format!("target artifact verification task failed: {err}"))??;

    if target_already_exists {
        protect_active_tool_history_artifact(
            codex_home,
            target_thread_id,
            artifact_id,
            expected_bytes,
            &expected_sha256,
        )
        .await
        .map_err(|err| format!("failed to protect existing target artifact: {err}"))?;
        return Ok(artifact_id.to_string());
    }

    let reminted = create_canonical_output_artifact_with_id(
        codex_home,
        target_thread_id,
        &canonical,
        source_id,
    )
    .await;
    let reminted_id = reminted.artifact_id().ok_or_else(|| {
        reminted
            .error
            .clone()
            .unwrap_or_else(|| "artifact remint failed".to_string())
    })?;
    // Roll back a reminted target unless its canonical bytes are complete.
    if !reminted.complete
        || reminted.retained_bytes != expected_bytes
        || !reminted.unavailable_ranges.is_empty()
    {
        rollback_reminted_artifact_creation(target_path, false).await;
        return Err(reminted
            .error
            .clone()
            .unwrap_or_else(|| "reminted artifact is not complete".to_string()));
    }
    if let Err(err) = protect_active_tool_history_artifact(
        codex_home,
        target_thread_id,
        &reminted_id,
        expected_bytes,
        &expected_sha256,
    )
    .await
    {
        rollback_reminted_artifact_creation(target_path, true).await;
        return Err(format!("failed to protect reminted artifact: {err}"));
    }
    Ok(reminted_id)
}

async fn rollback_reminted_artifact_creation(path: PathBuf, remove_protection: bool) {
    // The worker owns the complete cleanup and index publication. Dropping the
    // awaiting remint caller cannot discard an admitted cleanup operation.
    if let Err(error) = run_blocking_artifact_io(move || {
        let token = capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));
        if remove_protection {
            let _ = std::fs::remove_file(active_tool_history_protection_path(&path));
        }
        rollback_logical_artifact_creation(&token, &path);
        Ok(())
    })
    .await
    {
        tracing::warn!(%error, "reminted artifact cleanup worker failed");
    }
}

fn rollback_logical_artifact_creation(token: &RetentionIndexToken, path: &Path) {
    match remove_logical_artifact_files(path) {
        Ok(()) => publish_known_remove(token, path, LogicalRetentionMutation::Cleanup, false),
        Err(_) => reject_stale_delta(token),
    }
}

fn remove_logical_artifact_files(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let _cache_mutation = immutable_snapshot_cache::shared().mutating(path);
    if let Some(directory) = path.parent() {
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let transaction_path = logical_transaction_path(path);
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == format!("{stem}.log")
                || name == format!("{stem}.meta.json")
                || name.starts_with(&format!("{stem}.segment-"))
            {
                match std::fs::remove_file(entry.path()) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
                lock_retention_registry().pending_sync.remove(&entry.path());
            }
        }
        match std::fs::remove_file(&transaction_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn create_new_protection_marker(marker: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(marker)?;
    if let Err(err) = file.write_all(contents).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(marker);
        return Err(err);
    }
    Ok(())
}


#[derive(Debug)]
enum ArtifactVerificationError {
    Invalid(String),
    Unavailable(String),
}

impl ArtifactVerificationError {
    fn message(self) -> String {
        match self {
            Self::Invalid(message) | Self::Unavailable(message) => message,
        }
    }

    fn read(error: ReadToolOutputError) -> Self {
        let message = format!("artifact is not retrievable: {}", error.for_model());
        if error == ReadToolOutputError::Expired {
            Self::Invalid(message)
        } else {
            Self::Unavailable(message)
        }
    }
}

fn verify_tool_history_artifact_blocking(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), ArtifactVerificationError> {
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<ToolOutputArtifactId>().ok())
        .ok_or_else(|| {
            ArtifactVerificationError::Invalid("invalid tool-output artifact id".to_string())
        })?;
    let (metadata, snapshot, _family_lease) =
        load_logical_metadata_with_snapshot(path, id).map_err(ArtifactVerificationError::read)?;
    if !metadata.capture_complete
        || !metadata.complete
        || metadata.canonical_bytes != expected_bytes
        || metadata.retained_bytes != expected_bytes
        || !metadata.unavailable_ranges.is_empty()
    {
        return Err(ArtifactVerificationError::Invalid(
            "artifact byte count does not match receipt metadata".to_string(),
        ));
    }
    if metadata.canonical_sha256 != expected_sha256
        || metadata
            .retained_sha256
            .as_deref()
            .is_some_and(|digest| digest != expected_sha256)
    {
        return Err(ArtifactVerificationError::Invalid(
            "artifact digest does not match receipt metadata".to_string(),
        ));
    }
    // Raw loading already authenticates the bytes; logical loading validates
    // every segment, its lock, digest and index exactly once.
    snapshot
        .map_or_else(|| load_validated_logical_snapshot(path, &metadata), Ok)
        .map_err(ArtifactVerificationError::read)?;
    // A verified inherited receipt is also a durability dependency after a
    // process restart. Keep enrollment in the existing verification pass.
    defer_artifact_sync(path);
    for segment in &metadata.segments {
        defer_artifact_sync(&logical_segment_path(path, segment.index));
    }
    defer_artifact_sync(&logical_metadata_path(path));
    Ok(())
}

/// The caller owns a complete receipt from this module's creation transaction.
/// Check only whether its marker survived session reconciliation during staging.
pub(crate) async fn created_source_snapshot_is_protected(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
) -> Result<bool, String> {
    let path = codex_home.join("tool-output").join(thread_id).join(format!("{artifact_id}.log"));
    run_blocking_artifact_io(move || artifact_is_protected_blocking(&path))
        .await
        .map_err(|error| format!("failed to inspect new source protection: {error}"))
}


/// Protects an artifact only after confinement, size, and digest checks succeed.
pub(crate) async fn protect_active_tool_history_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    protect_active_tool_history_artifacts(
        codex_home,
        thread_id,
        BTreeMap::from([(
            artifact_id.to_string(),
            (expected_bytes, expected_sha256.to_string()),
        )]),
    )
    .await
}

/// Protect a checkpoint's references under one admission/retention lock. Validate
/// the complete set before publishing markers; interrupted publication is safe
/// to reconcile from the durable history using the existing pruning owner.
pub(crate) async fn protect_active_tool_history_artifacts(
    codex_home: &Path,
    thread_id: &str,
    references: BTreeMap<String, (u64, String)>,
) -> Result<(), String> {
    protect_tool_history_artifact_batch(codex_home, thread_id, references, false)
        .await
        .map(|_| ())
}

/// Lifecycle preparation must retain healthy siblings even when one receipt
/// is stale or corrupt. It shares the strict admission owner's worker and lock.
pub(crate) async fn protect_retrievable_tool_history_artifacts(
    codex_home: &Path,
    thread_id: &str,
    references: BTreeMap<String, (u64, String)>,
) -> Result<BTreeSet<String>, String> {
    protect_tool_history_artifact_batch(codex_home, thread_id, references, true).await
}

async fn protect_tool_history_artifact_batch(
    codex_home: &Path,
    thread_id: &str,
    references: BTreeMap<String, (u64, String)>,
    best_effort: bool,
) -> Result<BTreeSet<String>, String> {
    if references.is_empty() {
        return Ok(BTreeSet::new());
    }
    let directory = codex_home.join("tool-output").join(thread_id);
    let (root, semaphore) = retention_sweep_admission_for_directory(&directory)
        .await
        .map_err(|error| format!("artifact protection admission worker failed: {error}"))?;
    let process_permit = semaphore
        .acquire_owned()
        .await
        .map_err(|error| format!("failed to acquire artifact retention admission: {error}"))?;
    // One worker owns every filesystem operation and both retention locks through
    // publication. It never waits for another worker in the same blocking pool.
    tokio::task::spawn_blocking(move || {
        let _permit = retention_sweep_permit_blocking(&root, process_permit).map_err(|error| {
            record_retention_sweep_permit_failure(&root, &error);
            format!("failed to acquire artifact retention lock: {error}")
        })?;
        let mut verified = BTreeSet::new();
        for (id, (bytes, sha256)) in &references {
            let result = id
                .parse::<ToolOutputArtifactId>()
                .ok()
                .filter(|parsed| parsed.to_string() == *id)
                .ok_or_else(|| {
                    ArtifactVerificationError::Invalid(
                        "invalid tool-output artifact id".to_string(),
                    )
                })
                .and_then(|_| {
                    verify_tool_history_artifact_blocking(
                        &directory.join(format!("{id}.log")),
                        *bytes,
                        sha256,
                    )
                });
            match result {
                Ok(()) => {
                    verified.insert(id.clone());
                }
                Err(ArtifactVerificationError::Invalid(_)) if best_effort => {}
                Err(error) => return Err(error.message()),
            }
        }
        let mut protected = BTreeSet::new();
        for id in verified {
            let token = capture_retention_token(&directory);
            let path = directory.join(format!("{id}.log"));
            let marker = active_tool_history_protection_path(&path);
            let result = (|| {
                match std::fs::symlink_metadata(&marker) {
                    Ok(_) => {
                        if !protection_marker_status(
                            &marker,
                            ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
                        )
                        .unwrap_or(false)
                        {
                            return Err(
                                "active tool-history protection marker is invalid".to_string()
                            );
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        create_new_protection_marker(
                            &marker,
                            ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
                        )
                        .map_err(|error| format!("failed to protect artifact: {error}"))?;
                    }
                    Err(error) => {
                        return Err(format!("failed to inspect artifact protection: {error}"));
                    }
                }
                sync_parent_directory(&marker).map_err(|error| {
                    format!("failed to sync active tool-history protection: {error}")
                })?;
                let record = artifact_retention_record_blocking(&path)
                    .map_err(|error| format!("failed to index protected artifact: {error}"))?
                    .ok_or_else(|| {
                        "artifact disappeared before protection was indexed".to_string()
                    })?;
                publish_known_record(&token, record, LogicalRetentionMutation::Protection);
                Ok(())
            })();
            if result.is_err() {
                reject_stale_delta(&token);
            }
            match result {
                Ok(()) => {
                    protected.insert(id);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(protected)
    })
    .await
    .map_err(|error| format!("artifact protection worker failed: {error}"))?
}

/// Removes obsolete active-history markers after the replacement ledger snapshot
/// commits, retaining every referenced key without revalidating artifact bytes.
pub(crate) async fn prune_active_tool_history_artifact_protection(
    codex_home: &Path,
    thread_id: &str,
    referenced_artifacts: &BTreeMap<String, (u64, String)>,
) -> Result<(), String> {
    let directory = codex_home.join("tool-output").join(thread_id);
    let (root, semaphore) = retention_sweep_admission_for_directory(&directory)
        .await
        .map_err(|error| format!("artifact pruning admission worker failed: {error}"))?;
    let process_permit = semaphore
        .acquire_owned()
        .await
        .map_err(|error| format!("failed to acquire artifact pruning admission: {error}"))?;
    let referenced_artifacts = referenced_artifacts.clone();
    // Admission moves into the only filesystem worker before it can block on the
    // OS lock. Neither cancellation nor a one-thread pool can release ownership
    // early or strand nested filesystem work behind this worker.
    tokio::task::spawn_blocking(move || {
        let retention_token = capture_retention_token(&directory);
        let _permit = retention_sweep_permit_blocking(&root, process_permit).map_err(|error| {
            record_retention_sweep_permit_failure(&root, &error);
            reject_stale_delta(&retention_token);
            format!("failed to acquire artifact pruning retention lock: {error}")
        })?;
    let pruning: Result<Vec<PathBuf>, String> = (|| {
        let mut touched = Vec::new();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&directory) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => { reject_stale_delta(&retention_token); return Ok(touched); },
                    Err(error) => {
                        return Err(format!(
                            "failed to inspect active tool-history protection directory: {error}"
                        ));
                    }
                    Ok(_) => {
                        return Err(format!(
                            "failed to read active tool-history protection directory: {error}"
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(format!(
                    "failed to read active tool-history protection directory: {error}"
                ));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("failed to read active tool-history protection entry: {error}"))?;
            let marker = entry.path();
            if marker.extension().and_then(|extension| extension.to_str())
                != Some(ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION)
            {
                continue;
            }
            if marker
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|artifact_id| referenced_artifacts.contains_key(artifact_id))
            {
                continue;
            }
            std::fs::remove_file(&marker).map_err(|error| {
                format!(
                    "failed to remove obsolete active tool-history protection `{}`: {error}",
                    marker.display()
                )
            })?;
            touched.push(marker.with_extension("log"));
        }
        sync_parent_directory(&directory.join("retention-sync")).map_err(|error| {
            format!("failed to sync pruned active tool-history protection: {error}")
        })?;
        Ok(touched)
    })();
        let result = (|| {
            // Publish only the artifacts whose protection changed. Referenced
            // markers and their bytes are never revalidated by pruning.
            for path in pruning? {
                match artifact_retention_record_blocking(&path) {
                    Ok(Some(record)) => publish_known_record(
                        &retention_token, record, LogicalRetentionMutation::Protection,
                    ),
                    Ok(None) => publish_known_remove(
                        &retention_token, &path, LogicalRetentionMutation::Protection, false,
                    ),
                    Err(error) => return Err(format!("failed to index pruned artifact protection: {error}")),
                }
            }
            Ok(())
        })();
        if result.is_err() {
            reject_stale_delta(&retention_token);
        }
        result
    }).await.map_err(|error| format!("active tool-history protection pruning worker failed: {error}"))?
}

/// Reconciles the `active_tool_history` owner after resume, fork, compaction,
/// or history replacement.
pub(crate) async fn reconcile_active_tool_history_artifact_protection(
    codex_home: &Path,
    thread_id: &str,
    referenced_artifacts: &BTreeMap<String, (u64, String)>,
) -> Result<BTreeSet<String>, String> {
    let live = protect_retrievable_tool_history_artifacts(
        codex_home,
        thread_id,
        referenced_artifacts.clone(),
    )
    .await?;

    let directory = codex_home.join("tool-output").join(thread_id);
    let retention_token = match capture_retention_token_async(&directory).await {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(%error, "artifact protection reconciliation preparation failed");
            return Ok(live);
        }
    };
    let Some(permit) = retention_sweep_permit_for_directory(&directory).await else {
        reject_stale_delta(&retention_token);
        return Ok(live);
    };
    let failure_token = retention_token.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                reject_stale_delta(&retention_token);
                return live;
            }
        };
        loop {
            let entry = match entries.next().transpose() {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => {
                    reject_stale_delta(&retention_token);
                    return live;
                }
            };
            let marker = entry.path();
            if marker.extension().and_then(|extension| extension.to_str())
                != Some(ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION)
            {
                continue;
            }
            let Some(artifact_id) = marker.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if live.contains(artifact_id) {
                continue;
            }
            match std::fs::remove_file(&marker) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => reject_stale_delta(&retention_token),
            }
        }
        if sync_parent_directory(&directory.join("retention-sync")).is_err() {
            reject_stale_delta(&retention_token);
            return live;
        }
        // Only this thread's markers changed, so its directory's listing
        // corrects the index. A full-root rebuild here held the shared lock.
        let installed_mode = refresh_retention_thread_directory_blocking(
            &retention_token.root,
            &normalized_retention_path(&directory),
        );
        note_completed_protection_reconciliation(&retention_token.root, installed_mode);
        live
    })
    .await
    {
        Ok(live) => Ok(live),
        Err(error) => {
            reject_stale_delta(&failure_token);
            tracing::warn!(%error, "artifact protection reconciliation worker failed");
            Err(format!(
                "artifact protection reconciliation worker failed: {error}"
            ))
        }
    }
}

pub(crate) async fn replace_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    if let RawOutputArtifact::Pending {
        codex_home,
        thread_id,
    } = artifact
    {
        return if output.len() > LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES {
            create_raw_output_artifact(codex_home, thread_id, output).await
        } else {
            artifact.clone()
        };
    }
    let RawOutputArtifact::Stored {
        id,
        path,
        bytes,
        handle,
        ..
    } = artifact
    else {
        return artifact.clone();
    };
    let retention_token = match capture_retention_token_async(
        path.parent().unwrap_or_else(|| Path::new(".")),
    )
    .await
    {
        Ok(token) => token,
        Err(error) => {
            return failed_with_owned_path(
                path.clone(),
                *bytes,
                format!(
                    "failed to prepare `{}` for replacement: {error}",
                    path.display()
                ),
                None,
            )
            .await;
        }
    };

    match lock_artifact_handle(handle, SeekFrom::Start(0)).await {
        Ok(file) => {
            let file = tokio::fs::File::from_std(file);
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        *bytes,
                        format!("failed to lock `{}` for replacement: {err}", path.display()),
                        Some(&retention_token),
                    )
                    .await;
                }
            };
            let retained = &output[..output.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
            let truncated = retained.len() != output.len();
            let mut digest = Sha256::new();
            digest.update(retained);
            if let Err(error) = invalidate_raw_capture_metadata(path).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    *bytes,
                    format!("failed to invalidate raw capture status: {error}"),
                    Some(&retention_token),
                )
                .await;
            }
            // Replacement is a new data generation even after an earlier
            // successful barrier. Retain it before the first mutating await.
            defer_artifact_sync(path);
            if let Err(err) = file.set_len(0).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    *bytes,
                    format!(
                        "failed to truncate `{}` for replacement: {err}",
                        path.display()
                    ),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(err) = file.write_all(retained).await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    0,
                    format!("failed to replace `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to flush `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(error) = write_raw_capture_metadata(
                path,
                Some(*id),
                retained.len() as u64,
                !truncated,
                hex::encode(digest.clone().finalize()),
            )
            .await
            {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to persist raw capture status: {error}"),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(err) = unlock_output_file(file).await {
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!(
                        "failed to unlock `{}` after replacement: {err}",
                        path.display()
                    ),
                    Some(&retention_token),
                )
                .await;
            }
            enforce_retention_after_upsert(
                path.parent().unwrap_or_else(|| Path::new(".")),
                path,
                &retention_token,
                LogicalRetentionMutation::AppendReplace,
            )
            .await;
            RawOutputArtifact::Stored {
                id: *id,
                path: path.clone(),
                bytes: retained.len() as u64,
                truncated,
                handle: handle.clone(),
                digest,
                finalized: true,
            }
        }
        Err(err) => {
            failed_with_owned_path(
                path.clone(),
                *bytes,
                format!("failed to open `{}` for replacement: {err}", path.display()),
                Some(&retention_token),
            )
            .await
        }
    }
}

async fn failed_with_owned_path(
    path: PathBuf,
    fallback_bytes: u64,
    message: String,
    retention_token: Option<&RetentionIndexToken>,
) -> RawOutputArtifact {
    let bytes = tokio::fs::metadata(&path)
        .await
        .map_or(fallback_bytes, |metadata| metadata.len());
    if let Some(retention_token) = retention_token {
        reject_stale_delta(retention_token);
    }
    enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
    RawOutputArtifact::Failed {
        id: artifact_id_from_path(&path),
        message,
        owned_path: Some(path),
        bytes,
    }
}

fn artifact_id_from_path(path: &Path) -> Option<ToolOutputArtifactId> {
    path.file_stem()?.to_str()?.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadToolOutputError {
    InvalidArtifactId,
    InvalidRange(String),
    Expired,
    StillWriting,
    Io(String),
    /// The handle is missing here but retained in another thread's namespace.
    /// Reading stays confined to the calling thread; this only names the owner.
    OwnedByOtherThread {
        owner_thread_id: String,
        /// True only when the owner is a known spawned descendant of the caller.
        /// Subtree membership cannot be certified complete, so false is unknown.
        known_descendant: bool,
    },
}

impl ReadToolOutputError {
    pub(crate) fn for_model(&self) -> String {
        match self {
            Self::InvalidArtifactId => "artifact_id must be a UUID".to_string(),
            Self::InvalidRange(message) | Self::Io(message) => message.clone(),
            Self::Expired => ARTIFACT_EXPIRED_MESSAGE.to_string(),
            Self::StillWriting => ARTIFACT_WRITING_MESSAGE.to_string(),
            Self::OwnedByOtherThread {
                owner_thread_id,
                known_descendant,
            } => format!(
                "artifact belongs to another thread ({owner_thread_id}{}), not this one; it has not expired, but read_tool_output reads only artifacts retained by the calling thread. Have the owning thread read it, or re-read the original source; output already in context remains usable.",
                if *known_descendant {
                    ", a descendant agent of this thread"
                } else {
                    ""
                }
            ),
        }
    }
}

fn load_logical_metadata(
    path: &Path,
    id: ToolOutputArtifactId,
) -> Result<(LogicalArtifactMetadata, File), ReadToolOutputError> {
    let (mut metadata, snapshot, family_lease) = load_logical_metadata_with_snapshot(path, id)?;
    // Attachment and namespace transfer request the complete address metadata
    // so they cannot erase previously usable raw JSON pointers. Byte/line
    // recovery loads the snapshot directly and does not take this path.
    if let Some(bytes) = snapshot {
        metadata.json_pointers = raw_json_pointer_index(&bytes).unwrap_or_default();
        if !metadata.json_pointers.is_empty() {
            metadata.canonical_kind = CanonicalToolResultKind::Json;
        }
    }
    Ok((metadata, family_lease))
}

/// Borrowed lexical JSON addresses, with duplicate decoded names kept explicit.
struct RawJsonPointerIndex {
    pointers: BTreeMap<String, CanonicalJsonPointer>,
    ambiguous: BTreeSet<String>,
}

impl RawJsonPointerIndex {
    fn ambiguous(&self, pointer: &str) -> bool {
        self.ambiguous.iter().any(|path| {
            path == pointer
                || path
                    .strip_prefix(pointer)
                    .is_some_and(|tail| tail.starts_with('/'))
                || pointer
                    .strip_prefix(path)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
    }
}

/// Index borrowed lexical values without collapsing duplicate decoded keys.
fn lexical_json_pointer_index(contents: &[u8]) -> Option<RawJsonPointerIndex> {
    struct Members<'a>(BTreeMap<String, Option<&'a serde_json::value::RawValue>>);
    impl<'de> Deserialize<'de> for Members<'de> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Members<'de>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a JSON object with borrowed lexical values")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut members = BTreeMap::new();
                    while let Some((key, value)) =
                        map.next_entry::<String, &'de serde_json::value::RawValue>()?
                    {
                        members
                            .entry(key)
                            .and_modify(|entry| *entry = None)
                            .or_insert(Some(value));
                    }
                    Ok(Members(members))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    fn visit(
        raw: &serde_json::value::RawValue,
        base: usize,
        pointer: String,
        depth: usize,
        index: &mut RawJsonPointerIndex,
    ) -> Option<()> {
        if depth > 128 {
            return None;
        }
        let text = raw.get();
        let start = text.as_ptr() as usize - base;
        let mut children = Vec::new();
        if text.starts_with('{') {
            let Members(values) = serde_json::from_str(text).ok()?;
            for (key, value) in values {
                let child = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                if let Some(value) = value {
                    visit(value, base, child.clone(), depth + 1, index)?;
                } else {
                    index.ambiguous.insert(child.clone());
                }
                children.push(child);
            }
        } else if text.starts_with('[') {
            let values: Vec<&serde_json::value::RawValue> = serde_json::from_str(text).ok()?;
            for (position, value) in values.into_iter().enumerate() {
                let child = format!("{pointer}/{position}");
                visit(value, base, child.clone(), depth + 1, index)?;
                children.push(child);
            }
        }
        index.pointers.insert(
            pointer,
            CanonicalJsonPointer {
                range: CanonicalByteRange::new(start as u64, (start + text.len()) as u64),
                exact_bytes: text.len() as u64,
                direct_children: children,
                recovery_chunk_bytes: None,
            },
        );
        Some(())
    }
    let raw: &serde_json::value::RawValue = serde_json::from_slice(contents).ok()?;
    let mut index = RawJsonPointerIndex {
        pointers: BTreeMap::new(),
        ambiguous: BTreeSet::new(),
    };
    visit(
        raw,
        contents.as_ptr() as usize,
        String::new(),
        0,
        &mut index,
    )?;
    Some(index)
}

/// Compatibility producer API. The selector owner independently validates all
/// semantic requests, including ambiguity, before presenting decoded values.
pub(crate) fn raw_json_pointer_index(
    contents: &[u8],
) -> Option<BTreeMap<String, CanonicalJsonPointer>> {
    // Producer cache only; large selections get request-targeted indexes.
    if contents.len() > 16 * 1024 { return None; }
    lexical_json_pointer_index(contents).map(|index| index.pointers)
}

#[path = "command_output_artifact_json.rs"]
mod selective_json;

struct ValidatedJsonPointers {
    metadata: LogicalArtifactMetadata,
    index: Option<RawJsonPointerIndex>,
    error: Option<&'static str>,
    unavailable_targets: BTreeSet<String>,
}

impl ValidatedJsonPointers {
    fn new(metadata: &LogicalArtifactMetadata, bytes: &[u8]) -> Self {
        // Small complete indexes remain transaction-local and reusable. Large
        // documents use a bounded request-targeted index instead.
        let index = (metadata.complete && bytes.len() <= 16 * 1024)
            .then(|| lexical_json_pointer_index(bytes)).flatten();
        Self::with_index(metadata, index, None)
    }

    fn for_selectors(metadata: &LogicalArtifactMetadata, bytes: &[u8], selectors: &[ToolOutputSelector]) -> Self {
        let targets = selectors.iter().filter_map(|selector| match selector {
            ToolOutputSelector::JsonPointer { pointer } => Some(pointer.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        let index = metadata
            .complete
            .then(|| selective_json::index(bytes, &targets))
            .flatten();
        if index.is_none() && metadata.complete
            && let Some((index, failed)) = selective_json::index_preserving_siblings(bytes, &targets)
        {
            let available = targets.iter().filter(|target| !failed.contains(*target)).cloned().collect::<Vec<_>>();
            let mut validated = Self::with_index(metadata, Some(index), Some(&available));
            validated.unavailable_targets = failed;
            return validated;
        }
        Self::with_index(metadata, index, Some(&targets))
    }

    fn with_index(metadata: &LogicalArtifactMetadata, mut index: Option<RawJsonPointerIndex>, targets: Option<&[String]>) -> Self {
        let error = if !metadata.complete {
            Some(
                "Incomplete JSON cannot establish unique semantic addresses; recover retained bytes.",
            )
        } else if let Some(index) = &index {
            // Persisted pointers are only a cache. Hints may differ, but paths,
            // lexical extents and child relationships must describe these bytes.
            let matches = |key: &String, entry: &CanonicalJsonPointer| {
                index.ambiguous(key)
                    || index.pointers.get(key).is_some_and(|actual| {
                        entry.range == actual.range
                            && entry.exact_bytes == actual.exact_bytes
                            && (targets.is_some_and(|targets| !targets.contains(key))
                                || entry.direct_children == actual.direct_children)
                    })
            };
            let mismatch = !metadata.json_pointers.is_empty()
                && (metadata
                    .json_pointers
                    .iter()
                    .filter(|(key, _)| targets.is_none() || index.pointers.contains_key(*key))
                    .any(|(key, entry)| !matches(key, entry))
                    || index.pointers.keys().any(|key| {
                        !index.ambiguous(key) && !metadata.json_pointers.contains_key(key)
                    }));
            mismatch.then_some("Persisted JSON pointer index does not match the retained bytes; recover bytes instead.")
        } else {
            Some(
                "Malformed, depth-limited or index-budget-limited JSON has no verified semantic addresses; recover bytes instead.",
            )
        };
        let mut verified = metadata.clone();
        verified.json_pointers = index
            .as_mut()
            .map(|index| std::mem::take(&mut index.pointers))
            .unwrap_or_default();
        Self {
            metadata: verified,
            index,
            error,
            unavailable_targets: BTreeSet::new(),
        }
    }

    fn error(&self, selector: &ToolOutputSelector) -> Option<&'static str> {
        let ToolOutputSelector::JsonPointer { pointer } = selector else {
            return None;
        };
        if self.unavailable_targets.contains(pointer) {
            return Some("JSON pointer exceeds the bounded address-index budget; raw bytes and successful sibling selections remain available.");
        }
        self.error.or_else(|| self.index.as_ref().filter(|index| index.ambiguous(pointer))
            .map(|_| "JSON pointer is ambiguous because a decoded object member name is duplicated; recover bytes instead."))
    }
}

fn read_artifact_metadata_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_CANONICAL_METADATA_BYTES {
        return Err(std::io::Error::other(
            "artifact metadata exceeds its bounded representation",
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_CANONICAL_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CANONICAL_METADATA_BYTES {
        return Err(std::io::Error::other(
            "artifact metadata grew beyond its bounded representation",
        ));
    }
    Ok(bytes)
}

// Read-only recovery must make progress after a committed transaction loses
// its writer. Follow the writer's retention -> family lock order, using only
// nonblocking admission: live writers and slow storage never gain a new lock
// dependency on a reader that is waiting for them.
fn reconcile_orphaned_artifact_on_read(path: &Path) -> Result<(), ReadToolOutputError> {
    let directory = path.parent().ok_or(ReadToolOutputError::InvalidArtifactId)?;
    let root = normalized_tool_output_root(&tool_output_root_for_directory(directory));
    let process_permit = retention_sweep_semaphore(&root).try_acquire_owned()
        .map_err(|_| ReadToolOutputError::StillWriting)?;
    let target = retention_interprocess_lock_target(&root);
    let lock = codex_file_system::try_acquire_atomic_write_lock(&target)
        .map_err(|error| ReadToolOutputError::Io(error.to_string()))?
        .ok_or(ReadToolOutputError::StillWriting)?;
    advance_retention_interprocess_generation(&root, &target)
        .map_err(|error| ReadToolOutputError::Io(error.to_string()))?;
    held_retention_locks().insert(root.clone(), lock);
    let _retention = RetentionSweepPermit { root, _process_permit: process_permit };
    let (family, _) = open_regular_artifact(path)?;
    family.try_lock().map_err(|_| ReadToolOutputError::StillWriting)?;
    reconcile_logical_artifact_transaction(path)
        .map_err(|error| ReadToolOutputError::Io(error.to_string()))?;
    reject_stale_delta(&capture_retention_token(directory));
    Ok(())
}

fn load_logical_metadata_with_snapshot(
    path: &Path,
    id: ToolOutputArtifactId,
) -> Result<(LogicalArtifactMetadata, Option<Vec<u8>>, File), ReadToolOutputError> {
    load_logical_metadata_with_observation(path, id, None)
}

fn load_logical_metadata_with_observation(
    path: &Path,
    id: ToolOutputArtifactId,
    protected_metadata: Option<LogicalArtifactMetadata>,
) -> Result<(LogicalArtifactMetadata, Option<Vec<u8>>, File), ReadToolOutputError> {
    match std::fs::symlink_metadata(logical_transaction_path(path)) {
        Ok(_) => reconcile_orphaned_artifact_on_read(path)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ReadToolOutputError::Io(format!(
                "failed to inspect artifact transaction: {err}"
            )));
        }
    }
    let metadata_path = logical_metadata_path(path);
    // Pair raw completion status with its bytes under the producer's lock.
    // Reading status first could certify a same-size replacement using stale
    // metadata from the previous completed capture.
    let (file, _) = open_regular_artifact(path)?;
    file.try_lock_shared()
        .map_err(|_| ReadToolOutputError::StillWriting)?;
    if logical_transaction_path(path).try_exists()
        .map_err(|error| ReadToolOutputError::Io(error.to_string()))?
    {
        return Err(ReadToolOutputError::StillWriting);
    }
    if let Some(metadata) = protected_metadata {
        if metadata.version != LOGICAL_ARTIFACT_METADATA_VERSION || metadata.artifact_id != id.to_string() {
            return Err(ReadToolOutputError::Io("artifact metadata version or identity mismatch".into()));
        }
        return Ok((metadata, None, file));
    }
    let metadata_bytes = read_artifact_metadata_bounded(&metadata_path);
    let raw_capture = match metadata_bytes {
        Ok(bytes) => {
            if let Ok(raw) = serde_json::from_slice::<RawCaptureMetadata>(&bytes) {
                if !matches!(raw.raw_capture_version, 1 | 2) || raw.artifact_id != id.to_string() {
                    return Err(ReadToolOutputError::Io(
                        "raw capture metadata version or identity mismatch".to_string(),
                    ));
                }
                Some(raw)
            } else {
                let metadata: LogicalArtifactMetadata =
                    serde_json::from_slice(&bytes).map_err(|err| {
                        ReadToolOutputError::Io(format!("failed to parse artifact metadata: {err}"))
                    })?;
                if metadata.version != LOGICAL_ARTIFACT_METADATA_VERSION {
                    return Err(ReadToolOutputError::Io(
                        "artifact metadata version mismatch".to_string(),
                    ));
                }
                if metadata.artifact_id != id.to_string() {
                    return Err(ReadToolOutputError::Io(
                        "artifact metadata identity mismatch".to_string(),
                    ));
                }
                return Ok((metadata, None, file));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(ReadToolOutputError::Io(format!(
                "failed to read artifact metadata: {err}"
            )));
        }
    };
    let mut contents = Vec::new();
    (&file)
        .take(MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut contents)
        .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?;
    if contents.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES {
        return Err(ReadToolOutputError::Io(
            "raw artifact exceeds its retained-byte limit".into(),
        ));
    }
    let bytes = contents.len() as u64;
    if raw_capture
        .as_ref()
        .is_some_and(|raw| raw.retained_bytes != bytes)
    {
        return Err(ReadToolOutputError::Io(
            "raw capture size does not match metadata".to_string(),
        ));
    }
    let sha256 = hex::encode(Sha256::digest(&contents));
    if let Some(raw) = &raw_capture
        && raw.raw_capture_version == 2
        && raw.retained_sha256.as_deref() != Some(sha256.as_str())
    {
        return Err(ReadToolOutputError::Io(
            "raw capture SHA identity does not match metadata".into(),
        ));
    }
    let metadata = LogicalArtifactMetadata {
        // Zero is an internal-only marker, never a persisted logical format.
        // A legacy raw receipt can be checked against an independent expected
        // digest, but cannot authenticate original bytes by ID alone.
        version: if raw_capture
            .as_ref()
            .is_some_and(|raw| raw.raw_capture_version == 2)
        {
            LOGICAL_ARTIFACT_METADATA_VERSION
        } else {
            0
        },
        artifact_id: id.to_string(),
        canonical_kind: CanonicalToolResultKind::Bytes,
        canonical_sha256: sha256.clone(),
        retained_sha256: Some(sha256),
        canonical_bytes: bytes,
        retained_bytes: bytes,
        capture_complete: raw_capture.is_some_and(|raw| raw.capture_complete),
        complete: true,
        unavailable_ranges: Vec::new(),
        json_pointers: BTreeMap::new(),
        sections: Vec::new(),
        // Line addressing is optional; authenticated bytes must remain usable
        // when a dense line directory exceeds its independent memory bound.
        line_starts: canonical_line_starts(&contents).unwrap_or_default(),
        segments: vec![LogicalArtifactSegment {
            index: 0,
            range: CanonicalByteRange::new(0, bytes),
        }],
    };
    Ok((metadata, Some(contents), file))
}

fn read_logical_range(
    path: &Path,
    metadata: &LogicalArtifactMetadata,
    range: CanonicalByteRange,
) -> Result<Vec<u8>, ReadToolOutputError> {
    if range.start > range.end || range.end > metadata.retained_bytes {
        return Err(ReadToolOutputError::InvalidRange(format!(
            "byte range must be within retained bytes 0..{}",
            metadata.retained_bytes
        )));
    }
    let mut output = Vec::with_capacity(range.len() as usize);
    for segment in &metadata.segments {
        // Validate size and read bytes through the same opened regular file.
        // Snapshot recovery used to open every segment once for validation and
        // again for delivery, including for a one-byte requested selection.
        let segment_path = logical_segment_path(path, segment.index);
        let (mut file, actual_bytes) = open_regular_artifact(&segment_path)?;
        file.try_lock_shared()
            .map_err(|_| ReadToolOutputError::StillWriting)?;
        if actual_bytes != segment.range.len() {
            return Err(ReadToolOutputError::Io(
                "artifact segment size does not match metadata".to_string(),
            ));
        }
        let start = range.start.max(segment.range.start);
        let end = range.end.min(segment.range.end);
        if start >= end {
            continue;
        }
        file.seek(SeekFrom::Start(start - segment.range.start))
            .map_err(|err| ReadToolOutputError::Io(format!("failed to seek artifact: {err}")))?;
        let length = (end - start) as usize;
        let offset = output.len();
        output.resize(offset + length, 0);
        file.read_exact(&mut output[offset..])
            .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?;
    }
    if output.len() as u64 != range.len() {
        return Err(ReadToolOutputError::Io(
            "artifact segments do not cover the requested range".to_string(),
        ));
    }
    Ok(output)
}

fn load_validated_logical_snapshot(
    path: &Path,
    metadata: &LogicalArtifactMetadata,
) -> Result<Vec<u8>, ReadToolOutputError> {
    let unavailable_ranges_are_invalid =
        metadata
            .unavailable_ranges
            .iter()
            .enumerate()
            .any(|(index, range)| {
                range.start >= range.end
                    || range.end > metadata.canonical_bytes
                    || index.checked_sub(1).is_some_and(|previous| {
                        metadata.unavailable_ranges[previous].end >= range.start
                    })
            });
    if metadata.retained_bytes > metadata.canonical_bytes
        || unavailable_ranges_are_invalid
        || (metadata.complete
            && (metadata.retained_bytes != metadata.canonical_bytes
                || !metadata.unavailable_ranges.is_empty()))
        || (!metadata.complete && metadata.unavailable_ranges.is_empty())
    {
        return Err(ReadToolOutputError::Io(
            "artifact retained and unavailable ranges do not match metadata".to_string(),
        ));
    }
    let snapshot = read_logical_range(
        path,
        metadata,
        CanonicalByteRange::new(0, metadata.retained_bytes),
    )?;
    let expected_sha256 = metadata.retained_sha256.as_deref().or_else(|| {
        (metadata.complete && metadata.retained_bytes == metadata.canonical_bytes)
            .then_some(metadata.canonical_sha256.as_str())
    });
    let Some(expected_sha256) = expected_sha256 else {
        return Err(ReadToolOutputError::Io(
            "incomplete artifact metadata has no retained SHA identity".to_string(),
        ));
    };
    let actual_sha256 = hex::encode(Sha256::digest(&snapshot));
    if actual_sha256 != expected_sha256
        || (metadata.complete && actual_sha256 != metadata.canonical_sha256)
        || metadata.canonical_sha256.len() != 64
        || !metadata
            .canonical_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ReadToolOutputError::Io(
            "artifact SHA identity does not match metadata".to_string(),
        ));
    }
    // A newline-dense result stores an empty index, and its line and search
    // selectors fail explicitly. Any stored index must match exactly.
    if !metadata.line_starts.is_empty()
        && canonical_line_starts(&snapshot).ok().as_ref() != Some(&metadata.line_starts)
    {
        return Err(ReadToolOutputError::Io(
            "artifact line index does not match its validated snapshot".to_string(),
        ));
    }
    let ranges_are_valid = metadata.json_pointers.values().all(|entry| {
        entry.range.start <= entry.range.end && entry.range.end <= metadata.canonical_bytes
    }) && metadata.sections.iter().all(|section| {
        section
            .canonical_range
            .is_none_or(|range| range.start <= range.end && range.end <= metadata.canonical_bytes)
    });
    if !ranges_are_valid {
        return Err(ReadToolOutputError::Io(
            "artifact selector index is outside the validated snapshot".to_string(),
        ));
    }
    Ok(snapshot)
}

fn validated_snapshot_range(
    snapshot: &[u8],
    range: CanonicalByteRange,
) -> Result<&[u8], ReadToolOutputError> {
    let start = usize::try_from(range.start)
        .map_err(|_| ReadToolOutputError::InvalidRange("byte range is too large".to_string()))?;
    let end = usize::try_from(range.end)
        .map_err(|_| ReadToolOutputError::InvalidRange("byte range is too large".to_string()))?;
    snapshot.get(start..end).ok_or_else(|| {
        ReadToolOutputError::InvalidRange(
            "byte range is outside the validated artifact snapshot".to_string(),
        )
    })
}

type SelectorRangeAndChildren = (
    Option<CanonicalByteRange>,
    Vec<ToolOutputSelector>,
    Option<Value>,
);

fn selector_range(
    selector: &ToolOutputSelector,
    metadata: &LogicalArtifactMetadata,
) -> Result<Option<CanonicalByteRange>, ToolOutputSelectorStatus> {
    match selector {
        ToolOutputSelector::Bytes { start, end } => {
            // Like a lines end past EOF, a bytes end past the retained output clamps.
            if start > end || *start > metadata.retained_bytes {
                Err(ToolOutputSelectorStatus::Invalid)
            } else {
                Ok(Some(CanonicalByteRange::new(*start, (*end).min(metadata.retained_bytes))))
            }
        }
        ToolOutputSelector::Lines { start, end } => {
            if *start == 0 || end < start || *start > metadata.line_starts.len() {
                return Err(ToolOutputSelectorStatus::Invalid);
            }
            Ok(Some(CanonicalByteRange::new(
                metadata.line_starts[*start - 1],
                metadata.line_starts.get(*end).copied().unwrap_or(metadata.retained_bytes),
            )))
        }
        ToolOutputSelector::JsonPointer { pointer } => {
            if (!pointer.is_empty() && !pointer.starts_with('/'))
                || pointer.split('~').skip(1).any(|escape| !escape.starts_with(['0', '1']))
            {
                return Err(ToolOutputSelectorStatus::Invalid);
            }
            metadata.json_pointers.get(pointer).map(|entry| Some(entry.range))
                .ok_or(ToolOutputSelectorStatus::NotFound)
        }
        ToolOutputSelector::Section { id } => metadata.sections.iter()
            .find(|section| section.id == *id).map(|section| section.canonical_range)
            .ok_or(ToolOutputSelectorStatus::NotFound),
        ToolOutputSelector::Search { .. } => Err(ToolOutputSelectorStatus::Invalid),
    }
}

fn selector_range_and_children(
    selector: &ToolOutputSelector,
    metadata: &LogicalArtifactMetadata,
) -> Result<SelectorRangeAndChildren, ToolOutputSelectorStatus> {
    let range = selector_range(selector, metadata)?;
    let (children, value) = match selector {
        ToolOutputSelector::JsonPointer { pointer } => {
            let entry = metadata.json_pointers.get(pointer)
                .ok_or(ToolOutputSelectorStatus::NotFound)?;
            (entry.direct_children.iter().map(|pointer|
                ToolOutputSelector::JsonPointer { pointer: pointer.clone() }).collect(), None)
        }
        ToolOutputSelector::Section { id } => {
            let section = metadata.sections.iter().find(|section| section.id == *id)
                .ok_or(ToolOutputSelectorStatus::NotFound)?;
            let children = section.children.iter().map(|id|
                ToolOutputSelector::Section { id: id.clone() }).collect::<Vec<_>>();
            let value = if range.is_none() {
                let mut value = serde_json::json!({
                    "section_id": section.id, "kind": "directory", "children": children,
                });
                if let Some(details) = &section.value { value["details"] = details.clone(); }
                Some(value)
            } else { section.value.clone() };
            (children, value)
        }
        _ => (Vec::new(), None),
    };
    Ok((range, children, value))
}

fn normalized_selector_order_key(
    selector: &ToolOutputSelector,
    metadata: &LogicalArtifactMetadata,
) -> (u64, u64, u8, String) {
    let serialized = serde_json::to_string(selector).unwrap_or_default();
    if let ToolOutputSelector::Search { start_byte, .. } = selector {
        return (*start_byte, *start_byte, 4, serialized);
    }
    match selector_range(selector, metadata) {
        Ok(Some(range)) => {
            let kind = match selector {
                ToolOutputSelector::Bytes { .. } => 0,
                ToolOutputSelector::Lines { .. } => 1,
                ToolOutputSelector::Section { .. } => 2,
                ToolOutputSelector::JsonPointer { .. } => 3,
                ToolOutputSelector::Search { .. } => unreachable!(),
            };
            (range.start, range.end, kind, serialized)
        }
        Ok(None) => (u64::MAX - 1, u64::MAX - 1, 2, serialized),
        Err(_) => (u64::MAX, u64::MAX, u8::MAX, serialized),
    }
}

/// Whether one selector would still come back complete.
///
/// A merged range is acceptable when it fits the fragment ceiling outright, or
/// when the auto-drain path can still return it whole in this same call. What
/// normalization must never produce is a range that can only come back as
/// `selector_too_large`, because the caller's own fragments would each have
/// succeeded.
///
/// This runs the same selection the read itself performs, on the real bytes, so
/// normalization and selection cannot disagree about what fits.
fn merged_selector_still_completes(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selector: &ToolOutputSelector,
    fragment_token_ceiling: usize,
    final_token_ceiling: usize,
) -> bool {
    let selected = select_logical_artifact(
        metadata,
        snapshot,
        selector.clone(),
        fragment_token_ceiling,
        final_token_ceiling,
        &[],
        None,
    );
    match selected.status {
        ToolOutputSelectorStatus::Ok => true,
        ToolOutputSelectorStatus::SelectorTooLarge => {
            // Draining only counts when the read would actually accept the
            // drained result, which is the same ceiling the read applies.
            let Some(completed) =
                internally_drain_exact_subdivisions(metadata, snapshot, &selected)
            else {
                return false;
            };
            let individual = ReadToolOutputResult {
                artifact_id: metadata.artifact_id.clone(),
                canonical_sha256: metadata.canonical_sha256.clone(),
                canonical_bytes: metadata.canonical_bytes,
                retained_bytes: metadata.retained_bytes,
                capture_complete: metadata.capture_complete,
                complete: true,
                unavailable_ranges: Vec::new(),
                results: vec![completed],
            };
            response_fits_recovery_retry_avoidance_ceiling(&individual, final_token_ceiling)
        }
        _ => false,
    }
}

fn normalize_tool_output_selectors(
    selectors: Vec<ToolOutputSelector>,
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    fragment_token_ceiling: usize,
    final_token_ceiling: usize,
) -> Vec<ToolOutputSelector> {
    normalize_tool_output_selectors_with_fit(selectors, metadata, |selector| {
        merged_selector_still_completes(
            metadata,
            snapshot,
            selector,
            fragment_token_ceiling,
            final_token_ceiling,
        )
    })
}

fn normalize_tool_output_selectors_with_fit(
    selectors: Vec<ToolOutputSelector>,
    metadata: &LogicalArtifactMetadata,
    fits: impl Fn(&ToolOutputSelector) -> bool,
) -> Vec<ToolOutputSelector> {
    let mut byte_ranges = Vec::<(u64, u64)>::new();
    let mut line_ranges = Vec::<(usize, usize)>::new();
    let mut others = Vec::<ToolOutputSelector>::new();
    for selector in selectors {
        match selector {
            ToolOutputSelector::Bytes { start, end }
                if start <= end && end <= metadata.retained_bytes =>
            {
                byte_ranges.push((start, end));
            }
            ToolOutputSelector::Lines { start, end }
                if selector_range_and_children(
                    &ToolOutputSelector::Lines { start, end },
                    metadata,
                )
                .is_ok() =>
            {
                line_ranges.push((start, end));
            }
            selector if !others.contains(&selector) => others.push(selector),
            _ => {}
        }
    }

    // Normalization exists to remove duplicate and adjacent work, not to build
    // a range the caller never asked for and that cannot be returned. A merge
    // that would no longer fit the fragment ceiling is skipped, so selectors
    // that each fit keep working instead of collapsing into one that fails.
    byte_ranges.sort_unstable();
    let mut merged_bytes = Vec::<(u64, u64)>::new();
    for (start, end) in byte_ranges {
        match merged_bytes.last_mut() {
            // Already covered: dropping it removes work without growing a range.
            Some((_, previous_end)) if end <= *previous_end => {}
            Some((previous_start, previous_end)) if start <= *previous_end => {
                let candidate = ToolOutputSelector::Bytes {
                    start: *previous_start,
                    end,
                };
                if fits(&candidate) {
                    *previous_end = end;
                } else {
                    merged_bytes.push((start, end));
                }
            }
            _ => merged_bytes.push((start, end)),
        }
    }
    let merged_lines = normalize_line_ranges_within_ceiling(line_ranges, |start, end| {
        fits(&ToolOutputSelector::Lines { start, end })
    });

    let mut normalized = merged_bytes
        .into_iter()
        .map(|(start, end)| ToolOutputSelector::Bytes { start, end })
        .chain(
            merged_lines
                .into_iter()
                .map(|(start, end)| ToolOutputSelector::Lines { start, end }),
        )
        .chain(others)
        .collect::<Vec<_>>();
    normalized.sort_by_cached_key(|selector| normalized_selector_order_key(selector, metadata));
    normalized
}

pub(crate) fn normalize_line_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut normalized: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match normalized.last_mut() {
            Some((_, previous_end)) if start <= previous_end.saturating_add(1) => {
                *previous_end = (*previous_end).max(end);
            }
            _ => normalized.push((start, end)),
        }
    }
    normalized
}

/// Normalizes line ranges, declining any merge whose result would no longer
/// fit. Ranges the caller supplied individually stay individually readable.
fn normalize_line_ranges_within_ceiling(
    mut ranges: Vec<(usize, usize)>,
    fits: impl Fn(usize, usize) -> bool,
) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut normalized: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match normalized.last_mut() {
            Some((_, previous_end)) if end <= *previous_end => {}
            Some((previous_start, previous_end)) if start <= previous_end.saturating_add(1) => {
                if fits(*previous_start, end) {
                    *previous_end = end;
                } else {
                    normalized.push((start, end));
                }
            }
            _ => normalized.push((start, end)),
        }
    }
    normalized
}

fn invalid_selector_result(
    selector: ToolOutputSelector,
    message: impl Into<String>,
) -> ToolOutputSelectorResult {
    let mut result = ToolOutputSelectorResult::state(selector, ToolOutputSelectorStatus::Invalid);
    result.message = Some(message.into());
    result
}

/// Reuse only text already delivered in this response, never a prior call or
/// another reference. Canonical artifact bytes and recovery selectors stay intact.
fn share_search_hydration(
    range: CanonicalByteRange,
    previous_results: &[ToolOutputSelectorResult],
    reference: &mut Value,
) -> bool {
    if range.len() < 256 {
        return false;
    }
    let previous = previous_results
        .iter()
        .filter(|result| {
            result.status == ToolOutputSelectorStatus::Ok && result.selector.is_search()
        })
        .filter_map(|result| result.value.as_ref())
        .filter_map(|value| value["hydrated_ranges"].as_array())
        .flatten()
        .find(|value| {
            value["canonical_range"] == reference["canonical_range"]
                && (value["text"].is_string() || value["data_base64"].is_string())
        });
    let Some(previous) = previous else {
        return false;
    };
    reference["shared"] = serde_json::json!(true);
    if codex_utils_output_truncation::model_token_count(&reference.to_string())
        < codex_utils_output_truncation::model_token_count(&previous.to_string())
    {
        true
    } else {
        if let Some(object) = reference.as_object_mut() {
            object.remove("shared");
        }
        false
    }
}

/// Share only bytes actually present in earlier results of this response. Split
/// overlapping contexts rather than dropping the new evidence at either edge.
/// Exact child selectors remain unchanged for independent snapshot recovery.
fn share_overlapping_search_hydration(
    hydrated: Value,
    snapshot: &[u8],
    previous_results: &[ToolOutputSelectorResult],
) -> Vec<Value> {
    let Some(start) = hydrated["canonical_range"]["start"].as_u64() else {
        return vec![hydrated];
    };
    let Some(end) = hydrated["canonical_range"]["end"].as_u64() else {
        return vec![hydrated];
    };
    if hydrated["shared"] == true || end.saturating_sub(start) < 256 {
        return vec![hydrated];
    }
    let mut overlaps = previous_results
        .iter()
        .filter(|result| {
            result.status == ToolOutputSelectorStatus::Ok && result.selector.is_search()
        })
        .filter_map(|result| result.value.as_ref())
        .filter_map(|value| value["hydrated_ranges"].as_array())
        .flatten()
        .filter(|value| value["text"].is_string() || value["data_base64"].is_string())
        .filter_map(|value| {
            let left = start.max(value["canonical_range"]["start"].as_u64()?);
            let right = end.min(value["canonical_range"]["end"].as_u64()?);
            (right.saturating_sub(left) >= 256).then_some((left, right))
        })
        .chain(previous_results.iter().filter_map(|result| {
            if result.status != ToolOutputSelectorStatus::Ok
                || (result.text.is_none() && result.data_base64.is_none())
            {
                return None;
            }
            let range = result.canonical_range?;
            let left = start.max(range.start);
            let right = end.min(range.end);
            (right.saturating_sub(left) >= 256).then_some((left, right))
        }))
        .collect::<Vec<_>>();
    overlaps.sort_unstable();
    if overlaps.is_empty() {
        return vec![hydrated];
    }
    let mut parts = Vec::new();
    let mut cursor = start;
    for (left, right) in overlaps {
        if right <= cursor {
            continue;
        }
        if left > cursor {
            parts.push((cursor, left, false));
        }
        parts.push((left.max(cursor), right, true));
        cursor = right;
    }
    if cursor < end {
        parts.push((cursor, end, false));
    }
    let compact = parts
        .into_iter()
        .map(|(start, end, shared)| {
            let mut part = serde_json::json!({
                "selector": {"kind": "bytes", "start": start, "end": end},
                "canonical_range": {"start": start, "end": end},
                "exact_bytes": end - start,
            });
            if shared {
                part["shared"] = Value::Bool(true);
            } else {
                let exact =
                    snapshot.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)?;
                if let Ok(text) = std::str::from_utf8(exact) {
                    part["text"] = Value::String(text.to_string());
                } else {
                    part["data_base64"] = Value::String(BASE64_STANDARD.encode(exact));
                }
            }
            Some(part)
        })
        .collect::<Option<Vec<_>>>();
    let original = vec![hydrated];
    if let Some(compact) = compact
        && codex_utils_output_truncation::model_token_count(&serde_json::json!(compact).to_string())
            < codex_utils_output_truncation::model_token_count(
                &serde_json::json!(original).to_string(),
            )
    {
        compact
    } else {
        original
    }
}

#[derive(Clone, Copy)]
enum SearchBudget {
    ResponseTokens(usize),
    SelectorBytes(usize),
}

#[derive(Clone, PartialEq, Eq)]
struct AuthenticatedSearchCountKey {
    artifact_id: String,
    retained_sha256: String,
    retained_bytes: u64,
    query: String,
    case_insensitive: bool,
    start_byte: u64,
}

fn search_count_cache() -> &'static StdMutex<std::collections::VecDeque<(AuthenticatedSearchCountKey, usize)>> {
    static CACHE: OnceLock<StdMutex<std::collections::VecDeque<(AuthenticatedSearchCountKey, usize)>>> = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(std::collections::VecDeque::new()))
}

fn remember_search_count(key: AuthenticatedSearchCountKey, count: usize) {
    let mut cache = search_count_cache().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = cache.iter().position(|(candidate, _)| candidate == &key) {
        cache.remove(index);
    }
    cache.push_back((key, count));
    // Counts only: never keep artifact bytes, file handles, or family leases.
    while cache.len() > 128 { cache.pop_front(); }
}

fn search_logical_artifact(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selector: ToolOutputSelector,
    budget: SearchBudget,
    previous_results: &[ToolOutputSelectorResult],
    previous_cost: Option<&SelectorResponseCost>,
) -> ToolOutputSelectorResult {
    if metadata.line_starts.is_empty() {
        return invalid_selector_result(selector,
            "line/search indexing exceeds its bounded representation; exact byte selectors remain available");
    }
    let (query, start_byte, max_results, context_lines, case_insensitive, enclosing) =
        match &selector {
            ToolOutputSelector::Search {
                query,
                start_byte,
                max_results,
                context_lines,
                case_insensitive,
                enclosing,
            } => (
                query.clone(),
                *start_byte,
                *max_results,
                *context_lines,
                *case_insensitive,
                *enclosing,
            ),
            _ => unreachable!("search helper requires a search selector"),
        };
    if query.is_empty() || query.len() > ARTIFACT_SEARCH_MAX_QUERY_BYTES {
        return invalid_selector_result(
            selector,
            format!("search query must contain 1..={ARTIFACT_SEARCH_MAX_QUERY_BYTES} UTF-8 bytes"),
        );
    }
    if max_results == 0 {
        return invalid_selector_result(
            selector,
            format!("max_results must be between 1 and {ARTIFACT_SEARCH_MAX_RESULTS}"),
        );
    }
    let max_results = max_results.min(ARTIFACT_SEARCH_MAX_RESULTS);
    let context_lines = context_lines.min(ARTIFACT_SEARCH_MAX_CONTEXT_LINES);
    if start_byte > metadata.retained_bytes {
        return invalid_selector_result(
            selector,
            format!(
                "start_byte must be within retained bytes 0..{}",
                metadata.retained_bytes
            ),
        );
    }
    let query_bytes = query.as_bytes();
    let search_offset = usize::try_from(start_byte).unwrap_or(usize::MAX);
    let search_bytes = snapshot.get(search_offset..).unwrap_or_default();
    // Earlier results in this transaction counted the same immutable bytes.
    // Reuse only an identical scan origin or its exact non-overlapping cursor;
    // an arbitrary offset inside a previous match changes matching semantics.
    // Cross-call reuse follows authentication, expiration and ownership checks
    // in the snapshot loader. Counts are keyed to the authenticated bytes, not
    // a mutable filename or provider-supplied canonical identity alone.
    let retained_digest = metadata.retained_sha256.as_ref().or_else(|| {
        (metadata.capture_complete && metadata.retained_bytes == metadata.canonical_bytes)
            .then_some(&metadata.canonical_sha256)
    });
    let cache_key = retained_digest.map(|digest| AuthenticatedSearchCountKey {
        artifact_id: metadata.artifact_id.clone(), retained_sha256: digest.clone(),
        retained_bytes: metadata.retained_bytes, query: query.clone(), case_insensitive, start_byte,
    });
    let known_total = previous_results.iter().rev().find_map(|previous| {
        let ToolOutputSelector::Search {
            query: previous_query,
            case_insensitive: previous_case,
            start_byte: previous_start,
            ..
        } = &previous.selector else {
            return None;
        };
        if previous.status != ToolOutputSelectorStatus::Ok
            || previous_query != &query
            || *previous_case != case_insensitive
        {
            return None;
        }
        let value = previous.value.as_ref()?;
        if *previous_start == start_byte {
            value["total_matches"].as_u64().and_then(|count| usize::try_from(count).ok())
        } else if matches!(previous.continuation.as_ref(), Some(ToolOutputSelector::Search { start_byte: next, .. }) if *next == start_byte) {
            value["remaining_match_count"].as_u64().and_then(|count| usize::try_from(count).ok())
        } else {
            None
        }
    }).or_else(|| cache_key.as_ref().and_then(|key| {
        search_count_cache().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter().rev().find(|(candidate, _)| candidate == key).map(|(_, count)| *count)
    }));
    let mut total_matches = known_total.unwrap_or(0);
    let mut indexed_matches = Vec::with_capacity(max_results);
    let mut cursor = 0;
    while known_total != Some(0) {
        let Some(offset) = (if case_insensitive {
            crate::tools::shell_output_summary::find_ascii_case_bytes(
                &search_bytes[cursor..],
                query_bytes,
            )
        } else {
            memchr::memmem::find(&search_bytes[cursor..], query_bytes)
        }) else {
            break;
        };
        let relative_match_start = cursor + offset;
        let relative_match_end = relative_match_start.saturating_add(query_bytes.len());
        cursor = relative_match_end;
        let match_start = search_offset.saturating_add(relative_match_start);
        let match_end = search_offset.saturating_add(relative_match_end);
        if known_total.is_none() {
            total_matches = total_matches.saturating_add(1);
        }
        if indexed_matches.len() < max_results {
            indexed_matches.push((match_start, match_end));
        }
        if known_total.is_some() && indexed_matches.len() == max_results {
            break;
        }
    }

    if let Some(key) = cache_key.as_ref() { remember_search_count(key.clone(), total_matches); }
    let remember_continuation = |result: &ToolOutputSelectorResult| {
        if let (Some(key), Some(ToolOutputSelector::Search { start_byte, .. }), Some(value)) =
            (cache_key.as_ref(), result.continuation.as_ref(), result.value.as_ref())
            && let Some(count) = value["remaining_match_count"].as_u64().and_then(|count| usize::try_from(count).ok())
        {
            let mut key = key.clone(); key.start_byte = *start_byte;
            remember_search_count(key, count);
        }
    };

    // Item sections are retained with the immutable snapshot. Resolve each
    // discovered hit once, before fitting/presenting prefixes of this page.
    let enrichment_available = !enclosing
        || metadata
            .sections
            .iter()
            .any(|section| section.id == "outline"
                && section.value.as_ref().is_some_and(|value| value["parse_complete"] != false));
    let contexts = indexed_matches.iter().map(|&(match_start, match_end)| {
        let line = metadata.line_starts.partition_point(|start| *start <= match_start as u64).max(1);
        let end_line = metadata.line_starts.partition_point(|start| *start <= match_end.saturating_sub(1) as u64).max(line);
        let mut start = line.saturating_sub(context_lines).max(1);
        let mut end = end_line.saturating_add(context_lines).min(metadata.line_starts.len());
        let mut enclosing_item = None;
        if enclosing && let Some((section, range)) = metadata.sections.iter()
            .filter(|section| section.id.starts_with("code:"))
            .filter_map(|section| section.canonical_range.map(|range| (section, range)))
            .filter(|(_, range)| range.start <= match_start as u64 && match_end as u64 <= range.end)
            .min_by_key(|(_, range)| range.len())
        {
            start = start.min(metadata.line_starts.partition_point(|offset| *offset <= range.start).max(1));
            end = end.max(metadata.line_starts.partition_point(|offset| *offset < range.end).max(1));
            // Hydrated ranges are merged, so coordinates alone do not say which
            // function or impl a use site sits in. The retained outline does.
            enclosing_item = metadata.sections.iter()
                .filter(|page| page.id == "outline" || page.id.starts_with("outline:"))
                .filter_map(|page| page.value.as_ref()?.get("items")?.as_array())
                .flatten()
                .find(|item| item["id"] == section.id.as_str())
                .map(|item| {
                    let mut identity = serde_json::json!({"qualified_name": item["qualified_name"], "kind": item["kind"],
                        "start_line": item["start_line"], "end_line": item["end_line"]});
                    // Preserve the same parser uncertainty as symbol selectors,
                    // including when this search reuses an immutable snapshot.
                    if item["syntax_error"] == true {
                        identity["syntax_error"] = Value::Bool(true);
                    }
                    identity
                });
        }
        (line, end_line, start, end, enclosing_item)
    }).collect::<Vec<_>>();
    let build_result = |matches_returned: usize, hydrate: bool| {
        let mut matches = Vec::with_capacity(matches_returned);
        let mut ranges = Vec::with_capacity(matches_returned);
        for (
            (match_start, match_end),
            (line, end_line, context_start, context_end, enclosing_item),
        ) in indexed_matches
            .iter()
            .copied()
            .zip(&contexts)
            .take(matches_returned)
        {
            let mut record = serde_json::json!({
                "line": line,
                "end_line": end_line,
                "start_byte": match_start,
                "end_byte": match_end,
            });
            if let Some(item) = enclosing_item {
                record["enclosing_item"] = item.clone();
            }
            matches.push(record);
            ranges.push((*context_start, *context_end));
        }
        let child_selectors = normalize_line_ranges(ranges)
            .into_iter()
            .map(|(start, end)| ToolOutputSelector::Lines { start, end })
            .collect::<Vec<_>>();

        let remaining_match_count = total_matches.saturating_sub(matches_returned);
        let continuation = (remaining_match_count > 0).then(|| ToolOutputSelector::Search {
            query: query.clone(),
            enclosing,
            case_insensitive,
            start_byte: indexed_matches
                .get(..matches_returned)
                .and_then(|delivered| delivered.last())
                .map(|(_, end)| *end as u64)
                .unwrap_or(start_byte),
            max_results,
            context_lines,
        });
        let hydrated_ranges = child_selectors
            .iter()
            .filter(|_| hydrate)
            .filter_map(|child| {
                let (range, _, _) = selector_range_and_children(child, metadata).ok()?;
                let range = range?;
                let start = usize::try_from(range.start).ok()?;
                let end = usize::try_from(range.end).ok()?;
                let exact = snapshot.get(start..end)?;
                let mut hydrated = serde_json::json!({
                    "selector": child,
                    "canonical_range": range,
                    "exact_bytes": range.len(),
                });
                if share_search_hydration(range, previous_results, &mut hydrated) {
                    return Some(hydrated);
                }
                if let Ok(text) = std::str::from_utf8(exact) {
                    hydrated["text"] = Value::String(text.to_string());
                } else {
                    hydrated["data_base64"] = Value::String(BASE64_STANDARD.encode(exact));
                }
                Some(hydrated)
            })
            .flat_map(|hydrated| {
                share_overlapping_search_hydration(hydrated, snapshot, previous_results)
            })
            .collect::<Vec<_>>();
        let mut result =
            ToolOutputSelectorResult::state(selector.clone(), ToolOutputSelectorStatus::Ok);
        result.complete = matches_returned == indexed_matches.len() && enrichment_available;
        result.value = Some(serde_json::json!({
            "query": query,
            "start_byte": start_byte,
            "coverage_complete": metadata.capture_complete && metadata.complete && metadata.unavailable_ranges.is_empty(),
            "total_matches": total_matches,
            "matches_returned": matches_returned,
            "remaining_match_count": remaining_match_count,
            "search_exhausted": remaining_match_count == 0,
            "matches": matches,
            "hydrated_ranges": hydrated_ranges,
        }));
        if enclosing && let Some(value) = result.value.as_mut() {
            value["enclosing_complete"] = Value::Bool(enrichment_available);
        }
        result.child_selectors = child_selectors;
        result.continuation = continuation;
        result.message = match remaining_match_count > 0 {
            true => Some(
                "more matches are available; exact context for this page is already hydrated and continuation advances to the next page"
                    .to_string(),
            ),
            false => None,
        };
        if !enrichment_available {
            result.message = Some("Enclosing enrichment unavailable: this snapshot has no retained item sections. Ordinary search evidence is returned; it is not a complete enclosing item.".into());
        }
        result
    };

    let owned_cost;
    let previous_cost = match previous_cost {
        Some(cost) => cost,
        None => {
            owned_cost = SelectorResponseCost::new(metadata, previous_results);
            &owned_cost
        }
    };
    let fits = |candidate: &ToolOutputSelectorResult| match budget {
        SearchBudget::ResponseTokens(ceiling) => {
            previous_cost
                .with_next(selector_serialized_cost(candidate), metadata.complete)
                .tokens()
                <= ceiling
        }
        SearchBudget::SelectorBytes(ceiling) => {
            serde_json::to_vec(candidate).is_ok_and(|bytes| bytes.len() <= ceiling)
        }
    };
    // Most search pages fit as a whole. Avoid building and serializing every
    // smaller prefix in that case. Oversized pages retain the exact bounded
    // fallback: merging context ranges makes their sizes non-monotonic.
    let full_page = build_result(indexed_matches.len(), true);
    if fits(&full_page) {
        remember_continuation(&full_page);
        return full_page;
    }
    drop(full_page);
    // For unshared UTF-8 contexts, price the exact JSON body from escaped-byte
    // prefix counts. Each source byte is examined once, not copied/serialized
    // again for every growing prefix. Keep the reference path for binary and
    // cross-selector sharing, whose split/merge presentation has separate costs.
    let incremental = previous_results.is_empty() && std::str::from_utf8(snapshot).is_ok();
    let mut escaped_offsets = BTreeMap::new();
    if incremental {
        let mut boundaries = BTreeSet::new();
        for (_, _, start, end, _) in &contexts {
            if let Ok((Some(range), _, _)) = selector_range_and_children(
                &ToolOutputSelector::Lines { start: *start, end: *end }, metadata,
            ) {
                boundaries.insert(range.start as usize);
                boundaries.insert(range.end as usize);
            }
        }
        let mut cursor = boundaries.first().copied().unwrap_or(0);
        let mut escaped = 0;
        for end in boundaries {
            for byte in &snapshot[cursor..end] {
                escaped += match byte {
                    b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                    0..=31 => 6,
                    _ => 1,
                };
            }
            escaped_offsets.insert(end as u64, escaped);
            cursor = end;
        }
    }
    let mut result = build_result(0, false);
    let mut best_count = 0;
    let mut hydration_costs = BTreeMap::new();
    for matches_returned in 1..indexed_matches.len() {
        let candidate = build_result(matches_returned, !incremental);
        let admitted = if incremental {
            let measured = (|| {
                let mut bytes = serde_json::to_vec(&candidate).ok()?.len();
                for (index, child) in candidate.child_selectors.iter().enumerate() {
                    let (range, _, _) = selector_range_and_children(child, metadata).ok()?;
                    let range = range?;
                    let escaped = escaped_offsets.get(&range.end)? - escaped_offsets.get(&range.start)?;
                    let cost = hydration_costs.entry((range.start, range.end)).or_insert_with(|| {
                        let envelope = serde_json::json!({
                            "selector": child, "canonical_range": range,
                            "exact_bytes": range.len(), "text": "",
                        });
                        envelope.to_string().len() + escaped
                    });
                    bytes = bytes.saturating_add(*cost).saturating_add(usize::from(index > 0));
                }
                Some(bytes)
            })();
            measured.map_or_else(|| fits(&build_result(matches_returned, true)), |bytes| match budget {
                SearchBudget::SelectorBytes(ceiling) => bytes <= ceiling,
                SearchBudget::ResponseTokens(ceiling) => previous_cost.with_next(SelectorPartCost {
                    serialized: TokenCountEstimate::from_byte_count(bytes),
                    successful: true, complete: candidate.complete,
                }, metadata.complete).tokens() <= ceiling,
            })
        } else {
            fits(&candidate)
        };
        if admitted {
            best_count = matches_returned;
            if !incremental { result = candidate; }
        }
    }
    if incremental && best_count > 0 {
        result = build_result(best_count, true);
    }
    if result
        .value
        .as_ref()
        .is_some_and(|value| value["matches_returned"] == 0)
        && !indexed_matches.is_empty()
    {
        let mut coordinates = build_result(1, false);
        coordinates.complete = false;
        if let Some(value) = coordinates.value.as_mut() {
            value["hydrated_ranges"] = serde_json::json!([]);
            if enclosing {
                value["enclosing_complete"] = Value::Bool(false);
            }
        }
        coordinates.message = Some(
            "Match coordinates delivered; context exceeds this budget. Read child_selectors for exact context; the search continuation starts after this match.".to_string(),
        );
        if fits(&coordinates) {
            remember_continuation(&coordinates);
            return coordinates;
        }
        result.status = ToolOutputSelectorStatus::SelectorTooLarge;
        result.message = Some("Match record exceeds this budget; no matches delivered and the search cursor has not advanced.".to_string());
    }
    remember_continuation(&result);
    result
}

fn too_large_result(
    selector: ToolOutputSelector,
    range: CanonicalByteRange,
    child_selectors: Vec<ToolOutputSelector>,
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    token_ceiling: usize,
) -> ToolOutputSelectorResult {
    let chunk_bytes = largest_fitting_byte_prefix(
        &metadata.artifact_id,
        metadata.canonical_bytes,
        range,
        snapshot,
        token_ceiling,
    );
    let mut children = Vec::new();
    if !range.is_empty() {
        children.push(ToolOutputSelector::Bytes {
            start: range.start,
            end: range.start.saturating_add(chunk_bytes).min(range.end),
        });
    }
    children.extend(child_selectors);
    let mut result = ToolOutputSelectorResult {
        selector,
        status: ToolOutputSelectorStatus::SelectorTooLarge,
        complete: false,
        exact_bytes: Some(range.len()),
        canonical_range: Some(range),
        text: None,
        value: None,
        data_base64: None,
        // A uniform plan is a promise that every chunk it names can be read.
        // Advertise one only where that is verified; otherwise the recomputed
        // continuation is the honest answer, and the field already permits it.
        subdivision_plan: verified_subdivision_plan(range, chunk_bytes),
        child_selectors: children,
        continuation: None,
        message: Some(
            "exact selector exceeds the bounded response; retry only with continuation or child_selectors, never the parent selector"
                .to_string(),
        ),
    };
    result.continuation = result.child_selectors.first().cloned();
    while !response_fits_recovery_token_ceiling(
        &ReadToolOutputResult {
            artifact_id: metadata.artifact_id.clone(),
            canonical_sha256: metadata.canonical_sha256.clone(),
            canonical_bytes: metadata.canonical_bytes,
            retained_bytes: metadata.retained_bytes,
            capture_complete: metadata.capture_complete,
            complete: metadata.complete,
            unavailable_ranges: metadata.unavailable_ranges.clone(),
            results: vec![result.clone()],
        },
        token_ceiling,
    ) && result.child_selectors.len() > 1
    {
        result.child_selectors.pop();
    }
    result
}

/// A uniform subdivision plan, but only where its guarantees hold.
///
/// The plan claims every chunk of `chunk_bytes` in `range` can be drained, and
/// the auto-drain path only accepts a plan that stays inside its own limits.
/// Advertising one outside them — 265 chunks of 278 bytes, in the case that
/// motivated this — costs a round trip per chunk and never completes.
fn verified_subdivision_plan(
    range: CanonicalByteRange,
    chunk_bytes: u64,
) -> Option<ByteSubdivisionPlan> {
    if range.is_empty() || chunk_bytes == 0 || range.len() > MAX_AUTOMATIC_SUBDIVISION_BYTES {
        return None;
    }
    let chunk_count = range.len().div_ceil(chunk_bytes);
    if chunk_count > MAX_AUTOMATIC_SUBDIVISIONS {
        return None;
    }
    Some(ByteSubdivisionPlan {
        range,
        chunk_bytes,
        chunk_count,
        selector_kind: "bytes".to_string(),
    })
}

fn successful_exact_selector_result(
    selector: ToolOutputSelector,
    range: CanonicalByteRange,
    children: Vec<ToolOutputSelector>,
    bytes: &[u8],
) -> ToolOutputSelectorResult {
    if matches!(selector, ToolOutputSelector::Bytes { .. }) {
        return successful_byte_selector_result(range, bytes);
    }
    let mut result =
        ToolOutputSelectorResult::state(selector.clone(), ToolOutputSelectorStatus::Ok);
    result.complete = true;
    result.exact_bytes = Some(range.len());
    result.canonical_range = Some(range);
    result.child_selectors = children;
    match &selector {
        ToolOutputSelector::JsonPointer { .. } => {
            result.value = serde_json::from_slice(bytes).ok();
            if result.value.is_none() {
                result.data_base64 = Some(BASE64_STANDARD.encode(bytes));
            } else {
                // Every direct child is already addressable from the complete
                // value and parent pointer. Repeating a potentially unbounded
                // address list can spill an otherwise fitting JSON selection.
                // Directory-only and incomplete results still need their lists.
                result.child_selectors.clear();
            }
        }
        ToolOutputSelector::Lines { .. } | ToolOutputSelector::Section { .. } => {
            match std::str::from_utf8(bytes) {
                Ok(text) => result.text = Some(text.to_string()),
                Err(_) => result.data_base64 = Some(BASE64_STANDARD.encode(bytes)),
            }
        }
        ToolOutputSelector::Bytes { .. } | ToolOutputSelector::Search { .. } => unreachable!(),
    }
    result
}

fn exact_selector_result(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selector: ToolOutputSelector,
) -> ToolOutputSelectorResult {
    if matches!(selector, ToolOutputSelector::JsonPointer { .. })
        && metadata.json_pointers.is_empty()
    {
        return invalid_selector_result(
            selector,
            "json_pointer requires valid JSON within the existing depth limit; raw snapshot bytes and sibling selections remain available",
        );
    }
    if matches!(selector, ToolOutputSelector::Lines { .. }) && metadata.line_starts.is_empty() {
        return invalid_selector_result(
            selector,
            "line indexing exceeds its bounded representation; exact byte selectors remain available",
        );
    }
    let (range, children, directory_value) = match selector_range_and_children(&selector, metadata)
    {
        Ok(result) => result,
        Err(status) => {
            let mut result = ToolOutputSelectorResult::state(selector, status);
            result.message = Some(match status {
                ToolOutputSelectorStatus::NotFound => "selector was not found".to_string(),
                _ => "selector is invalid for this artifact".to_string(),
            });
            return result;
        }
    };
    let Some(range) = range else {
        let mut result = ToolOutputSelectorResult::state(selector, ToolOutputSelectorStatus::Ok);
        result.complete = true;
        result.value = directory_value;
        result.child_selectors = children;
        return result;
    };
    let bytes = match validated_snapshot_range(snapshot, range) {
        Ok(bytes) => bytes,
        Err(err) => {
            let mut result =
                ToolOutputSelectorResult::state(selector, ToolOutputSelectorStatus::Invalid);
            result.exact_bytes = Some(range.len());
            result.canonical_range = Some(range);
            result.message = Some(err.for_model());
            return result;
        }
    };
    successful_exact_selector_result(selector, range, children, bytes)
}

fn internally_drain_exact_subdivisions(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    original: &ToolOutputSelectorResult,
) -> Option<ToolOutputSelectorResult> {
    let plan = original.subdivision_plan.clone()?;
    if plan.selector_kind != "bytes"
        || plan.chunk_bytes == 0
        || plan.chunk_count == 0
        || plan.chunk_count > MAX_AUTOMATIC_SUBDIVISIONS
        || plan.range.len() > MAX_AUTOMATIC_SUBDIVISION_BYTES
        || plan.range.len().div_ceil(plan.chunk_bytes) != plan.chunk_count
    {
        return None;
    }
    let (range, children, _) = selector_range_and_children(&original.selector, metadata).ok()?;
    if range != Some(plan.range) {
        return None;
    }
    // Every validated subdivision is contiguous in this same immutable source.
    // The checked count/width above defines that exact partition; validating
    // its complete range proves every child without reconstructing a Vec.
    let bytes = validated_snapshot_range(snapshot, plan.range).ok()?;
    let mut completed =
        successful_exact_selector_result(original.selector.clone(), plan.range, children, bytes);
    completed.subdivision_plan = Some(plan.clone());
    completed.message = Some(format!(
        "internally drained all {} deterministic byte subdivisions",
        plan.chunk_count
    ));
    Some(completed)
}

fn select_logical_artifact(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selector: ToolOutputSelector,
    fragment_token_ceiling: usize,
    final_token_ceiling: usize,
    previous_results: &[ToolOutputSelectorResult],
    previous_cost: Option<&SelectorResponseCost>,
) -> ToolOutputSelectorResult {
    if selector.is_search() {
        return search_logical_artifact(
            metadata,
            snapshot,
            selector,
            SearchBudget::ResponseTokens(final_token_ceiling),
            previous_results,
            previous_cost,
        );
    }
    // Exact text and base64 cannot serialize to fewer bytes than their source.
    // Validate the range before this lower-bound rejection; semantic JSON
    // values and directories intentionally retain their ordinary path.
    let preflight = true;
    if preflight && !matches!(selector, ToolOutputSelector::JsonPointer { .. })
        && let Ok((Some(range), children, _)) = selector_range_and_children(&selector, metadata)
        && validated_snapshot_range(snapshot, range).is_ok()
        && range.len() > (final_token_ceiling as u64).saturating_mul(4)
    {
        return too_large_result(selector, range, children, metadata, snapshot, fragment_token_ceiling);
    }
    let mut result = exact_selector_result(metadata, snapshot, selector.clone());
    let range = result.canonical_range;
    let children = result.child_selectors.clone();
    let individual = ReadToolOutputResult {
        artifact_id: metadata.artifact_id.clone(),
        canonical_sha256: metadata.canonical_sha256.clone(),
        canonical_bytes: metadata.canonical_bytes,
        retained_bytes: metadata.retained_bytes,
        capture_complete: metadata.capture_complete,
        complete: result.status == ToolOutputSelectorStatus::Ok && result.complete,
        unavailable_ranges: Vec::new(),
        results: vec![result.clone()],
    };
    if result.status == ToolOutputSelectorStatus::Ok
        // Try the whole exact selection under the unchanged final ceiling
        // before paying for display-sized fragments and reconstruction.
        && !response_fits_recovery_token_ceiling(&individual, final_token_ceiling)
        && let Some(range) = range
    {
        result = too_large_result(
            selector,
            range,
            children,
            metadata,
            snapshot,
            fragment_token_ceiling,
        );
    }
    result
}


/// Host-side exact read for transformations of retained evidence. Reuses the
/// selector reader's identity, confinement, integrity, and writer-lock checks;
/// unlike a model projection it must reject any incomplete source.
pub(crate) async fn read_complete_canonical_snapshot(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, ReadToolOutputError> {
    let id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| ReadToolOutputError::InvalidArtifactId)?;
    if id.to_string() != artifact_id {
        return Err(ReadToolOutputError::InvalidArtifactId);
    }
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    {
        let registry = lock_retention_registry();
        if registry.pending_canonical.contains(&path) {
            return Err(ReadToolOutputError::StillWriting);
        }
        if let Some(error) = registry.failed_canonical.get(&path) {
            return Err(ReadToolOutputError::Io(format!(
                "canonical retention failed: {error}; do not replay effects"
            )));
        }
    }
    tokio::task::spawn_blocking(move || {
        let (metadata, snapshot, _family_lease) = load_logical_metadata_with_snapshot(&path, id)?;
        if metadata.version == 0
            || !metadata.capture_complete
            || !metadata.complete
            || metadata.canonical_bytes != metadata.retained_bytes
            || !metadata.unavailable_ranges.is_empty()
            || metadata.canonical_bytes > max_bytes as u64
        {
            return Err(ReadToolOutputError::InvalidRange(
                "source artifact is incomplete or exceeds the host transformation limit"
                    .to_string(),
            ));
        }
        snapshot.map_or_else(|| load_validated_logical_snapshot(&path, &metadata), Ok)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?
}



pub(crate) async fn read_tool_output_selectors_with_ceiling_and_reuse(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
) -> Result<(ReadToolOutputResult, bool), ReadToolOutputError> {
    let snapshot = load_tool_output_snapshot(codex_home, thread_id, artifact_id).await?;
    let result = snapshot.select(selectors, token_ceiling).await?;
    Ok((result, false))
}

/// One authenticated immutable observation. Cross-call reuse is permitted only
/// while an OS-enforced lease pins the complete family and its namespace;
/// ordinary mutable artifacts are authenticated anew on every call.
pub(crate) struct ToolOutputSnapshot {
    metadata: LogicalArtifactMetadata,
    bytes: Vec<u8>,
    indexed_metadata: OnceLock<ValidatedJsonPointers>,
}

impl ToolOutputSnapshot {
    /// Original byte identity, independent of capture completeness or freshness.
    pub(crate) fn historical_authenticity(&self) -> &'static str {
        if self.metadata.version == 0 {
            "unverified"
        } else {
            "authenticated"
        }
    }

    pub(super) fn canonical_result(&self) -> CanonicalToolResult {
        let mut result = CanonicalToolResult::bytes(self.bytes.clone());
        result.complete =
            self.metadata.version != 0 && self.metadata.capture_complete && self.metadata.complete;
        result.unavailable_ranges = self.metadata.unavailable_ranges.clone();
        result
    }

    pub(crate) fn page_selectors(
        &self,
        start: u64,
        end: u64,
        token_ceiling: usize,
    ) -> Vec<ToolOutputSelector> {
        bounded_page_selectors(
            &self.metadata.artifact_id,
            &self.bytes,
            start,
            end,
            token_ceiling,
        )
    }

    pub(crate) async fn select(
        self: &Arc<Self>,
        selectors: Vec<ToolOutputSelector>,
        token_ceiling: usize,
    ) -> Result<ReadToolOutputResult, ReadToolOutputError> {
        let snapshot = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            // Derive missing lexical addresses once per authenticated recovery
            // transaction, regardless of how the snapshot was first selected.
            let targeted;
            let pointers = if selectors
                .iter()
                .any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. }))
            {
                if snapshot.bytes.len() <= 16 * 1024 {
                    Some(snapshot.indexed_metadata.get_or_init(|| {
                        ValidatedJsonPointers::new(&snapshot.metadata, &snapshot.bytes)
                    }))
                } else {
                    targeted = ValidatedJsonPointers::for_selectors(&snapshot.metadata, &snapshot.bytes, &selectors);
                    Some(&targeted)
                }
            } else {
                None
            };
            select_tool_output_snapshot_indexed(
                &snapshot.metadata,
                &snapshot.bytes,
                selectors,
                token_ceiling,
                pointers,
            )
        })
        .await
        .map_err(|err| ReadToolOutputError::Io(format!("failed to select artifact: {err}")))?
    }
}

pub(crate) async fn load_tool_output_snapshot(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
    let id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| ReadToolOutputError::InvalidArtifactId)?;
    if id.to_string() != artifact_id {
        return Err(ReadToolOutputError::InvalidArtifactId);
    }
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    tokio::task::spawn_blocking(move || {
        // Capabilities are published before a possibly stalled writer creates
        // its metadata file. Consult custody before interpreting a missing file
        // as expiry; no disk or cache lock is held under this registry guard.
        {
            let registry = lock_retention_registry();
            if registry.pending_canonical.contains(&path) {
                return Err(ReadToolOutputError::StillWriting);
            }
            if let Some(error) = registry.failed_canonical.get(&path) {
                return Err(ReadToolOutputError::Io(format!(
                    "canonical retention failed: {error}; do not replay effects"
                )));
            }
        }
        #[cfg(windows)]
        {
            immutable_snapshot_cache::shared()
                .load_with_metadata(&path, id, |metadata| load_tool_output_snapshot_with_observation(&path, id, metadata))
        }
        #[cfg(not(windows))]
        load_tool_output_snapshot_uncached(&path, id)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?
}

#[cfg(not(windows))]
fn load_tool_output_snapshot_uncached(
    path: &Path,
    id: ToolOutputArtifactId,
) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
    load_tool_output_snapshot_with_observation(path, id, None)
}

fn load_tool_output_snapshot_with_observation(
    path: &Path,
    id: ToolOutputArtifactId,
    protected_metadata: Option<LogicalArtifactMetadata>,
) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
    let (metadata, snapshot, _family_lease) = load_logical_metadata_with_observation(path, id, protected_metadata)?;
    let bytes = snapshot.map_or_else(|| load_validated_logical_snapshot(path, &metadata), Ok)?;
    Ok(Arc::new(ToolOutputSnapshot {
        metadata,
        bytes,
        indexed_metadata: OnceLock::new(),
    }))
}

/// Bounds the ownership diagnosis of a missing handle: one metadata lookup per
/// sibling thread namespace, on the error path only.
const OTHER_THREAD_ARTIFACT_SCAN_LIMIT: usize = 4_096;

/// Diagnoses a handle missing from `thread_id` by naming another thread whose
/// namespace retains the same artifact ID, such as a subagent that produced
/// it. This checks only for the base file; it never opens, authenticates or
/// returns artifact bytes, so recovery remains confined to the calling thread.
/// `None` covers absence, an invalid handle, and an incomplete or failed scan.
pub(crate) async fn find_other_thread_artifact_owner(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
) -> Option<String> {
    let id = artifact_id.parse::<ToolOutputArtifactId>().ok()?;
    if id.to_string() != artifact_id {
        return None;
    }
    let root = codex_home.join("tool-output");
    let thread_id = thread_id.to_string();
    let file_name = format!("{id}.log");
    run_blocking_artifact_io(move || {
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        for entry in entries.take(OTHER_THREAD_ARTIFACT_SCAN_LIMIT) {
            let Ok(entry) = entry else { continue };
            let Ok(owner) = entry.file_name().into_string() else {
                continue;
            };
            // Directories without valid thread IDs are not thread-owned, and
            // reparse points are not followed into another location.
            if owner == thread_id
                || codex_protocol::ThreadId::from_string(&owner).is_err()
                || !entry.file_type().is_ok_and(|kind| kind.is_dir())
            {
                continue;
            }
            let base = std::fs::symlink_metadata(entry.path().join(&file_name));
            if base.is_ok_and(|base| base.is_file() && !metadata_is_reparse_point(&base)) {
                return Ok(Some(owner));
            }
        }
        Ok(None)
    })
    .await
    .ok()
    .flatten()
}

/// Select from bytes already owned by a producer. Disk recovery uses this same
/// selector implementation after validating its persisted snapshot.
fn select_tool_output_snapshot(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
) -> Result<ReadToolOutputResult, ReadToolOutputError> {
    let pointers = selectors
        .iter()
        .any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. }))
        .then(|| ValidatedJsonPointers::for_selectors(metadata, snapshot, &selectors));
    select_tool_output_snapshot_indexed(
        metadata,
        snapshot,
        selectors,
        token_ceiling,
        pointers.as_ref(),
    )
}

fn select_tool_output_snapshot_indexed(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
    pointers: Option<&ValidatedJsonPointers>,
) -> Result<ReadToolOutputResult, ReadToolOutputError> {
    let metadata = pointers.map_or(metadata, |pointers| &pointers.metadata);
    // Normalization decides merges against the same fragment ceiling selection
    // enforces, so it needs the bytes those merges would have to return.
    let selectors = normalize_tool_output_selectors(
        selectors,
        metadata,
        snapshot,
        token_ceiling.min(RECOVERY_FRAGMENT_TOKEN_CEILING),
        token_ceiling,
    );
    let mut response = ReadToolOutputResult {
        artifact_id: metadata.artifact_id.clone(),
        canonical_sha256: metadata.canonical_sha256.clone(),
        canonical_bytes: metadata.canonical_bytes,
        retained_bytes: metadata.retained_bytes,
        capture_complete: metadata.version != 0 && metadata.capture_complete,
        complete: false,
        unavailable_ranges: metadata.unavailable_ranges.clone(),
        results: Vec::with_capacity(selectors.len()),
    };
    let mut cost = SelectorResponseCost::new(metadata, &[]);
    let retry_ceiling = token_ceiling.saturating_add(RECOVERY_RETRY_AVOIDANCE_TOKEN_MARGIN);
    for (index, selector) in selectors.iter().enumerate() {
        // A search is a page, not an indivisible exact value. Share the remaining
        // response capacity so the first query cannot starve every later query.
        let selection_ceiling = if selector.is_search() {
            let occupied = cost.size(response.complete).tokens();
            // Search measures the entire response, including its existing envelope.
            occupied
                .saturating_add(token_ceiling.saturating_sub(occupied) / (selectors.len() - index))
                .min(token_ceiling)
        } else {
            token_ceiling
        };
        let mut selected =
            if let Some(message) = pointers.and_then(|pointers| pointers.error(selector)) {
                let mut result = ToolOutputSelectorResult::state(
                    selector.clone(),
                    ToolOutputSelectorStatus::Invalid,
                );
                result.message = Some(message.into());
                result.child_selectors = vec![ToolOutputSelector::Bytes {
                    start: 0,
                    end: metadata.retained_bytes,
                }];
                result
            } else {
                select_logical_artifact(
                    metadata,
                    snapshot,
                    selector.clone(),
                    selection_ceiling.min(RECOVERY_FRAGMENT_TOKEN_CEILING),
                    selection_ceiling,
                    &response.results,
                    Some(&cost),
                )
            };
        if selected.status == ToolOutputSelectorStatus::SelectorTooLarge
            && let Some(completed) =
                internally_drain_exact_subdivisions(metadata, snapshot, &selected)
        {
            let completed_cost = selector_serialized_cost(&completed);
            let complete = (response.results.is_empty() || response.complete)
                && completed.status == ToolOutputSelectorStatus::Ok
                && completed.complete;
            if cost.with_next(completed_cost, complete).tokens() <= retry_ceiling
                || SelectorResponseCost::new(metadata, &[])
                    .with_next(completed_cost, true)
                    .tokens()
                    <= retry_ceiling
            {
                selected = completed;
            }
        }
        response.complete = (response.results.is_empty() || response.complete)
            && selected.status == ToolOutputSelectorStatus::Ok
            && selected.complete;
        cost.push(selector_serialized_cost(&selected));
        response.results.push(selected);
        if cost.size(response.complete).tokens() <= retry_ceiling {
            continue;
        }
        let Some(selected) = response.results.last_mut() else {
            unreachable!("the selected result was just appended");
        };
        if selected.status == ToolOutputSelectorStatus::Ok {
            let previous = &*selected;
            let mut omitted = previous
                .canonical_range
                .map(|range| {
                    too_large_result(
                        previous.selector.clone(),
                        range,
                        previous.child_selectors.clone(),
                        metadata,
                        snapshot,
                        token_ceiling.min(RECOVERY_FRAGMENT_TOKEN_CEILING),
                    )
                })
                .unwrap_or_else(|| {
                    let mut omitted = ToolOutputSelectorResult::state(
                        previous.selector.clone(),
                        ToolOutputSelectorStatus::AggregateOmitted,
                    );
                    omitted.child_selectors = previous.child_selectors.clone();
                    omitted
                });
            omitted.status = ToolOutputSelectorStatus::AggregateOmitted;
            omitted.exact_bytes = previous.exact_bytes;
            omitted.canonical_range = previous.canonical_range;
            // This page was omitted, so its next-page cursor is not deliverable.
            omitted.continuation = Some(previous.selector.clone());
            omitted.message = Some(
                "exact value cannot fit this final transaction; retry only with the advertised continuation or deterministic child selectors"
                    .to_string(),
            );
            *selected = omitted;
            cost.replace(cost.parts.len() - 1, selector_serialized_cost(selected));
        }
        response.complete = false;
        // A later selector can exhaust the envelope after an earlier value has
        // fitted. Keep the batch addressable instead of discarding every result
        // and forcing a model/tool retry with fewer selectors. Compact from the
        // tail so retained search hydration never references an omitted prefix.
        if cost.size(false).tokens() > token_ceiling {
            for index in (0..response.results.len()).rev() {
                let previous = &response.results[index];
                if !matches!(
                    previous.status,
                    ToolOutputSelectorStatus::Ok
                        | ToolOutputSelectorStatus::SelectorTooLarge
                        | ToolOutputSelectorStatus::AggregateOmitted
                ) {
                    continue;
                }
                let mut omitted = ToolOutputSelectorResult::state(
                    previous.selector.clone(),
                    ToolOutputSelectorStatus::AggregateOmitted,
                );
                omitted.exact_bytes = previous.exact_bytes;
                omitted.canonical_range = previous.canonical_range;
                omitted.continuation = Some(previous.selector.clone());
                response.results[index] = omitted;
                cost.replace(index, selector_serialized_cost(&response.results[index]));
                if cost.size(false).tokens() <= token_ceiling {
                    break;
                }
            }
        }
        if cost.size(false).tokens() > token_ceiling {
            return Err(ReadToolOutputError::InvalidRange(
                "normalized selector manifest cannot fit a bounded typed-overflow response; use fewer selectors"
                    .to_string(),
            ));
        }
    }
    response.complete = response
        .results
        .iter()
        .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete);
    if !response_fits_recovery_retry_avoidance_ceiling(&response, token_ceiling) {
        return Err(ReadToolOutputError::InvalidRange(
            "selector response exceeds its final recovery ceiling".to_string(),
        ));
    }
    Ok(response)
}

fn producer_snapshot_metadata(
    canonical: &CanonicalToolResult,
    artifact_id: String,
) -> Result<LogicalArtifactMetadata, ReadToolOutputError> {
    Ok(LogicalArtifactMetadata {
        version: LOGICAL_ARTIFACT_METADATA_VERSION,
        artifact_id,
        canonical_kind: canonical.kind,
        canonical_sha256: canonical.sha256.clone(),
        retained_sha256: None,
        canonical_bytes: canonical.exact_bytes,
        retained_bytes: canonical.exact_bytes,
        capture_complete: canonical.complete,
        complete: canonical.unavailable_ranges.is_empty(),
        unavailable_ranges: canonical.unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
        // A newline-dense result keeps byte, pointer and section selection;
        // only its line and search selectors fail, explicitly.
        line_starts: canonical_line_starts(&canonical.bytes).unwrap_or_default(),
        segments: Vec::new(),
    })
}

/// Select exact excerpts from the canonical bytes the producer still owns.
/// The caller must have persisted this same identity before exposing its handle.
pub(crate) fn select_producer_snapshot(
    canonical: &CanonicalToolResult,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
) -> Result<ReadToolOutputResult, ReadToolOutputError> {
    select_tool_output_snapshot(
        &producer_snapshot_metadata(canonical, artifact_id.to_owned())?,
        &canonical.bytes,
        selectors,
        token_ceiling,
    )
}


/// Script consumers get exact data independently of the cell's display budget.
/// Bound the serialized payload, including escaping, rather than its token estimate.
pub(crate) fn select_file_snapshot_for_script(
    canonical: &CanonicalToolResult,
    selectors: Option<Vec<ToolOutputSelector>>,
) -> Result<(ReadToolOutputResult, Option<ToolOutputSelector>), ReadToolOutputError> {
    const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
    let metadata = producer_snapshot_metadata(canonical, uuid::Uuid::nil().to_string())?;
    let default_read = selectors.is_none();
    let selectors = selectors.unwrap_or_else(|| {
        let mut end = canonical.exact_bytes.min(MAX_SCRIPT_BYTES as u64) as usize;
        if let Err(error) = std::str::from_utf8(&canonical.bytes[..end])
            && error.error_len().is_none()
        {
            // A default text page must not split its last Unicode scalar.
            // Genuine binary data keeps the exact byte/base64 contract.
            end = error.valid_up_to();
        }
        vec![ToolOutputSelector::Bytes {
            start: 0,
            end: end as u64,
        }]
    });
    // Scripts get the same semantic-address validation as direct and recovered
    // selections: malformed, incomplete or ambiguous JSON never presents a
    // decoded value.
    let pointers = selectors
        .iter()
        .any(|selector| matches!(selector, ToolOutputSelector::JsonPointer { .. }))
        .then(|| ValidatedJsonPointers::for_selectors(&metadata, &canonical.bytes, &selectors));
    let metadata = pointers
        .as_ref()
        .map_or(metadata, |pointers| pointers.metadata.clone());
    let mut response = ReadToolOutputResult {
        artifact_id: metadata.artifact_id.clone(),
        canonical_sha256: canonical.sha256.clone(),
        canonical_bytes: canonical.exact_bytes,
        retained_bytes: canonical.exact_bytes,
        capture_complete: canonical.complete,
        complete: true,
        unavailable_ranges: canonical.unavailable_ranges.clone(),
        results: Vec::new(),
    };
    let mut continuation = None;
    // The envelope and previously selected fragments do not change while
    // admitting the next selector. Size each fragment once instead of
    // serializing the growing prefix (up to 64 times).
    let mut cost = SelectorResponseCost::new(&metadata, &[]);
    let envelope_bytes = serde_json::to_vec(&response)
        .map(|bytes| bytes.len())
        .unwrap_or(MAX_SCRIPT_BYTES);
    let selectors = normalize_tool_output_selectors_with_fit(selectors, &metadata, |selector| {
        let selected = exact_selector_result(&metadata, &canonical.bytes, selector.clone());
        selected.status == ToolOutputSelectorStatus::Ok
            && selected.complete
            && serde_json::to_vec(&selected).is_ok_and(|bytes| {
                envelope_bytes.saturating_add(bytes.len()) <= MAX_SCRIPT_BYTES - 128 * 1024
            })
    });
    let mut result_bytes = 0usize;
    for selector in selectors {
        let remaining = (MAX_SCRIPT_BYTES - 128 * 1024)
            .saturating_sub(envelope_bytes)
            .saturating_sub(result_bytes)
            .saturating_sub(usize::from(!response.results.is_empty()));
        let mut selected = if let Some(message) = pointers
            .as_ref()
            .and_then(|pointers| pointers.error(&selector))
        {
            let mut result = ToolOutputSelectorResult::state(
                selector.clone(),
                ToolOutputSelectorStatus::Invalid,
            );
            result.message = Some(message.into());
            result.child_selectors = vec![ToolOutputSelector::Bytes {
                start: 0,
                end: metadata.retained_bytes,
            }];
            result
        } else if selector.is_search() {
            search_logical_artifact(
                &metadata,
                &canonical.bytes,
                selector.clone(),
                SearchBudget::SelectorBytes(remaining),
                &response.results,
                Some(&cost),
            )
        } else {
            exact_selector_result(&metadata, &canonical.bytes, selector.clone())
        };
        loop {
            // Reserve enough room for all remaining selectors' overflow metadata.
            let serialized = serde_json::to_string(&selected)
                .map_err(|error| ReadToolOutputError::InvalidRange(error.to_string()))?;
            let selected_cost = SelectorPartCost {
                serialized: TokenCountEstimate::new(&serialized),
                successful: selected.status == ToolOutputSelectorStatus::Ok,
                complete: selected.complete,
            };
            let next_bytes = result_bytes
                .saturating_add(serialized.len())
                .saturating_add(usize::from(!response.results.is_empty()));
            let fits = envelope_bytes.saturating_add(next_bytes) <= MAX_SCRIPT_BYTES - 128 * 1024;
            if fits {
                result_bytes = next_bytes;
                cost.push(selected_cost);
                response.results.push(selected);
                break;
            }
            if default_read
                && let Some(range) = selected.canonical_range
                && range.end > 1
            {
                if let Ok(text) = std::str::from_utf8(&canonical.bytes[..range.end as usize]) {
                    // Prefix serialization is monotonic only on this UTF-8
                    // byte-selector branch, not on merged search hydration.
                    let mut low = 0;
                    let mut high = text.len();
                    let mut best = successful_byte_selector_result(CanonicalByteRange::new(0, 0), &[]);
                    while low < high {
                        let mut end = low + (high - low).div_ceil(2);
                        while end > low && !text.is_char_boundary(end) { end -= 1; }
                        if end == low {
                            end += text[low..].chars().next().map_or(0, char::len_utf8);
                        }
                        let candidate = successful_byte_selector_result(CanonicalByteRange::new(0, end as u64), &canonical.bytes[..end]);
                        let fits = serde_json::to_vec(&candidate).map_err(|error| ReadToolOutputError::InvalidRange(error.to_string()))?.len() <= remaining;
                        if fits { low = end; best = candidate; } else {
                            high = end.saturating_sub(1);
                            while high > low && !text.is_char_boundary(high) { high -= 1; }
                        }
                    }
                    selected = best;
                    // The common admission path below still checks the hard cap.
                    continue;
                }
                // Binary prefixes are exact base64 selections; requiring UTF-8
                // here can shrink a leading invalid byte to an empty page.
                let end = range.end as usize / 2;
                selected = successful_byte_selector_result(
                    CanonicalByteRange::new(0, end as u64),
                    &canonical.bytes[..end],
                );
                continue;
            }
            let mut omitted = selected
                .canonical_range
                .map(|range| {
                    too_large_result(
                        selector.clone(),
                        range,
                        Vec::new(),
                        &metadata,
                        &canonical.bytes,
                        RECOVERY_AGGREGATE_TOKEN_CEILING,
                    )
                })
                .unwrap_or_else(|| {
                    ToolOutputSelectorResult::state(
                        selector.clone(),
                        ToolOutputSelectorStatus::AggregateOmitted,
                    )
                });
            omitted.status = ToolOutputSelectorStatus::AggregateOmitted;
            omitted.exact_bytes = selected.exact_bytes;
            omitted.canonical_range = selected.canonical_range;
            // The bounded prefix alone is not a complete recovery recipe when
            // the range is too large for an automatic subdivision plan.
            if let (Some(range), Some(ToolOutputSelector::Bytes { end, .. })) =
                (selected.canonical_range, omitted.child_selectors.first())
                && *end < range.end
            {
                omitted.child_selectors.push(ToolOutputSelector::Bytes {
                    start: *end,
                    end: range.end,
                });
            }
            omitted.message = Some("Selection exceeds the 1 MiB script payload cap; recover the exact range through continuation or child_selectors, not the original selector.".into());
            let serialized = serde_json::to_string(&omitted)
                .map_err(|error| ReadToolOutputError::InvalidRange(error.to_string()))?;
            result_bytes = result_bytes
                .saturating_add(serialized.len())
                .saturating_add(usize::from(!response.results.is_empty()));
            cost.push(SelectorPartCost {
                serialized: TokenCountEstimate::new(&serialized),
                successful: omitted.status == ToolOutputSelectorStatus::Ok,
                complete: omitted.complete,
            });
            response.results.push(omitted);
            break;
        }
    }
    response.complete = response
        .results
        .iter()
        .all(|result| result.status == ToolOutputSelectorStatus::Ok && result.complete);
    if default_read
        && let Some(end) = response
            .results
            .first()
            .and_then(|result| result.canonical_range.map(|range| range.end))
        && end < canonical.exact_bytes
    {
        continuation = Some(ToolOutputSelector::Bytes {
            start: end,
            end: canonical.exact_bytes,
        });
    }
    if !serde_json::to_vec(&response).is_ok_and(|bytes| bytes.len() <= MAX_SCRIPT_BYTES) {
        return Err(ReadToolOutputError::InvalidRange(
            "selector response exceeds the 1 MiB script payload cap; use fewer selectors".into(),
        ));
    }
    Ok((response, continuation))
}

pub(crate) fn select_file_snapshot_with_ceiling(
    canonical: &CanonicalToolResult,
    selectors: Option<Vec<ToolOutputSelector>>,
    token_ceiling: usize,
) -> Result<(ReadToolOutputResult, Option<ToolOutputSelector>), ReadToolOutputError> {
    let token_ceiling = token_ceiling.min(RECOVERY_AGGREGATE_TOKEN_CEILING.saturating_sub(1_000));
    let metadata = producer_snapshot_metadata(canonical, uuid::Uuid::nil().to_string())?;
    if let Some(selectors) = selectors {
        return select_tool_output_snapshot(&metadata, &canonical.bytes, selectors, token_ceiling)
            .map(|result| (result, None));
    }
    // Leave room for the file-specific path, coverage, and continuation fields,
    // and the enclosing code-mode result, outside the shared recovery envelope.
    let page_bytes = largest_fitting_byte_prefix(
        &metadata.artifact_id,
        canonical.exact_bytes,
        CanonicalByteRange::new(0, canonical.exact_bytes),
        &canonical.bytes,
        token_ceiling,
    );
    let range = CanonicalByteRange::new(0, page_bytes);
    let continuation = (page_bytes < canonical.exact_bytes).then_some(ToolOutputSelector::Bytes {
        start: page_bytes,
        end: canonical.exact_bytes,
    });
    Ok((
        ReadToolOutputResult {
            artifact_id: metadata.artifact_id,
            canonical_sha256: canonical.sha256.clone(),
            canonical_bytes: canonical.exact_bytes,
            retained_bytes: canonical.exact_bytes,
            capture_complete: true,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![successful_byte_selector_result(
                range,
                &canonical.bytes[..page_bytes as usize],
            )],
        },
        continuation,
    ))
}

/// Advertise a bounded window of independently readable pages. The original
/// continuation still describes the entire remaining extent.
pub(crate) fn bounded_page_selectors(
    artifact_id: &str,
    bytes: &[u8],
    mut start: u64,
    end: u64,
    token_ceiling: usize,
) -> Vec<ToolOutputSelector> {
    let mut pages = Vec::new();
    let end = end.min(bytes.len() as u64);
    // A validated UTF-8 tail can be sliced at later page boundaries without
    // rescanning the overlapping suffix. Invalid/binary tails keep the exact
    // per-range validation path, including readable suffixes after bad bytes.
    let text = bytes.get(start as usize..end as usize)
        .and_then(|tail| std::str::from_utf8(tail).ok());
    let text_start = start;
    while start < end && pages.len() < 8 {
        let range_text = text.and_then(|text| text.get((start - text_start) as usize..))
            .or_else(|| bytes.get(start as usize..end as usize)
                .and_then(|tail| std::str::from_utf8(tail).ok()));
        let length = largest_fitting_byte_prefix_with_text(
            artifact_id,
            bytes.len() as u64,
            CanonicalByteRange::new(start, end),
            bytes,
            token_ceiling,
            range_text,
        );
        if length == 0 {
            break;
        }
        pages.push(ToolOutputSelector::Bytes {
            start,
            end: start + length,
        });
        start += length;
    }
    pages
}




fn open_regular_artifact(path: &Path) -> Result<(File, u64), ReadToolOutputError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let file = options.open(path).map_err(map_artifact_open_error)?;
    let metadata = file
        .metadata()
        .map_err(|err| ReadToolOutputError::Io(format!("failed to inspect artifact: {err}")))?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(ReadToolOutputError::Expired);
    }
    Ok((file, metadata.len()))
}

fn map_artifact_open_error(error: std::io::Error) -> ReadToolOutputError {
    if error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error().is_some_and(is_no_follow_error)
    {
        ReadToolOutputError::Expired
    } else {
        ReadToolOutputError::Io(format!("failed to open artifact: {error}"))
    }
}

#[cfg(windows)]
fn configure_no_follow_open(options: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
fn configure_no_follow_open(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow_open(_options: &mut std::fs::OpenOptions) {}

#[cfg(windows)]
fn is_no_follow_error(_raw_os_error: i32) -> bool {
    false
}

#[cfg(unix)]
fn is_no_follow_error(raw_os_error: i32) -> bool {
    raw_os_error == libc::ELOOP
}

#[cfg(not(any(unix, windows)))]
fn is_no_follow_error(_raw_os_error: i32) -> bool {
    false
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn active_tool_history_protection_path(artifact_path: &Path) -> PathBuf {
    artifact_path.with_extension(ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION)
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let result = sync_directory(parent);
    match result {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // FlushFileBuffers does not support ordinary read-only directory handles.
    // Previously this always-failing open/flush was masked by the caller. File
    // data is synced at the barrier; Windows directory durability is best effort.
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path).and_then(|directory| directory.sync_all())
}

async fn run_blocking_artifact_io<T, F>(operation: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(std::io::Error::other)?
}

fn artifact_is_protected_blocking(artifact_path: &Path) -> std::io::Result<bool> {
    retention_protection_marker_status(
        &active_tool_history_protection_path(artifact_path),
        ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
    )
}


fn note_completed_protection_reconciliation(root: &Path, installed_mode: RetentionModeKind) {
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        return;
    };
    if installed_mode != RetentionModeKind::Indexed
        && let RetentionRootMode::ScanOnly {
            operations_since_probe,
        } = &mut state.mode
    {
        *operations_since_probe = operations_since_probe.saturating_add(1);
    }
}

fn publish_known_record(
    token: &RetentionIndexToken,
    record: ArtifactRetentionRecord,
    mutation: LogicalRetentionMutation,
) {
    let Some(generation) = token.generation else {
        return;
    };
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&token.root) else {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    };
    debug_assert_eq!(token.generation, Some(generation));
    let disposition = retention_delta_disposition(token, state);
    if disposition == RetentionDeltaDisposition::RejectStale {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    }
    let _ = mutation;
    match disposition {
        RetentionDeltaDisposition::ApplyIndexed => {}
        RetentionDeltaDisposition::IgnoreScanOnly => return,
        RetentionDeltaDisposition::RejectCurrent => {
            transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
            return;
        }
        RetentionDeltaDisposition::RejectStale => unreachable!("handled above"),
    }
    let invariant_failed = match &mut state.mode {
        RetentionRootMode::Indexed(index) => {
            if index.insert(record) {
                index.note_logical_mutation();
                if index.is_near_limit() && !index.near_limit_reconciled {
                    index.near_limit_pending = true;
                }
                false
            } else {
                true
            }
        }
        RetentionRootMode::Dirty | RetentionRootMode::ScanOnly { .. } => false,
        RetentionRootMode::Reconciling { invalidated } => {
            *invalidated = true;
            false
        }
    };
    if invariant_failed {
        transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
    }
}

fn publish_known_remove(
    token: &RetentionIndexToken,
    path: &Path,
    mutation: LogicalRetentionMutation,
    eviction: bool,
) {
    let Some(generation) = token.generation else {
        return;
    };
    let path = normalized_retention_path(path);
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&token.root) else {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    };
    debug_assert_eq!(token.generation, Some(generation));
    let disposition = retention_delta_disposition(token, state);
    if disposition == RetentionDeltaDisposition::RejectStale {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    }
    let _ = mutation;
    match disposition {
        RetentionDeltaDisposition::ApplyIndexed => {}
        RetentionDeltaDisposition::IgnoreScanOnly => return,
        RetentionDeltaDisposition::RejectCurrent => {
            transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
            return;
        }
        RetentionDeltaDisposition::RejectStale => unreachable!("handled above"),
    }
    match &mut state.mode {
        RetentionRootMode::Indexed(index) => {
            index.remove(&path);
            if !eviction {
                index.note_logical_mutation();
            }
            if !index.is_near_limit() {
                index.near_limit_reconciled = false;
            }
        }
        RetentionRootMode::Dirty | RetentionRootMode::ScanOnly { .. } => {}
        RetentionRootMode::Reconciling { invalidated } => {
            *invalidated = true;
        }
    }
}

async fn publish_streaming_size_async(
    token: &RetentionIndexToken,
    path: &Path,
    bytes: u64,
    modified: SystemTime,
    completed: bool,
) {
    if token.generation.is_none() {
        return;
    }
    let owned_token = token.clone();
    let path = path.to_path_buf();
    // Keep normalization and publication in one owned operation so cancelling
    // the async writer cannot discard an admitted size update.
    if let Err(error) = run_blocking_artifact_io(move || {
        let path = normalized_retention_path(&path);
        publish_streaming_size(&owned_token, &path, bytes, modified, completed);
        Ok(())
    })
    .await
    {
        reject_stale_delta(token);
        tracing::warn!(%error, "streaming artifact size publication worker failed");
    }
}

fn publish_streaming_size(
    token: &RetentionIndexToken,
    path: &Path,
    bytes: u64,
    modified: SystemTime,
    completed: bool,
) {
    let Some(generation) = token.generation else {
        return;
    };
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&token.root) else {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    };
    debug_assert_eq!(token.generation, Some(generation));
    let disposition = retention_delta_disposition(token, state);
    if disposition == RetentionDeltaDisposition::RejectStale {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    }
    match disposition {
        RetentionDeltaDisposition::ApplyIndexed => {}
        RetentionDeltaDisposition::IgnoreScanOnly => return,
        RetentionDeltaDisposition::RejectCurrent => {
            transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
            return;
        }
        RetentionDeltaDisposition::RejectStale => unreachable!("handled above"),
    }
    let missing_record = match &mut state.mode {
        RetentionRootMode::Indexed(index) => {
            if !index.update_streaming_size(path, bytes, modified) {
                true
            } else {
                if completed {
                    index.note_logical_mutation();
                }
                if index.is_near_limit() && !index.near_limit_reconciled {
                    index.near_limit_pending = true;
                }
                false
            }
        }
        RetentionRootMode::Dirty | RetentionRootMode::ScanOnly { .. } => false,
        RetentionRootMode::Reconciling { invalidated } => {
            *invalidated = true;
            false
        }
    };
    if missing_record {
        transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
    }
}

fn publish_streaming_abandonment(token: &RetentionIndexToken) {
    let Some(generation) = token.generation else {
        return;
    };
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&token.root) else {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    };
    debug_assert_eq!(token.generation, Some(generation));
    let disposition = retention_delta_disposition(token, state);
    if disposition == RetentionDeltaDisposition::RejectStale {
        transition_current_root_to_dirty(&mut registry, &token.root);
        return;
    }
    match disposition {
        RetentionDeltaDisposition::ApplyIndexed => {}
        RetentionDeltaDisposition::IgnoreScanOnly => return,
        RetentionDeltaDisposition::RejectCurrent => {
            transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
            return;
        }
        RetentionDeltaDisposition::RejectStale => unreachable!("handled above"),
    }
    match &mut state.mode {
        RetentionRootMode::Indexed(index) => index.note_logical_mutation(),
        RetentionRootMode::Reconciling { invalidated } => *invalidated = true,
        RetentionRootMode::Dirty | RetentionRootMode::ScanOnly { .. } => {}
    }
}



/// Rebuilds the root's index from a full scan, and reports the artifacts
/// whose transaction markers the scan listed.
fn reconcile_retention_root_listing_transactions_blocking(
    root: &Path,
    transactions: &mut Vec<PathBuf>,
) -> RetentionModeKind {
    if indexing_disabled().load(Ordering::Acquire) {
        return RetentionModeKind::Disabled;
    }
    let root = normalized_tool_output_root(root);
    let (generation, capacity, was_scan_only) = {
        let mut registry = lock_retention_registry();
        if !registry.roots.contains_key(&root)
            && insert_dirty_root(&mut registry, root.clone()).is_none()
        {
            return RetentionModeKind::Disabled;
        }
        let Some(generation) = next_index_generation() else {
            return RetentionModeKind::Disabled;
        };
        registry.access_clock = registry.access_clock.saturating_add(1);
        let access = registry.access_clock;
        let Some(state) = registry.roots.get_mut(&root) else {
            return RetentionModeKind::Disabled;
        };
        let was_scan_only = matches!(state.mode, RetentionRootMode::ScanOnly { .. });
        state.generation = generation;
        state.mode = RetentionRootMode::Reconciling { invalidated: false };
        state.last_access = access;
        let capacity = {
            {
                MAX_RETENTION_INDEX_RECORDS
            }
        };
        (generation, capacity, was_scan_only)
    };
    let _ = was_scan_only;

    let scan = scan_retention_root_blocking(&root, capacity).map(
        |(candidate, directories, candidates, listed)| {
            *transactions = listed;
            (candidate, directories, candidates)
        },
    );
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&root) else {
        let _ = insert_dirty_root(&mut registry, root);
        return RetentionModeKind::Dirty;
    };
    if state.generation != generation {
        return root_mode_kind(&state.mode);
    }
    let invalidated = matches!(
        state.mode,
        RetentionRootMode::Reconciling { invalidated: true }
    );
    if invalidated {
        if let Some(generation) = next_index_generation() {
            state.generation = generation;
        }
        state.mode = RetentionRootMode::Dirty;
        return RetentionModeKind::Dirty;
    }
    match scan {
        Ok((candidate, directories, candidates)) => {
            let _ = (directories, candidates);
            match candidate {
                RetentionScanCandidate::Indexed(index) => {
                    state.mode = RetentionRootMode::Indexed(index);
                    RetentionModeKind::Indexed
                }
                RetentionScanCandidate::Oversized => {
                    state.mode = RetentionRootMode::ScanOnly {
                        operations_since_probe: 0,
                    };
                    RetentionModeKind::ScanOnly
                }
            }
        }
        Err(_) => {
            state.mode = RetentionRootMode::Dirty;
            RetentionModeKind::Dirty
        }
    }
}

fn prepare_retention_mode_blocking(root: &Path, force_reconciliation: bool) -> RetentionModeKind {
    if indexing_disabled().load(Ordering::Acquire) {
        return RetentionModeKind::Disabled;
    }
    let root = normalized_tool_output_root(root);
    let mode = {
        let mut registry = lock_retention_registry();
        if let Some(state) = registry.roots.get_mut(&root) {
            match &mut state.mode {
                RetentionRootMode::Indexed(index) => {
                    if force_reconciliation || index.near_limit_pending {
                        RetentionModeKind::Dirty
                    } else if index.logical_mutations_since_reconciliation
                        < RETENTION_RECONCILIATION_INTERVAL
                        || index.background_reconciliation_pending
                    {
                        RetentionModeKind::Indexed
                    } else if schedule_background_retention_reconciliation(&root) {
                        index.background_reconciliation_pending = true;
                        RetentionModeKind::Indexed
                    } else {
                        RetentionModeKind::Dirty
                    }
                }
                RetentionRootMode::Dirty | RetentionRootMode::Reconciling { .. } => {
                    RetentionModeKind::Dirty
                }
                RetentionRootMode::ScanOnly {
                    operations_since_probe,
                } => {
                    if force_reconciliation
                        || *operations_since_probe >= RETENTION_RECONCILIATION_INTERVAL - 1
                    {
                        RetentionModeKind::Dirty
                    } else {
                        *operations_since_probe = operations_since_probe.saturating_add(1);
                        RetentionModeKind::ScanOnly
                    }
                }
            }
        } else {
            RetentionModeKind::Dirty
        }
    };
    if mode != RetentionModeKind::Dirty {
        return mode;
    }
    // A permit holder rebuilds without the shared lock, then recovers the
    // interrupted transactions the scan listed once the lock is back.
    let mut transactions = Vec::new();
    let reconciled = without_shared_retention_lock(&root, || {
        reconcile_retention_root_listing_transactions_blocking(&root, &mut transactions)
    });
    if !transactions.is_empty() && retention_shared_lock_held(&root) {
        recover_listed_logical_artifact_transactions_blocking(&root, &transactions);
    }
    if reconciled != RetentionModeKind::Indexed {
        if reconciled == RetentionModeKind::ScanOnly {
        }
        return reconciled;
    }
    RetentionModeKind::Indexed
}

/// Periodic reconciliation only corrects drift in an index that deltas keep
/// current, so the triggering commit keeps that index and the rescan runs
/// afterwards. Returns false without a runtime.
fn schedule_background_retention_reconciliation(root: &Path) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return false;
    };
    runtime.spawn(refresh_indexed_retention_root(root.to_path_buf()));
    true
}

fn retention_index_capacity(state: &RetentionRootState) -> usize {
    {
        let _ = state;
        MAX_RETENTION_INDEX_RECORDS
    }
}

/// Rescans an indexed root one thread directory at a time while the index
/// stays in use. A whole-root rebuild held this process's admission for the
/// full scan, so the process's own artifact writes waited seconds behind it.
/// Each directory is now listed and applied under that admission alone, so a
/// write waits for at most one directory listing, and another process's write
/// not at all. Deltas keep applying between directories, and generation and
/// mode stay unchanged, so tokens captured before the rescan remain valid.
async fn refresh_indexed_retention_root(root: PathBuf) {
    let (root, semaphore) = match retention_sweep_admission(&root).await {
        Ok(admission) => admission,
        Err(error) => {
            record_retention_sweep_permit_failure(&root, &error);
            return;
        }
    };
    let Ok(admission) = Arc::clone(&semaphore).acquire_owned().await else {
        return;
    };
    let begin_root = root.clone();
    let Ok(Some((generation, capacity, directories))) = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        begin_retention_refresh_blocking(&begin_root)
    })
    .await
    else {
        return;
    };
    let mut candidates = 0_u64;
    let mut transactions = Vec::new();
    let directory_count = directories.len() as u64;
    for directory in directories {
        let Ok(admission) = Arc::clone(&semaphore).acquire_owned().await else {
            return;
        };
        let step_root = root.clone();
        let step = tokio::task::spawn_blocking(move || {
            let _admission = admission;
            refresh_retention_directory_step_blocking(&step_root, generation, &directory, capacity)
        })
        .await;
        let Ok(Some((listed_candidates, listed_transactions))) = step else {
            return;
        };
        candidates = candidates.saturating_add(listed_candidates);
        let room = capacity.saturating_sub(transactions.len());
        transactions.extend(listed_transactions.into_iter().take(room));
    }
    let finish_root = root.clone();
    let _ = tokio::task::spawn_blocking(move || {
        finish_retention_refresh_blocking(&finish_root, generation, directory_count, candidates);
    })
    .await;
    if transactions.is_empty() {
        return;
    }
    let Some(permit) = retention_sweep_permit(&root).await else {
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        recover_listed_logical_artifact_transactions_blocking(&root, &transactions);
    })
    .await;
}

/// Starts a due refresh: observes the shared generation it starts from and
/// lists the root. Returns the generation, capacity, and directories to list,
/// including indexed directories that have since been removed.
fn begin_retention_refresh_blocking(root: &Path) -> Option<(u64, usize, Vec<PathBuf>)> {
    let (generation, capacity) = {
        let registry = lock_retention_registry();
        let state = registry.roots.get(root)?;
        // A rebuild that replaced the index since scheduling also cleared this.
        let RetentionRootMode::Indexed(index) = &state.mode else {
            return None;
        };
        if !index.background_reconciliation_pending {
            return None;
        }
        (state.generation, retention_index_capacity(state))
    };
    if let Err(error) = observe_retention_interprocess_generation(root) {
        // The stale mark stays set, so a later pass rescans.
        tracing::debug!(%error, "artifact retention generation is unavailable");
    }
    let listed = retention_thread_directories_blocking(root);
    let mut registry = lock_retention_registry();
    let state = registry.roots.get_mut(root)?;
    if state.generation != generation {
        return None;
    }
    let Ok(listed) = listed else {
        // Like a failed rebuild, an unreadable root leaves no usable index.
        transition_current_root_to_dirty(&mut registry, root);
        return None;
    };
    let RetentionRootMode::Indexed(index) = &state.mode else {
        return None;
    };
    let mut directories = listed.into_iter().collect::<BTreeSet<_>>();
    directories.extend(index.threads.keys().cloned());
    Some((generation, capacity, directories.into_iter().collect()))
}

/// Lists one directory and applies it to the live index, unless a rebuild has
/// replaced that index since the refresh began. Returns the listed log count
/// and transactions, or `None` when the refresh must stop.
fn refresh_retention_directory_step_blocking(
    root: &Path,
    generation: u64,
    directory: &Path,
    capacity: usize,
) -> Option<(u64, Vec<PathBuf>)> {
    let listing = list_thread_retention_directory_blocking(directory, Some(capacity));
    let mut registry = lock_retention_registry();
    let state = registry.roots.get_mut(root)?;
    if state.generation != generation {
        return None;
    }
    let RetentionRootMode::Indexed(index) = &mut state.mode else {
        return None;
    };
    let listing = match listing {
        Ok(Some(listing)) if !listing.over_budget => Some(listing),
        Ok(None) => Some(ThreadRetentionListing {
            records: Vec::new(),
            transactions: Vec::new(),
            candidates: 0,
            over_budget: false,
        }),
        Ok(Some(_)) | Err(_) => None,
    };
    if let Some(listing) = listing
        && apply_thread_retention_listing(index, directory, listing.records)
        && index.records.len() <= capacity
    {
        return Some((listing.candidates, listing.transactions));
    }
    // Like a failed or oversized rebuild, this leaves no usable index.
    transition_current_root_to_dirty(&mut registry, root);
    None
}

fn finish_retention_refresh_blocking(
    root: &Path,
    generation: u64,
    #[allow(unused_variables)] directories: u64,
    #[allow(unused_variables)] candidates: u64,
) {
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        return;
    };
    if state.generation != generation {
        return;
    }
    let RetentionRootMode::Indexed(index) = &mut state.mode else {
        return;
    };
    index.logical_mutations_since_reconciliation = 0;
    index.near_limit_reconciled = index.is_near_limit();
    index.near_limit_pending = false;
    index.background_reconciliation_pending = false;
}

/// Replaces one thread directory's records with its listing. A record that a
/// streaming write published after the listing was taken keeps that state.
fn apply_thread_retention_listing(
    index: &mut RetentionIndex,
    directory: &Path,
    records: Vec<ArtifactRetentionRecord>,
) -> bool {
    let listed = records
        .into_iter()
        .map(|record| (record.path.clone(), record))
        .collect::<BTreeMap<_, _>>();
    // Reconcile protected records too, without scanning unrelated thread directories.
    let vanished = index
        .records
        .range(directory.to_path_buf()..)
        .take_while(|(path, _)| path.starts_with(directory))
        .filter(|(path, record)| {
            record.thread_directory.as_path() == directory && !listed.contains_key(*path)
        })
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for path in vanished {
        index.remove(&path);
    }
    for (path, record) in listed {
        if index
            .records
            .get(&path)
            .is_some_and(|indexed| indexed.modified > record.modified)
        {
            continue;
        }
        if !index.insert(record) {
            return false;
        }
    }
    true
}

/// Corrects one thread directory's records after its protection markers
/// changed. Only an indexed root has records to correct.
fn refresh_retention_thread_directory_blocking(root: &Path, directory: &Path) -> RetentionModeKind {
    let (generation, capacity) = {
        let registry = lock_retention_registry();
        let Some(state) = registry.roots.get(root) else {
            return RetentionModeKind::Dirty;
        };
        if !matches!(state.mode, RetentionRootMode::Indexed(_)) {
            return root_mode_kind(&state.mode);
        }
        (state.generation, retention_index_capacity(state))
    };
    let listing = list_thread_retention_directory_blocking(directory, Some(capacity));
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        return RetentionModeKind::Dirty;
    };
    let mode = root_mode_kind(&state.mode);
    if state.generation != generation {
        return mode;
    }
    let RetentionRootMode::Indexed(index) = &mut state.mode else {
        return mode;
    };
    let records = match listing {
        Ok(Some(listing)) if !listing.over_budget => Some(listing.records),
        Ok(None) => Some(Vec::new()),
        Ok(Some(_)) | Err(_) => None,
    };
    if let Some(records) = records
        && apply_thread_retention_listing(index, directory, records)
        && index.records.len() <= capacity
    {
        if !index.is_near_limit() {
            index.near_limit_reconciled = false;
        } else if !index.near_limit_reconciled {
            index.near_limit_pending = true;
        }
        return RetentionModeKind::Indexed;
    }
    transition_current_root_to_dirty(&mut registry, root);
    RetentionModeKind::Dirty
}

/// Rebuilds the index under this process's admission only. Holding the shared
/// lock for the scan queued every other process's artifact commit behind it:
/// on a root of ~100K files, for seconds at each process start and rescan.
/// The scan instead starts from an observed shared generation; a commit by
/// another process during or after it advances that generation and marks the
/// index stale, which leaves it incomplete rather than wrong, as after any
/// external write. A listed transaction marker may belong to a commit still
/// holding the lock, so it is recovered only afterwards, under that lock.
fn reconcile_external_retention_changes_blocking(
    root: &Path,
    process_permit: OwnedSemaphorePermit,
    only_if_unindexed: bool,
) {
    if only_if_unindexed && lock_retention_registry().roots.contains_key(root) {
        return;
    }
    if let Err(error) = observe_retention_interprocess_generation(root) {
        // The stale mark stays set, so a later pass rescans.
        tracing::debug!(%error, "artifact retention generation is unavailable");
    }
    let mut transactions = Vec::new();
    reconcile_retention_root_listing_transactions_blocking(root, &mut transactions);
    if transactions.is_empty() {
        return;
    }
    let _permit = match retention_sweep_permit_blocking(root, process_permit) {
        Ok(permit) => permit,
        Err(error) => {
            record_retention_sweep_permit_failure(root, &error);
            return;
        }
    };
    recover_listed_logical_artifact_transactions_blocking(root, &transactions);
}

/// Recovers transactions that a scan without the shared lock listed. The caller
/// now holds that lock, so a marker still present belongs to no running commit.
/// The scan may have indexed such a family mid-commit or before recovery
/// changed it, so each family is observed again.
fn recover_listed_logical_artifact_transactions_blocking(root: &Path, artifacts: &[PathBuf]) {
    for path in artifacts {
        let token = capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));
        if reconcile_logical_artifact_transaction(path).is_err() {
            let mut registry = lock_retention_registry();
            transition_current_root_to_dirty(&mut registry, root);
            return;
        }
        match artifact_retention_record_blocking(path) {
            Ok(Some(record)) => {
                publish_known_record(&token, record, LogicalRetentionMutation::AppendReplace);
            }
            Ok(None) => {
                publish_known_remove(&token, path, LogicalRetentionMutation::Cleanup, false);
            }
            Err(_) => reject_stale_delta(&token),
        }
    }
}

/// Another process's writes leave this process's index incomplete rather than
/// wrong: its own deltas still apply. Like periodic drift, the rescan that
/// picks those writes up runs after the triggering commit. Returns false when
/// the root has no usable index or is near a limit and must reconcile inline.
///
/// Every permit acquisition, in any process, marks the other processes'
/// indexes stale. Rescanning after each such change kept a full-root scan
/// running whenever several processes were active. These rescans therefore run
/// at most once per interval; until then the stale mark stays set and the index
/// only undercounts other processes' evictable bytes.
fn defer_external_retention_reconciliation(root: &Path) -> bool {
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        return false;
    };
    let recently_rescanned = state
        .last_external_reconciliation
        .is_some_and(|scheduled| scheduled.elapsed() < EXTERNAL_RETENTION_RECONCILIATION_INTERVAL);
    let RetentionRootMode::Indexed(index) = &mut state.mode else {
        return false;
    };
    if index.near_limit_pending {
        return false;
    }
    if !index.background_reconciliation_pending && !recently_rescanned {
        if !schedule_background_retention_reconciliation(root) {
            return false;
        }
        index.background_reconciliation_pending = true;
        state.last_external_reconciliation = Some(std::time::Instant::now());
    }
    true
}


fn invalidate_root_after_ambiguous_failure(root: &Path) {
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        let _ = insert_dirty_root(&mut registry, root.to_path_buf());
        return;
    };
    match &mut state.mode {
        RetentionRootMode::Dirty => {
            if let Some(generation) = next_index_generation() {
                state.generation = generation;
            }
        }
        RetentionRootMode::Reconciling { invalidated } => *invalidated = true,
        RetentionRootMode::ScanOnly { .. } => {}
        RetentionRootMode::Indexed(_) => {
            if let Some(generation) = next_index_generation() {
                state.generation = generation;
                state.mode = RetentionRootMode::Dirty;
            }
        }
    }
}

fn enforce_indexed_thread_retention_blocking(
    root: &Path,
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> bool {
    let directory = normalized_retention_path(directory);
    let keep_path = normalized_retention_path(keep_path);
    let mut skipped = BTreeSet::new();
    loop {
        let candidate = {
            let registry = lock_retention_registry();
            let Some(state) = registry.roots.get(root) else {
                return false;
            };
            let RetentionRootMode::Indexed(index) = &state.mode else {
                return false;
            };
            let (thread_bytes, thread_unprotected) = index.thread_cache_totals(&directory);
            if thread_unprotected.saturating_add(reserved_artifacts)
                <= max_retained_artifacts_per_thread()
                && thread_bytes.saturating_add(reserved_bytes)
                    <= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
            {
                return true;
            }
            index.threads.get(&directory).and_then(|thread| {
                thread.unprotected_paths.iter().find_map(|path| {
                    let record = index.records.get(path)?;
                    (!record.protected && path != &keep_path && !skipped.contains(path)).then(
                        || {
                            (
                                path.clone(),
                                RetentionIndexToken {
                                    root: root.to_path_buf(),
                                    generation: Some(state.generation),
                                    starting_mode: RetentionModeKind::Indexed,
                                },
                            )
                        },
                    )
                })
            })
        };
        let Some((path, token)) = candidate else {
            return true;
        };
        match remove_inactive_output_path_blocking(path.clone()) {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                publish_known_remove(&token, &path, LogicalRetentionMutation::Delete, true);
            }
            InactiveRemovalOutcome::Active => {
                skipped.insert(path);
            }
            InactiveRemovalOutcome::Ambiguous(error) => {
                tracing::debug!(%error, "artifact removal remained ambiguous");
                invalidate_root_after_ambiguous_failure(root);
                return false;
            }
        }
    }
}

fn enforce_indexed_global_retention_blocking(
    root: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> bool {
    let keep_path = normalized_retention_path(keep_path);
    let mut skipped = BTreeSet::new();
    loop {
        let candidate = {
            let registry = lock_retention_registry();
            let Some(state) = registry.roots.get(root) else {
                return false;
            };
            let RetentionRootMode::Indexed(index) = &state.mode else {
                return false;
            };
            if index.unprotected.saturating_add(reserved_artifacts)
                <= max_retained_artifacts_total()
                && index.unprotected_bytes.saturating_add(reserved_bytes)
                    <= MAX_RETAINED_ARTIFACT_BYTES_TOTAL
            {
                return true;
            }
            index.global_order.iter().find_map(|(_, path)| {
                let record = index.records.get(path)?;
                (!record.protected && path != &keep_path && !skipped.contains(path)).then(|| {
                    (
                        path.clone(),
                        RetentionIndexToken {
                            root: root.to_path_buf(),
                            generation: Some(state.generation),
                            starting_mode: RetentionModeKind::Indexed,
                        },
                    )
                })
            })
        };
        let Some((path, token)) = candidate else {
            return true;
        };
        match remove_inactive_output_path_blocking(path.clone()) {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                publish_known_remove(&token, &path, LogicalRetentionMutation::Delete, true);
            }
            InactiveRemovalOutcome::Active => {
                skipped.insert(path);
            }
            InactiveRemovalOutcome::Ambiguous(error) => {
                tracing::debug!(%error, "artifact removal remained ambiguous");
                invalidate_root_after_ambiguous_failure(root);
                return false;
            }
        }
    }
}

async fn enforce_retention(directory: &Path, keep_path: &Path) {
    let token = match capture_retention_token_async(directory).await {
        Ok(token) => token,
        Err(error) => {
            tracing::warn!(%error, "artifact retention preparation failed");
            return;
        }
    };
    let Some(permit) = retention_sweep_permit_for_directory(directory).await else {
        reject_stale_delta(&token);
        return;
    };
    let directory = directory.to_path_buf();
    let keep_path = keep_path.to_path_buf();
    let failure_token = token.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        publish_observed_path_blocking(&token, &keep_path);
        enforce_retention_locked_blocking(&directory, &keep_path, 0, 0);
    })
    .await
    {
        reject_stale_delta(&failure_token);
        tracing::warn!(%error, "artifact retention worker failed");
    }
}

async fn enforce_retention_after_observation(
    directory: &Path,
    path: &Path,
    token: &RetentionIndexToken,
) {
    let Some(permit) = retention_sweep_permit(&token.root).await else {
        reject_stale_delta(token);
        return;
    };
    let directory = directory.to_path_buf();
    let path = path.to_path_buf();
    let token = token.clone();
    let failure_token = token.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        publish_observed_path_blocking(&token, &path);
        enforce_retention_locked_blocking(&directory, &path, 0, 0);
    })
    .await
    {
        reject_stale_delta(&failure_token);
        tracing::warn!(%error, "artifact retention worker failed");
    }
}

async fn enforce_retention_after_upsert(
    directory: &Path,
    path: &Path,
    token: &RetentionIndexToken,
    mutation: LogicalRetentionMutation,
) {
    let Some(permit) = retention_sweep_permit(&token.root).await else {
        reject_stale_delta(token);
        return;
    };
    let directory = directory.to_path_buf();
    let path = path.to_path_buf();
    let token = token.clone();
    let failure_token = token.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match artifact_retention_record_blocking(&path) {
            Ok(Some(record)) => publish_known_record(&token, record, mutation),
            Ok(None) => publish_known_remove(&token, &path, mutation, false),
            Err(_) => reject_stale_delta(&token),
        }
        enforce_retention_locked_blocking(&directory, &path, 0, 0);
    })
    .await
    {
        reject_stale_delta(&failure_token);
        tracing::warn!(%error, "artifact retention worker failed");
    }
}

/// A thread directory stays addressable only while its thread can be resumed:
/// `read_tool_output` resolves the caller's own directory, and only a rollout
/// can bring that caller back. Without one, the directory's protection markers
/// pin it forever and grow the shared root past its index capacity, so every
/// later artifact write pays a full-root scan. The grace period keeps live
/// threads whose rollout is not materialized, in any process, out of reach.
const UNRESUMABLE_THREAD_ARTIFACT_GRACE: std::time::Duration =
    std::time::Duration::from_secs(7 * 24 * 60 * 60);
const UNRESUMABLE_THREAD_ARTIFACT_SWEEP_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);
const UNRESUMABLE_THREAD_ARTIFACT_BATCH: usize = 64;
const UNRESUMABLE_THREAD_ARTIFACT_SCAN_ENTRIES: usize = 4096;
const UNRESUMABLE_THREAD_ARTIFACT_MOVE_BUDGET: std::time::Duration =
    std::time::Duration::from_millis(250);

/// Measurement follows the actual move owner into blocking removal, including
/// cancellation. Release the existing lock before publishing its observed window.
struct ReclaimMoveOwner {
    file: Option<File>,
    observed: Option<(std::time::Instant, u64, usize)>,
}

impl ReclaimMoveOwner {
    fn new(file: File, candidates: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let observed = tracing::enabled!(target: "codex_storage_runtime", tracing::Level::TRACE)
            .then(|| {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_lock_acquired",
                    batch_id = id, candidates);
                (std::time::Instant::now(), id, candidates)
            });
        Self { file: Some(file), observed }
    }
}

impl Drop for ReclaimMoveOwner {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some((started, id, candidates)) = self.observed {
            tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_lock_released",
                batch_id = id, candidates, elapsed_us = started.elapsed().as_micros() as u64);
        }
    }
}

/// Starts a background reclaim at most once per sweep interval per home.
pub(crate) fn spawn_unresumable_thread_artifact_reclaim(
    codex_home: PathBuf,
    active_thread_id: String,
) {
    static LAST_SWEEPS: StdMutex<BTreeMap<PathBuf, std::time::Instant>> =
        StdMutex::new(BTreeMap::new());
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let mut last_sweeps = LAST_SWEEPS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last_sweeps
            .get(&codex_home)
            .is_some_and(|started| started.elapsed() < UNRESUMABLE_THREAD_ARTIFACT_SWEEP_INTERVAL)
        {
            return;
        }
        last_sweeps.insert(codex_home.clone(), std::time::Instant::now());
    }
    runtime.spawn(async move {
        let observed = tracing::enabled!(target: "codex_storage_runtime", tracing::Level::TRACE)
            .then(std::time::Instant::now);
        if observed.is_some() {
            tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_started");
        }
        let reclaimed = reclaim_unresumable_thread_artifacts(
            &codex_home,
            &active_thread_id,
            SystemTime::now(),
        )
        .await;
        if let Some(started) = observed {
            tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_finished",
                elapsed_us = started.elapsed().as_micros() as u64,
                removed = reclaimed.as_ref().copied().unwrap_or(0), success = reclaimed.is_ok());
        }
        match reclaimed {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "reclaimed artifacts of unresumable threads"),
            Err(error) => tracing::warn!(%error, "unresumable thread artifact reclaim failed"),
        }
    });
}

async fn reclaim_unresumable_thread_artifacts(
    codex_home: &Path,
    active_thread_id: &str,
    now: SystemTime,
) -> std::io::Result<usize> {
    let mut removed = 0;
    for directory in ["tool-output", "command-output"] {
        removed += reclaim_unresumable_thread_artifacts_under_root(
            codex_home, codex_home.join(directory), active_thread_id, now,
        ).await?;
    }
    Ok(removed)
}

async fn reclaim_unresumable_thread_artifacts_under_root(
    codex_home: &Path,
    root: PathBuf,
    active_thread_id: &str,
    now: SystemTime,
) -> std::io::Result<usize> {
    // Candidate discovery needs no movement exclusion. Actual absence is
    // established by one fresh inventory after acquiring that exclusion.
    let scan_root = root.clone();
    let active_thread_id = active_thread_id.to_string();
    let idle = run_blocking_artifact_io(move || {
        idle_thread_artifact_directories(&scan_root, &active_thread_id, now)
    })
    .await?;
    tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_discovery",
        eligible_directories = idle.len());
    let mut removed = 0;
    // Preserve complete discovery outside movement ownership. Process every
    // candidate in bounded batches instead of starving entries behind a kept
    // prefix on every daily sweep.
    for batch in idle.chunks(UNRESUMABLE_THREAD_ARTIFACT_BATCH) {
        removed += reclaim_unresumable_thread_artifact_batch(
            codex_home, root.clone(), now, batch.to_vec(),
        ).await?;
        tokio::task::yield_now().await;
    }
    Ok(removed)
}

async fn reclaim_unresumable_thread_artifact_batch(
    codex_home: &Path,
    root: PathBuf,
    now: SystemTime,
    idle: Vec<String>,
) -> std::io::Result<usize> {
    if idle.is_empty() {
        return Ok(0);
    }
    let Some(move_owner) = codex_rollout::try_lock_rollout_moves(codex_home).await? else {
        return Ok(0);
    };
    let move_owner = ReclaimMoveOwner::new(move_owner, idle.len());
    let deadline = tokio::time::Instant::now() + UNRESUMABLE_THREAD_ARTIFACT_MOVE_BUDGET;
    if codex_rollout::has_recoverable_delete_staging(codex_home).await? {
        return Ok(0);
    }
    let ids = idle.iter()
        .filter_map(|id| codex_protocol::ThreadId::from_string(id).ok())
        .collect::<Vec<_>>();
    let Ok(mut inventory) = tokio::time::timeout_at(
        deadline,
        codex_rollout::ThreadRolloutInventory::scan(codex_home, &ids),
    ).await else {
        tracing::trace!(target: "codex_storage_runtime", stage = "cleanup_inventory_timeout");
        return Ok(0);
    };
    // An incomplete traversal cannot prove absence for ANY candidate, even
    // after a previous take_paths call consumed its enumeration error.
    if !inventory.is_complete() {
        return Ok(0);
    }
    let mut unresumable = Vec::new();
    for thread_id in idle {
        if tokio::time::Instant::now() >= deadline {
            return Ok(0);
        }
        // Explicit-path resume has durable ownership outside the home-local
        // discovery roots. Its known provenance is not proof of unresumability.
        if !matches!(
            tokio::fs::try_exists(
                codex_home
                    .join("tool-output-residency")
                    .join(format!("{thread_id}.external-rollout"))
            )
            .await,
            Ok(false)
        ) {
            continue;
        }
        let home = codex_home.to_path_buf();
        let id = thread_id.clone();
        let residency =
            match run_blocking_artifact_io(move || artifact_residency_file(&home, &id, true)).await
            {
                Ok(owner) => owner,
                Err(_) => continue, // live or uncertain liveness: never wait under movement ownership
            };
        // Header/identity failures remain conservative, just like uncertain
        // liveness. The bulk inventory retains legacy filename suffixes.
        let id = codex_protocol::ThreadId::from_string(&thread_id)
            .map_err(std::io::Error::other)?;
        if matches!(tokio::time::timeout_at(deadline, inventory.take_paths(id)).await,
            Ok(Ok(paths)) if paths.is_empty()) {
            unresumable.push((thread_id, residency));
        }
    }
    if unresumable.is_empty() {
        return Ok(0);
    }
    let Ok(Some(permit)) = tokio::time::timeout_at(deadline, retention_sweep_permit(&root)).await else {
        return Ok(0);
    };
    run_blocking_artifact_io(move || {
        let _permit = permit;
        // The blocking owner retains the move exclusion if its caller is dropped.
        let _move_owner = move_owner;
        let mut removed = 0;
        for (thread_id, _residency) in unresumable {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            // First publication may have raced the initial age scan before we
            // acquired residency. Recheck under ownership through removal.
            let aged = std::fs::symlink_metadata(root.join(&thread_id))
                .ok()
                .is_some_and(|metadata| {
                    metadata.is_dir()
                        && !metadata_is_reparse_point(&metadata)
                        && metadata.modified().ok().is_some_and(|mtime| {
                            now.duration_since(mtime)
                                .is_ok_and(|age| age >= UNRESUMABLE_THREAD_ARTIFACT_GRACE)
                        })
                });
            if !aged {
                continue;
            }
            // Appending an existing file does not refresh directory mtime.
            // Ownership proves quiescence, not that its newest output is old.
            let contents_aged =
                std::fs::read_dir(root.join(&thread_id)).is_ok_and(|entries| {
                    entries.enumerate().all(|(index, entry)| {
                        index < UNRESUMABLE_THREAD_ARTIFACT_SCAN_ENTRIES
                            && tokio::time::Instant::now() < deadline
                            && entry.is_ok_and(|entry| {
                            std::fs::symlink_metadata(entry.path()).is_ok_and(|metadata| {
                                metadata.is_file()
                                    && !metadata_is_reparse_point(&metadata)
                                    && metadata.modified().is_ok_and(|mtime| {
                                        now.duration_since(mtime).is_ok_and(|age| {
                                            age >= UNRESUMABLE_THREAD_ARTIFACT_GRACE
                                        })
                                    })
                            })
                        })
                    })
                });
            if !contents_aged {
                continue;
            }
            match std::fs::remove_dir_all(root.join(&thread_id)) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(
                    %error,
                    thread_id = %thread_id,
                    "failed to reclaim artifacts of an unresumable thread"
                ),
            }
        }
        if root.file_name().is_some_and(|name| name == "tool-output") {
            // Plain command logs do not participate in this snapshot index.
            let root = normalized_tool_output_root(&root);
            let mut registry = lock_retention_registry();
            transition_current_root_to_dirty(&mut registry, &root);
        }
        Ok(removed)
    })
    .await
}

fn idle_thread_artifact_directories(
    root: &Path,
    active_thread_id: &str,
    now: SystemTime,
) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut idle = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Ok(thread_id) = entry.file_name().into_string() else {
            continue;
        };
        // Directories without valid thread IDs are not thread-owned.
        if thread_id == active_thread_id
            || codex_protocol::ThreadId::from_string(&thread_id).is_err()
        {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
            continue;
        }
        if now
            .duration_since(metadata.modified()?)
            .is_ok_and(|idle_for| idle_for >= UNRESUMABLE_THREAD_ARTIFACT_GRACE)
        {
            idle.push(thread_id);
        }
    }
    Ok(idle)
}

fn publish_observed_path_blocking(token: &RetentionIndexToken, path: &Path) {
    match artifact_retention_record_blocking(path) {
        Ok(Some(record)) => {
            let Some(generation) = token.generation else {
                return;
            };
            let mut registry = lock_retention_registry();
            let Some(state) = registry.roots.get_mut(&token.root) else {
                transition_current_root_to_dirty(&mut registry, &token.root);
                return;
            };
            debug_assert_eq!(token.generation, Some(generation));
            match retention_delta_disposition(token, state) {
                RetentionDeltaDisposition::ApplyIndexed => {}
                RetentionDeltaDisposition::IgnoreScanOnly => return,
                RetentionDeltaDisposition::RejectCurrent => {
                    transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
                    return;
                }
                RetentionDeltaDisposition::RejectStale => {
                    transition_current_root_to_dirty(&mut registry, &token.root);
                    return;
                }
            }
            let invariant_failed = if let RetentionRootMode::Indexed(index) = &mut state.mode {
                !index.insert(record)
            } else {
                false
            };
            if invariant_failed {
                transition_current_root_to_dirty_for_conflict(&mut registry, &token.root);
            }
        }
        Ok(None) => {}
        Err(_) => reject_stale_delta(token),
    }
}

fn enforce_retention_locked_blocking(
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    let root = tool_output_root_for_directory(directory);
    let shared_lock_held = retention_shared_lock_held(&root);
    if reconcile_logical_artifact_transactions(directory).is_err() {
        let mut registry = lock_retention_registry();
        transition_current_root_to_dirty(&mut registry, &root);
        return;
    }
    let mut external_generation_changed = take_retention_interprocess_index_stale(&root);
    if external_generation_changed && defer_external_retention_reconciliation(&root) {
        // The scheduled pass consumes the flag.
        mark_retention_interprocess_index_stale(&root);
        external_generation_changed = false;
    }
    // A forced rebuild also recovers other threads' interrupted transactions.
    let mode = prepare_retention_mode_blocking(&root, external_generation_changed);
    if shared_lock_held && !retention_shared_lock_held(&root) {
        return;
    }
    match mode {
        RetentionModeKind::Indexed => {
            if !enforce_indexed_thread_retention_blocking(
                &root,
                directory,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            ) {
                return;
            }
            let _ = enforce_indexed_global_retention_blocking(
                &root,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            );
        }
        RetentionModeKind::ScanOnly | RetentionModeKind::Disabled => {
            run_scan_only_retention_blocking(
                &root,
                directory,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            );
        }
        RetentionModeKind::Dirty | RetentionModeKind::Reconciling => {}
    }
}

fn run_scan_only_retention_blocking(
    root: &Path,
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    let thread_scan = enforce_retention_scan_locked_blocking(
        directory,
        keep_path,
        reserved_bytes,
        reserved_artifacts,
    );
    #[allow(unused_variables)]
    let global_scan = if thread_scan.complete {
        enforce_global_retention_scan_locked_blocking(
            root,
            keep_path,
            reserved_bytes,
            reserved_artifacts,
        )
    } else {
        RetentionScanProgress::default()
    };
}

fn enforce_retention_scan_locked_blocking(
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> RetentionScanProgress {
    let mut progress = RetentionScanProgress::default();
    let Ok(family_sizes) = logical_artifact_sizes_in_directory(directory) else {
        return progress;
    };
    let Ok(mut entries) = std::fs::read_dir(directory) else {
        return progress;
    };
    progress.directories_visited = 1;
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let entry = match entries.next().transpose() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => return progress,
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            progress.candidates_visited = progress.candidates_visited.saturating_add(1);
            let Some(bytes) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| family_sizes.get(stem))
                .copied()
            else {
                return progress;
            };
            let Ok(protected) = artifact_is_protected_blocking(&path) else {
                return progress;
            };
            if !protected {
                let Some(updated_total) = total_bytes.checked_add(bytes) else {
                    return progress;
                };
                total_bytes = updated_total;
            }
            paths.push((path.clone(), bytes, protected));
        }
    }
    paths.sort_unstable_by(|(left, ..), (right, ..)| left.cmp(right));

    let mut remove_count = paths
        .iter()
        .filter(|(_, _, protected)| !protected)
        .count()
        .saturating_add(reserved_artifacts)
        .saturating_sub(max_retained_artifacts_per_thread());
    for (path, bytes, protected) in paths {
        if remove_count == 0
            && total_bytes.saturating_add(reserved_bytes) <= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        {
            break;
        }
        if path == keep_path || protected {
            continue;
        }
        match remove_inactive_output_path_blocking(path) {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                remove_count = remove_count.saturating_sub(1);
                total_bytes = total_bytes.saturating_sub(bytes);
            }
            InactiveRemovalOutcome::Active => {}
            InactiveRemovalOutcome::Ambiguous(error) => {
                tracing::debug!(%error, "artifact removal remained ambiguous");
                return progress;
            }
        }
    }
    progress.complete = true;
    progress
}


fn enforce_global_retention_scan_locked_blocking(
    tool_output_root: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> RetentionScanProgress {
    let mut progress = RetentionScanProgress::default();
    let Ok(mut thread_directories) = std::fs::read_dir(tool_output_root) else {
        return progress;
    };
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let thread_directory = match thread_directories.next().transpose() {
            Ok(Some(entry)) => entry.path(),
            Ok(None) => break,
            Err(_) => return progress,
        };
        let Ok(family_sizes) = logical_artifact_sizes_in_directory(&thread_directory) else {
            return progress;
        };
        let Ok(mut entries) = std::fs::read_dir(&thread_directory) else {
            return progress;
        };
        progress.directories_visited = progress.directories_visited.saturating_add(1);
        loop {
            let entry = match entries.next().transpose() {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => return progress,
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
                progress.candidates_visited = progress.candidates_visited.saturating_add(1);
                let Ok(metadata) = entry.metadata() else {
                    return progress;
                };
                let Some(bytes) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| family_sizes.get(stem))
                    .copied()
                else {
                    return progress;
                };
                let Ok(modified) = metadata.modified() else {
                    return progress;
                };
                let Ok(protected) = artifact_is_protected_blocking(&path) else {
                    return progress;
                };
                if !protected {
                    let Some(updated_total) = total_bytes.checked_add(bytes) else {
                        return progress;
                    };
                    total_bytes = updated_total;
                }
                paths.push((modified, path.clone(), bytes, protected));
            }
        }
    }
    paths.sort_unstable_by(|(left_time, left_path, ..), (right_time, right_path, ..)| {
        left_time
            .cmp(right_time)
            .then_with(|| left_path.cmp(right_path))
    });
    let mut remove_count = paths
        .iter()
        .filter(|(_, _, _, protected)| !protected)
        .count()
        .saturating_add(reserved_artifacts)
        .saturating_sub(max_retained_artifacts_total());
    for (_, path, bytes, protected) in paths {
        if remove_count == 0
            && total_bytes.saturating_add(reserved_bytes) <= MAX_RETAINED_ARTIFACT_BYTES_TOTAL
        {
            break;
        }
        if !protected && path != keep_path {
            match remove_inactive_output_path_blocking(path) {
                InactiveRemovalOutcome::RemovedOrAbsent => {
                    remove_count = remove_count.saturating_sub(1);
                    total_bytes = total_bytes.saturating_sub(bytes);
                }
                InactiveRemovalOutcome::Active => {}
                InactiveRemovalOutcome::Ambiguous(error) => {
                    tracing::debug!(%error, "artifact removal remained ambiguous");
                    return progress;
                }
            }
        }
    }
    progress.complete = true;
    progress
}

fn retention_usage_locked_blocking(directory: &Path) -> RetentionUsage {
    let root = tool_output_root_for_directory(directory);
    let directory = normalized_retention_path(directory);
    let indexed_usage = {
        let registry = lock_retention_registry();
        registry
            .roots
            .get(&root)
            .and_then(|state| match &state.mode {
                RetentionRootMode::Indexed(index) => Some(RetentionUsage {
                    thread_bytes: index.thread_cache_totals(&directory).0,
                    global_bytes: index.unprotected_bytes,
                }),
                // An unavailable index is not evidence of an exhausted quota.
                // The caller holds the retention permit; scan outside the
                // registry mutex and the shared lock, just as for scan-only
                // roots. Real scan errors still fail closed below.
                RetentionRootMode::Dirty
                | RetentionRootMode::Reconciling { .. }
                | RetentionRootMode::ScanOnly { .. } => None,
            })
    };
    if let Some(usage) = indexed_usage {
        return usage;
    }
    RetentionUsage {
        thread_bytes: log_bytes_in_directory_blocking(&directory, false).unwrap_or(u64::MAX),
        global_bytes: without_shared_retention_lock(&root, || {
            log_bytes_in_tool_output_root_blocking(&root)
        })
        .unwrap_or(u64::MAX),
    }
}

fn log_bytes_in_directory_blocking(
    directory: &Path,
    include_protected: bool,
) -> std::io::Result<u64> {
    let family_sizes = logical_artifact_sizes_in_directory(directory)?;
    let mut entries = std::fs::read_dir(directory)?;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next().transpose()? {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            if !include_protected && artifact_is_protected_blocking(&entry.path())? {
                continue;
            }
            let path = entry.path();
            let family_bytes = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| family_sizes.get(stem))
                .copied()
                .ok_or_else(|| {
                    std::io::Error::other("artifact family changed during retention scan")
                })?;
            bytes = bytes.checked_add(family_bytes).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "thread artifact byte total overflowed",
                )
            })?;
        }
    }
    Ok(bytes)
}

fn log_bytes_in_tool_output_root_blocking(root: &Path) -> std::io::Result<u64> {
    let mut entries = std::fs::read_dir(root)?;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next().transpose()? {
        if entry.metadata()?.is_dir() {
            bytes = bytes
                .checked_add(log_bytes_in_directory_blocking(&entry.path(), false)?)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "global artifact byte total overflowed",
                    )
                })?;
        }
    }
    Ok(bytes)
}

/// This process's admission for a root plus the shared lock, which
/// `held_retention_locks` owns so a full-root pass can release it meanwhile.
struct RetentionSweepPermit {
    root: PathBuf,
    _process_permit: OwnedSemaphorePermit,
}

impl Drop for RetentionSweepPermit {
    fn drop(&mut self) {
        // Release the shared lock before the admission that guards it.
        let lock = held_retention_locks().remove(&self.root);
        drop(lock);
    }
}

/// Shared locks held by this process's retention permits. Admission makes a
/// root's entry exclusive to its one permit holder in this process.
fn held_retention_locks() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, AtomicWriteLock>> {
    static LOCKS: StdMutex<BTreeMap<PathBuf, AtomicWriteLock>> = StdMutex::new(BTreeMap::new());
    LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn retention_shared_lock_held(root: &Path) -> bool {
    held_retention_locks().contains_key(&normalized_tool_output_root(root))
}

/// Runs a full-root pass without the shared lock that this process's permit
/// for `root` holds, then takes the lock back. A scan under that lock queued
/// every other process's artifact commit behind it for seconds. Commits that
/// land meanwhile leave the pass incomplete rather than wrong, and retaking
/// the lock advances the generation, which marks this index stale for them.
/// If the lock cannot be retaken, `retention_shared_lock_held` turns false and
/// the holder must not mutate the root. Without a held permit, just runs.
fn without_shared_retention_lock<R>(root: &Path, pass: impl FnOnce() -> R) -> R {
    let root = normalized_tool_output_root(root);
    let released = held_retention_locks().remove(&root);
    let Some(lock) = released else {
        return pass();
    };
    drop(lock);
    let result = pass();
    let lock_target = retention_interprocess_lock_target(&root);
    match acquire_atomic_write_lock(&lock_target).and_then(|lock| {
        advance_retention_interprocess_generation(&root, &lock_target).map(|()| lock)
    }) {
        Ok(lock) => {
            held_retention_locks().insert(root, lock);
        }
        Err(error) => record_retention_sweep_permit_failure(&root, &error),
    }
    result
}

static RETENTION_SWEEP_PERMIT_FAILURES: AtomicU64 = AtomicU64::new(0);


fn record_retention_sweep_permit_failure(root: &Path, error: &dyn fmt::Display) {
    RETENTION_SWEEP_PERMIT_FAILURES.fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        root = %root.display(),
        %error,
        "skipping artifact retention bookkeeping because its shared lock is unavailable"
    );
}

#[derive(Default)]
struct RetentionInterprocessRootState {
    generation: Option<u64>,
    index_stale: bool,
}

fn retention_interprocess_lock_target(root: &Path) -> PathBuf {
    let normalized_root = normalized_tool_output_root(root);
    let identity = hex::encode(Sha256::digest(normalized_root.to_string_lossy().as_bytes()));
    std::env::temp_dir().join(format!("openai-codex-tool-output-retention-v2-{identity}"))
}

fn retention_interprocess_states()
-> &'static StdMutex<BTreeMap<PathBuf, RetentionInterprocessRootState>> {
    static STATES: OnceLock<StdMutex<BTreeMap<PathBuf, RetentionInterprocessRootState>>> =
        OnceLock::new();
    STATES.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

fn take_retention_interprocess_index_stale(root: &Path) -> bool {
    let root = normalized_tool_output_root(root);
    let mut states = retention_interprocess_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states
        .entry(root)
        .or_insert_with(|| RetentionInterprocessRootState {
            generation: None,
            index_stale: true,
        });
    std::mem::take(&mut state.index_stale)
}

fn mark_retention_interprocess_index_stale(root: &Path) {
    let root = normalized_tool_output_root(root);
    if let Some(state) = retention_interprocess_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&root)
    {
        state.index_stale = true;
    }
}

fn advance_retention_interprocess_generation(
    root: &Path,
    lock_target: &Path,
) -> std::io::Result<()> {
    let (previous, next, recovered) = match std::fs::read_to_string(lock_target) {
        Ok(value) => match value
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|previous| previous.checked_add(1).map(|next| (previous, next)))
        {
            Some((previous, next)) => (previous, next, false),
            None => (0, 1, true),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (0, 1, false),
        Err(err) => return Err(err),
    };
    // This counter invalidates in-memory indexes in other live processes; it
    // is not artifact recovery data. Atomic replacement under the existing OS
    // lock is required, but syncing it on every admission buys no durability:
    // after a machine restart every process rebuilds its index from disk.
    let parent = lock_target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "retention generation has no parent",
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(next.to_string().as_bytes())?;
    let (_file, temporary_path) = temporary.keep().map_err(|error| error.error)?;
    let temporary_path = tempfile::TempPath::try_from_path(temporary_path)?;
    std::fs::rename(&temporary_path, lock_target)?;
    let root = normalized_tool_output_root(root);
    let mut states = retention_interprocess_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = retention_interprocess_state(&mut states, root);
    if recovered || state.generation.is_none_or(|observed| observed != previous) {
        state.index_stale = true;
    }
    state.generation = Some(next);
    Ok(())
}

/// Records the shared generation that a scan without the shared lock starts
/// from. The lock is held only to read it, so no commit is mid-flight, and the
/// generation is left unchanged, so other processes keep their indexes. Any
/// later permit, in any process, advances past it.
fn observe_retention_interprocess_generation(root: &Path) -> std::io::Result<()> {
    let lock_target = retention_interprocess_lock_target(root);
    let _lock = acquire_atomic_write_lock(&lock_target)?;
    let observed = match std::fs::read_to_string(&lock_target) {
        Ok(value) => value.trim().parse::<u64>().ok(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(err) => return Err(err),
    };
    let root = normalized_tool_output_root(root);
    let mut states = retention_interprocess_states()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = retention_interprocess_state(&mut states, root);
    // A malformed generation stays stale until the next permit repairs it.
    state.index_stale = observed.is_none();
    state.generation = observed;
    Ok(())
}

fn retention_interprocess_state(
    states: &mut BTreeMap<PathBuf, RetentionInterprocessRootState>,
    root: PathBuf,
) -> &mut RetentionInterprocessRootState {
    if !states.contains_key(&root)
        && states.len() >= MAX_RETENTION_INDEX_ROOTS
        && let Some(evicted) = states.keys().next().cloned()
    {
        states.remove(&evicted);
    }
    states
        .entry(root)
        .or_insert_with(|| RetentionInterprocessRootState {
            generation: None,
            index_stale: true,
        })
}

/// This process's retention admission, without the shared lock.
async fn retention_process_admission(root: &Path) -> Option<(PathBuf, OwnedSemaphorePermit)> {
    let (root, semaphore) = match retention_sweep_admission(root).await {
        Ok(admission) => admission,
        Err(error) => {
            record_retention_sweep_permit_failure(root, &error);
            return None;
        }
    };
    match semaphore.acquire_owned().await {
        Ok(permit) => Some((root, permit)),
        Err(error) => {
            record_retention_sweep_permit_failure(&root, &error);
            None
        }
    }
}

async fn retention_sweep_permit(root: &Path) -> Option<RetentionSweepPermit> {
    let (root, process_permit) = retention_process_admission(root).await?;
    let lock_root = root.clone();
    match tokio::task::spawn_blocking(move || {
        retention_sweep_permit_blocking(&lock_root, process_permit)
    })
    .await
    {
        Ok(Ok(permit)) => Some(permit),
        Ok(Err(error)) => {
            record_retention_sweep_permit_failure(&root, &error);
            None
        }
        Err(error) => {
            record_retention_sweep_permit_failure(&root, &error);
            None
        }
    }
}

fn retention_sweep_permit_blocking(
    root: &Path,
    process_permit: OwnedSemaphorePermit,
) -> std::io::Result<RetentionSweepPermit> {
    // The OS lock acquisition can outlive its awaiter. Move admission into this
    // worker before the first filesystem wait, including on error and panic.
    let lock_target = retention_interprocess_lock_target(root);
    let interprocess_lock = acquire_atomic_write_lock(&lock_target)?;
    advance_retention_interprocess_generation(root, &lock_target)?;
    let root = normalized_tool_output_root(root);
    held_retention_locks().insert(root.clone(), interprocess_lock);
    Ok(RetentionSweepPermit {
        root,
        _process_permit: process_permit,
    })
}

async fn retention_sweep_permit_for_directory(directory: &Path) -> Option<RetentionSweepPermit> {
    retention_sweep_permit(directory.parent().unwrap_or_else(|| Path::new("."))).await
}



async fn retention_sweep_admission(
    root: &Path,
) -> Result<(PathBuf, Arc<Semaphore>), tokio::task::JoinError> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let root = normalized_tool_output_root(&root);
        let semaphore = retention_sweep_semaphore(&root);
        (root, semaphore)
    })
    .await
}

async fn retention_sweep_admission_for_directory(
    directory: &Path,
) -> Result<(PathBuf, Arc<Semaphore>), tokio::task::JoinError> {
    retention_sweep_admission(directory.parent().unwrap_or_else(|| Path::new("."))).await
}

static RETENTION_SWEEP_SEMAPHORES: StdMutex<BTreeMap<PathBuf, Weak<Semaphore>>> =
    StdMutex::new(BTreeMap::new());

fn retention_sweep_semaphore(root: &Path) -> Arc<Semaphore> {
    let root = normalized_tool_output_root(root);
    let mut semaphores = RETENTION_SWEEP_SEMAPHORES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    semaphores.retain(|_, semaphore| semaphore.strong_count() > 0);
    if let Some(semaphore) = semaphores.get(&root).and_then(Weak::upgrade) {
        return semaphore;
    }
    let semaphore = Arc::new(Semaphore::new(1));
    semaphores.insert(root, Arc::downgrade(&semaphore));
    semaphore
}











fn remove_inactive_output_path_blocking(path: PathBuf) -> InactiveRemovalOutcome {
    // Release completed recovery residency before the existing writer/retention
    // admission probes; otherwise a cache lease would look like active work.
    #[cfg(windows)]
    let _cache_mutation = immutable_snapshot_cache::shared().mutating(&path);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return InactiveRemovalOutcome::RemovedOrAbsent;
        }
        Err(err) => return InactiveRemovalOutcome::Ambiguous(err),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return InactiveRemovalOutcome::Active;
        }
        Err(err) => return InactiveRemovalOutcome::Ambiguous(err.into()),
    }
    // A cached retention index can predate another process's protection marker.
    // Recheck under the existing retention and writer locks before deleting any
    // family member. Treat a stale index as ambiguous so its obsolete byte/count
    // totals cannot drive eviction of unrelated artifacts either.
    match artifact_is_protected_blocking(&path) {
        Ok(false) => {}
        Ok(true) => {
            return InactiveRemovalOutcome::Ambiguous(std::io::Error::other(
                "artifact acquired tool-history protection after retention indexing",
            ));
        }
        Err(err) => return InactiveRemovalOutcome::Ambiguous(err),
    }
    match remove_logical_artifact_files(&path) {
        Ok(()) => InactiveRemovalOutcome::RemovedOrAbsent,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            InactiveRemovalOutcome::RemovedOrAbsent
        }
        Err(err) => InactiveRemovalOutcome::Ambiguous(err),
    }
}

// One directory pass accounts for every segment and sidecar. Scan-only roots
// must not rescan the directory once per artifact (quadratic in family count).
fn logical_artifact_sizes_in_directory(directory: &Path) -> std::io::Result<BTreeMap<String, u64>> {
    let mut sizes = BTreeMap::<String, u64>::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(logical_artifact_stem) else {
            continue;
        };
        let bytes = sizes.entry(stem.to_string()).or_default();
        *bytes = bytes.checked_add(entry.metadata()?.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "logical artifact byte total overflowed",
            )
        })?;
    }
    Ok(sizes)
}

fn logical_artifact_disk_bytes(path: &Path) -> std::io::Result<u64> {
    let Some(directory) = path.parent() else {
        return Ok(0);
    };
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let entries = std::fs::read_dir(directory)?;
    let mut bytes = 0_u64;
    let base_name = format!("{stem}.log");
    let metadata_name = format!("{stem}.meta.json");
    let segment_prefix = format!("{stem}.segment-");
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == base_name
            || name == metadata_name
            || name.starts_with(segment_prefix.as_str())
        {
            bytes = bytes.checked_add(entry.metadata()?.len()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "logical artifact byte total overflowed",
                )
            })?;
        }
    }
    Ok(bytes)
}

