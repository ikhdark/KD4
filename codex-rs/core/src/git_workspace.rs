use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_git_utils::get_git_remote_urls_assume_git_repo;
use codex_git_utils::get_git_repo_root;
use codex_git_utils::get_git_repo_root_with_fs;
use codex_git_utils::get_has_changes;
use codex_git_utils::get_head_commit_hash;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::warn;

use crate::environment_selection::TurnEnvironmentSnapshot;

const GIT_DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(5);
const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
const SOURCE_CHANGE_JOURNAL_CAPACITY: usize = 4_096;
static NEXT_WATCHER_EPOCH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct EnvironmentWorkspaceKey {
    environment_id: String,
    cwd: AbsolutePathBuf,
    remote: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RootCacheKey {
    environment_generation: u64,
    environments: Vec<EnvironmentWorkspaceKey>,
}

#[derive(Clone, Debug)]
struct GitWorkspaceEntry {
    environment_id: String,
    cwd: AbsolutePathBuf,
    repo_root: Option<AbsolutePathBuf>,
    remote: bool,
}

/// Shared, stable workspace identity for one environment generation.
///
/// The snapshot deliberately excludes worktree dirtiness. Local Git metadata is
/// resolved lazily through [`GitWorkspaceMetadataSource`] and dirtiness is read
/// fresh for every enrichment.
#[derive(Clone)]
pub(crate) struct GitWorkspaceSnapshot {
    environment_generation: u64,
    entries: Vec<GitWorkspaceEntry>,
    cache: Arc<GitWorkspaceCache>,
}

impl std::fmt::Debug for GitWorkspaceSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitWorkspaceSnapshot")
            .field("environment_generation", &self.environment_generation)
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl GitWorkspaceSnapshot {
    pub(crate) fn display_roots(&self) -> Vec<(String, PathBuf)> {
        self.entries
            .iter()
            .map(|entry| {
                (
                    entry.environment_id.clone(),
                    entry.repo_root.as_ref().unwrap_or(&entry.cwd).to_path_buf(),
                )
            })
            .collect()
    }

    pub(crate) fn primary_is_git(&self) -> Option<bool> {
        self.entries.first().map(|entry| entry.repo_root.is_some())
    }

    pub(crate) fn primary_local_metadata_source(&self) -> Option<GitWorkspaceMetadataSource> {
        let entry = self.entries.first()?;
        if entry.remote {
            return None;
        }
        let repo_root = entry.repo_root.clone()?;
        Some(GitWorkspaceMetadataSource {
            cwd: entry.cwd.clone(),
            repo_root,
            cache: Arc::clone(&self.cache),
        })
    }
}

#[derive(Clone)]
pub(crate) struct GitWorkspaceMetadataSource {
    cwd: AbsolutePathBuf,
    repo_root: AbsolutePathBuf,
    cache: Arc<GitWorkspaceCache>,
}

impl std::fmt::Debug for GitWorkspaceMetadataSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitWorkspaceMetadataSource")
            .field("cwd", &self.cwd)
            .field("repo_root", &self.repo_root)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct GitWorkspaceMetadata {
    pub(crate) associated_remote_urls: Option<BTreeMap<String, String>>,
    pub(crate) latest_git_commit_hash: Option<String>,
    pub(crate) has_changes: Option<bool>,
}

/// One immutable, read-only Git observation used by the completion candidate.
/// The caller owns artifact retention and checkpoint preview bounds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidateDiffCapture {
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) raw_diff: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkspaceEvidenceIdentity {
    #[serde(default)]
    pub(crate) repository_root: Option<String>,
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
}

pub(crate) async fn capture_workspace_evidence_identity(
    cwd: &Path,
) -> Option<WorkspaceEvidenceIdentity> {
    let repo_root = get_git_repo_root(cwd)?;
    let capture = capture_candidate_diff(cwd).await?;
    Some(WorkspaceEvidenceIdentity {
        repository_root: Some(
            dunce::canonicalize(&repo_root)
                .unwrap_or(repo_root)
                .to_string_lossy()
                .into_owned(),
        ),
        head_identity: capture.head_identity,
        index_identity: capture.index_identity,
        worktree_identity: capture.worktree_identity,
    })
}

pub(crate) async fn capture_candidate_diff(cwd: &Path) -> Option<CandidateDiffCapture> {
    let repo_root = get_git_repo_root(cwd)?;
    let (head, index_diff, worktree_diff, index_paths, worktree_paths, untracked) = tokio::join!(
        candidate_git_output(&repo_root, &["rev-parse", "--verify", "HEAD"]),
        candidate_git_output(
            &repo_root,
            &["diff", "--cached", "--binary", "--no-ext-diff"]
        ),
        candidate_git_output(&repo_root, &["diff", "--binary", "--no-ext-diff"]),
        candidate_git_output(
            &repo_root,
            &["diff", "--cached", "--name-only", "-z", "--no-ext-diff"],
        ),
        candidate_git_output(&repo_root, &["diff", "--name-only", "-z", "--no-ext-diff"],),
        candidate_git_output(
            &repo_root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        ),
    );
    let index_diff = index_diff?;
    let worktree_diff = worktree_diff?;
    let index_paths = index_paths?;
    let worktree_paths = worktree_paths?;
    let untracked = untracked?;
    let head_identity = head.and_then(|head| {
        String::from_utf8(head)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    let index_identity = Some(format!("{:x}", Sha256::digest(&index_diff)));
    let untracked_paths = untracked
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .ok()?
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let untracked_manifest =
        candidate_untracked_manifest(repo_root.clone(), untracked_paths.clone()).await?;
    let mut worktree_hasher = Sha256::new();
    worktree_hasher.update(&worktree_diff);
    worktree_hasher.update(&untracked_manifest);
    let worktree_identity = Some(format!("{:x}", worktree_hasher.finalize()));
    let mut changed_paths = candidate_changed_paths(&index_paths)?;
    changed_paths.extend(candidate_changed_paths(&worktree_paths)?);
    changed_paths.extend(untracked_paths);
    changed_paths.sort();
    changed_paths.dedup();
    let mut raw_diff = Vec::with_capacity(index_diff.len().saturating_add(worktree_diff.len()));
    raw_diff.extend_from_slice(b"KD4_CANDIDATE_INDEX_DIFF_V1\n");
    raw_diff.extend_from_slice(&index_diff);
    raw_diff.extend_from_slice(b"\nKD4_CANDIDATE_WORKTREE_DIFF_V1\n");
    raw_diff.extend_from_slice(&worktree_diff);
    raw_diff.extend_from_slice(b"\nKD4_CANDIDATE_UNTRACKED_MANIFEST_V1\n");
    raw_diff.extend_from_slice(&untracked_manifest);
    Some(CandidateDiffCapture {
        head_identity,
        index_identity,
        worktree_identity,
        changed_paths,
        raw_diff,
    })
}

async fn candidate_untracked_manifest(
    repo_root: PathBuf,
    mut paths: Vec<String>,
) -> Option<Vec<u8>> {
    paths.sort();
    paths.dedup();
    tokio::task::spawn_blocking(move || {
        let mut manifest = Vec::new();
        for path in paths {
            let absolute = repo_root.join(&path);
            let metadata = std::fs::symlink_metadata(&absolute).ok()?;
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&absolute).ok()?;
                let target = target.to_str()?;
                manifest.extend_from_slice(path.as_bytes());
                manifest.push(0);
                manifest.extend_from_slice(b"symlink");
                manifest.push(0);
                manifest.extend_from_slice(
                    format!("{:x}", Sha256::digest(target.as_bytes())).as_bytes(),
                );
                manifest.push(b'\n');
                continue;
            }
            if !metadata.is_file() {
                return None;
            }
            let mut file = File::open(&absolute).ok()?;
            let mut hasher = Sha256::new();
            let mut bytes = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                bytes = bytes.saturating_add(u64::try_from(read).ok()?);
                hasher.update(&buffer[..read]);
            }
            manifest.extend_from_slice(path.as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(bytes.to_string().as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(format!("{:x}", hasher.finalize()).as_bytes());
            manifest.push(b'\n');
        }
        Some(manifest)
    })
    .await
    .ok()?
}

fn candidate_changed_paths(output: &[u8]) -> Option<Vec<String>> {
    output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(std::str::from_utf8)
        .map(|path| path.map(str::to_string))
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

async fn candidate_git_output(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo_root)
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

impl GitWorkspaceMetadataSource {
    pub(crate) fn discover_local(cwd: AbsolutePathBuf) -> Option<Self> {
        let repo_root =
            AbsolutePathBuf::from_absolute_path(get_git_repo_root(cwd.as_path())?).ok()?;
        Some(Self {
            cwd,
            repo_root,
            cache: GitWorkspaceCache::new(),
        })
    }

    pub(crate) fn repo_root(&self) -> &AbsolutePathBuf {
        &self.repo_root
    }

    pub(crate) async fn metadata(&self) -> GitWorkspaceMetadata {
        let (stable, has_changes) = tokio::join!(
            self.cache.stable_metadata(self),
            get_has_changes(self.cwd.as_path()),
        );
        GitWorkspaceMetadata {
            associated_remote_urls: stable.associated_remote_urls,
            latest_git_commit_hash: stable.latest_git_commit_hash,
            has_changes,
        }
    }

    /// Return the repository's root-history namespace using the same bounded
    /// watcher-backed cache as the rest of the stable workspace metadata.
    ///
    /// This deliberately excludes worktree and index observations. When the
    /// watcher cannot prove freshness, the cache fails open and recomputes.
    pub(crate) async fn project_namespace(&self) -> Option<String> {
        self.cache.project_namespace(self).await
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StableGitMetadata {
    associated_remote_urls: Option<BTreeMap<String, String>>,
    latest_git_commit_hash: Option<String>,
}

struct RootCacheEntry {
    key: RootCacheKey,
    dependencies: Vec<DependencyFingerprint>,
    watcher_generation: u64,
    entries: Vec<GitWorkspaceEntry>,
    _registration: WatchRegistration,
}

struct MetadataCacheEntry {
    dependencies: StableMetadataDependencies,
    watcher_generation: u64,
    metadata: StableGitMetadata,
    _registration: WatchRegistration,
}

struct ProjectNamespaceCacheEntry {
    dependencies: StableMetadataDependencies,
    watcher_generation: u64,
    namespace: Option<String>,
    _registration: WatchRegistration,
}

#[derive(Default)]
struct GitWorkspaceCacheState {
    root: Option<RootCacheEntry>,
    metadata: HashMap<PathBuf, MetadataCacheEntry>,
    project_namespaces: HashMap<PathBuf, ProjectNamespaceCacheEntry>,
}

pub(crate) struct GitWorkspaceCache {
    state: Mutex<GitWorkspaceCacheState>,
    watcher_epoch: u64,
    watcher_generation: AtomicU64,
    host_mutation_generation: AtomicU64,
    watcher_reliable: AtomicBool,
    watcher_subscriber: Option<FileWatcherSubscriber>,
    source_watch_registrations: StdMutex<HashMap<PathBuf, WatchRegistration>>,
    source_change_journal: StdMutex<SourceChangeJournal>,
    #[cfg(test)]
    workspace_evidence_capture_count: AtomicU64,
}

pub(crate) struct WorkspaceChangeObservation {
    watcher_generation: u64,
    host_mutation_generation: u64,
    _registration: WatchRegistration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SourcePathChangeObservation {
    watcher_epoch: u64,
    watcher_generation: u64,
    repo_root: PathBuf,
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
}

#[derive(Clone, Debug)]
struct SourceChangeEvent {
    generation: u64,
    changed_paths: Option<Vec<PathBuf>>,
}

#[derive(Default)]
struct SourceChangeJournal {
    retained_floor: u64,
    latest_generation: u64,
    events: VecDeque<SourceChangeEvent>,
}

impl GitWorkspaceCache {
    pub(crate) fn new() -> Arc<Self> {
        if tokio::runtime::Handle::try_current().is_err() {
            warn!("Git workspace cache disabled because no Tokio runtime is available");
            return Self::with_watcher(None);
        }
        match FileWatcher::new() {
            Ok(watcher) => Self::with_watcher(Some(Arc::new(watcher))),
            Err(err) => {
                warn!("Git workspace cache disabled because file watching is unavailable: {err}");
                Self::with_watcher(None)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_noop_watcher_for_tests() -> Arc<Self> {
        Self::with_watcher(Some(Arc::new(FileWatcher::noop())))
    }

    fn with_watcher(watcher: Option<Arc<FileWatcher>>) -> Arc<Self> {
        let (watcher_subscriber, receiver) = match watcher {
            Some(watcher) => {
                let (subscriber, receiver) = watcher.add_subscriber();
                (Some(subscriber), Some(receiver))
            }
            None => (None, None),
        };
        let watcher_epoch = (u64::from(std::process::id()) << 32)
            | NEXT_WATCHER_EPOCH.fetch_add(1, Ordering::Relaxed);
        let cache = Arc::new(Self {
            state: Mutex::new(GitWorkspaceCacheState::default()),
            watcher_epoch,
            watcher_generation: AtomicU64::new(0),
            host_mutation_generation: AtomicU64::new(0),
            watcher_reliable: AtomicBool::new(watcher_subscriber.is_some()),
            watcher_subscriber,
            source_watch_registrations: StdMutex::new(HashMap::new()),
            source_change_journal: StdMutex::new(SourceChangeJournal::default()),
            #[cfg(test)]
            workspace_evidence_capture_count: AtomicU64::new(0),
        });
        if let Some(mut receiver) = receiver {
            let weak_cache = Arc::downgrade(&cache);
            tokio::spawn(async move {
                while let Some(event) = receiver.recv().await {
                    let Some(cache) = weak_cache.upgrade() else {
                        return;
                    };
                    let changed_paths = (!event.rescan_required).then_some(event.paths);
                    cache.record_source_change_event(changed_paths);
                }
                if let Some(cache) = weak_cache.upgrade() {
                    cache.invalidate_for_watcher_failure().await;
                }
            });
        }
        cache
    }

    async fn invalidate_for_watcher_failure(&self) {
        self.watcher_reliable.store(false, Ordering::Release);
        self.watcher_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.state.lock().await;
        state.root = None;
        state.metadata.clear();
        state.project_namespaces.clear();
    }

    /// Returns a freshly captured content-based workspace identity.
    ///
    /// Watcher delivery is asynchronous, so a watcher generation cannot prove
    /// that an external edit has not happened but is still waiting in the
    /// event queue. Evidence freshness therefore must not reuse the metadata
    /// caches used by non-authoritative repository discovery.
    pub(crate) async fn workspace_evidence_identity(
        &self,
        cwd: &Path,
    ) -> Option<WorkspaceEvidenceIdentity> {
        #[cfg(test)]
        self.workspace_evidence_capture_count
            .fetch_add(1, Ordering::AcqRel);
        capture_workspace_evidence_identity(cwd).await
    }

    #[cfg(test)]
    pub(crate) fn workspace_evidence_capture_count(&self) -> u64 {
        self.workspace_evidence_capture_count
            .load(Ordering::Acquire)
    }

    pub(crate) async fn snapshot(
        self: &Arc<Self>,
        environments: &TurnEnvironmentSnapshot,
    ) -> GitWorkspaceSnapshot {
        let key = RootCacheKey {
            environment_generation: environments.generation,
            environments: environments
                .turn_environments
                .iter()
                .filter_map(|environment| {
                    Some(EnvironmentWorkspaceKey {
                        environment_id: environment.environment_id.clone(),
                        cwd: environment.cwd().to_abs_path().ok()?,
                        remote: environment.environment.is_remote(),
                    })
                })
                .collect(),
        };
        let cacheable = environments.starting.is_empty()
            && !key
                .environments
                .iter()
                .any(|environment| environment.remote)
            && self.watcher_reliable.load(Ordering::Acquire);
        let dependencies = cacheable
            .then(|| root_dependencies(&key.environments))
            .flatten();
        let watcher_generation = self.watcher_generation.load(Ordering::Acquire);

        if let Some(dependencies) = dependencies.as_ref() {
            let state = self.state.lock().await;
            if let Some(entry) = state.root.as_ref()
                && entry.key == key
                && entry.watcher_generation == watcher_generation
                && entry.dependencies == *dependencies
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                return GitWorkspaceSnapshot {
                    environment_generation: key.environment_generation,
                    entries: entry.entries.clone(),
                    cache: Arc::clone(self),
                };
            }
        }

        let mut entries = Vec::with_capacity(key.environments.len());
        for (environment, key_environment) in environments
            .turn_environments
            .iter()
            .filter(|environment| environment.cwd().to_abs_path().is_ok())
            .zip(&key.environments)
        {
            let repo_root = get_git_repo_root_with_fs(
                environment.environment.get_filesystem().as_ref(),
                &key_environment.cwd,
            )
            .await;
            entries.push(GitWorkspaceEntry {
                environment_id: key_environment.environment_id.clone(),
                cwd: key_environment.cwd.clone(),
                repo_root,
                remote: key_environment.remote,
            });
        }

        if let Some(before_dependencies) = dependencies {
            let after_dependencies = root_dependencies(&key.environments);
            if after_dependencies.as_ref() == Some(&before_dependencies)
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                let registration = self.register_dependencies(&before_dependencies);
                self.state.lock().await.root = Some(RootCacheEntry {
                    key: key.clone(),
                    dependencies: before_dependencies,
                    watcher_generation,
                    entries: entries.clone(),
                    _registration: registration,
                });
            }
        }

        GitWorkspaceSnapshot {
            environment_generation: key.environment_generation,
            entries,
            cache: Arc::clone(self),
        }
    }

    async fn stable_metadata(&self, source: &GitWorkspaceMetadataSource) -> StableGitMetadata {
        let watcher_generation = self.watcher_generation.load(Ordering::Acquire);
        let dependencies = StableMetadataDependencies::capture(source).await;
        if self.watcher_reliable.load(Ordering::Acquire)
            && let Some(dependencies) = dependencies.as_ref()
        {
            let state = self.state.lock().await;
            if let Some(entry) = state.metadata.get(source.repo_root.as_path())
                && entry.watcher_generation == watcher_generation
                && entry.dependencies == *dependencies
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                return entry.metadata.clone();
            }
        }

        let (head_commit_hash, associated_remote_urls) = tokio::join!(
            get_head_commit_hash(source.cwd.as_path()),
            get_git_remote_urls_assume_git_repo(source.cwd.as_path()),
        );
        let metadata = StableGitMetadata {
            associated_remote_urls,
            latest_git_commit_hash: head_commit_hash.map(|sha| sha.0),
        };

        if let Some(before_dependencies) = dependencies {
            let after_dependencies = StableMetadataDependencies::capture(source).await;
            if after_dependencies.as_ref() == Some(&before_dependencies)
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                let registration = self.register_dependencies(&before_dependencies.files);
                self.state.lock().await.metadata.insert(
                    source.repo_root.to_path_buf(),
                    MetadataCacheEntry {
                        dependencies: before_dependencies,
                        watcher_generation,
                        metadata: metadata.clone(),
                        _registration: registration,
                    },
                );
            }
        }
        metadata
    }

    async fn project_namespace(&self, source: &GitWorkspaceMetadataSource) -> Option<String> {
        let watcher_generation = self.watcher_generation.load(Ordering::Acquire);
        let dependencies = StableMetadataDependencies::capture_project_namespace(source);
        if self.watcher_reliable.load(Ordering::Acquire)
            && let Some(dependencies) = dependencies.as_ref()
        {
            let state = self.state.lock().await;
            if let Some(entry) = state.project_namespaces.get(source.repo_root.as_path())
                && entry.watcher_generation == watcher_generation
                && entry.dependencies == *dependencies
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                return entry.namespace.clone();
            }
        }

        let namespace = collect_project_namespace(source.cwd.as_path()).await;
        if let Some(before_dependencies) = dependencies {
            let after_dependencies = StableMetadataDependencies::capture_project_namespace(source);
            if after_dependencies.as_ref() == Some(&before_dependencies)
                && self.watcher_reliable.load(Ordering::Acquire)
                && self.watcher_generation.load(Ordering::Acquire) == watcher_generation
            {
                let registration = self.register_dependencies(&before_dependencies.files);
                self.state.lock().await.project_namespaces.insert(
                    source.repo_root.to_path_buf(),
                    ProjectNamespaceCacheEntry {
                        dependencies: before_dependencies,
                        watcher_generation,
                        namespace: namespace.clone(),
                        _registration: registration,
                    },
                );
            }
        }
        namespace
    }

    fn register_dependencies(&self, dependencies: &[DependencyFingerprint]) -> WatchRegistration {
        let Some(subscriber) = self.watcher_subscriber.as_ref() else {
            return WatchRegistration::default();
        };
        match subscriber.register_paths(
            dependencies
                .iter()
                .map(|dependency| WatchPath {
                    path: dependency.path.clone(),
                    recursive: false,
                })
                .collect(),
        ) {
            Ok(registration) => registration,
            Err(err) => {
                warn!("Git workspace cache disabled after watch registration failed: {err}");
                self.watcher_reliable.store(false, Ordering::Release);
                self.watcher_generation.fetch_add(1, Ordering::AcqRel);
                WatchRegistration::default()
            }
        }
    }

    pub(crate) fn reliable_watcher_generation(&self) -> Option<u64> {
        self.watcher_reliable
            .load(Ordering::Acquire)
            .then(|| self.watcher_generation.load(Ordering::Acquire))
    }

    fn record_source_change_event(&self, changed_paths: Option<Vec<PathBuf>>) -> u64 {
        let generation = self
            .watcher_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        journal.latest_generation = generation;
        journal.events.push_back(SourceChangeEvent {
            generation,
            changed_paths,
        });
        while journal.events.len() > SOURCE_CHANGE_JOURNAL_CAPACITY {
            if let Some(removed) = journal.events.pop_front() {
                journal.retained_floor = removed.generation;
            }
        }
        generation
    }

    /// Starts a path-scoped observation backed by a retained recursive watch.
    /// The returned token is safe to persist in model-visible tool output: it
    /// is accepted only by this live watcher epoch and only while the bounded
    /// journal proves that the path and its ancestors were untouched.
    pub(crate) fn begin_source_path_change_observation(
        &self,
        repo_root: &Path,
        path: &Path,
        recursive: bool,
    ) -> Option<SourcePathChangeObservation> {
        if !self.watcher_reliable.load(Ordering::Acquire) {
            return None;
        }
        let repo_root = repo_root
            .canonicalize()
            .unwrap_or_else(|_| repo_root.to_path_buf());
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path_is_same_or_descendant(&path, &repo_root) {
            return None;
        }
        {
            let mut registrations = self
                .source_watch_registrations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !registrations.contains_key(&repo_root) {
                let registration = self
                    .watcher_subscriber
                    .as_ref()?
                    .register_paths(vec![WatchPath {
                        path: repo_root.clone(),
                        recursive: true,
                    }])
                    .ok()?;
                registrations.insert(repo_root.clone(), registration);
            }
        }
        let watcher_generation = self.reliable_watcher_generation()?;
        Some(SourcePathChangeObservation {
            watcher_epoch: self.watcher_epoch,
            watcher_generation,
            repo_root,
            path,
            recursive,
        })
    }

    pub(crate) fn source_path_change_observation_is_current(
        &self,
        observation: &SourcePathChangeObservation,
    ) -> bool {
        if observation.watcher_epoch != self.watcher_epoch
            || !path_is_same_or_descendant(&observation.path, &observation.repo_root)
        {
            return false;
        }
        let Some(current_generation) = self.reliable_watcher_generation() else {
            return false;
        };
        if current_generation < observation.watcher_generation {
            return false;
        }
        let journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if (journal.retained_floor != 0 && observation.watcher_generation <= journal.retained_floor)
            || journal.latest_generation != current_generation
        {
            return false;
        }
        journal
            .events
            .iter()
            .filter(|event| event.generation > observation.watcher_generation)
            .all(|event| {
                event.changed_paths.as_ref().is_some_and(|paths| {
                    paths.iter().all(|changed| {
                        let changed_ancestor =
                            path_is_same_or_descendant(&observation.path, changed);
                        let changed_descendant = observation.recursive
                            && path_is_same_or_descendant(changed, &observation.path);
                        !changed_ancestor && !changed_descendant
                    })
                })
            })
    }

    pub(crate) fn begin_workspace_change_observation(
        &self,
        repo_root: &Path,
    ) -> Option<WorkspaceChangeObservation> {
        let watcher_generation = self.reliable_watcher_generation()?;
        let host_mutation_generation = self.host_mutation_generation.load(Ordering::Acquire);
        let registration = self
            .watcher_subscriber
            .as_ref()?
            .register_paths(vec![WatchPath {
                path: repo_root.to_path_buf(),
                recursive: true,
            }])
            .ok()?;
        if self.reliable_watcher_generation() != Some(watcher_generation)
            || self.host_mutation_generation.load(Ordering::Acquire) != host_mutation_generation
        {
            return None;
        }
        Some(WorkspaceChangeObservation {
            watcher_generation,
            host_mutation_generation,
            _registration: registration,
        })
    }

    pub(crate) fn workspace_change_observation_is_current(
        &self,
        observation: &WorkspaceChangeObservation,
    ) -> bool {
        self.reliable_watcher_generation() == Some(observation.watcher_generation)
            && self.host_mutation_generation.load(Ordering::Acquire)
                == observation.host_mutation_generation
    }

    pub(crate) fn note_host_workspace_mutation(&self) {
        self.host_mutation_generation.fetch_add(1, Ordering::AcqRel);
        self.record_source_change_event(None);
    }

    pub(crate) fn note_host_workspace_mutation_paths(
        &self,
        repo_root: &Path,
        changed_paths: &[String],
    ) {
        self.host_mutation_generation.fetch_add(1, Ordering::AcqRel);
        let paths = (!changed_paths.is_empty()).then(|| {
            changed_paths
                .iter()
                .map(|path| {
                    let path = Path::new(path);
                    if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        repo_root.join(path)
                    }
                })
                .collect()
        });
        self.record_source_change_event(paths);
    }
}

fn path_is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    let normalize = |path: &Path| {
        let value = path.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            value.to_lowercase()
        } else {
            value
        }
    };
    let path = normalize(path);
    let ancestor = normalize(ancestor);
    path == ancestor
        || path
            .strip_prefix(&ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StableMetadataDependencies {
    files: Vec<DependencyFingerprint>,
    config_signature: [u8; 32],
}

impl StableMetadataDependencies {
    async fn capture(source: &GitWorkspaceMetadataSource) -> Option<Self> {
        let executable = which::which("git").ok()?;
        let executable = executable.canonicalize().unwrap_or(executable);
        let (git_dir, common_dir, head_ref) = resolve_git_dirs(&source.repo_root)?;
        let mut paths = vec![
            (executable.clone(), false),
            (source.repo_root.join(".git").into_path_buf(), true),
            (git_dir.join("HEAD"), true),
            (git_dir.join("commondir"), true),
            (git_dir.join("config.worktree"), true),
            (common_dir.join("config"), true),
            (common_dir.join("packed-refs"), true),
            (common_dir.join("reftable").join("tables.list"), true),
            (common_dir.join("shallow"), true),
            (common_dir.join("info").join("grafts"), true),
            (common_dir.join("refs").join("replace"), false),
        ];
        if let Some(head_ref) = head_ref {
            paths.push((common_dir.join(head_ref), true));
        }
        paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        paths.dedup_by(|left, right| left.0 == right.0);
        let files = paths
            .into_iter()
            .map(|(path, hash_contents)| dependency_fingerprint(path, hash_contents))
            .collect::<Option<Vec<_>>>()?;
        let config_signature = git_config_signature(&executable, source.cwd.as_path()).await?;
        Some(Self {
            files,
            config_signature,
        })
    }

    fn capture_project_namespace(source: &GitWorkspaceMetadataSource) -> Option<Self> {
        let executable = which::which("git").ok()?;
        let executable = executable.canonicalize().unwrap_or(executable);
        let git_marker = source.repo_root.join(".git").into_path_buf();
        let (git_dir, common_dir, head_ref) = resolve_git_dirs(&source.repo_root)?;
        let mut paths = vec![
            (executable, false),
            (git_dir.join("HEAD"), true),
            (git_dir.join("commondir"), true),
            (git_dir.join("config.worktree"), true),
            (common_dir.join("config"), true),
            (common_dir.join("packed-refs"), true),
            (common_dir.join("reftable").join("tables.list"), true),
            (common_dir.join("shallow"), true),
            (common_dir.join("info").join("grafts"), true),
            (common_dir.join("refs").join("replace"), false),
        ];
        if git_marker.is_file() {
            paths.push((git_marker, true));
        }
        if let Some(head_ref) = head_ref {
            paths.push((common_dir.join(head_ref), true));
        }
        paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        paths.dedup_by(|left, right| left.0 == right.0);
        let files = paths
            .into_iter()
            .map(|(path, hash_contents)| dependency_fingerprint(path, hash_contents))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            files,
            // Namespace dependencies are represented by the repository files above. Avoid a
            // separate `git config --list` process on every cache lookup.
            config_signature: [0; 32],
        })
    }
}

async fn collect_project_namespace(cwd: &Path) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .current_dir(cwd)
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let mut roots = stdout.lines().map(str::to_owned).collect::<Vec<_>>();
    if roots.is_empty()
        || roots.iter().any(|root| {
            root.len() < 40 || root.len() > 64 || !root.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return None;
    }
    roots.sort_unstable();
    Some(format!(
        "{:x}",
        Sha256::digest(format!("git-project-roots-v1\0{}", roots.join("\0")).as_bytes())
    ))
}

fn resolve_git_dirs(repo_root: &AbsolutePathBuf) -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
    let marker = repo_root.join(".git");
    let git_dir = if marker.is_dir() {
        marker.into_path_buf()
    } else {
        let pointer = std::fs::read_to_string(marker.as_path()).ok()?;
        let target = pointer.trim().strip_prefix("gitdir:")?.trim();
        let target = PathBuf::from(target);
        if target.is_absolute() {
            target
        } else {
            repo_root.join(target).into_path_buf()
        }
    };
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|path| PathBuf::from(path.trim()))
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            }
        })
        .unwrap_or_else(|| git_dir.clone());
    let head_ref = std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref:")
                .map(str::trim)
                .map(PathBuf::from)
        });
    Some((git_dir, common_dir, head_ref))
}

async fn git_config_signature(executable: &Path, cwd: &Path) -> Option<[u8; 32]> {
    let mut command = Command::new(executable);
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args([
            "-c",
            "core.fsmonitor=false",
            "config",
            "--includes",
            "--show-origin",
            "--null",
            "--list",
        ])
        .current_dir(cwd)
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| Sha256::digest(output.stdout).into())
}

fn root_dependencies(
    environments: &[EnvironmentWorkspaceKey],
) -> Option<Vec<DependencyFingerprint>> {
    environments
        .iter()
        .flat_map(|environment| environment.cwd.ancestors())
        .map(|ancestor| ancestor.join(".git"))
        .map(|path| dependency_fingerprint(path.into_path_buf(), true))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DependencyFingerprint {
    path: PathBuf,
    state: DependencyState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StableFileIdentity {
    #[cfg(windows)]
    Windows { volume: u64, index: [u8; 16] },
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DependencyState {
    Missing,
    Directory {
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
    },
    File {
        len: u64,
        modified: Option<SystemTime>,
        created: Option<SystemTime>,
        stable_id: Option<StableFileIdentity>,
        digest: Option<[u8; 32]>,
    },
}

fn dependency_fingerprint(path: PathBuf, hash_contents: bool) -> Option<DependencyFingerprint> {
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Some(DependencyFingerprint {
            path,
            state: DependencyState::Directory {
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
            },
        }),
        Ok(metadata) if metadata.is_file() => {
            let file = File::open(&path).ok()?;
            let digest = hash_contents.then(|| std::fs::read(&path).ok()).flatten();
            if hash_contents && digest.is_none() {
                return None;
            }
            Some(DependencyFingerprint {
                path,
                state: DependencyState::File {
                    len: metadata.len(),
                    modified: metadata.modified().ok(),
                    created: metadata.created().ok(),
                    stable_id: stable_file_identity(&file),
                    digest: digest.map(|contents| Sha256::digest(contents).into()),
                },
            })
        }
        Ok(_) => None,
        Err(err) if err.kind() == ErrorKind::NotFound => Some(DependencyFingerprint {
            path,
            state: DependencyState::Missing,
        }),
        Err(_) => None,
    }
}

#[cfg(windows)]
fn stable_file_identity(file: &File) -> Option<StableFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as isize,
            FileIdInfo,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if success == 0 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let index = info.FileId.Identifier;
    (info.VolumeSerialNumber != 0 || index.iter().any(|byte| *byte != 0)).then_some(
        StableFileIdentity::Windows {
            volume: info.VolumeSerialNumber,
            index,
        },
    )
}

#[cfg(unix)]
fn stable_file_identity(file: &File) -> Option<StableFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().ok()?;
    Some(StableFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(any(windows, unix)))]
fn stable_file_identity(_file: &File) -> Option<StableFileIdentity> {
    None
}

#[cfg(test)]
#[path = "git_workspace_tests.rs"]
mod tests;
