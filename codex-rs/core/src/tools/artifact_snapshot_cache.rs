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
    pub(super) fn load(
        self: &Arc<Self>,
        path: &Path,
        id: ToolOutputArtifactId,
        loader: impl FnOnce() -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError>,
    ) -> Result<Arc<ToolOutputSnapshot>, ReadToolOutputError> {
        let Ok(runtime) = Handle::try_current() else { return loader(); };
        // Transaction reconciliation belongs to the uncached owner. Never let a
        // hit conceal a new marker, even one from an interrupted external writer.
        if !matches!(std::fs::symlink_metadata(logical_transaction_path(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            let _mutation = self.mutating(path);
            return loader();
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
            return Ok(Arc::clone(&entry.snapshot));
        }
        // Acquire exclusion BEFORE authentication. Acquiring it afterward could
        // bless a same-size replacement using bytes read before the lease.
        let lease = if may_cache {
            let removed = {
                let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut removed = take_path_entries(&mut state, path);
                if state.entries.len() >= MAX_ENTRIES {
                    if let Some(entry) = state.entries.pop_front() { removed.push(entry); }
                }
                removed
            };
            drop(removed);
            // The same admission covers both cold authentication and cached
            // residency, bounding new native handles under concurrent misses.
            self.2.clone().try_acquire_owned().ok().and_then(|permit| {
                protect_complete_family(path, id).map(|(files, bytes)| (files, bytes, permit))
            })
        } else { None };
        let snapshot = loader()?;
        if let Some((files, metadata_bytes, permit)) = lease {
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
                                removed.push(state.entries.remove(index).expect("present cache entry"));
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
            removed.push(state.entries.remove(index).expect("present cache entry"));
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

fn protect_complete_family(path: &Path, id: ToolOutputArtifactId) -> Option<(Vec<File>, usize)> {
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
    Some((files, bytes.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn leased_recovery_reuses_authentication_and_fences_mutation_expiration_and_threads() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(vec![b'x'; 8 * 1024 * 1024]);
        let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
        assert!(artifact.complete, "{:?}", artifact.error);
        let id = artifact.id.unwrap();
        let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
        // The work-count fixture does not impose a machine-speed deadline.
        // Absolute production expiration is covered by the separate fixture.
        let cache = Arc::new(SnapshotCache(Mutex::default(), Duration::from_secs(60), Arc::new(Semaphore::new(MAX_ENTRIES))));
        let loads = AtomicUsize::new(0);
        let load = || cache.load(&path, id, || {
            loads.fetch_add(1, Ordering::Relaxed);
            super::super::load_tool_output_snapshot_uncached(&path, id)
        });
        let first = load().unwrap();
        // The 512 x 16-KiB workload must authenticate its 8-MiB payload once.
        // This counts the real uncached owner, independently of output equality.
        for page in 0..512 {
            let snapshot = load().unwrap();
            assert!(Arc::ptr_eq(&first, &snapshot));
            let start = page * 16 * 1024;
            let selected = snapshot.select(vec![super::super::ToolOutputSelector::Bytes {
                start: start as u64, end: (start + 16 * 1024) as u64,
            }], super::super::RECOVERY_AGGREGATE_TOKEN_CEILING).await.unwrap();
            assert_eq!(selected.results[0].text.as_ref().unwrap().as_bytes(), &canonical.bytes[start..start + 16 * 1024]);
        }
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert!(std::fs::write(&path, vec![b'y'; canonical.bytes.len()]).is_err());
        assert!(std::fs::write(logical_metadata_path(&path), b"{}").is_err());
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(path.parent().unwrap(), home.path().join("moved")).is_err());
        let other = home.path().join("tool-output/other").join(format!("{id}.log"));
        assert!(matches!(cache.load(&other, id, || super::super::load_tool_output_snapshot_uncached(&other, id)),
            Err(ReadToolOutputError::Expired)));
        {
            let _mutation = cache.mutating(&path);
            std::fs::write(&path, vec![b'y'; canonical.bytes.len()]).unwrap();
        }
        assert!(load().is_err(), "after lease release same-size corruption must fail authentication");
        std::fs::write(&path, &canonical.bytes).unwrap();
        assert_eq!(load().unwrap().bytes, canonical.bytes);
        {
            let _mutation = cache.mutating(&path);
            std::fs::remove_file(&path).unwrap();
        }
        assert!(matches!(load(), Err(ReadToolOutputError::Expired)));
        // Retained caller snapshots do not keep the native mutation lease alive.
        assert_eq!(first.bytes, canonical.bytes);
    }

    #[tokio::test]
    async fn lease_expiration_releases_files_without_another_recovery_call() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"expires".to_vec());
        let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
        let id = artifact.id.unwrap();
        let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
        let cache = Arc::new(SnapshotCache::default());
        let snapshot = cache.load(&path, id, || super::super::load_tool_output_snapshot_uncached(&path, id)).unwrap();
        tokio::time::timeout(LEASE_TTL + Duration::from_secs(2), async {
            loop {
                match std::fs::remove_file(&path) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(32) => {}
                    Err(error) => panic!("unexpected removal failure: {error}"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        assert_eq!(snapshot.bytes, b"expires");
    }

    #[tokio::test]
    async fn expired_replacement_and_capacity_do_not_leave_hidden_family_leases() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"bounded".to_vec());
        let cache = Arc::new(SnapshotCache(Mutex::default(), Duration::from_secs(60), Arc::new(Semaphore::new(MAX_ENTRIES))));
        let mut paths = Vec::new();
        for _ in 0..MAX_ENTRIES + 1 {
            let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
            let id = artifact.id.unwrap();
            let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
            cache.load(&path, id, || super::super::load_tool_output_snapshot_uncached(&path, id)).unwrap();
            paths.push((path, id));
            let state = cache.0.lock().unwrap();
            assert!(state.entries.len() <= MAX_ENTRIES);
            assert!(state.entries.iter().map(|entry| entry.charge).sum::<usize>() <= MAX_CHARGE);
        }
        // Eviction releases objects even when no timer has run.
        std::fs::remove_file(&paths[0].0).unwrap();
        let (path, id) = paths.last().unwrap();
        {
            let mut state = cache.0.lock().unwrap();
            let entry = state.entries.iter_mut().find(|entry| entry.path == *path).unwrap();
            Arc::get_mut(entry).unwrap().expires = Instant::now();
        }
        cache.load(path, *id, || super::super::load_tool_output_snapshot_uncached(path, *id)).unwrap();
        {
            let state = cache.0.lock().unwrap();
            assert_eq!(state.entries.iter().filter(|entry| entry.path == *path).count(), 1);
        }
        for (path, _) in paths.iter().skip(1) {
            let _mutation = cache.mutating(path);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn mutation_evicts_path_aliases_and_fences_overlapping_reinsertion() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"alias".to_vec());
        let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
        let id = artifact.id.unwrap();
        let verbatim_home = std::fs::canonicalize(home.path()).unwrap();
        let retained_home = super::super::normalized_retention_path(home.path());
        assert_ne!(verbatim_home, retained_home);
        let path = verbatim_home.join("tool-output/owner").join(format!("{id}.log"));
        let retained_path = retained_home.join("tool-output/owner").join(format!("{id}.log"));
        let cache = Arc::new(SnapshotCache(Mutex::default(), Duration::from_secs(60), Arc::new(Semaphore::new(MAX_ENTRIES))));
        let loads = AtomicUsize::new(0);
        let load = || cache.load(&path, id, || {
            loads.fetch_add(1, Ordering::Relaxed);
            super::super::load_tool_output_snapshot_uncached(&path, id)
        });
        let first = load().unwrap();
        assert_eq!(cache.0.lock().unwrap().entries.len(), 1);
        let mutation = cache.mutating(&retained_path);
        assert!(cache.0.lock().unwrap().entries.is_empty());
        let overlapping = cache.mutating(&path);
        drop(mutation);
        assert_eq!(load().unwrap().bytes, canonical.bytes);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
        assert!(cache.0.lock().unwrap().entries.is_empty(), "an overlapping owner still fences insertion");
        std::fs::remove_file(&retained_path).unwrap();
        drop(overlapping);
        assert!(matches!(load(), Err(ReadToolOutputError::Expired)));
        assert_eq!(first.bytes, canonical.bytes, "caller snapshots do not retain native leases");
    }

    #[tokio::test]
    async fn the_retention_owner_evicts_cached_residency_before_writer_admission() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"retention".to_vec());
        let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
        let id = artifact.id.unwrap();
        let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
        let verbatim_home = std::fs::canonicalize(home.path()).unwrap();
        let path = super::super::normalized_retention_path(&path);
        let snapshot = super::super::load_tool_output_snapshot(&verbatim_home, "owner", &id.to_string()).await.unwrap();
        assert!(matches!(super::super::remove_inactive_output_path_blocking(path),
            super::super::InactiveRemovalOutcome::RemovedOrAbsent));
        let missing = super::super::load_tool_output_snapshot(home.path(), "owner", &id.to_string()).await;
        assert!(matches!(missing, Err(ReadToolOutputError::Expired)));
        assert_eq!(snapshot.bytes, b"retention");
    }

    #[tokio::test]
    async fn durable_barrier_releases_recovery_residency_without_reusing_changed_bytes() {
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"original".to_vec());
        let artifact = super::super::create_canonical_output_artifact(home.path(), "barrier", &canonical).await;
        let id = artifact.id.unwrap().to_string();
        let path = home.path().join("tool-output/barrier").join(format!("{id}.log"));
        // Queue the same dirty-file barrier used by captured artifact writes.
        super::super::defer_artifact_sync(&path);
        let snapshot = super::super::load_tool_output_snapshot(home.path(), "barrier", &id).await.unwrap();
        assert_eq!(snapshot.bytes, b"original");
        assert!(File::options().write(true).open(&path).is_err(), "fixture holds write exclusion");
        super::super::sync_tool_output_artifacts(home.path(), "barrier").await.unwrap();
        assert!(!super::super::lock_retention_registry().pending_sync.contains_key(&path));
        std::fs::write(&path, b"tampered").unwrap();
        assert_eq!(snapshot.bytes, b"original", "returned bytes remain immutable");
        assert!(super::super::load_tool_output_snapshot(home.path(), "barrier", &id).await.is_err(),
            "a post-barrier read must reauthenticate, not reuse a released lease");
    }

    #[tokio::test]
    async fn an_existing_writer_keeps_full_authentication_instead_of_a_cache_lease() {
        use std::io::Write;
        let home = tempfile::tempdir().unwrap();
        let canonical = codex_tools::CanonicalToolResult::bytes(b"original".to_vec());
        let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
        let id = artifact.id.unwrap();
        let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
        let mut writer = File::options().write(true).open(&path).unwrap();
        let cache = Arc::new(SnapshotCache::default());
        let first = cache.load(&path, id, || super::super::load_tool_output_snapshot_uncached(&path, id)).unwrap();
        assert_eq!(first.bytes, b"original");
        assert!(cache.0.lock().unwrap().entries.is_empty());
        writer.write_all(b"changed!").unwrap();
        assert!(cache.load(&path, id, || super::super::load_tool_output_snapshot_uncached(&path, id)).is_err());
    }

    #[test]
    fn runtime_shutdown_releases_even_an_unpolled_cache_cleanup_owner() {
        let home = tempfile::tempdir().unwrap();
        let cache = Arc::new(SnapshotCache::default());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (path, snapshot) = runtime.block_on(async {
            let canonical = codex_tools::CanonicalToolResult::bytes(b"shutdown".to_vec());
            let artifact = super::super::create_canonical_output_artifact(home.path(), "owner", &canonical).await;
            let id = artifact.id.unwrap();
            let path = home.path().join("tool-output/owner").join(format!("{id}.log"));
            let snapshot = cache.load(&path, id, || super::super::load_tool_output_snapshot_uncached(&path, id)).unwrap();
            assert!(!cache.0.lock().unwrap().entries.is_empty());
            (path, snapshot)
        });
        drop(runtime);
        assert!(cache.0.lock().unwrap().entries.is_empty());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(snapshot.bytes, b"shutdown");
    }
}
