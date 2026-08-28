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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::time::Instant;
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
#[cfg(test)]
use codex_tools::ToolProjectionInclusion;
use codex_tools::ToolProjectionSection;
use codex_utils_string::approx_token_count;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;

const ARTIFACT_EXPIRED_MESSAGE: &str = "artifact expired or does not belong to this thread; rerun the command if the output is still needed";
const ARTIFACT_WRITING_MESSAGE: &str =
    "artifact is still being written; retry after the command yields or exits";
const EVIDENCE_PROTECTION_EXTENSION: &str = "evidence-protected";
const EVIDENCE_PROTECTION_MARKER_BYTES: &[u8; 34] = b"KD4_EXTERNAL_EVIDENCE_ARTIFACT_V1\n";
const ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION: &str = "active-tool-history";
const ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES: &[u8] = b"KD4_ACTIVE_TOOL_HISTORY_ARTIFACT_V1\n";
const MAX_RAW_OUTPUT_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
const LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES: usize = 4 * 1024;
const MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD: u64 = 256 * 1024 * 1024;
const MAX_RETAINED_ARTIFACT_BYTES_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_RETENTION_INDEX_ROOTS: usize = 4;
const MAX_RETENTION_INDEX_RECORDS: usize = 8_192;
const RETENTION_RECONCILIATION_INTERVAL: u64 = 128;
const RETENTION_BYTE_GUARD_BAND: u64 = MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64;
pub(crate) const RECOVERY_AGGREGATE_TOKEN_CEILING: usize = 10_000;
const RECOVERY_FRAGMENT_TOKEN_CEILING: usize = 1_000;
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
        #[serde(default)]
        start_byte: u64,
        #[serde(default = "artifact_search_default_max_results")]
        max_results: usize,
        #[serde(default)]
        context_lines: usize,
    },
}

const fn artifact_search_default_max_results() -> usize {
    ARTIFACT_SEARCH_DEFAULT_MAX_RESULTS
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ReadToolOutputResult {
    pub artifact_id: String,
    pub canonical_sha256: String,
    pub canonical_bytes: u64,
    pub retained_bytes: u64,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable_ranges: Vec<CanonicalByteRange>,
    pub results: Vec<ToolOutputSelectorResult>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LogicalArtifactSegment {
    index: u32,
    range: CanonicalByteRange,
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RetentionDiagnostics {
    scans: u64,
    scan_wall_nanos: u64,
    directories_visited: u64,
    candidates_visited: u64,
    logical_mutations: u64,
    streaming_size_updates: u64,
    stale_delta_rejections: u64,
    dirty_transitions: u64,
    reconciliations: u64,
    creates: u64,
    deletes: u64,
    protection_changes: u64,
    evictions: u64,
    oversized_root_fallbacks: u64,
    scan_only_operations: u64,
    scan_only_entries: u64,
    scan_only_exits: u64,
}

#[cfg(test)]
macro_rules! record_retention_diagnostics {
    ($($body:tt)*) => {{ $($body)* }};
}

#[cfg(not(test))]
macro_rules! record_retention_diagnostics {
    ($($body:tt)*) => {{}};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionModeKind {
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
    paths: BTreeSet<PathBuf>,
    bytes: u64,
    unprotected: usize,
}

#[derive(Debug, Clone, Default)]
struct RetentionIndex {
    records: BTreeMap<PathBuf, ArtifactRetentionRecord>,
    threads: BTreeMap<PathBuf, ThreadRetentionIndex>,
    global_order: BTreeSet<(SystemTime, PathBuf)>,
    total_bytes: u64,
    unprotected: usize,
    logical_mutations_since_reconciliation: u64,
    near_limit_reconciled: bool,
    near_limit_pending: bool,
}

impl RetentionIndex {
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
        thread.paths.insert(record.path.clone());
        thread.bytes = thread_bytes;
        if !record.protected {
            thread.unprotected = thread.unprotected.saturating_add(1);
            self.unprotected = self.unprotected.saturating_add(1);
        }
        self.total_bytes = total_bytes;
        self.global_order
            .insert((record.modified, record.path.clone()));
        self.records.insert(record.path.clone(), record);
        true
    }

    fn remove(&mut self, path: &Path) -> Option<ArtifactRetentionRecord> {
        let path = normalized_retention_path(path);
        let record = self.records.remove(&path)?;
        self.global_order
            .remove(&(record.modified, record.path.clone()));
        self.total_bytes = self.total_bytes.saturating_sub(record.bytes);
        if !record.protected {
            self.unprotected = self.unprotected.saturating_sub(1);
        }
        let remove_thread = if let Some(thread) = self.threads.get_mut(&record.thread_directory) {
            thread.paths.remove(&path);
            thread.bytes = thread.bytes.saturating_sub(record.bytes);
            if !record.protected {
                thread.unprotected = thread.unprotected.saturating_sub(1);
            }
            thread.paths.is_empty()
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

    fn thread_totals(&self, directory: &Path) -> (u64, usize) {
        let directory = normalized_retention_path(directory);
        self.threads
            .get(&directory)
            .map_or((0, 0), |thread| (thread.bytes, thread.unprotected))
    }

    fn is_near_limit(&self) -> bool {
        let global_near = self.unprotected >= max_retained_artifacts_total().saturating_sub(1)
            || self.total_bytes
                >= MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(RETENTION_BYTE_GUARD_BAND);
        global_near
            || self.threads.values().any(|thread| {
                thread.unprotected >= max_retained_artifacts_per_thread().saturating_sub(1)
                    || thread.bytes
                        >= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
                            .saturating_sub(RETENTION_BYTE_GUARD_BAND)
            })
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
    #[cfg(test)]
    diagnostics: RetentionDiagnostics,
    #[cfg(test)]
    index_capacity_override: Option<usize>,
}

#[derive(Debug, Default)]
struct RetentionRegistry {
    roots: BTreeMap<PathBuf, RetentionRootState>,
    access_clock: u64,
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
    #[cfg(test)]
    EvidenceReconcile,
    #[cfg(test)]
    StreamComplete,
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

fn max_retained_artifacts_total() -> usize {
    1_024
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
    match NEXT_GENERATION.fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
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
            #[cfg(test)]
            diagnostics: RetentionDiagnostics::default(),
            #[cfg(test)]
            index_capacity_override: None,
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

fn transition_current_root_to_dirty(registry: &mut RetentionRegistry, root: &Path) {
    transition_root_to_dirty(registry, root, true);
}

fn transition_current_root_to_dirty_for_conflict(registry: &mut RetentionRegistry, root: &Path) {
    transition_root_to_dirty(registry, root, false);
}

fn transition_root_to_dirty(
    registry: &mut RetentionRegistry,
    root: &Path,
    #[cfg_attr(not(test), allow(unused_variables))] stale_delta_rejected: bool,
) {
    let Some(state) = registry.roots.get_mut(root) else {
        let _ = insert_dirty_root(registry, root.to_path_buf());
        #[cfg(test)]
        if let Some(state) = registry.roots.get_mut(root) {
            state.diagnostics.stale_delta_rejections = u64::from(stale_delta_rejected);
            state.diagnostics.dirty_transitions = 1;
        }
        return;
    };
    #[cfg(test)]
    if stale_delta_rejected {
        state.diagnostics.stale_delta_rejections =
            state.diagnostics.stale_delta_rejections.saturating_add(1);
    }
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
            record_retention_diagnostics! {
                state.diagnostics.dirty_transitions =
                    state.diagnostics.dirty_transitions.saturating_add(1);
            }
        }
        RetentionRootMode::Indexed(_) => {
            let Some(generation) = next_index_generation() else {
                indexing_disabled().store(true, Ordering::Release);
                return;
            };
            state.generation = generation;
            state.mode = RetentionRootMode::Dirty;
            record_retention_diagnostics! {
                state.diagnostics.dirty_transitions =
                    state.diagnostics.dirty_transitions.saturating_add(1);
            }
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

async fn artifact_retention_record(
    path: &Path,
) -> std::io::Result<Option<ArtifactRetentionRecord>> {
    let bytes = logical_artifact_disk_bytes(path).await?;
    artifact_retention_record_with_bytes(path, bytes).await
}

async fn artifact_retention_record_with_bytes(
    path: &Path,
    bytes: u64,
) -> std::io::Result<Option<ArtifactRetentionRecord>> {
    let link_metadata = match tokio::fs::symlink_metadata(path).await {
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
    let metadata = match tokio::fs::metadata(path).await {
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
    let evidence_marker = evidence_protection_path(path);
    let tool_history_marker = active_tool_history_protection_path(path);
    let protected = tokio::task::spawn_blocking(move || {
        let evidence =
            retention_protection_marker_status(&evidence_marker, EVIDENCE_PROTECTION_MARKER_BYTES)?;
        let tool_history = retention_protection_marker_status(
            &tool_history_marker,
            ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
        )?;
        Ok::<_, std::io::Error>(evidence || tool_history)
    })
    .await
    .map_err(std::io::Error::other)??;
    let final_link_metadata = tokio::fs::symlink_metadata(path).await?;
    if final_link_metadata.file_type().is_symlink()
        || metadata_is_reparse_point(&final_link_metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact path changed to a link or reparse point during retention observation",
        ));
    }
    let final_metadata = tokio::fs::metadata(path).await?;
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

async fn scan_retention_root(
    root: &Path,
    capacity: usize,
) -> std::io::Result<(RetentionScanCandidate, u64, u64)> {
    #[cfg(test)]
    wait_at_reconciliation_barrier(root).await;

    let mut index = Some(RetentionIndex::default());
    let mut directories_visited = 0_u64;
    let mut candidates_visited = 0_u64;
    let mut oversized = false;
    let mut thread_directories = tokio::fs::read_dir(root).await?;
    loop {
        let thread_entry = match thread_directories.next_entry().await? {
            Some(entry) => entry,
            None => break,
        };
        if !thread_entry.file_type().await?.is_dir() {
            continue;
        }
        directories_visited = directories_visited.saturating_add(1);
        let mut entries = tokio::fs::read_dir(thread_entry.path()).await?;
        let mut log_paths = Vec::new();
        let mut bytes_by_stem = BTreeMap::<String, u64>::new();
        loop {
            let entry = match entries.next_entry().await? {
                Some(entry) => entry,
                None => break,
            };
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let is_log = path.extension().and_then(|extension| extension.to_str()) == Some("log");
            if is_log {
                candidates_visited = candidates_visited.saturating_add(1);
                if candidates_visited as usize > capacity {
                    oversized = true;
                    index = None;
                    log_paths.clear();
                    bytes_by_stem.clear();
                } else if !oversized {
                    log_paths.push(path.clone());
                }
            }
            if oversized {
                continue;
            }
            let Some(stem) = logical_artifact_stem(name) else {
                continue;
            };
            let bytes = entry.metadata().await?.len();
            let entry_bytes = bytes_by_stem.entry(stem.to_string()).or_default();
            *entry_bytes = entry_bytes.checked_add(bytes).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "logical artifact byte total overflowed",
                )
            })?;
        }
        if oversized {
            continue;
        }
        for path in log_paths {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            let bytes = bytes_by_stem.get(stem).copied().unwrap_or_default();
            let Some(record) = artifact_retention_record_with_bytes(&path, bytes).await? else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "artifact disappeared during retention reconciliation",
                ));
            };
            let Some(candidate_index) = index.as_mut() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "retention candidate was discarded before reconciliation completed",
                ));
            };
            if !candidate_index.insert(record) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "retention index byte totals overflowed",
                ));
            }
        }
    }
    if oversized {
        Ok((
            RetentionScanCandidate::Oversized,
            directories_visited,
            candidates_visited,
        ))
    } else {
        let Some(mut index) = index else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "in-capacity retention scan has no index",
            ));
        };
        index.near_limit_reconciled = index.is_near_limit();
        Ok((
            RetentionScanCandidate::Indexed(index),
            directories_visited,
            candidates_visited,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolOutputArtifactId(uuid::Uuid);

impl ToolOutputArtifactId {
    fn new() -> Self {
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

pub(crate) struct RawOutputArtifactWriter {
    id: Option<ToolOutputArtifactId>,
    path: Option<PathBuf>,
    file: Option<tokio::fs::File>,
    bytes: u64,
    truncated: bool,
    handle: Option<Arc<File>>,
    retention_token: Option<RetentionIndexToken>,
    lifecycle_completed: bool,
    pending_target: Option<(PathBuf, String)>,
    pending_output: Vec<u8>,
}

impl RawOutputArtifactWriter {
    pub(crate) async fn open(state: Option<&Arc<Mutex<RawOutputArtifact>>>) -> Option<Self> {
        let state = state?;
        let artifact = state.lock().await.clone();
        if let RawOutputArtifact::Pending {
            codex_home,
            thread_id,
        } = artifact
        {
            return Some(Self {
                id: None,
                path: None,
                file: None,
                bytes: 0,
                truncated: false,
                handle: None,
                retention_token: None,
                lifecycle_completed: false,
                pending_target: Some((codex_home, thread_id)),
                pending_output: Vec::new(),
            });
        }
        let RawOutputArtifact::Stored {
            id,
            path,
            bytes,
            truncated,
            handle,
        } = artifact
        else {
            return Some(Self {
                id: None,
                path: None,
                file: None,
                bytes: 0,
                truncated: false,
                handle: None,
                retention_token: None,
                lifecycle_completed: true,
                pending_target: None,
                pending_output: Vec::new(),
            });
        };
        let retention_token =
            capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));
        match lock_artifact_handle(&handle, SeekFrom::End(0)) {
            Ok(file) => {
                let file = tokio::fs::File::from_std(file);
                match lock_output_file(file).await {
                    Ok(file) => Some(Self {
                        id: Some(id),
                        path: Some(path),
                        file: Some(file),
                        bytes,
                        truncated,
                        handle: Some(handle),
                        retention_token: Some(retention_token),
                        lifecycle_completed: false,
                        pending_target: None,
                        pending_output: Vec::new(),
                    }),
                    Err(err) => {
                        reject_stale_delta(&retention_token);
                        enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path)
                            .await;
                        *state.lock().await = RawOutputArtifact::Failed {
                            id: artifact_id_from_path(&path),
                            message: format!(
                                "failed to lock `{}` for streaming: {err}",
                                path.display()
                            ),
                            owned_path: Some(path.clone()),
                            bytes,
                        };
                        Some(Self {
                            id: Some(id),
                            path: Some(path),
                            file: None,
                            bytes,
                            truncated,
                            handle: Some(handle),
                            retention_token: Some(retention_token),
                            lifecycle_completed: true,
                            pending_target: None,
                            pending_output: Vec::new(),
                        })
                    }
                }
            }
            Err(err) => {
                reject_stale_delta(&retention_token);
                enforce_retention(path.parent().unwrap_or_else(|| Path::new(".")), &path).await;
                *state.lock().await = RawOutputArtifact::Failed {
                    id: artifact_id_from_path(&path),
                    message: format!("failed to open `{}` for streaming: {err}", path.display()),
                    owned_path: Some(path.clone()),
                    bytes,
                };
                Some(Self {
                    id: Some(id),
                    path: Some(path),
                    file: None,
                    bytes,
                    truncated,
                    handle: Some(handle),
                    retention_token: Some(retention_token),
                    lifecycle_completed: true,
                    pending_target: None,
                    pending_output: Vec::new(),
                })
            }
        }
    }

    pub(crate) async fn write_chunk(
        &mut self,
        state: Option<&Arc<Mutex<RawOutputArtifact>>>,
        output: &[u8],
    ) {
        if let Some((codex_home, thread_id)) = self.pending_target.clone() {
            // Keep small output inline and defer all artifact I/O until this buffer grows.
            self.pending_output.extend_from_slice(output);
            if self.pending_output.len() <= LAZY_RAW_OUTPUT_ARTIFACT_THRESHOLD_BYTES {
                return;
            }
            let artifact =
                create_raw_output_artifact(codex_home.as_path(), &thread_id, &self.pending_output)
                    .await;
            if let Some(state) = state {
                *state.lock().await = artifact;
                if let Some(opened) = Self::open(Some(state)).await {
                    *self = opened;
                }
            }
            return;
        }
        let (Some(state), Some(id), Some(path)) = (state, self.id, self.path.clone()) else {
            return;
        };
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let remaining = MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_sub(self.bytes as usize);
        let retained = &output[..output.len().min(remaining)];
        self.truncated |= retained.len() != output.len();
        if let Err(err) = file.write_all(retained).await {
            if let Some(file) = self.file.take() {
                let _ = unlock_output_file(file).await;
            }
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to stream `{}`: {err}", path.display()),
                self.retention_token.as_ref(),
            )
            .await;
            self.lifecycle_completed = true;
            return;
        }
        self.bytes = self.bytes.saturating_add(retained.len() as u64);
        match file.metadata().await.and_then(|metadata| {
            metadata
                .modified()
                .map(|modified| (metadata.len(), modified))
        }) {
            Ok((bytes, modified)) => {
                if let Some(token) = self.retention_token.as_ref() {
                    publish_streaming_size(token, &path, bytes, modified, false);
                }
            }
            Err(_) => {
                if let Some(token) = self.retention_token.as_ref() {
                    reject_stale_delta(token);
                }
            }
        }
        let Some(handle) = self.handle.clone() else {
            return;
        };
        *state.lock().await = RawOutputArtifact::Stored {
            id,
            path,
            bytes: self.bytes,
            truncated: self.truncated,
            handle,
        };
    }

    pub(crate) async fn finish(&mut self, state: Option<&Arc<Mutex<RawOutputArtifact>>>) {
        if let Some((codex_home, thread_id)) = self.pending_target.take() {
            let artifact =
                create_raw_output_artifact(&codex_home, &thread_id, &self.pending_output).await;
            if let Some(state) = state {
                *state.lock().await = artifact;
            }
            self.pending_output.clear();
            self.lifecycle_completed = true;
            return;
        }
        let (Some(state), Some(path), Some(mut file)) =
            (state, self.path.clone(), self.file.take())
        else {
            return;
        };
        if let Err(err) = file.flush().await {
            let _ = unlock_output_file(file).await;
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to flush `{}`: {err}", path.display()),
                self.retention_token.as_ref(),
            )
            .await;
            self.lifecycle_completed = true;
            return;
        }
        let metadata = file.metadata().await.and_then(|metadata| {
            metadata
                .modified()
                .map(|modified| (metadata.len(), modified))
        });
        if let Err(err) = unlock_output_file(file).await {
            *state.lock().await = failed_with_owned_path(
                path.clone(),
                self.bytes,
                format!("failed to unlock `{}`: {err}", path.display()),
                self.retention_token.as_ref(),
            )
            .await;
            self.lifecycle_completed = true;
            return;
        }
        if let Some(token) = self.retention_token.as_ref() {
            match metadata {
                Ok((bytes, modified)) => {
                    publish_streaming_size(token, &path, bytes, modified, true);
                }
                Err(_) => reject_stale_delta(token),
            }
        }
        self.lifecycle_completed = true;
    }
}

impl Drop for RawOutputArtifactWriter {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        if let Ok(file) = file.try_into_std() {
            let _ = file.unlock();
        }
        if !self.lifecycle_completed {
            if let Some(token) = self.retention_token.as_ref() {
                publish_streaming_abandonment(token);
            }
            self.lifecycle_completed = true;
        }
    }
}

async fn lock_output_file(file: tokio::fs::File) -> std::io::Result<tokio::fs::File> {
    let file = file.into_std().await;
    file.try_lock()?;
    Ok(tokio::fs::File::from_std(file))
}

async fn unlock_output_file(file: tokio::fs::File) -> std::io::Result<()> {
    let file = file.into_std().await;
    file.unlock()
}

fn lock_artifact_handle(handle: &Arc<File>, position: SeekFrom) -> std::io::Result<File> {
    let mut file = handle.try_clone()?;
    file.seek(position)?;
    Ok(file)
}

impl RawOutputArtifact {
    #[cfg(test)]
    pub(crate) fn artifact_id(&self) -> Option<ToolOutputArtifactId> {
        match self {
            Self::Pending { .. } => None,
            Self::Stored { id, .. } => Some(*id),
            Self::Failed { id, .. } => *id,
        }
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
                    ", truncated at safety limit"
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

    pub(crate) fn render_for_model_with_source_truncation(
        &self,
        source_capture_truncated: bool,
    ) -> String {
        let mut rendered = self.render_for_model();
        if source_capture_truncated {
            if !rendered.is_empty() {
                rendered.push_str("; ");
            }
            rendered.push_str(
                "source capture truncated at execution retained-byte limit; omitted bytes unavailable",
            );
        }
        rendered
    }

    pub(crate) fn model_projection(
        &self,
    ) -> (Option<ToolOutputArtifactId>, Option<u64>, Option<String>) {
        match self {
            Self::Pending { .. } => (None, None, None),
            Self::Stored { id, bytes, .. } => (Some(*id), Some(*bytes), None),
            Self::Failed { .. } => (
                None,
                None,
                Some("raw output artifact storage failed".to_string()),
            ),
        }
    }

    pub(crate) fn reduction_notice(&self) -> Option<String> {
        let Self::Stored {
            path, truncated, ..
        } = self
        else {
            return None;
        };
        if open_regular_artifact(path).is_err() {
            return None;
        }
        let scope = if *truncated {
            "the retained prefix"
        } else {
            "the full retained output"
        };
        Some(format!(
            "[command output reduced; recover {scope} with read_tool_output using the raw output artifact above. Batch exact ranges when possible; do not rerun the producer.]"
        ))
    }

    pub(crate) fn retained_bytes(&self) -> Option<u64> {
        match self {
            Self::Pending { .. } => None,
            Self::Stored { bytes, .. } => Some(*bytes),
            Self::Failed { .. } => None,
        }
    }

    pub(crate) fn retention_limit_hit(&self) -> bool {
        matches!(
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

    pub(crate) async fn validation_integrity_with_output(
        &self,
    ) -> Option<(String, String, Vec<u8>)> {
        let Self::Stored {
            id, bytes, path, ..
        } = self
        else {
            return None;
        };
        let expected_bytes = *bytes;
        let id = id.to_string();
        let (mut file, _) = open_regular_artifact(path).ok()?;
        tokio::task::spawn_blocking(move || {
            file.seek(SeekFrom::Start(0)).ok()?;
            let mut retained = Vec::with_capacity(
                usize::try_from(expected_bytes)
                    .ok()?
                    .min(MAX_RAW_OUTPUT_ARTIFACT_BYTES),
            );
            file.read_to_end(&mut retained).ok()?;
            (u64::try_from(retained.len()).ok()? == expected_bytes)
                .then(|| (id, format!("{:x}", Sha256::digest(&retained)), retained))
        })
        .await
        .ok()?
    }
}

pub(crate) async fn create_raw_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
) -> RawOutputArtifact {
    let directory = codex_home.join("tool-output").join(thread_id);
    if let Err(err) = tokio::fs::create_dir_all(&directory).await {
        return RawOutputArtifact::unavailable(format!(
            "failed to create `{}`: {err}",
            directory.display()
        ));
    }
    let retention_token = capture_retention_token(&directory);

    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    let retained = &output[..output.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
    let truncated = retained.len() != output.len();
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
            if let Err(err) = file.sync_all().await {
                let _ = unlock_output_file(file).await;
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to sync `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            let file = file.into_std().await;
            let _ = file.unlock();
            let handle = Arc::new(file);
            let sync_path = path.clone();
            if let Err(err) =
                run_blocking_artifact_io(move || sync_parent_directory(&sync_path)).await
            {
                return failed_with_owned_path(
                    path.clone(),
                    retained.len() as u64,
                    format!("failed to sync artifact directory: {err}"),
                    Some(&retention_token),
                )
                .await;
            }
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
            }
        }
        Err(err) => {
            enforce_retention_after_observation(&directory, &path, &retention_token).await;
            RawOutputArtifact::unavailable(format!("failed to create `{}`: {err}", path.display()))
        }
    }
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
    match std::fs::symlink_metadata(&transaction_path) {
        Ok(metadata) if metadata.is_file() && !metadata_is_reparse_point(&metadata) => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid logical artifact transaction marker `{}`",
                    transaction_path.display()
                ),
            ));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
    let transaction_bytes = std::fs::read(&transaction_path)?;
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

    let metadata = std::fs::read(logical_metadata_path(path))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LogicalArtifactMetadata>(&bytes).ok())
        .filter(|metadata| metadata.version == LOGICAL_ARTIFACT_METADATA_VERSION);
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

fn reconcile_logical_artifact_transactions_in_root(root: &Path) -> std::io::Result<()> {
    let directories = match std::fs::read_dir(root) {
        Ok(directories) => directories,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for directory in directories {
        let directory = directory?;
        let metadata = directory.metadata()?;
        if metadata.is_dir() && !metadata_is_reparse_point(&metadata) {
            reconcile_logical_artifact_transactions(&directory.path())?;
        }
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

struct StagedLogicalSegment {
    index: u32,
    range: CanonicalByteRange,
    path: PathBuf,
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

fn staged_logical_segment_path(directory: &Path, id: ToolOutputArtifactId, index: u32) -> PathBuf {
    directory.join(format!(".{id}.segment-{index:06}.pending"))
}

async fn remove_staged_logical_segments(segments: &[StagedLogicalSegment]) {
    for segment in segments {
        let _ = tokio::fs::remove_file(&segment.path).await;
    }
}

async fn write_staged_logical_segment(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

async fn stage_logical_segments(
    directory: &Path,
    id: ToolOutputArtifactId,
    bytes: &[u8],
    canonical_start: u64,
    first_index: u32,
) -> Result<Vec<StagedLogicalSegment>, (u64, PathBuf, std::io::Error)> {
    let mut staged = Vec::new();
    for (offset, chunk) in bytes.chunks(MAX_RAW_OUTPUT_ARTIFACT_BYTES).enumerate() {
        let index = first_index + offset as u32;
        let start = canonical_start + offset as u64 * MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64;
        let path = staged_logical_segment_path(directory, id, index);
        if let Err(err) = write_staged_logical_segment(&path, chunk).await {
            let _ = tokio::fs::remove_file(&path).await;
            remove_staged_logical_segments(&staged).await;
            return Err((start, path, err));
        }
        staged.push(StagedLogicalSegment {
            index,
            range: CanonicalByteRange::new(start, start + chunk.len() as u64),
            path,
        });
    }
    if bytes.is_empty() && first_index == 0 {
        let path = staged_logical_segment_path(directory, id, 0);
        if let Err(err) = write_staged_logical_segment(&path, &[]).await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err((canonical_start, path, err));
        }
        staged.push(StagedLogicalSegment {
            index: 0,
            range: CanonicalByteRange::new(canonical_start, canonical_start),
            path,
        });
    }
    Ok(staged)
}

async fn install_staged_logical_segments(
    final_path: &Path,
    staged: Vec<StagedLogicalSegment>,
    retained_bytes: u64,
) -> Result<Vec<LogicalArtifactSegment>, (u64, PathBuf, std::io::Error)> {
    let mut installed: Vec<LogicalArtifactSegment> = Vec::new();
    for segment in staged {
        if segment.range.start >= retained_bytes && !segment.range.is_empty() {
            let _ = tokio::fs::remove_file(&segment.path).await;
            continue;
        }
        let end = segment.range.end.min(retained_bytes);
        if end < segment.range.end {
            match tokio::fs::OpenOptions::new()
                .write(true)
                .open(&segment.path)
                .await
            {
                Ok(file) => {
                    if let Err(err) = file.set_len(end.saturating_sub(segment.range.start)).await {
                        for installed_segment in &installed {
                            let _ = tokio::fs::remove_file(logical_segment_path(
                                final_path,
                                installed_segment.index,
                            ))
                            .await;
                        }
                        return Err((segment.range.start, segment.path, err));
                    }
                    if let Err(err) = file.sync_all().await {
                        for installed_segment in &installed {
                            let _ = tokio::fs::remove_file(logical_segment_path(
                                final_path,
                                installed_segment.index,
                            ))
                            .await;
                        }
                        return Err((segment.range.start, segment.path, err));
                    }
                }
                Err(err) => {
                    for installed_segment in &installed {
                        let _ = tokio::fs::remove_file(logical_segment_path(
                            final_path,
                            installed_segment.index,
                        ))
                        .await;
                    }
                    return Err((segment.range.start, segment.path, err));
                }
            }
        }
        let destination = logical_segment_path(final_path, segment.index);
        if let Err(err) = tokio::fs::rename(&segment.path, &destination).await {
            for installed_segment in &installed {
                let _ = tokio::fs::remove_file(logical_segment_path(
                    final_path,
                    installed_segment.index,
                ))
                .await;
            }
            return Err((segment.range.start, segment.path, err));
        }
        installed.push(LogicalArtifactSegment {
            index: segment.index,
            range: CanonicalByteRange::new(segment.range.start, end),
        });
    }
    if let Err(err) = sync_parent_directory(final_path) {
        for installed_segment in &installed {
            let _ =
                tokio::fs::remove_file(logical_segment_path(final_path, installed_segment.index))
                    .await;
        }
        return Err((retained_bytes, final_path.to_path_buf(), err));
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

fn canonical_line_starts(bytes: &[u8]) -> Vec<u64> {
    let mut starts = vec![0];
    starts.extend(
        bytes
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index as u64 + 1))
            .filter(|offset| *offset < bytes.len() as u64),
    );
    starts
}

#[cfg(test)]
fn response_fits_recovery_ceiling(value: &impl Serialize) -> bool {
    response_fits_recovery_token_ceiling(value, RECOVERY_AGGREGATE_TOKEN_CEILING)
}

fn response_fits_recovery_token_ceiling(value: &impl Serialize, token_ceiling: usize) -> bool {
    serde_json::to_string(value)
        .is_ok_and(|rendered| approx_token_count(&rendered) <= token_ceiling)
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

fn largest_fitting_byte_chunk(
    artifact_id: &str,
    canonical_bytes: u64,
    range: CanonicalByteRange,
    token_ceiling: usize,
) -> u64 {
    if range.is_empty() {
        return 0;
    }
    let fits = |candidate_bytes: u64| {
        let candidate_range = CanonicalByteRange::new(range.start, range.start + candidate_bytes);
        let result = ReadToolOutputResult {
            artifact_id: artifact_id.to_string(),
            canonical_sha256: "0".repeat(64),
            canonical_bytes,
            retained_bytes: canonical_bytes,
            complete: true,
            unavailable_ranges: Vec::new(),
            results: vec![successful_byte_selector_result(
                candidate_range,
                &vec![0; candidate_bytes as usize],
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
                return candidate;
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
    best
}

fn populate_recovery_subdivisions(metadata: &mut LogicalArtifactMetadata) {
    for pointer in metadata.json_pointers.values_mut() {
        pointer.recovery_chunk_bytes = Some(largest_fitting_byte_chunk(
            &metadata.artifact_id,
            metadata.canonical_bytes,
            pointer.range,
            RECOVERY_AGGREGATE_TOKEN_CEILING,
        ));
    }
    for section in &mut metadata.sections {
        if let Some(range) = section.canonical_range {
            section.recovery_chunk_bytes = Some(largest_fitting_byte_chunk(
                &metadata.artifact_id,
                metadata.canonical_bytes,
                range,
                RECOVERY_AGGREGATE_TOKEN_CEILING,
            ));
        }
    }
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

async fn create_canonical_output_artifact_with_id(
    codex_home: &Path,
    thread_id: &str,
    canonical: &CanonicalToolResult,
    id: ToolOutputArtifactId,
) -> CanonicalOutputArtifact {
    let directory = codex_home.join("tool-output").join(thread_id);
    if let Err(err) = tokio::fs::create_dir_all(&directory).await {
        return CanonicalOutputArtifact {
            id: None,
            retained_bytes: 0,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
            error: Some(format!("failed to create `{}`: {err}", directory.display())),
        };
    }
    let max_staged_bytes = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL)
        .min(canonical.exact_bytes) as usize;
    let staged_bytes = &canonical.bytes[..canonical.bytes.len().min(max_staged_bytes)];
    let staged_segments = match stage_logical_segments(&directory, id, staged_bytes, 0, 0).await {
        Ok(segments) => segments,
        Err((start, staged_path, err)) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: start,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                error: Some(format!(
                    "failed to stage `{}`: {err}",
                    staged_path.display()
                )),
            };
        }
    };
    let _staged_cleanup = StagedLogicalSegmentCleanup::new(&staged_segments);

    // Bulk output writes use per-artifact staging paths. Hold the process-wide
    // permit only while admitting the retained bytes and installing the staged
    // family, so independent tool completions can persist their output in
    // parallel without racing retention accounting.
    let _retention_permit = retention_sweep_permit().await;
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
    enforce_retention_locked(&directory, &path, canonical.exact_bytes, 1).await;
    let usage = retention_usage_locked(&directory).await;
    let available = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .saturating_sub(usage.thread_bytes)
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(usage.global_bytes));
    let retained_bytes = canonical
        .exact_bytes
        .min(available)
        .min(staged_bytes.len() as u64);
    let retention_token = capture_retention_token(&directory);
    let complete = canonical.complete && retained_bytes == canonical.exact_bytes;
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
        match install_staged_logical_segments(&path, staged_segments, retained_bytes).await {
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

    let mut metadata = LogicalArtifactMetadata {
        version: LOGICAL_ARTIFACT_METADATA_VERSION,
        artifact_id: id.to_string(),
        canonical_kind: canonical.kind,
        canonical_sha256: canonical.sha256.clone(),
        retained_sha256: Some(format!("{:x}", Sha256::digest(retained))),
        canonical_bytes: canonical.exact_bytes,
        retained_bytes,
        complete,
        unavailable_ranges: unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
        line_starts: canonical_line_starts(retained),
        segments,
    };
    populate_recovery_subdivisions(&mut metadata);
    let metadata_bytes = match serde_json::to_vec(&metadata) {
        Ok(bytes) => bytes,
        Err(err) => {
            rollback_logical_artifact_creation(&retention_token, &path);
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes,
                complete: false,
                unavailable_ranges,
                error: Some(format!("failed to serialize artifact metadata: {err}")),
            };
        }
    };
    if let Err(err) = write_bytes_atomically(&logical_metadata_path(&path), &metadata_bytes) {
        rollback_logical_artifact_creation(&retention_token, &path);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes,
            complete: false,
            unavailable_ranges,
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
    match artifact_retention_record(&path).await {
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
    let id = match artifact_id.parse::<ToolOutputArtifactId>() {
        Ok(id) if id.to_string() == artifact_id => id,
        _ => {
            return CanonicalOutputArtifact {
                id: None,
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some("existing artifact ID is invalid".to_string()),
            };
        }
    };
    let directory = codex_home.join("tool-output").join(thread_id);
    let path = directory.join(format!("{id}.log"));
    let existing = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return CanonicalOutputArtifact {
                id: Some(id),
                retained_bytes: 0,
                complete: false,
                unavailable_ranges: vec![CanonicalByteRange::new(0, canonical.exact_bytes)],
                error: Some(format!("failed to read existing artifact: {err}")),
            };
        }
    };
    let expected_prefix =
        &canonical.bytes[..canonical.bytes.len().min(MAX_RAW_OUTPUT_ARTIFACT_BYTES)];
    if existing != expected_prefix {
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes: existing.len() as u64,
            complete: false,
            unavailable_ranges: vec![CanonicalByteRange::new(
                existing.len() as u64,
                canonical.exact_bytes,
            )],
            error: Some("existing artifact does not match the canonical byte prefix".to_string()),
        };
    }
    let max_staged_total = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL)
        .min(canonical.exact_bytes) as usize;
    let staged_end = canonical.bytes.len().min(max_staged_total);
    let staged_additional = canonical
        .bytes
        .get(existing.len()..staged_end)
        .unwrap_or_default();
    let staged_segments =
        match stage_logical_segments(&directory, id, staged_additional, existing.len() as u64, 1)
            .await
        {
            Ok(segments) => segments,
            Err((start, staged_path, err)) => {
                return CanonicalOutputArtifact {
                    id: Some(id),
                    retained_bytes: start,
                    complete: false,
                    unavailable_ranges: vec![CanonicalByteRange::new(start, canonical.exact_bytes)],
                    error: Some(format!(
                        "failed to stage artifact segment `{}`: {err}",
                        staged_path.display()
                    )),
                };
            }
        };
    let _staged_cleanup = StagedLogicalSegmentCleanup::new(&staged_segments);
    let _retention_permit = retention_sweep_permit().await;
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
    enforce_retention_locked(
        &directory,
        &path,
        canonical.exact_bytes.saturating_sub(existing.len() as u64),
        0,
    )
    .await;
    let usage = retention_usage_locked(&directory).await;
    let additional_available = MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        .saturating_sub(usage.thread_bytes)
        .min(MAX_RETAINED_ARTIFACT_BYTES_TOTAL.saturating_sub(usage.global_bytes));
    let retained_bytes = canonical
        .exact_bytes
        .min(existing.len() as u64 + additional_available)
        .min(existing.len() as u64 + staged_additional.len() as u64);
    let retention_token = capture_retention_token(&directory);
    let retained = &canonical.bytes[..retained_bytes as usize];
    let mut segments = vec![LogicalArtifactSegment {
        index: 0,
        range: CanonicalByteRange::new(0, existing.len() as u64),
    }];
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
        match install_staged_logical_segments(&path, staged_segments, retained_bytes).await {
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
    let complete = canonical.complete && retained_bytes == canonical.exact_bytes;
    let unavailable_ranges = normalized_unavailable_ranges(
        canonical.exact_bytes,
        &canonical.unavailable_ranges,
        retained_bytes,
    );
    let mut metadata = LogicalArtifactMetadata {
        version: LOGICAL_ARTIFACT_METADATA_VERSION,
        artifact_id: id.to_string(),
        canonical_kind: canonical.kind,
        canonical_sha256: canonical.sha256.clone(),
        retained_sha256: Some(format!(
            "{:x}",
            Sha256::digest(&canonical.bytes[..retained_bytes as usize])
        )),
        canonical_bytes: canonical.exact_bytes,
        retained_bytes,
        complete,
        unavailable_ranges: unavailable_ranges.clone(),
        json_pointers: canonical.json_pointers.clone(),
        sections: canonical.sections.clone(),
        line_starts: canonical_line_starts(retained),
        segments,
    };
    populate_recovery_subdivisions(&mut metadata);
    let write = serde_json::to_vec(&metadata)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_bytes_atomically(&logical_metadata_path(&path), &bytes));
    if let Err(err) = write {
        let _ = reconcile_logical_artifact_transaction(&path);
        reject_stale_delta(&retention_token);
        return CanonicalOutputArtifact {
            id: Some(id),
            retained_bytes,
            complete: false,
            unavailable_ranges,
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
    match artifact_retention_record(&path).await {
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
        let metadata = load_logical_metadata(&source_path, source_id).map_err(|err| {
            format!(
                "failed to load source artifact metadata: {}",
                err.for_model()
            )
        })?;
        if !metadata.complete
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
        if format!("{:x}", Sha256::digest(&bytes)) != expected_sha256_for_check {
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
        let target_metadata = load_logical_metadata(&target_path_for_check, source_id)
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
        if format!("{:x}", Sha256::digest(&target_bytes)) != expected_sha256_for_target {
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
    let target_retention_token =
        capture_retention_token(target_path.parent().unwrap_or_else(|| Path::new(".")));
    if !reminted.complete
        || reminted.retained_bytes != expected_bytes
        || !reminted.unavailable_ranges.is_empty()
    {
        rollback_logical_artifact_creation(&target_retention_token, &target_path);
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
        let _ = std::fs::remove_file(active_tool_history_protection_path(&target_path));
        rollback_logical_artifact_creation(&target_retention_token, &target_path);
        return Err(format!("failed to protect reminted artifact: {err}"));
    }
    Ok(reminted_id)
}

fn rollback_logical_artifact_creation(token: &RetentionIndexToken, path: &Path) {
    match remove_logical_artifact_files(path) {
        Ok(()) => publish_known_remove(token, path, LogicalRetentionMutation::Cleanup, false),
        Err(_) => reject_stale_delta(token),
    }
}

fn remove_logical_artifact_files(path: &Path) -> std::io::Result<()> {
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

pub(crate) async fn create_evidence_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
) -> Result<PendingEvidenceArtifact, String> {
    if output.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES {
        return Err(format!(
            "evidence output exceeds the {MAX_RAW_OUTPUT_ARTIFACT_BYTES} byte artifact safety limit"
        ));
    }
    create_evidence_output_artifact_inner(codex_home, thread_id, output, None).await
}

async fn create_evidence_output_artifact_inner(
    codex_home: &Path,
    thread_id: &str,
    output: &[u8],
    pre_marker_barrier: Option<&tokio::sync::Barrier>,
) -> Result<PendingEvidenceArtifact, String> {
    if output.len() > MAX_RAW_OUTPUT_ARTIFACT_BYTES {
        return Err(format!(
            "evidence output exceeds the {MAX_RAW_OUTPUT_ARTIFACT_BYTES} byte artifact safety limit"
        ));
    }
    let directory = codex_home.join("tool-output").join(thread_id);
    let retention_permit = retention_sweep_permit().await;
    let initial_directory = directory.clone();
    let _initial_retention_token = run_blocking_artifact_io(move || {
        std::fs::create_dir_all(&initial_directory)?;
        Ok(capture_retention_token(&initial_directory))
    })
    .await
    .map_err(|err| format!("failed to create `{}`: {err}", directory.display()))?;
    // Make room before rejecting the reservation. Expired and inactive ordinary command output
    // should not cause durable evidence creation to fail spuriously.
    enforce_retention_locked(&directory, Path::new(""), output.len() as u64, 1).await;
    let usage = retention_usage_locked(&directory).await;
    if usage.thread_bytes.saturating_add(output.len() as u64)
        > MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
        || usage.global_bytes.saturating_add(output.len() as u64)
            > MAX_RETAINED_ARTIFACT_BYTES_TOTAL
    {
        return Err("evidence artifact retention budget is exhausted".to_string());
    }
    // The reservation above may have installed a replacement generation. Artifact creation starts
    // against the post-reservation state so its eventual durable record can never update the
    // generation that preceded reconciliation.
    let token_directory = directory.clone();
    let retention_token = run_blocking_artifact_io(move || {
        Ok::<_, std::io::Error>(capture_retention_token(&token_directory))
    })
    .await
    .map_err(|err| format!("failed to inspect artifact retention state: {err}"))?;

    let id = ToolOutputArtifactId::new();
    let path = directory.join(format!("{id}.log"));
    let create_path = path.clone();
    let file = match run_blocking_artifact_io(move || {
        std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&create_path)
    })
    .await
    {
        Ok(file) => file,
        Err(err) => {
            reject_stale_delta(&retention_token);
            return Err(format!("failed to create `{}`: {err}", path.display()));
        }
    };
    let cleanup = PendingEvidenceArtifactCleanup::new(path.clone(), retention_token.clone());
    let lock_path = path.clone();
    let file = run_blocking_artifact_io(move || {
        file.try_lock()?;
        Ok(file)
    })
    .await
    .map_err(|err| {
        format!(
            "failed to lock `{}` for creation: {err}",
            lock_path.display()
        )
    })?;
    let mut file = tokio::fs::File::from_std(file);
    file.write_all(output)
        .await
        .map_err(|err| format!("failed to write `{}`: {err}", path.display()))?;
    file.flush()
        .await
        .map_err(|err| format!("failed to flush `{}`: {err}", path.display()))?;
    file.sync_all()
        .await
        .map_err(|err| format!("failed to sync `{}`: {err}", path.display()))?;

    #[cfg(test)]
    if let Some(barrier) = pre_marker_barrier {
        barrier.wait().await;
        barrier.wait().await;
    }
    #[cfg(not(test))]
    let _ = pre_marker_barrier;

    let marker = evidence_protection_path(&path);
    let marker_path = marker.clone();
    run_blocking_artifact_io(move || create_new_evidence_protection_marker(&marker_path))
        .await
        .map_err(|err| format!("failed to protect `{}` as evidence: {err}", path.display()))?;
    let sync_path = path.clone();
    run_blocking_artifact_io(move || sync_parent_directory(&sync_path))
        .await
        .map_err(|err| format!("failed to sync evidence directory: {err}"))?;

    let record = artifact_retention_record(&path)
        .await
        .map_err(|err| format!("failed to index evidence artifact: {err}"))?
        .ok_or_else(|| "evidence artifact disappeared before indexing".to_string())?;
    publish_known_record(&retention_token, record, LogicalRetentionMutation::Create);

    let file = file.into_std().await;
    let _ = file.unlock();
    let handle = Arc::new(file);
    Ok(PendingEvidenceArtifact {
        id,
        path,
        bytes: output.len() as u64,
        handle,
        cleanup,
        retention_permit: Some(retention_permit),
    })
}

pub(crate) struct PendingEvidenceArtifact {
    id: ToolOutputArtifactId,
    path: PathBuf,
    bytes: u64,
    handle: Arc<File>,
    cleanup: PendingEvidenceArtifactCleanup,
    retention_permit: Option<RetentionSweepPermit>,
}

impl PendingEvidenceArtifact {
    pub(crate) fn id(&self) -> ToolOutputArtifactId {
        self.id
    }

    pub(crate) fn mark_durable(mut self) -> RawOutputArtifact {
        self.cleanup.disarm();
        drop(self.retention_permit.take());
        RawOutputArtifact::Stored {
            id: self.id,
            path: self.path.clone(),
            bytes: self.bytes,
            truncated: false,
            handle: self.handle.clone(),
        }
    }
}

struct PendingEvidenceArtifactCleanup {
    path: PathBuf,
    retention_token: RetentionIndexToken,
    armed: bool,
}

impl PendingEvidenceArtifactCleanup {
    fn new(path: PathBuf, retention_token: RetentionIndexToken) -> Self {
        Self {
            path,
            retention_token,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingEvidenceArtifactCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let log_removed = match std::fs::remove_file(&self.path) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        let marker_removed = match std::fs::remove_file(evidence_protection_path(&self.path)) {
            Ok(()) => true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if log_removed && marker_removed {
            publish_known_remove(
                &self.retention_token,
                &self.path,
                LogicalRetentionMutation::Cleanup,
                false,
            );
        } else {
            reject_stale_delta(&self.retention_token);
        }
    }
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

fn create_new_evidence_protection_marker(marker: &Path) -> std::io::Result<()> {
    create_new_protection_marker(marker, EVIDENCE_PROTECTION_MARKER_BYTES)
}

pub(crate) async fn delete_evidence_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
) -> std::io::Result<()> {
    let id = artifact_id.parse::<ToolOutputArtifactId>().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid artifact id")
    })?;
    if id.to_string() != artifact_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-canonical artifact id",
        ));
    }
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    let marker = evidence_protection_path(&path);
    let retention_token = capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));
    let _retention_permit = retention_sweep_permit().await;
    if let Err(err) = sync_parent_directory(&marker) {
        reject_stale_delta(&retention_token);
        return Err(err);
    }
    match tokio::fs::remove_file(&marker).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            reject_stale_delta(&retention_token);
            return Err(err);
        }
    }
    if let Err(err) = sync_parent_directory(&marker) {
        reject_stale_delta(&retention_token);
        return Err(err);
    }
    match remove_inactive_output_path(path.clone()).await {
        InactiveRemovalOutcome::RemovedOrAbsent => {
            if let Err(err) = sync_parent_directory(&path) {
                reject_stale_delta(&retention_token);
                return Err(err);
            }
            publish_known_remove(
                &retention_token,
                &path,
                LogicalRetentionMutation::Delete,
                false,
            );
            Ok(())
        }
        InactiveRemovalOutcome::Active => {
            match artifact_retention_record(&path).await {
                Ok(Some(record)) => publish_known_record(
                    &retention_token,
                    record,
                    LogicalRetentionMutation::Protection,
                ),
                Ok(None) | Err(_) => reject_stale_delta(&retention_token),
            }
            Err(std::io::Error::other(
                "evidence artifact is still active and could not be deleted",
            ))
        }
        InactiveRemovalOutcome::Ambiguous(err) => {
            reject_stale_delta(&retention_token);
            Err(err)
        }
    }
}

pub(crate) async fn reconcile_evidence_artifact_protection(
    codex_home: &Path,
    thread_id: &str,
    referenced_artifact_ids: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let directory = codex_home.join("tool-output").join(thread_id);
    let retention_token = capture_retention_token(&directory);
    let _retention_permit = retention_sweep_permit().await;
    let mut live_artifact_ids = std::collections::BTreeSet::new();
    for artifact_id in referenced_artifact_ids {
        let Ok(id) = artifact_id.parse::<ToolOutputArtifactId>() else {
            continue;
        };
        if id.to_string() != *artifact_id {
            continue;
        }
        let path = directory.join(format!("{id}.log"));
        let marker = evidence_protection_path(&path);
        let marker_is_valid = tokio::task::spawn_blocking(move || {
            let Ok((_log_file, _)) = open_regular_artifact(&path) else {
                return false;
            };
            match std::fs::symlink_metadata(&marker) {
                Ok(_) => evidence_protection_marker_is_valid(&marker),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    create_new_evidence_protection_marker(&marker).is_ok()
                }
                Err(_) => false,
            }
        })
        .await
        .unwrap_or(false);
        if marker_is_valid {
            live_artifact_ids.insert(artifact_id.clone());
        }
    }

    let Ok(mut entries) = tokio::fs::read_dir(&directory).await else {
        reject_stale_delta(&retention_token);
        return live_artifact_ids;
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => {
                reject_stale_delta(&retention_token);
                return live_artifact_ids;
            }
        };
        let marker = entry.path();
        if marker.extension().and_then(|extension| extension.to_str())
            != Some(EVIDENCE_PROTECTION_EXTENSION)
        {
            continue;
        }
        let Some(artifact_id) = marker.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if live_artifact_ids.contains(artifact_id) {
            continue;
        }
        let path = directory.join(format!("{artifact_id}.log"));
        match tokio::fs::remove_file(marker).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => reject_stale_delta(&retention_token),
        }
        if matches!(
            remove_inactive_output_path(path).await,
            InactiveRemovalOutcome::Ambiguous(_)
        ) {
            reject_stale_delta(&retention_token);
        }
    }
    if sync_parent_directory(&directory.join("retention-sync")).is_err() {
        reject_stale_delta(&retention_token);
        return live_artifact_ids;
    }
    let installed_mode = reconcile_retention_root(&retention_token.root).await;
    note_completed_evidence_reconciliation(&retention_token.root, installed_mode);
    live_artifact_ids
}

/// Ensures an exact artifact referenced by an active completed-tool receipt is
/// protected independently from task-evidence ownership. The marker is only
/// created after thread confinement, regular-file, byte-count, and digest
/// checks all succeed.
pub(crate) async fn protect_active_tool_history_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    let id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| "invalid tool-output artifact id".to_string())?;
    if id.to_string() != artifact_id {
        return Err("non-canonical tool-output artifact id".to_string());
    }
    let directory = codex_home.join("tool-output").join(thread_id);
    let path = directory.join(format!("{id}.log"));
    let marker = active_tool_history_protection_path(&path);
    let retention_token = capture_retention_token(&directory);
    let _retention_permit = retention_sweep_permit().await;
    let expected_sha256 = expected_sha256.to_string();
    let path_for_check = path.clone();
    let marker_for_check = marker.clone();
    let protection_result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let (mut file, artifact_bytes) = open_regular_artifact(&path_for_check)
            .map_err(|err| format!("artifact is not retrievable: {}", err.for_model()))?;
        if artifact_bytes != expected_bytes {
            return Err("artifact byte count does not match receipt metadata".to_string());
        }
        let capacity = usize::try_from(expected_bytes)
            .map_err(|_| "artifact is too large to verify".to_string())?;
        let mut bytes = Vec::with_capacity(capacity);
        file.read_to_end(&mut bytes)
            .map_err(|err| format!("failed to verify artifact: {err}"))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != expected_sha256 {
            return Err("artifact digest does not match receipt metadata".to_string());
        }
        match std::fs::symlink_metadata(&marker_for_check) {
            Ok(_) => {
                if !protection_marker_status(
                    &marker_for_check,
                    ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
                )
                .unwrap_or(false)
                {
                    return Err("active tool-history protection marker is invalid".to_string());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                create_new_protection_marker(
                    &marker_for_check,
                    ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
                )
                .map_err(|err| format!("failed to protect artifact: {err}"))?;
            }
            Err(err) => return Err(format!("failed to inspect artifact protection: {err}")),
        }
        Ok(())
    })
    .await
    .map_err(|err| format!("artifact verification task failed: {err}"))
    .and_then(std::convert::identity);
    if let Err(err) = protection_result {
        reject_stale_delta(&retention_token);
        return Err(err);
    }
    if let Err(err) = sync_parent_directory(&marker) {
        reject_stale_delta(&retention_token);
        return Err(format!(
            "failed to sync active tool-history protection: {err}"
        ));
    }
    let record = match artifact_retention_record(&path).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            reject_stale_delta(&retention_token);
            return Err("artifact disappeared before protection was indexed".to_string());
        }
        Err(err) => {
            reject_stale_delta(&retention_token);
            return Err(format!("failed to index protected artifact: {err}"));
        }
    };
    publish_known_record(
        &retention_token,
        record,
        LogicalRetentionMutation::Protection,
    );
    Ok(())
}

/// Reconciles the `active_tool_history` owner after resume, fork, compaction,
/// or history replacement. Evidence protection remains untouched.
pub(crate) async fn reconcile_active_tool_history_artifact_protection(
    codex_home: &Path,
    thread_id: &str,
    referenced_artifacts: &BTreeMap<String, (u64, String)>,
) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    for (artifact_id, (expected_bytes, expected_sha256)) in referenced_artifacts {
        if protect_active_tool_history_artifact(
            codex_home,
            thread_id,
            artifact_id,
            *expected_bytes,
            expected_sha256,
        )
        .await
        .is_ok()
        {
            live.insert(artifact_id.clone());
        }
    }

    let directory = codex_home.join("tool-output").join(thread_id);
    let retention_token = capture_retention_token(&directory);
    let _retention_permit = retention_sweep_permit().await;
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(_) => {
            reject_stale_delta(&retention_token);
            return live;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
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
        match tokio::fs::remove_file(&marker).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => reject_stale_delta(&retention_token),
        }
    }
    if sync_parent_directory(&directory.join("retention-sync")).is_err() {
        reject_stale_delta(&retention_token);
        return live;
    }
    let installed_mode = reconcile_retention_root(&retention_token.root).await;
    note_completed_evidence_reconciliation(&retention_token.root, installed_mode);
    live
}

fn evidence_protection_marker_is_valid(marker: &Path) -> bool {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    configure_no_follow_open(&mut options);
    let Ok(mut file) = options.open(marker) else {
        return false;
    };
    let Ok(metadata) = file.metadata() else {
        return false;
    };
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return false;
    }
    if metadata.len() != EVIDENCE_PROTECTION_MARKER_BYTES.len() as u64 {
        return false;
    }
    let mut bytes = [0_u8; EVIDENCE_PROTECTION_MARKER_BYTES.len()];
    if file.read_exact(&mut bytes).is_err() || bytes != *EVIDENCE_PROTECTION_MARKER_BYTES {
        return false;
    }
    let mut trailing = [0_u8; 1];
    matches!(file.read(&mut trailing), Ok(0))
}

pub(crate) async fn append_raw_output_artifact(
    artifact: &RawOutputArtifact,
    output: &[u8],
) -> RawOutputArtifact {
    let RawOutputArtifact::Stored {
        id,
        path,
        bytes,
        truncated,
        handle,
    } = artifact
    else {
        return artifact.clone();
    };
    let retention_token = capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));

    match lock_artifact_handle(handle, SeekFrom::End(0)) {
        Ok(file) => {
            let file = tokio::fs::File::from_std(file);
            let mut file = match lock_output_file(file).await {
                Ok(file) => file,
                Err(err) => {
                    return failed_with_owned_path(
                        path.clone(),
                        *bytes,
                        format!("failed to lock `{}` for append: {err}", path.display()),
                        Some(&retention_token),
                    )
                    .await;
                }
            };
            let remaining = MAX_RAW_OUTPUT_ARTIFACT_BYTES.saturating_sub(*bytes as usize);
            let retained = &output[..output.len().min(remaining)];
            let truncated = *truncated || retained.len() != output.len();
            if let Err(err) = file.write_all(retained).await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    *bytes,
                    format!("failed to append `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            if let Err(err) = file.flush().await {
                drop(file);
                return failed_with_owned_path(
                    path.clone(),
                    (*bytes).saturating_add(retained.len() as u64),
                    format!("failed to flush `{}`: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            let metadata = file.metadata().await;
            if let Err(err) = unlock_output_file(file).await {
                return failed_with_owned_path(
                    path.clone(),
                    (*bytes).saturating_add(retained.len() as u64),
                    format!("failed to unlock `{}` after append: {err}", path.display()),
                    Some(&retention_token),
                )
                .await;
            }
            match metadata {
                Ok(metadata) => {
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
                        bytes: metadata.len(),
                        truncated,
                        handle: handle.clone(),
                    }
                }
                Err(err) => {
                    failed_with_owned_path(
                        path.clone(),
                        (*bytes).saturating_add(retained.len() as u64),
                        format!("failed to stat `{}` after append: {err}", path.display()),
                        Some(&retention_token),
                    )
                    .await
                }
            }
        }
        Err(err) => {
            failed_with_owned_path(
                path.clone(),
                *bytes,
                format!("failed to open `{}` for append: {err}", path.display()),
                Some(&retention_token),
            )
            .await
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
    let retention_token = capture_retention_token(path.parent().unwrap_or_else(|| Path::new(".")));

    match lock_artifact_handle(handle, SeekFrom::Start(0)) {
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
}

impl ReadToolOutputError {
    pub(crate) fn for_model(&self) -> String {
        match self {
            Self::InvalidArtifactId => "artifact_id must be a UUID".to_string(),
            Self::InvalidRange(message) | Self::Io(message) => message.clone(),
            Self::Expired => ARTIFACT_EXPIRED_MESSAGE.to_string(),
            Self::StillWriting => ARTIFACT_WRITING_MESSAGE.to_string(),
        }
    }
}

fn load_logical_metadata(
    path: &Path,
    id: ToolOutputArtifactId,
) -> Result<LogicalArtifactMetadata, ReadToolOutputError> {
    match std::fs::symlink_metadata(logical_transaction_path(path)) {
        Ok(_) => return Err(ReadToolOutputError::StillWriting),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(ReadToolOutputError::Io(format!(
                "failed to inspect artifact transaction: {err}"
            )));
        }
    }
    let metadata_path = logical_metadata_path(path);
    match std::fs::read(&metadata_path) {
        Ok(bytes) => {
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
            Ok(metadata)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let (_, bytes) = open_regular_artifact(path)?;
            let contents = std::fs::read(path).map_err(|err| {
                ReadToolOutputError::Io(format!("failed to read artifact: {err}"))
            })?;
            Ok(LogicalArtifactMetadata {
                version: LOGICAL_ARTIFACT_METADATA_VERSION,
                artifact_id: id.to_string(),
                canonical_kind: CanonicalToolResultKind::Bytes,
                canonical_sha256: format!("{:x}", Sha256::digest(&contents)),
                retained_sha256: Some(format!("{:x}", Sha256::digest(&contents))),
                canonical_bytes: bytes,
                retained_bytes: bytes,
                complete: true,
                unavailable_ranges: Vec::new(),
                json_pointers: BTreeMap::new(),
                sections: Vec::new(),
                line_starts: canonical_line_starts(&contents),
                segments: vec![LogicalArtifactSegment {
                    index: 0,
                    range: CanonicalByteRange::new(0, bytes),
                }],
            })
        }
        Err(err) => Err(ReadToolOutputError::Io(format!(
            "failed to read artifact metadata: {err}"
        ))),
    }
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
        let start = range.start.max(segment.range.start);
        let end = range.end.min(segment.range.end);
        if start >= end {
            continue;
        }
        let segment_path = logical_segment_path(path, segment.index);
        let (mut file, _) = open_regular_artifact(&segment_path)?;
        file.try_lock_shared()
            .map_err(|_| ReadToolOutputError::StillWriting)?;
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
    for segment in &metadata.segments {
        let segment_path = logical_segment_path(path, segment.index);
        let (_, actual_bytes) = open_regular_artifact(&segment_path)?;
        if actual_bytes != segment.range.len() {
            return Err(ReadToolOutputError::Io(
                "artifact segment size does not match metadata".to_string(),
            ));
        }
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
    if format!("{:x}", Sha256::digest(&snapshot)) != expected_sha256 {
        return Err(ReadToolOutputError::Io(
            "artifact SHA identity does not match metadata".to_string(),
        ));
    }
    if canonical_line_starts(&snapshot) != metadata.line_starts {
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

fn selector_range_and_children(
    selector: &ToolOutputSelector,
    metadata: &LogicalArtifactMetadata,
) -> Result<SelectorRangeAndChildren, ToolOutputSelectorStatus> {
    match selector {
        ToolOutputSelector::Bytes { start, end } => {
            if start > end || *end > metadata.retained_bytes {
                Err(ToolOutputSelectorStatus::Invalid)
            } else {
                Ok((
                    Some(CanonicalByteRange::new(*start, *end)),
                    Vec::new(),
                    None,
                ))
            }
        }
        ToolOutputSelector::Lines { start, end } => {
            if *start == 0 || end < start || *start > metadata.line_starts.len() {
                return Err(ToolOutputSelectorStatus::Invalid);
            }
            let range_start = metadata.line_starts[*start - 1];
            let range_end = metadata
                .line_starts
                .get(*end)
                .copied()
                .unwrap_or(metadata.retained_bytes);
            Ok((
                Some(CanonicalByteRange::new(range_start, range_end)),
                Vec::new(),
                None,
            ))
        }
        ToolOutputSelector::JsonPointer { pointer } => {
            let entry = metadata
                .json_pointers
                .get(pointer)
                .ok_or(ToolOutputSelectorStatus::NotFound)?;
            Ok((
                Some(entry.range),
                entry
                    .direct_children
                    .iter()
                    .map(|pointer| ToolOutputSelector::JsonPointer {
                        pointer: pointer.clone(),
                    })
                    .collect(),
                None,
            ))
        }
        ToolOutputSelector::Section { id } => {
            let section = metadata
                .sections
                .iter()
                .find(|section| section.id == *id)
                .ok_or(ToolOutputSelectorStatus::NotFound)?;
            let children = section
                .children
                .iter()
                .map(|id| ToolOutputSelector::Section { id: id.clone() })
                .collect::<Vec<_>>();
            if section.canonical_range.is_none() {
                let value = serde_json::json!({
                    "section_id": section.id,
                    "kind": "directory",
                    "children": children,
                });
                Ok((None, children, Some(value)))
            } else {
                Ok((section.canonical_range, children, section.value.clone()))
            }
        }
        ToolOutputSelector::Search { .. } => Err(ToolOutputSelectorStatus::Invalid),
    }
}

fn normalized_selector_order_key(
    selector: &ToolOutputSelector,
    metadata: &LogicalArtifactMetadata,
) -> (u64, u64, u8, String) {
    let serialized = serde_json::to_string(selector).unwrap_or_default();
    if let ToolOutputSelector::Search { start_byte, .. } = selector {
        return (*start_byte, *start_byte, 4, serialized);
    }
    match selector_range_and_children(selector, metadata) {
        Ok((Some(range), _, _)) => {
            let kind = match selector {
                ToolOutputSelector::Bytes { .. } => 0,
                ToolOutputSelector::Lines { .. } => 1,
                ToolOutputSelector::Section { .. } => 2,
                ToolOutputSelector::JsonPointer { .. } => 3,
                ToolOutputSelector::Search { .. } => unreachable!(),
            };
            (range.start, range.end, kind, serialized)
        }
        Ok((None, _, _)) => (u64::MAX - 1, u64::MAX - 1, 2, serialized),
        Err(_) => (u64::MAX, u64::MAX, u8::MAX, serialized),
    }
}

fn normalize_tool_output_selectors(
    selectors: Vec<ToolOutputSelector>,
    metadata: &LogicalArtifactMetadata,
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

    byte_ranges.sort_unstable();
    let mut merged_bytes = Vec::<(u64, u64)>::new();
    for (start, end) in byte_ranges {
        match merged_bytes.last_mut() {
            Some((_, previous_end)) if start <= *previous_end => {
                *previous_end = (*previous_end).max(end);
            }
            _ => merged_bytes.push((start, end)),
        }
    }
    let merged_lines = normalize_line_ranges(line_ranges);

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
    normalized.sort_by(|left, right| {
        normalized_selector_order_key(left, metadata)
            .cmp(&normalized_selector_order_key(right, metadata))
    });
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

fn invalid_selector_result(
    selector: ToolOutputSelector,
    message: impl Into<String>,
) -> ToolOutputSelectorResult {
    let mut result = ToolOutputSelectorResult::state(selector, ToolOutputSelectorStatus::Invalid);
    result.message = Some(message.into());
    result
}

fn search_logical_artifact(
    metadata: &LogicalArtifactMetadata,
    snapshot: &[u8],
    selector: ToolOutputSelector,
    token_ceiling: usize,
) -> ToolOutputSelectorResult {
    let (query, start_byte, max_results, context_lines) = match &selector {
        ToolOutputSelector::Search {
            query,
            start_byte,
            max_results,
            context_lines,
        } => (query.clone(), *start_byte, *max_results, *context_lines),
        _ => unreachable!("search helper requires a search selector"),
    };
    if query.is_empty() || query.len() > ARTIFACT_SEARCH_MAX_QUERY_BYTES {
        return invalid_selector_result(
            selector,
            format!("search query must contain 1..={ARTIFACT_SEARCH_MAX_QUERY_BYTES} UTF-8 bytes"),
        );
    }
    if max_results == 0 || max_results > ARTIFACT_SEARCH_MAX_RESULTS {
        return invalid_selector_result(
            selector,
            format!("max_results must be between 1 and {ARTIFACT_SEARCH_MAX_RESULTS}"),
        );
    }
    if context_lines > ARTIFACT_SEARCH_MAX_CONTEXT_LINES {
        return invalid_selector_result(
            selector,
            format!("context_lines must not exceed {ARTIFACT_SEARCH_MAX_CONTEXT_LINES}"),
        );
    }
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
    let mut cursor = 0_usize;
    let mut total_matches = 0_usize;
    let mut indexed_matches = Vec::with_capacity(max_results);
    while cursor <= search_bytes.len().saturating_sub(query_bytes.len()) {
        let Some(relative_start) = search_bytes[cursor..]
            .windows(query_bytes.len())
            .position(|window| window == query_bytes)
        else {
            break;
        };
        let relative_match_start = cursor.saturating_add(relative_start);
        let relative_match_end = relative_match_start.saturating_add(query_bytes.len());
        let match_start = search_offset.saturating_add(relative_match_start);
        let match_end = search_offset.saturating_add(relative_match_end);
        total_matches = total_matches.saturating_add(1);
        if indexed_matches.len() < max_results {
            indexed_matches.push((match_start, match_end));
        }
        cursor = relative_match_end;
    }

    let build_result = |matches_returned: usize| {
        let mut matches = Vec::with_capacity(matches_returned);
        let mut child_selectors = Vec::<ToolOutputSelector>::new();
        for (match_start, match_end) in indexed_matches.iter().copied().take(matches_returned) {
            let line = metadata
                .line_starts
                .partition_point(|line_start| *line_start <= match_start as u64)
                .max(1);
            let end_line = metadata
                .line_starts
                .partition_point(|line_start| *line_start <= match_end.saturating_sub(1) as u64)
                .max(line);
            let context_start = line.saturating_sub(context_lines).max(1);
            let context_end = end_line
                .saturating_add(context_lines)
                .min(metadata.line_starts.len());
            matches.push(serde_json::json!({
                "line": line,
                "end_line": end_line,
                "start_byte": match_start,
                "end_byte": match_end,
            }));
            match child_selectors.last_mut() {
                Some(ToolOutputSelector::Lines { end, .. })
                    if context_start <= end.saturating_add(1) =>
                {
                    *end = (*end).max(context_end);
                }
                _ => child_selectors.push(ToolOutputSelector::Lines {
                    start: context_start,
                    end: context_end,
                }),
            }
        }

        let remaining_match_count = total_matches.saturating_sub(matches_returned);
        let continuation = (remaining_match_count > 0).then(|| ToolOutputSelector::Search {
            query: query.clone(),
            start_byte: indexed_matches
                .get(matches_returned.saturating_sub(1))
                .map(|(_, end)| *end as u64)
                .unwrap_or(start_byte),
            max_results,
            context_lines,
        });
        let hydrated_ranges = child_selectors
            .iter()
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
                if let Ok(text) = std::str::from_utf8(exact) {
                    hydrated["text"] = Value::String(text.to_string());
                } else {
                    hydrated["data_base64"] = Value::String(BASE64_STANDARD.encode(exact));
                }
                Some(hydrated)
            })
            .collect::<Vec<_>>();
        let mut result =
            ToolOutputSelectorResult::state(selector.clone(), ToolOutputSelectorStatus::Ok);
        result.complete = continuation.is_none();
        result.value = Some(serde_json::json!({
            "query": query,
            "start_byte": start_byte,
            "coverage_complete": true,
            "total_matches": total_matches,
            "matches_returned": matches_returned,
            "remaining_match_count": remaining_match_count,
            "matches": matches,
            "hydrated_ranges": hydrated_ranges,
        }));
        result.child_selectors = child_selectors;
        result.continuation = continuation;
        result.message = match remaining_match_count > 0 {
            true => Some(
                "more matches are available; exact context for this page is already hydrated and continuation advances to the next page"
                    .to_string(),
            ),
            false => None,
        };
        result
    };

    let mut result = build_result(0);
    for matches_returned in 1..=indexed_matches.len() {
        let candidate = build_result(matches_returned);
        let response = ReadToolOutputResult {
            artifact_id: metadata.artifact_id.clone(),
            canonical_sha256: metadata.canonical_sha256.clone(),
            canonical_bytes: metadata.canonical_bytes,
            retained_bytes: metadata.retained_bytes,
            complete: metadata.complete,
            unavailable_ranges: metadata.unavailable_ranges.clone(),
            results: vec![candidate.clone()],
        };
        if !response_fits_recovery_token_ceiling(&response, token_ceiling) {
            break;
        }
        result = candidate;
    }
    result
}

fn too_large_result(
    selector: ToolOutputSelector,
    range: CanonicalByteRange,
    child_selectors: Vec<ToolOutputSelector>,
    metadata: &LogicalArtifactMetadata,
    token_ceiling: usize,
) -> ToolOutputSelectorResult {
    let chunk_bytes = largest_fitting_byte_chunk(
        &metadata.artifact_id,
        metadata.canonical_bytes,
        range,
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
        subdivision_plan: Some(ByteSubdivisionPlan {
            range,
            chunk_bytes,
            chunk_count: range.len().div_ceil(chunk_bytes.max(1)),
            selector_kind: "bytes".to_string(),
        }),
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
    let mut bytes = Vec::with_capacity(usize::try_from(plan.range.len()).ok()?);
    let mut chunk_count = 0_u64;
    let mut start = plan.range.start;
    while start < plan.range.end {
        let end = start.saturating_add(plan.chunk_bytes).min(plan.range.end);
        if end <= start {
            return None;
        }
        bytes.extend_from_slice(
            validated_snapshot_range(snapshot, CanonicalByteRange::new(start, end)).ok()?,
        );
        start = end;
        chunk_count = chunk_count.saturating_add(1);
    }
    if chunk_count != plan.chunk_count || u64::try_from(bytes.len()).ok()? != plan.range.len() {
        return None;
    }
    let mut completed =
        successful_exact_selector_result(original.selector.clone(), plan.range, children, &bytes);
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
) -> ToolOutputSelectorResult {
    if matches!(&selector, ToolOutputSelector::Search { .. }) {
        return search_logical_artifact(metadata, snapshot, selector, final_token_ceiling);
    }
    let mut result = exact_selector_result(metadata, snapshot, selector.clone());
    let range = result.canonical_range;
    let children = result.child_selectors.clone();
    let individual = ReadToolOutputResult {
        artifact_id: metadata.artifact_id.clone(),
        canonical_sha256: metadata.canonical_sha256.clone(),
        canonical_bytes: metadata.canonical_bytes,
        retained_bytes: metadata.retained_bytes,
        complete: result.status == ToolOutputSelectorStatus::Ok && result.complete,
        unavailable_ranges: Vec::new(),
        results: vec![result.clone()],
    };
    if result.status == ToolOutputSelectorStatus::Ok
        && !response_fits_recovery_token_ceiling(&individual, fragment_token_ceiling)
        && let Some(range) = range
    {
        result = too_large_result(selector, range, children, metadata, fragment_token_ceiling);
    }
    result
}

#[cfg(test)]
pub(crate) async fn read_tool_output_selectors(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
) -> Result<ReadToolOutputResult, ReadToolOutputError> {
    read_tool_output_selectors_with_reuse(codex_home, thread_id, artifact_id, selectors)
        .await
        .map(|(result, _)| result)
}

pub(crate) async fn read_tool_output_selectors_with_reuse(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
) -> Result<(ReadToolOutputResult, bool), ReadToolOutputError> {
    read_tool_output_selectors_with_ceiling_and_reuse(
        codex_home,
        thread_id,
        artifact_id,
        selectors,
        RECOVERY_AGGREGATE_TOKEN_CEILING,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn read_tool_output_selectors_with_ceiling(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
) -> Result<ReadToolOutputResult, ReadToolOutputError> {
    read_tool_output_selectors_with_ceiling_and_reuse(
        codex_home,
        thread_id,
        artifact_id,
        selectors,
        token_ceiling,
    )
    .await
    .map(|(result, _)| result)
}

pub(crate) async fn read_tool_output_selectors_with_ceiling_and_reuse(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    selectors: Vec<ToolOutputSelector>,
    token_ceiling: usize,
) -> Result<(ReadToolOutputResult, bool), ReadToolOutputError> {
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
    let metadata_path = path.clone();
    let metadata = tokio::task::spawn_blocking(move || load_logical_metadata(&metadata_path, id))
        .await
        .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))??;
    let selectors = normalize_tool_output_selectors(selectors, &metadata);
    let validation_path = path.clone();
    let metadata_for_snapshot = metadata.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        open_regular_artifact(&validation_path)?;
        load_validated_logical_snapshot(&validation_path, &metadata_for_snapshot)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))??;
    let result = tokio::task::spawn_blocking(move || {
        let mut response = ReadToolOutputResult {
            artifact_id: id.to_string(),
            canonical_sha256: metadata.canonical_sha256.clone(),
            canonical_bytes: metadata.canonical_bytes,
            retained_bytes: metadata.retained_bytes,
            complete: false,
            unavailable_ranges: metadata.unavailable_ranges.clone(),
            results: Vec::with_capacity(selectors.len()),
        };
        for selector in &selectors {
            let mut selected = select_logical_artifact(
                &metadata,
                &snapshot,
                selector.clone(),
                token_ceiling.min(RECOVERY_FRAGMENT_TOKEN_CEILING),
                token_ceiling,
            );
            if selected.status == ToolOutputSelectorStatus::SelectorTooLarge
                && let Some(completed) =
                    internally_drain_exact_subdivisions(&metadata, &snapshot, &selected)
            {
                let mut candidate = response.clone();
                candidate.results.push(completed.clone());
                candidate.complete = candidate.results.iter().all(|result| {
                    result.status == ToolOutputSelectorStatus::Ok && result.complete
                });
                let mut individual_candidate = response.clone();
                individual_candidate.results.clear();
                individual_candidate.results.push(completed.clone());
                individual_candidate.complete = true;
                if response_fits_recovery_token_ceiling(&candidate, token_ceiling)
                    || response_fits_recovery_token_ceiling(&individual_candidate, token_ceiling)
                {
                    selected = completed;
                }
            }
            response.results.push(selected);
            response.complete = response.results.iter().all(|result| {
                result.status == ToolOutputSelectorStatus::Ok && result.complete
            });
            if response_fits_recovery_token_ceiling(&response, token_ceiling) {
                continue;
            }
            let Some(selected) = response.results.last_mut() else {
                unreachable!("the selected result was just appended");
            };
            if selected.status == ToolOutputSelectorStatus::Ok {
                let previous = selected.clone();
                let mut omitted = previous
                    .canonical_range
                    .map(|range| {
                        too_large_result(
                            previous.selector.clone(),
                            range,
                            previous.child_selectors.clone(),
                            &metadata,
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
                omitted.continuation = previous
                    .continuation
                    .or_else(|| omitted.child_selectors.first().cloned())
                    .or_else(|| Some(previous.selector.clone()));
                omitted.message = Some(
                    "exact value cannot fit this final transaction; retry only with the advertised continuation or deterministic child selectors"
                        .to_string(),
                );
                *selected = omitted;
            }
            response.complete = false;
            if !response_fits_recovery_token_ceiling(&response, token_ceiling) {
                return Err(ReadToolOutputError::InvalidRange(
                    "normalized selector manifest cannot fit a bounded typed-overflow response; use fewer selectors"
                        .to_string(),
                ));
            }
        }
        response.complete = response.results.iter().all(|result| {
            result.status == ToolOutputSelectorStatus::Ok && result.complete
        });
        Ok(response)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))??;
    Ok((result, false))
}

#[cfg(test)]
pub(crate) async fn read_tool_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<String, ReadToolOutputError> {
    let id = artifact_id
        .parse::<ToolOutputArtifactId>()
        .map_err(|_| ReadToolOutputError::InvalidArtifactId)?;
    if id.to_string() != artifact_id {
        return Err(ReadToolOutputError::InvalidArtifactId);
    }
    validate_read_range(start_line, end_line, max_bytes)?;
    let path = codex_home
        .join("tool-output")
        .join(thread_id)
        .join(format!("{id}.log"));
    tokio::task::spawn_blocking(move || {
        read_tool_output_artifact_blocking(&path, id, start_line, end_line, max_bytes)
    })
    .await
    .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?
}

/// Read the exact retained artifact bytes for crate-private deterministic
/// evidence replay. This keeps the same UUID, thread confinement, regular-file,
/// retention, and writer-lock protections as the model-visible line reader.
#[cfg(test)]
pub(crate) async fn read_exact_tool_output_artifact(
    codex_home: &Path,
    thread_id: &str,
    artifact_id: &str,
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
    tokio::task::spawn_blocking(move || read_exact_tool_output_artifact_blocking(&path))
        .await
        .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?
}

#[cfg(test)]
fn read_exact_tool_output_artifact_blocking(path: &Path) -> Result<Vec<u8>, ReadToolOutputError> {
    let (mut file, total_bytes) = open_regular_artifact(path)?;
    if total_bytes > MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64 {
        return Err(ReadToolOutputError::Io(
            "artifact exceeds the exact replay safety limit".to_string(),
        ));
    }
    file.try_lock_shared()
        .map_err(|_| ReadToolOutputError::StillWriting)?;
    let mut bytes = Vec::with_capacity(total_bytes as usize);
    let result = file
        .read_to_end(&mut bytes)
        .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")));
    let _ = file.unlock();
    result.map(|_| bytes)
}

#[cfg(test)]
fn validate_read_range(
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<(), ReadToolOutputError> {
    if start_line == 0 {
        return Err(ReadToolOutputError::InvalidRange(
            "start_line must be at least 1".to_string(),
        ));
    }
    if end_line < start_line {
        return Err(ReadToolOutputError::InvalidRange(
            "end_line must be greater than or equal to start_line".to_string(),
        ));
    }
    if end_line - start_line >= 2_000 {
        return Err(ReadToolOutputError::InvalidRange(
            "requested line span must not exceed 2000 lines".to_string(),
        ));
    }
    if max_bytes == 0 || max_bytes > 16_384 {
        return Err(ReadToolOutputError::InvalidRange(
            "max_bytes must be between 1 and 16384".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn read_tool_output_artifact_blocking(
    path: &Path,
    id: ToolOutputArtifactId,
    start_line: usize,
    end_line: usize,
    max_bytes: usize,
) -> Result<String, ReadToolOutputError> {
    let (mut file, total_bytes) = open_regular_artifact(path)?;
    file.try_lock_shared()
        .map_err(|_| ReadToolOutputError::StillWriting)?;

    let mut retained = Vec::with_capacity(max_bytes.min(16 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut current_line = 1_usize;
    let mut last_line = None;
    let mut clamped = false;
    let mut pending_cr = false;
    let mut done = false;

    while !done {
        let count = file
            .read(&mut buffer)
            .map_err(|err| ReadToolOutputError::Io(format!("failed to read artifact: {err}")))?;
        if count == 0 {
            if pending_cr && current_line >= start_line && current_line <= end_line {
                push_bounded(&mut retained, b'\r', max_bytes, &mut clamped);
                last_line = Some(current_line);
            }
            break;
        }

        for &byte in &buffer[..count] {
            if pending_cr {
                if byte == b'\n' {
                    if current_line >= start_line && current_line <= end_line {
                        push_bounded(&mut retained, b'\n', max_bytes, &mut clamped);
                        last_line = Some(current_line);
                    }
                    pending_cr = false;
                    if current_line == end_line {
                        done = true;
                        break;
                    }
                    current_line += 1;
                    continue;
                }
                if current_line >= start_line && current_line <= end_line {
                    push_bounded(&mut retained, b'\r', max_bytes, &mut clamped);
                    last_line = Some(current_line);
                }
                pending_cr = false;
            }

            if byte == b'\r' {
                pending_cr = true;
            } else if byte == b'\n' {
                if current_line >= start_line && current_line <= end_line {
                    push_bounded(&mut retained, b'\n', max_bytes, &mut clamped);
                    last_line = Some(current_line);
                }
                if current_line == end_line {
                    done = true;
                    break;
                }
                current_line += 1;
            } else if current_line >= start_line && current_line <= end_line {
                push_bounded(&mut retained, byte, max_bytes, &mut clamped);
                last_line = Some(current_line);
            }
        }
    }

    let (text, invalid_utf8) = match String::from_utf8(retained) {
        Ok(text) => (text, false),
        Err(err) => (String::from_utf8_lossy(err.as_bytes()).into_owned(), true),
    };
    let lines = match last_line {
        Some(last) => format!("{start_line}–{last}"),
        None => "none".to_string(),
    };
    let mut rendered =
        format!("artifact {id}, lines {lines}, {total_bytes} retained bytes\n{text}");
    if invalid_utf8 {
        rendered.push_str("\n[invalid UTF-8 replaced]");
    }
    if clamped {
        rendered.push_str("\n[range clamped at byte limit]");
    }
    Ok(rendered)
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

fn configure_no_follow_open(options: &mut std::fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

fn is_no_follow_error(_raw_os_error: i32) -> bool {
    false
}

fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(test)]
fn push_bounded(output: &mut Vec<u8>, byte: u8, max_bytes: usize, clamped: &mut bool) {
    if output.len() < max_bytes {
        output.push(byte);
    } else {
        *clamped = true;
    }
}

fn evidence_protection_path(artifact_path: &Path) -> PathBuf {
    artifact_path.with_extension(EVIDENCE_PROTECTION_EXTENSION)
}

fn active_tool_history_protection_path(artifact_path: &Path) -> PathBuf {
    artifact_path.with_extension(ACTIVE_TOOL_HISTORY_PROTECTION_EXTENSION)
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let result = {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)
            .and_then(|directory| directory.sync_all())
    };
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

async fn run_blocking_artifact_io<T, F>(operation: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(std::io::Error::other)?
}

async fn artifact_is_protected(artifact_path: &Path) -> std::io::Result<bool> {
    let evidence_marker = evidence_protection_path(artifact_path);
    let tool_history_marker = active_tool_history_protection_path(artifact_path);
    tokio::task::spawn_blocking(move || {
        let evidence =
            retention_protection_marker_status(&evidence_marker, EVIDENCE_PROTECTION_MARKER_BYTES)?;
        let tool_history = retention_protection_marker_status(
            &tool_history_marker,
            ACTIVE_TOOL_HISTORY_PROTECTION_MARKER_BYTES,
        )?;
        Ok::<_, std::io::Error>(evidence || tool_history)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
fn note_logical_mutation_diagnostics(
    diagnostics: &mut RetentionDiagnostics,
    mutation: LogicalRetentionMutation,
) {
    diagnostics.logical_mutations = diagnostics.logical_mutations.saturating_add(1);
    match mutation {
        LogicalRetentionMutation::Create => {
            diagnostics.creates = diagnostics.creates.saturating_add(1);
        }
        LogicalRetentionMutation::Delete | LogicalRetentionMutation::Cleanup => {
            diagnostics.deletes = diagnostics.deletes.saturating_add(1);
        }
        LogicalRetentionMutation::Protection | LogicalRetentionMutation::EvidenceReconcile => {
            diagnostics.protection_changes = diagnostics.protection_changes.saturating_add(1);
        }
        LogicalRetentionMutation::AppendReplace | LogicalRetentionMutation::StreamComplete => {}
    }
}

fn note_completed_evidence_reconciliation(root: &Path, installed_mode: RetentionModeKind) {
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(root) else {
        return;
    };
    record_retention_diagnostics! {
        note_logical_mutation_diagnostics(
            &mut state.diagnostics,
            LogicalRetentionMutation::EvidenceReconcile,
        );
    }
    if installed_mode != RetentionModeKind::Indexed
        && let RetentionRootMode::ScanOnly {
            operations_since_probe,
        } = &mut state.mode
    {
        *operations_since_probe = operations_since_probe.saturating_add(1);
        record_retention_diagnostics! {
            state.diagnostics.scan_only_operations =
                state.diagnostics.scan_only_operations.saturating_add(1);
        }
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
    #[cfg(not(test))]
    let _ = mutation;
    record_retention_diagnostics! {
        note_logical_mutation_diagnostics(&mut state.diagnostics, mutation);
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
    #[cfg(not(test))]
    let _ = mutation;
    record_retention_diagnostics! {
        if eviction {
            state.diagnostics.evictions = state.diagnostics.evictions.saturating_add(1);
        } else {
            note_logical_mutation_diagnostics(&mut state.diagnostics, mutation);
        }
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
        RetentionRootMode::Indexed(index) => {
            index.remove(path);
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
    record_retention_diagnostics! {
        state.diagnostics.streaming_size_updates =
            state.diagnostics.streaming_size_updates.saturating_add(1);
        if completed {
            note_logical_mutation_diagnostics(
                &mut state.diagnostics,
                LogicalRetentionMutation::StreamComplete,
            );
        }
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
    record_retention_diagnostics! {
        note_logical_mutation_diagnostics(
            &mut state.diagnostics,
            LogicalRetentionMutation::Cleanup,
        );
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

async fn reconcile_retention_root(root: &Path) -> RetentionModeKind {
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
        record_retention_diagnostics! {
            state.diagnostics.reconciliations =
                state.diagnostics.reconciliations.saturating_add(1);
            state.diagnostics.scans = state.diagnostics.scans.saturating_add(1);
        }
        let capacity = {
            #[cfg(test)]
            {
                state
                    .index_capacity_override
                    .unwrap_or(MAX_RETENTION_INDEX_RECORDS)
            }
            #[cfg(not(test))]
            {
                MAX_RETENTION_INDEX_RECORDS
            }
        };
        (generation, capacity, was_scan_only)
    };
    #[cfg(not(test))]
    let _ = was_scan_only;

    #[cfg(test)]
    let started = Instant::now();
    let scan = scan_retention_root(&root, capacity).await;
    #[cfg(test)]
    let elapsed_nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let mut registry = lock_retention_registry();
    let Some(state) = registry.roots.get_mut(&root) else {
        let _ = insert_dirty_root(&mut registry, root);
        return RetentionModeKind::Dirty;
    };
    if state.generation != generation {
        return root_mode_kind(&state.mode);
    }
    record_retention_diagnostics! {
        state.diagnostics.scan_wall_nanos = state
            .diagnostics
            .scan_wall_nanos
            .saturating_add(elapsed_nanos);
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
        record_retention_diagnostics! {
            state.diagnostics.dirty_transitions =
                state.diagnostics.dirty_transitions.saturating_add(1);
        }
        return RetentionModeKind::Dirty;
    }
    match scan {
        Ok((candidate, directories, candidates)) => {
            record_retention_diagnostics! {
                state.diagnostics.directories_visited = state
                    .diagnostics
                    .directories_visited
                    .saturating_add(directories);
                state.diagnostics.candidates_visited = state
                    .diagnostics
                    .candidates_visited
                    .saturating_add(candidates);
            }
            #[cfg(not(test))]
            let _ = (directories, candidates);
            match candidate {
                RetentionScanCandidate::Indexed(index) => {
                    state.mode = RetentionRootMode::Indexed(index);
                    #[cfg(test)]
                    if was_scan_only {
                        record_retention_diagnostics! {
                            state.diagnostics.scan_only_exits =
                                state.diagnostics.scan_only_exits.saturating_add(1);
                        }
                    }
                    RetentionModeKind::Indexed
                }
                RetentionScanCandidate::Oversized => {
                    state.mode = RetentionRootMode::ScanOnly {
                        operations_since_probe: 0,
                    };
                    record_retention_diagnostics! {
                        state.diagnostics.oversized_root_fallbacks =
                            state.diagnostics.oversized_root_fallbacks.saturating_add(1);
                    }
                    #[cfg(test)]
                    if !was_scan_only {
                        record_retention_diagnostics! {
                            state.diagnostics.scan_only_entries =
                                state.diagnostics.scan_only_entries.saturating_add(1);
                        }
                    }
                    RetentionModeKind::ScanOnly
                }
            }
        }
        Err(_) => {
            state.mode = RetentionRootMode::Dirty;
            record_retention_diagnostics! {
                state.diagnostics.dirty_transitions =
                    state.diagnostics.dirty_transitions.saturating_add(1);
            }
            RetentionModeKind::Dirty
        }
    }
}

async fn prepare_retention_mode(root: &Path, force_reconciliation: bool) -> RetentionModeKind {
    if indexing_disabled().load(Ordering::Acquire) {
        return RetentionModeKind::Disabled;
    }
    let root = normalized_tool_output_root(root);
    let mode = {
        let mut registry = lock_retention_registry();
        if let Some(state) = registry.roots.get_mut(&root) {
            match &mut state.mode {
                RetentionRootMode::Indexed(index) => {
                    if force_reconciliation
                        || index.logical_mutations_since_reconciliation
                            >= RETENTION_RECONCILIATION_INTERVAL
                        || index.near_limit_pending
                    {
                        RetentionModeKind::Dirty
                    } else {
                        RetentionModeKind::Indexed
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
                        record_retention_diagnostics! {
                            state.diagnostics.scan_only_operations =
                                state.diagnostics.scan_only_operations.saturating_add(1);
                        }
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
    let reconciled = reconcile_retention_root(&root).await;
    if reconciled != RetentionModeKind::Indexed {
        if reconciled == RetentionModeKind::ScanOnly {
            #[cfg(test)]
            let mut registry = lock_retention_registry();
            #[cfg(test)]
            if let Some(state) = registry.roots.get_mut(&root) {
                state.diagnostics.scan_only_operations =
                    state.diagnostics.scan_only_operations.saturating_add(1);
            }
        }
        return reconciled;
    }
    RetentionModeKind::Indexed
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
                record_retention_diagnostics! {
                    state.diagnostics.dirty_transitions =
                        state.diagnostics.dirty_transitions.saturating_add(1);
                }
            }
        }
    }
}

async fn enforce_indexed_thread_retention(
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
            let (thread_bytes, thread_unprotected) = index.thread_totals(&directory);
            if thread_unprotected.saturating_add(reserved_artifacts)
                <= max_retained_artifacts_per_thread()
                && thread_bytes.saturating_add(reserved_bytes)
                    <= MAX_RETAINED_ARTIFACT_BYTES_PER_THREAD
            {
                return true;
            }
            index.threads.get(&directory).and_then(|thread| {
                thread.paths.iter().find_map(|path| {
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
        match remove_inactive_output_path(path.clone()).await {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                publish_known_remove(&token, &path, LogicalRetentionMutation::Delete, true);
            }
            InactiveRemovalOutcome::Active => {
                skipped.insert(path);
            }
            InactiveRemovalOutcome::Ambiguous(_) => {
                invalidate_root_after_ambiguous_failure(root);
                return false;
            }
        }
    }
}

async fn enforce_indexed_global_retention(
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
                && index.total_bytes.saturating_add(reserved_bytes)
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
        match remove_inactive_output_path(path.clone()).await {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                publish_known_remove(&token, &path, LogicalRetentionMutation::Delete, true);
            }
            InactiveRemovalOutcome::Active => {
                skipped.insert(path);
            }
            InactiveRemovalOutcome::Ambiguous(_) => {
                invalidate_root_after_ambiguous_failure(root);
                return false;
            }
        }
    }
}

async fn enforce_retention(directory: &Path, keep_path: &Path) {
    let token = capture_retention_token(directory);
    let _retention_permit = retention_sweep_permit().await;
    publish_observed_path(&token, keep_path).await;
    enforce_retention_locked(directory, keep_path, 0, 0).await;
}

async fn enforce_retention_after_observation(
    directory: &Path,
    path: &Path,
    token: &RetentionIndexToken,
) {
    let _retention_permit = retention_sweep_permit().await;
    publish_observed_path(token, path).await;
    enforce_retention_locked(directory, path, 0, 0).await;
}

async fn enforce_retention_after_upsert(
    directory: &Path,
    path: &Path,
    token: &RetentionIndexToken,
    mutation: LogicalRetentionMutation,
) {
    let _retention_permit = retention_sweep_permit().await;
    match artifact_retention_record(path).await {
        Ok(Some(record)) => publish_known_record(token, record, mutation),
        Ok(None) => publish_known_remove(token, path, mutation, false),
        Err(_) => reject_stale_delta(token),
    }
    enforce_retention_locked(directory, path, 0, 0).await;
}

async fn publish_observed_path(token: &RetentionIndexToken, path: &Path) {
    match artifact_retention_record(path).await {
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

async fn enforce_retention_locked(
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    let root = tool_output_root_for_directory(directory);
    if reconcile_logical_artifact_transactions(directory).is_err() {
        let mut registry = lock_retention_registry();
        transition_current_root_to_dirty(&mut registry, &root);
        return;
    }
    let external_generation_changed =
        retention_interprocess_index_stale().swap(false, Ordering::AcqRel);
    if external_generation_changed
        && reconcile_logical_artifact_transactions_in_root(&root).is_err()
    {
        let mut registry = lock_retention_registry();
        transition_current_root_to_dirty(&mut registry, &root);
        return;
    }
    match prepare_retention_mode(&root, external_generation_changed).await {
        RetentionModeKind::Indexed => {
            if !enforce_indexed_thread_retention(
                &root,
                directory,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            )
            .await
            {
                return;
            }
            let _ = enforce_indexed_global_retention(
                &root,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            )
            .await;
        }
        RetentionModeKind::ScanOnly | RetentionModeKind::Disabled => {
            run_scan_only_retention(
                &root,
                directory,
                keep_path,
                reserved_bytes,
                reserved_artifacts,
            )
            .await;
        }
        RetentionModeKind::Dirty | RetentionModeKind::Reconciling => {}
    }
}

async fn run_scan_only_retention(
    root: &Path,
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) {
    #[cfg(test)]
    let started = Instant::now();
    let thread_scan =
        enforce_retention_scan_locked(directory, keep_path, reserved_bytes, reserved_artifacts)
            .await;
    #[cfg_attr(not(test), allow(unused_variables))]
    let global_scan = if thread_scan.complete {
        enforce_global_retention_scan_locked(root, keep_path, reserved_bytes, reserved_artifacts)
            .await
    } else {
        RetentionScanProgress::default()
    };
    #[cfg(test)]
    let elapsed = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    #[cfg(test)]
    let mut registry = lock_retention_registry();
    #[cfg(test)]
    if let Some(state) = registry.roots.get_mut(root) {
        state.diagnostics.scans = state.diagnostics.scans.saturating_add(1);
        state.diagnostics.scan_wall_nanos =
            state.diagnostics.scan_wall_nanos.saturating_add(elapsed);
        state.diagnostics.directories_visited = state
            .diagnostics
            .directories_visited
            .saturating_add(thread_scan.directories_visited)
            .saturating_add(global_scan.directories_visited);
        state.diagnostics.candidates_visited = state
            .diagnostics
            .candidates_visited
            .saturating_add(thread_scan.candidates_visited)
            .saturating_add(global_scan.candidates_visited);
    }
}

async fn enforce_retention_scan_locked(
    directory: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> RetentionScanProgress {
    let mut progress = RetentionScanProgress::default();
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return progress;
    };
    progress.directories_visited = 1;
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => return progress,
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
            progress.candidates_visited = progress.candidates_visited.saturating_add(1);
            let Ok(metadata) = entry.metadata().await else {
                return progress;
            };
            let bytes = metadata.len();
            let Some(updated_total) = total_bytes.checked_add(bytes) else {
                return progress;
            };
            total_bytes = updated_total;
            let Ok(protected) = artifact_is_protected(&path).await else {
                return progress;
            };
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
        match remove_inactive_output_path(path).await {
            InactiveRemovalOutcome::RemovedOrAbsent => {
                remove_count = remove_count.saturating_sub(1);
                total_bytes = total_bytes.saturating_sub(bytes);
            }
            InactiveRemovalOutcome::Active => {}
            InactiveRemovalOutcome::Ambiguous(_) => return progress,
        }
    }
    progress.complete = true;
    progress
}

#[cfg(test)]
async fn enforce_global_retention(tool_output_root: &Path, keep_path: &Path) {
    let _retention_permit = retention_sweep_permit().await;
    let root = normalized_tool_output_root(tool_output_root);
    match prepare_retention_mode(&root, true).await {
        RetentionModeKind::Indexed => {
            let _ = enforce_indexed_global_retention(&root, keep_path, 0, 0).await;
        }
        RetentionModeKind::ScanOnly | RetentionModeKind::Disabled => {
            let _ = enforce_global_retention_scan_locked(&root, keep_path, 0, 0).await;
        }
        RetentionModeKind::Dirty | RetentionModeKind::Reconciling => {}
    }
}

async fn enforce_global_retention_scan_locked(
    tool_output_root: &Path,
    keep_path: &Path,
    reserved_bytes: u64,
    reserved_artifacts: usize,
) -> RetentionScanProgress {
    let mut progress = RetentionScanProgress::default();
    let Ok(mut thread_directories) = tokio::fs::read_dir(tool_output_root).await else {
        return progress;
    };
    let mut paths = Vec::new();
    let mut total_bytes = 0_u64;
    loop {
        let thread_directory = match thread_directories.next_entry().await {
            Ok(Some(entry)) => entry.path(),
            Ok(None) => break,
            Err(_) => return progress,
        };
        let Ok(mut entries) = tokio::fs::read_dir(&thread_directory).await else {
            return progress;
        };
        progress.directories_visited = progress.directories_visited.saturating_add(1);
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => return progress,
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("log") {
                progress.candidates_visited = progress.candidates_visited.saturating_add(1);
                let Ok(metadata) = entry.metadata().await else {
                    return progress;
                };
                let bytes = metadata.len();
                let Some(updated_total) = total_bytes.checked_add(bytes) else {
                    return progress;
                };
                total_bytes = updated_total;
                let Ok(modified) = metadata.modified() else {
                    return progress;
                };
                let Ok(protected) = artifact_is_protected(&path).await else {
                    return progress;
                };
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
            match remove_inactive_output_path(path).await {
                InactiveRemovalOutcome::RemovedOrAbsent => {
                    remove_count = remove_count.saturating_sub(1);
                    total_bytes = total_bytes.saturating_sub(bytes);
                }
                InactiveRemovalOutcome::Active => {}
                InactiveRemovalOutcome::Ambiguous(_) => return progress,
            }
        }
    }
    progress.complete = true;
    progress
}

async fn retention_usage_locked(directory: &Path) -> RetentionUsage {
    let root = tool_output_root_for_directory(directory);
    let indexed_usage = {
        let registry = lock_retention_registry();
        registry
            .roots
            .get(&root)
            .and_then(|state| match &state.mode {
                RetentionRootMode::Indexed(index) => Some(RetentionUsage {
                    thread_bytes: index.thread_totals(directory).0,
                    global_bytes: index.total_bytes,
                }),
                RetentionRootMode::Dirty | RetentionRootMode::Reconciling { .. } => {
                    Some(RetentionUsage {
                        thread_bytes: u64::MAX,
                        global_bytes: u64::MAX,
                    })
                }
                RetentionRootMode::ScanOnly { .. } => None,
            })
    };
    if let Some(usage) = indexed_usage {
        return usage;
    }
    RetentionUsage {
        thread_bytes: log_bytes_in_directory(directory).await.unwrap_or(u64::MAX),
        global_bytes: log_bytes_in_tool_output_root(&root)
            .await
            .unwrap_or(u64::MAX),
    }
}

async fn log_bytes_in_directory(directory: &Path) -> std::io::Result<u64> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().extension().and_then(|value| value.to_str()) == Some("log") {
            bytes = bytes
                .checked_add(entry.metadata().await?.len())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "thread artifact byte total overflowed",
                    )
                })?;
        }
    }
    Ok(bytes)
}

async fn log_bytes_in_tool_output_root(root: &Path) -> std::io::Result<u64> {
    let mut entries = tokio::fs::read_dir(root).await?;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next_entry().await? {
        if entry.metadata().await?.is_dir() {
            bytes = bytes
                .checked_add(log_bytes_in_directory(&entry.path()).await?)
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

struct RetentionSweepPermit {
    _process_permit: SemaphorePermit<'static>,
    _interprocess_lock: AtomicWriteLock,
}

fn retention_interprocess_lock_target() -> PathBuf {
    std::env::temp_dir().join("openai-codex-tool-output-retention-v1")
}

fn retention_interprocess_index_stale() -> &'static AtomicBool {
    static STALE: AtomicBool = AtomicBool::new(true);
    &STALE
}

fn retention_interprocess_generation() -> &'static StdMutex<Option<u64>> {
    static GENERATION: OnceLock<StdMutex<Option<u64>>> = OnceLock::new();
    GENERATION.get_or_init(|| StdMutex::new(None))
}

fn advance_retention_interprocess_generation(lock_target: &Path) -> std::io::Result<()> {
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
    write_bytes_atomically(lock_target, next.to_string().as_bytes())?;
    let mut observed = retention_interprocess_generation()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if recovered || observed.is_none_or(|observed| observed != previous) {
        retention_interprocess_index_stale().store(true, Ordering::Release);
    }
    *observed = Some(next);
    Ok(())
}

async fn retention_sweep_permit() -> RetentionSweepPermit {
    let process_permit = match retention_sweep_semaphore().acquire().await {
        Ok(permit) => permit,
        Err(_) => unreachable!("the process-wide retention sweep semaphore is never closed"),
    };
    let lock_target = retention_interprocess_lock_target();
    let interprocess_lock = match tokio::task::spawn_blocking(move || {
        let lock = acquire_atomic_write_lock(&lock_target)?;
        advance_retention_interprocess_generation(&lock_target)?;
        Ok::<_, std::io::Error>(lock)
    })
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(error)) => panic!("shared artifact retention lock must be available: {error}"),
        Err(error) => panic!("retention lock task must complete: {error}"),
    };
    RetentionSweepPermit {
        _process_permit: process_permit,
        _interprocess_lock: interprocess_lock,
    }
}

fn retention_sweep_semaphore() -> &'static Semaphore {
    static RETENTION_SWEEP_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();
    RETENTION_SWEEP_SEMAPHORE.get_or_init(|| Semaphore::new(1))
}

#[cfg(test)]
fn reconciliation_barriers() -> &'static StdMutex<BTreeMap<PathBuf, Arc<tokio::sync::Barrier>>> {
    static BARRIERS: OnceLock<StdMutex<BTreeMap<PathBuf, Arc<tokio::sync::Barrier>>>> =
        OnceLock::new();
    BARRIERS.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

#[cfg(test)]
async fn wait_at_reconciliation_barrier(root: &Path) {
    let barrier = reconciliation_barriers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(root);
    if let Some(barrier) = barrier {
        barrier.wait().await;
        barrier.wait().await;
    }
}

#[cfg(test)]
fn set_reconciliation_barrier(root: &Path, barrier: Arc<tokio::sync::Barrier>) {
    reconciliation_barriers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(normalized_tool_output_root(root), barrier);
}

#[cfg(test)]
fn set_retention_index_capacity_for_test(root: &Path, capacity: usize) {
    let root = normalized_tool_output_root(root);
    let directory = root.join("test-thread");
    let _ = capture_retention_token(&directory);
    let mut registry = lock_retention_registry();
    if let Some(state) = registry.roots.get_mut(&root) {
        state.index_capacity_override = Some(capacity);
        if !matches!(state.mode, RetentionRootMode::Dirty) {
            state.mode = RetentionRootMode::Dirty;
        }
    }
}

#[cfg(test)]
fn retention_diagnostics_for_test(root: &Path) -> RetentionDiagnostics {
    lock_retention_registry()
        .roots
        .get(&normalized_tool_output_root(root))
        .map_or_else(RetentionDiagnostics::default, |state| state.diagnostics)
}

#[cfg(test)]
fn retention_mode_for_test(root: &Path) -> RetentionModeKind {
    lock_retention_registry()
        .roots
        .get(&normalized_tool_output_root(root))
        .map_or(RetentionModeKind::Dirty, |state| {
            root_mode_kind(&state.mode)
        })
}

#[cfg(test)]
fn retention_generation_for_test(root: &Path) -> Option<u64> {
    lock_retention_registry()
        .roots
        .get(&normalized_tool_output_root(root))
        .map(|state| state.generation)
}

#[cfg(test)]
fn retention_registry_mutex_is_available_for_test() -> bool {
    retention_registry().try_lock().is_ok()
}

#[cfg(test)]
async fn force_retention_reconciliation_for_test(root: &Path) -> RetentionModeKind {
    let _permit = retention_sweep_permit().await;
    reconcile_retention_root(root).await
}

#[cfg(test)]
#[path = "command_output_artifact_tests.rs"]
mod hardening_tests;

async fn remove_inactive_output_path(path: PathBuf) -> InactiveRemovalOutcome {
    tokio::task::spawn_blocking(move || {
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
        match remove_logical_artifact_files(&path) {
            Ok(()) => InactiveRemovalOutcome::RemovedOrAbsent,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                InactiveRemovalOutcome::RemovedOrAbsent
            }
            Err(err) => InactiveRemovalOutcome::Ambiguous(err),
        }
    })
    .await
    .unwrap_or_else(|err| InactiveRemovalOutcome::Ambiguous(std::io::Error::other(err)))
}

async fn logical_artifact_disk_bytes(path: &Path) -> std::io::Result<u64> {
    let Some(directory) = path.parent() else {
        return Ok(0);
    };
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut bytes = 0_u64;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == format!("{stem}.log")
            || name == format!("{stem}.meta.json")
            || name.starts_with(&format!("{stem}.segment-"))
        {
            bytes = bytes
                .checked_add(entry.metadata().await?.len())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "logical artifact byte total overflowed",
                    )
                })?;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn artifact_retains_exact_bytes_across_chunks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = create_raw_output_artifact(temp.path(), "thread", b"alpha\0beta\n").await;
        let second = append_raw_output_artifact(&first, b"unicode: \xce\xbb\n").await;

        let RawOutputArtifact::Stored { path, bytes, .. } = second else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, 23);
        assert_eq!(
            tokio::fs::read(path).await.expect("read artifact"),
            b"alpha\0beta\nunicode: \xce\xbb\n"
        );
    }

    #[tokio::test]
    async fn replacement_finalizes_background_output_without_duplicates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let initial = create_raw_output_artifact(temp.path(), "thread", b"partial\n").await;
        let appended = append_raw_output_artifact(&initial, b"tail\n").await;
        let final_output = b"partial\ntail\ncomplete\n";
        let replaced = replace_raw_output_artifact(&appended, final_output).await;

        let RawOutputArtifact::Stored { path, bytes, .. } = replaced else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, final_output.len() as u64);
        assert_eq!(
            tokio::fs::read(path).await.expect("read artifact"),
            final_output
        );
    }

    #[tokio::test]
    async fn artifact_creation_truncates_at_the_per_artifact_byte_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let oversized = vec![b'x'; MAX_RAW_OUTPUT_ARTIFACT_BYTES + 1];

        let artifact = create_raw_output_artifact(temp.path(), "thread", &oversized).await;
        assert_eq!(
            artifact.retention_limit_reason(),
            Some("per_artifact_safety_limit")
        );

        let RawOutputArtifact::Stored {
            path,
            bytes,
            truncated,
            ..
        } = artifact
        else {
            panic!("expected stored artifact");
        };
        assert_eq!(bytes, MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64);
        assert!(truncated);
        assert_eq!(
            tokio::fs::metadata(path)
                .await
                .expect("artifact metadata")
                .len(),
            MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64
        );
    }

    #[tokio::test]
    async fn append_uses_the_original_handle_after_path_substitution() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"original").await;
        let RawOutputArtifact::Stored { path, .. } = &artifact else {
            panic!("expected stored artifact");
        };
        let displaced = path.with_extension("displaced");
        std::fs::rename(path, &displaced).expect("displace artifact path");
        std::fs::write(path, b"substitute").expect("write substitute path");

        let appended = append_raw_output_artifact(&artifact, b"-tail").await;

        assert!(matches!(appended, RawOutputArtifact::Stored { .. }));
        assert_eq!(std::fs::read(path).expect("read substitute"), b"substitute");
        assert_eq!(
            std::fs::read(displaced).expect("read original handle target"),
            b"original-tail"
        );
    }

    #[test]
    fn failed_artifact_preserves_owned_partial_metadata() {
        let artifact = RawOutputArtifact::Failed {
            id: None,
            message: "flush failed".to_string(),
            owned_path: Some(PathBuf::from("C:/codex/tool-output/partial.log")),
            bytes: 17,
        };

        assert!(!artifact.render_for_model().contains("partial.log"));
        assert!(matches!(
            artifact,
            RawOutputArtifact::Failed { bytes: 17, .. }
        ));
    }

    #[test]
    fn evidence_protection_marker_rejects_oversized_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("artifact.evidence-protected");
        create_new_evidence_protection_marker(&marker).expect("create evidence marker");
        assert!(evidence_protection_marker_is_valid(&marker));

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&marker)
            .expect("open evidence marker for append");
        file.write_all(b"x").expect("append trailing marker byte");

        assert!(!evidence_protection_marker_is_valid(&marker));
    }

    fn stored_id(artifact: &RawOutputArtifact) -> ToolOutputArtifactId {
        let RawOutputArtifact::Stored { id, .. } = artifact else {
            panic!("expected stored artifact");
        };
        *id
    }

    #[tokio::test]
    async fn read_returns_exact_range_and_treats_crlf_as_one_break() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bytes = b"one\r\ntwo\r\nthree\r\nfour\r\n";
        let artifact = create_raw_output_artifact(temp.path(), "thread", bytes).await;
        let id = stored_id(&artifact);

        let output =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 2, 3, 16_384)
                .await
                .expect("read artifact");

        assert_eq!(
            output,
            format!(
                "artifact {id}, lines 2–3, {} retained bytes\ntwo\nthree\n",
                bytes.len()
            )
        );

        let through_eof =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 200, 16_384)
                .await
                .expect("read through eof");
        assert!(through_eof.starts_with(&format!(
            "artifact {id}, lines 1–4, {} retained bytes\n",
            bytes.len()
        )));
    }

    #[tokio::test]
    async fn read_clamps_at_byte_limit_with_trailer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"abcdef\n").await;
        let id = stored_id(&artifact);

        let output = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 3)
            .await
            .expect("read artifact");

        assert!(output.contains("\nabc\n[range clamped at byte limit]"));
    }

    #[tokio::test]
    async fn read_rejects_invalid_uuid_and_traversal() {
        let temp = tempfile::tempdir().expect("tempdir");
        for invalid in ["not-a-uuid", "../019fa782-f8e1-7533-a3f7-60d3f9a42997"] {
            let error = read_tool_output_artifact(temp.path(), "thread", invalid, 1, 1, 16_384)
                .await
                .expect_err("invalid id should fail");
            assert_eq!(error, ReadToolOutputError::InvalidArtifactId);
        }
    }

    #[tokio::test]
    async fn read_rejects_uuid_named_reparse_point_outside_thread_directory() {
        use std::os::windows::fs::symlink_file;

        let temp = tempfile::tempdir().expect("tempdir");
        let thread_directory = temp.path().join("tool-output").join("thread");
        std::fs::create_dir_all(&thread_directory).expect("create thread directory");
        let outside = temp.path().join("outside.log");
        std::fs::write(&outside, b"outside secret\n").expect("write outside artifact");
        let id = ToolOutputArtifactId::new();
        if let Err(error) = symlink_file(&outside, thread_directory.join(format!("{id}.log"))) {
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("create file reparse point: {error}");
        }

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("reparse artifact should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
    }

    #[tokio::test]
    async fn read_rejects_artifact_from_other_thread() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread-a", b"secret\n").await;
        let id = stored_id(&artifact);

        let error =
            read_tool_output_artifact(temp.path(), "thread-b", &id.to_string(), 1, 1, 16_384)
                .await
                .expect_err("cross-thread read should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
        assert_eq!(error.for_model(), ARTIFACT_EXPIRED_MESSAGE);
    }

    #[tokio::test]
    async fn read_reports_evicted_artifact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"old\n").await;
        let id = stored_id(&artifact);
        let RawOutputArtifact::Stored { path, .. } = artifact else {
            unreachable!();
        };
        tokio::fs::remove_file(path).await.expect("evict artifact");

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("evicted artifact should fail");

        assert_eq!(error, ReadToolOutputError::Expired);
    }

    #[tokio::test]
    async fn read_replaces_invalid_utf8_and_adds_notice() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"bad:\xff\n").await;
        let id = stored_id(&artifact);

        let output =
            read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
                .await
                .expect("read artifact");

        assert!(output.contains("bad:\u{fffd}"));
        assert!(output.ends_with("[invalid UTF-8 replaced]"));
    }

    #[tokio::test]
    async fn read_reports_active_writer_lock_without_blocking() {
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact = create_raw_output_artifact(temp.path(), "thread", b"partial\n").await;
        let id = stored_id(&artifact);
        let state = Arc::new(Mutex::new(artifact));
        let _writer = RawOutputArtifactWriter::open(Some(&state))
            .await
            .expect("writer state");

        let error = read_tool_output_artifact(temp.path(), "thread", &id.to_string(), 1, 1, 16_384)
            .await
            .expect_err("locked artifact should not be read");

        assert_eq!(error, ReadToolOutputError::StillWriting);
        assert_eq!(error.for_model(), ARTIFACT_WRITING_MESSAGE);
    }

    #[tokio::test]
    async fn retention_removes_oldest_inactive_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for index in 0..(max_retained_artifacts_per_thread() + 3) {
            let artifact = create_raw_output_artifact(
                temp.path(),
                "thread",
                format!("artifact-{index}").as_bytes(),
            )
            .await;
            let RawOutputArtifact::Stored { path, .. } = artifact else {
                panic!("expected stored artifact");
            };
            paths.push(path);
        }

        let mut retained = tokio::fs::read_dir(temp.path().join("tool-output").join("thread"))
            .await
            .expect("read artifact directory");
        let mut retained_count = 0;
        while retained
            .next_entry()
            .await
            .expect("read retained artifact")
            .is_some()
        {
            retained_count += 1;
        }
        assert_eq!(retained_count, max_retained_artifacts_per_thread());
        assert!(!paths[0].exists());
        assert!(paths.last().expect("newest path").exists());
    }
}
