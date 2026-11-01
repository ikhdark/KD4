//! Windows-only reuse under mandatory write/delete exclusion, not mtime-based
//! invalidation. Unsupported paths and mutable/incomplete captures keep the
//! ordinary full-authentication path. No storage format or public DTO changes.

use super::LogicalArtifactMetadata;
use super::ReadToolOutputError;
use super::ToolOutputArtifactId;
use super::ToolOutputSnapshot;
use super::configure_no_follow_open;
use super::logical_metadata_path;
use super::logical_segment_path;
use super::logical_transaction_path;
use super::metadata_is_reparse_point;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use tokio::runtime::Handle;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use uuid::Uuid;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

const LEASE_TTL: Duration = Duration::from_secs(2);
const MAX_ENTRIES: usize = 4;
// A conservative admission charge, not an assertion of allocator-exact RAM.
const MAX_CHARGE: usize = 32 * 1024 * 1024;
const MAX_SEGMENTS: usize = 128;
const MAX_PARENT_HANDLES: usize = 64;

struct Entry {
    path: PathBuf,
    snapshot: Arc<ToolOutputSnapshot>,
    expires: Instant,
    charge: usize,
    // Files and every ancestor remain pinned for precisely the cache residency,
    // not the caller's potentially longer-lived Arc to the immutable bytes.
    _lease: Vec<File>,
    _admission: OwnedSemaphorePermit,
}

#[derive(Default)]
struct State {
    entries: VecDeque<Arc<Entry>>,
    mutating: usize,
    worker: Option<Uuid>,
}

pub(super) struct SnapshotCache(Mutex<State>, Duration, Arc<Semaphore>);

impl Default for SnapshotCache {
    fn default() -> Self {
        Self(Mutex::default(), LEASE_TTL, Arc::new(Semaphore::new(MAX_ENTRIES)))
    }
}

pub(super) fn shared() -> &'static Arc<SnapshotCache> {
    static CACHE: OnceLock<Arc<SnapshotCache>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(SnapshotCache::default()))
}

impl SnapshotCache {

    pub(super) fn load_with_metadata(
        self: &Arc<Self>,
        path: &Path,
        id: ToolOutputArtifactId,
        loader: impl FnOnce(Option<LogicalArtifactMetadata>) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError>,
    ) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
        let Ok(runtime) = Handle::try_current() else { return loader(None); };
        // Transaction reconciliation belongs to the uncached owner. Never let a
        // hit conceal a new marker, even one from an interrupted external writer.
        if !matches!(std::fs::symlink_metadata(logical_transaction_path(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            let _mutation = self.mutating(path);
            return loader(None);
        }
        let (cached, may_cache) = {
            let state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let may_cache = state.mutating == 0;
            let cached = may_cache.then(|| state.entries.iter()
                .find(|entry| entry.path == path && entry.expires > Instant::now()
                    && entry.snapshot.metadata.artifact_id == id.to_string())
                .cloned()).flatten();
            (cached, may_cache)
        };
        if let Some(entry) = cached {
            // No payload read/hash is needed: the actual objects and namespace
            // cannot be changed or expired while this enforced lease is held.
            // A lease preserves bytes, not current read authority: Windows can
            // revoke an ACL while previously granted handles remain usable.
            // Reopen every content-bearing member under current access checks.
            if protected_file(path).is_some()
                && protected_file(&logical_metadata_path(path)).is_some()
                && entry.snapshot.metadata.segments.iter().filter(|segment| segment.index != 0)
                    .all(|segment| protected_file(&logical_segment_path(path, segment.index)).is_some())
            {
                return Ok(Arc::clone(&entry.snapshot));
            }
            return loader(None);
        }
        // Acquire exclusion BEFORE authentication. Acquiring it afterward could
        // bless a same-size replacement using bytes read before the lease.
        let mut lease = if may_cache {
            let removed = {
                let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut removed = take_path_entries(&mut state, path);
                if state.entries.len() >= MAX_ENTRIES
                    && let Some(entry) = state.entries.pop_front()
                {
                    removed.push(entry);
                }
                removed
            };
            drop(removed);
            // The same admission covers both cold authentication and cached
            // residency, bounding new native handles under concurrent misses.
            self.2.clone().try_acquire_owned().ok().and_then(|permit| {
                protect_complete_family(path, id).map(|(files, bytes, metadata)| (files, bytes, Some(metadata), permit))
            })
        } else { None };
        // The observation is valid only while all family/ancestor exclusions
        // above are still held. Authentication below retains every byte check.
        let metadata = lease.as_mut().and_then(|(_, _, metadata, _)| metadata.take());
        let snapshot = loader(metadata)?;
        if let Some((files, metadata_bytes, _, permit)) = lease {
            let charge = snapshot.bytes.len().saturating_add(metadata_bytes.saturating_mul(4));
            if charge <= MAX_CHARGE {
                self.insert(path, Arc::clone(&snapshot), files, permit, charge, &runtime);
            }
        }
        Ok(snapshot)
    }

    fn insert(self: &Arc<Self>, path: &Path, snapshot: Arc<ToolOutputSnapshot>, files: Vec<File>, permit: OwnedSemaphorePermit,
        charge: usize, runtime: &Handle)
    {
        let entry = Arc::new(Entry { path: path.to_owned(), snapshot, charge,
            expires: Instant::now() + self.1, _lease: files, _admission: permit });
        let mut removed = Vec::new();
        let worker = {
            let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.mutating != 0
                || state.entries.iter().any(|item| item.path == path && item.expires > Instant::now())
            {
                return;
            }
            // An expired entry can still await its cleanup worker. Do not
            // retain two leases for one path or let mutation evict only one.
            removed.extend(take_path_entries(&mut state, path));
            while state.entries.len() >= MAX_ENTRIES
                || state.entries.iter().map(|item| item.charge).sum::<usize>() + charge > MAX_CHARGE
            {
                if let Some(item) = state.entries.pop_front() { removed.push(item); }
            }
            state.entries.push_back(entry);
            if state.worker.is_none() {
                let generation = Uuid::new_v4();
                state.worker = Some(generation);
                Some(generation)
            } else { None }
        };
        // Never close files or acquire filesystem/retention locks under State.
        drop(removed);
        if let Some(generation) = worker {
            let cleanup = WorkerCleanup { cache: Arc::clone(self), generation };
            runtime.spawn(async move {
                // Constructed before spawn: runtime shutdown cleans up even if
                // this future is cancelled without ever being polled.
                let cleanup = cleanup;
                loop {
                    let (removed, next) = {
                        let mut state = cleanup.cache.0.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let now = Instant::now();
                        let mut removed = Vec::new();
                        let mut index = 0;
                        while index < state.entries.len() {
                            if state.entries[index].expires <= now {
                                removed.extend(state.entries.remove(index));
                            } else { index += 1; }
                        }
                        let next = state.entries.iter().map(|entry| entry.expires).min();
                        if next.is_none() { state.worker = None; }
                        (removed, next)
                    };
                    drop(removed);
                    let Some(next) = next else { break; };
                    tokio::time::sleep_until(tokio::time::Instant::from_std(next)).await;
                }
            });
        }
    }

    /// Release all bounded residency before local attachment/retention: the
    /// same Windows object can have different path spellings at those owners.
    /// Suppress reuse and insertion until every overlapping mutation ends.
    /// In-flight authenticating readers retain their ordinary transaction lease.
    pub(super) fn mutating(self: &Arc<Self>, _path: &Path) -> Mutation {
        let removed = {
            let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.mutating += 1;
            std::mem::take(&mut state.entries)
        };
        drop(removed);
        Mutation { cache: Arc::clone(self) }
    }
}

fn take_path_entries(state: &mut State, path: &Path) -> Vec<Arc<Entry>> {
    let mut removed = Vec::new();
    let mut index = 0;
    while index < state.entries.len() {
        if state.entries[index].path == path {
            removed.extend(state.entries.remove(index));
        } else {
            index += 1;
        }
    }
    removed
}

pub(super) struct Mutation { cache: Arc<SnapshotCache> }

impl Drop for Mutation {
    fn drop(&mut self) {
        let mut state = self.cache.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.mutating -= 1;
    }
}

struct WorkerCleanup { cache: Arc<SnapshotCache>, generation: Uuid }

impl Drop for WorkerCleanup {
    fn drop(&mut self) {
        let removed = {
            let mut state = self.cache.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.worker == Some(self.generation) {
                state.worker = None;
                std::mem::take(&mut state.entries)
            } else { VecDeque::new() }
        };
        drop(removed);
    }
}

fn protected_file(path: &Path) -> Option<File> {
    let mut options = File::options();
    options.read(true).share_mode(FILE_SHARE_READ);
    configure_no_follow_open(&mut options);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && !metadata_is_reparse_point(&metadata)).then_some(file)
}

fn protect_complete_family(path: &Path, id: ToolOutputArtifactId) -> Option<(Vec<File>, usize, LogicalArtifactMetadata)> {
    let parent = path.parent()?;
    if parent.components().count() > MAX_PARENT_HANDLES { return None; }
    let mut files = codex_file_system::lock_directory_chain(parent).ok()?;
    if files.len() > MAX_PARENT_HANDLES { return None; }
    let base = protected_file(path)?;
    base.try_lock_shared().ok()?;
    files.push(base);
    let mut sidecar = protected_file(&logical_metadata_path(path))?;
    let length = usize::try_from(sidecar.metadata().ok()?.len()).ok()?;
    if length > MAX_CHARGE / 4 { return None; }
    let mut bytes = Vec::with_capacity(length);
    sidecar.read_to_end(&mut bytes).ok()?;
    let metadata: LogicalArtifactMetadata = serde_json::from_slice(&bytes).ok()?;
    if metadata.version != super::LOGICAL_ARTIFACT_METADATA_VERSION
        || metadata.artifact_id != id.to_string() || !metadata.complete || !metadata.capture_complete
        || metadata.retained_bytes > super::MAX_RAW_OUTPUT_ARTIFACT_BYTES as u64
        || metadata.segments.len() > MAX_SEGMENTS
    { return None; }
    files.push(sidecar);
    for segment in &metadata.segments {
        if segment.index != 0 {
            files.push(protected_file(&logical_segment_path(path, segment.index))?);
        }
    }
    Some((files, bytes.len(), metadata))
}

