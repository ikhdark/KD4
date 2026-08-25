use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
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

use codex_config::ProjectDiscoveryContext;
use codex_file_system::ExecutorFileSystem;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_git_utils::DISABLED_HOOKS_PATH;
use codex_git_utils::get_git_remote_urls_assume_git_repo;
use codex_git_utils::get_git_repo_root;
use codex_git_utils::get_git_repo_root_with_fs;
use codex_git_utils::get_has_changes;
use codex_git_utils::get_head_commit_hash;
use codex_otel::MetricsClient;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::future::Shared;
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
const SOURCE_CHANGE_JOURNAL_CAPACITY: usize = 4_096;
const GENERATED_CODEX_EVAL_PATHSPEC: &str = ":(exclude).codex/evals/**";
const PROJECT_DISCOVERY_REUSE_METRIC: &str = "codex.project_discovery_reuse";
const ROOT_DISCOVERY_CONCURRENCY: usize = 4;
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct GitWorkspaceMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) associated_remote_urls: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latest_git_commit_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) has_changes: Option<bool>,
}

impl GitWorkspaceMetadata {
    pub(crate) fn is_empty(&self) -> bool {
        self.associated_remote_urls.is_none()
            && self.latest_git_commit_hash.is_none()
            && self.has_changes.is_none()
    }
}

/// One immutable, read-only Git observation used by the completion candidate.
/// The caller owns artifact retention and checkpoint preview bounds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CandidateDiffCapture {
    repository_root: Option<String>,
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) raw_diff: Vec<u8>,
}

impl CandidateDiffCapture {
    pub(crate) fn workspace_evidence_identity(&self) -> WorkspaceEvidenceIdentity {
        WorkspaceEvidenceIdentity {
            repository_root: self.repository_root.clone(),
            head_identity: self.head_identity.clone(),
            index_identity: self.index_identity.clone(),
            worktree_identity: self.worktree_identity.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkspaceEvidenceIdentity {
    #[serde(default)]
    pub(crate) repository_root: Option<String>,
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum WorkspaceEvidenceGitDependency {
    Head,
    Index,
    Worktree,
    Untracked,
}

impl WorkspaceEvidenceGitDependency {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Head => "head",
            Self::Index => "index",
            Self::Worktree => "worktree",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkspaceEvidenceCapture {
    pub(crate) identity: Option<WorkspaceEvidenceIdentity>,
    pub(crate) timed_out_git_dependencies: Vec<WorkspaceEvidenceGitDependency>,
}

#[derive(Debug)]
enum CandidateGitOutput {
    Output(Vec<u8>),
    Failed,
    TimedOut,
}

impl CandidateGitOutput {
    fn into_output(self) -> Option<Vec<u8>> {
        match self {
            Self::Output(output) => Some(output),
            Self::Failed | Self::TimedOut => None,
        }
    }
}

pub(crate) async fn capture_workspace_evidence_identity(
    cwd: &Path,
) -> Option<WorkspaceEvidenceIdentity> {
    let repo_root = get_git_repo_root(cwd)?;
    stable_workspace_capture(|| capture_candidate_diff_once(&repo_root, None))
        .await
        .map(|capture| capture.workspace_evidence_identity())
}

async fn capture_workspace_evidence_identity_with_attribution(
    cwd: &Path,
) -> WorkspaceEvidenceCapture {
    let Some(repo_root) = get_git_repo_root(cwd) else {
        return WorkspaceEvidenceCapture::default();
    };
    let timed_out_git_dependencies = StdMutex::new(BTreeSet::new());
    let identity = stable_workspace_capture(|| {
        capture_candidate_diff_once(&repo_root, Some(&timed_out_git_dependencies))
    })
    .await
    .map(|capture| capture.workspace_evidence_identity());
    let timed_out_git_dependencies = timed_out_git_dependencies
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .into_iter()
        .collect();
    WorkspaceEvidenceCapture {
        identity,
        timed_out_git_dependencies,
    }
}

pub(crate) async fn capture_candidate_diff(cwd: &Path) -> Option<CandidateDiffCapture> {
    let repo_root = get_git_repo_root(cwd)?;
    stable_workspace_capture(|| capture_candidate_diff_once(&repo_root, None)).await
}

async fn capture_candidate_diff_once(
    repo_root: &Path,
    timed_out_git_dependencies: Option<&StdMutex<BTreeSet<WorkspaceEvidenceGitDependency>>>,
) -> Option<CandidateDiffCapture> {
    let (head, index_capture, worktree_capture, untracked) = tokio::join!(
        candidate_git_output(repo_root, &["rev-parse", "--verify", "HEAD"]),
        candidate_git_output(
            repo_root,
            &[
                "diff",
                "--cached",
                "--raw",
                "-z",
                "--patch",
                "--binary",
                "--no-ext-diff",
                "--",
                ".",
                GENERATED_CODEX_EVAL_PATHSPEC,
            ]
        ),
        candidate_git_output(
            repo_root,
            &[
                "diff",
                "--raw",
                "-z",
                "--patch",
                "--binary",
                "--no-ext-diff",
                "--",
                ".",
                GENERATED_CODEX_EVAL_PATHSPEC,
            ]
        ),
        candidate_git_output(
            repo_root,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
                GENERATED_CODEX_EVAL_PATHSPEC,
            ],
        ),
    );
    let timed_out = candidate_git_timeouts(&head, &index_capture, &worktree_capture, &untracked);
    if !timed_out.is_empty()
        && let Some(recorded) = timed_out_git_dependencies
    {
        recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(timed_out);
    }
    let head = head.into_output();
    let index_capture = index_capture.into_output();
    let worktree_capture = worktree_capture.into_output();
    let untracked = untracked.into_output();
    let (index_paths, index_diff) = candidate_diff_and_paths(index_capture?)?;
    let (worktree_paths, worktree_diff) = candidate_diff_and_paths(worktree_capture?)?;
    let untracked = untracked?;
    let head_identity = candidate_head_identity(head);
    let index_identity = Some(format!("{:x}", Sha256::digest(&index_diff)));
    let untracked_paths = candidate_untracked_paths(&untracked)?;
    let (untracked_paths, untracked_manifest) =
        candidate_untracked_manifest(repo_root.to_path_buf(), untracked_paths).await?;
    let mut worktree_hasher = Sha256::new();
    worktree_hasher.update(&worktree_diff);
    worktree_hasher.update(&untracked_manifest);
    let worktree_identity = Some(format!("{:x}", worktree_hasher.finalize()));
    let changed_paths = index_paths
        .into_iter()
        .chain(worktree_paths)
        .chain(untracked_paths)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut raw_diff = Vec::with_capacity(index_diff.len().saturating_add(worktree_diff.len()));
    raw_diff.extend_from_slice(b"KD4_CANDIDATE_INDEX_DIFF_V1\n");
    raw_diff.extend_from_slice(&index_diff);
    raw_diff.extend_from_slice(b"\nKD4_CANDIDATE_WORKTREE_DIFF_V1\n");
    raw_diff.extend_from_slice(&worktree_diff);
    raw_diff.extend_from_slice(b"\nKD4_CANDIDATE_UNTRACKED_MANIFEST_V1\n");
    raw_diff.extend_from_slice(&untracked_manifest);
    Some(CandidateDiffCapture {
        repository_root: Some(
            dunce::canonicalize(repo_root)
                .unwrap_or_else(|_| repo_root.to_path_buf())
                .to_string_lossy()
                .into_owned(),
        ),
        head_identity,
        index_identity,
        worktree_identity,
        changed_paths,
        raw_diff,
    })
}

fn candidate_git_timeouts(
    head: &CandidateGitOutput,
    index: &CandidateGitOutput,
    worktree: &CandidateGitOutput,
    untracked: &CandidateGitOutput,
) -> Vec<WorkspaceEvidenceGitDependency> {
    [
        (WorkspaceEvidenceGitDependency::Head, head),
        (WorkspaceEvidenceGitDependency::Index, index),
        (WorkspaceEvidenceGitDependency::Worktree, worktree),
        (WorkspaceEvidenceGitDependency::Untracked, untracked),
    ]
    .into_iter()
    .filter_map(|(dependency, output)| {
        matches!(output, CandidateGitOutput::TimedOut).then_some(dependency)
    })
    .collect()
}

fn candidate_diff_and_paths(mut output: Vec<u8>) -> Option<(Vec<String>, Vec<u8>)> {
    if output.is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    let mut offset = 0;
    let mut paths = Vec::new();
    while output.get(offset) == Some(&b':') {
        let header_end = output[offset..].iter().position(|byte| *byte == 0)? + offset;
        let header = std::str::from_utf8(&output[offset..header_end]).ok()?;
        let status = header.split_ascii_whitespace().next_back()?;
        let path_start = header_end.saturating_add(1);
        let path_end = output[path_start..].iter().position(|byte| *byte == 0)? + path_start;
        let first_path = std::str::from_utf8(&output[path_start..path_end]).ok()?;
        offset = path_end.saturating_add(1);
        if matches!(status.as_bytes().first(), Some(b'R' | b'C')) {
            paths.push(first_path.to_string());
            let second_path_end = output[offset..].iter().position(|byte| *byte == 0)? + offset;
            paths.push(
                std::str::from_utf8(&output[offset..second_path_end])
                    .ok()?
                    .to_string(),
            );
            offset = second_path_end.saturating_add(1);
        } else {
            paths.push(first_path.to_string());
        }
    }
    if output.get(offset) == Some(&0) {
        offset = offset.saturating_add(1);
    }
    if offset < output.len() && !output[offset..].starts_with(b"diff --git ") {
        return None;
    }
    let patch = output.split_off(offset);
    Some((paths, patch))
}

fn candidate_untracked_paths(output: &[u8]) -> Option<Vec<String>> {
    output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| std::str::from_utf8(entry).map(str::to_string))
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

async fn stable_workspace_capture<T, Capture, CaptureFuture>(mut capture_once: Capture) -> Option<T>
where
    T: Eq,
    Capture: FnMut() -> CaptureFuture,
    CaptureFuture: std::future::Future<Output = Option<T>>,
{
    const MAX_CAPTURE_ATTEMPTS: usize = 3;

    let mut previous = capture_once().await?;
    for _ in 1..MAX_CAPTURE_ATTEMPTS {
        let current = capture_once().await?;
        if current == previous {
            return Some(current);
        }
        previous = current;
    }
    None
}

fn candidate_head_identity(head: Option<Vec<u8>>) -> Option<String> {
    head.and_then(|head| {
        String::from_utf8(head)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

async fn candidate_untracked_manifest(
    repo_root: PathBuf,
    paths: Vec<String>,
) -> Option<(Vec<String>, Vec<u8>)> {
    tokio::task::spawn_blocking(move || {
        let mut paths = paths;
        paths.sort();
        paths.dedup();
        let mut manifest = Vec::new();
        for path in &paths {
            let absolute = repo_root.join(path);
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
        Some((paths, manifest))
    })
    .await
    .ok()?
}

async fn candidate_git_output(repo_root: &Path, args: &[&str]) -> CandidateGitOutput {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo_root)
        .kill_on_drop(true);
    match timeout(GIT_DEPENDENCY_TIMEOUT, command.output()).await {
        Err(_) => CandidateGitOutput::TimedOut,
        Ok(Err(_)) => CandidateGitOutput::Failed,
        Ok(Ok(output)) if output.status.success() => CandidateGitOutput::Output(output.stdout),
        Ok(Ok(_)) => CandidateGitOutput::Failed,
    }
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

#[derive(Clone)]
struct CachedWorkspaceEvidenceIdentity {
    capture_sequence: u64,
    identity: Option<WorkspaceEvidenceIdentity>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkspaceEvidenceCaptureKey {
    repo_root: PathBuf,
    watcher_generation: u64,
    host_mutation_generation: u64,
}

#[derive(Clone)]
struct InFlightWorkspaceEvidenceCapture {
    capture_sequence: u64,
    future: Shared<BoxFuture<'static, WorkspaceEvidenceCapture>>,
}

#[cfg(test)]
struct WorkspaceEvidenceCapturePause {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
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
    latest_workspace_evidence: StdMutex<HashMap<PathBuf, CachedWorkspaceEvidenceIdentity>>,
    in_flight_workspace_evidence:
        StdMutex<HashMap<WorkspaceEvidenceCaptureKey, InFlightWorkspaceEvidenceCapture>>,
    workspace_evidence_capture_sequence: AtomicU64,
    #[cfg(test)]
    workspace_evidence_capture_count: AtomicU64,
    #[cfg(test)]
    next_workspace_evidence_capture_pause: StdMutex<Option<Arc<WorkspaceEvidenceCapturePause>>>,
    #[cfg(test)]
    workspace_evidence_waiter_joined: tokio::sync::Notify,
    #[cfg(test)]
    root_resolution_count: AtomicU64,
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
            latest_workspace_evidence: StdMutex::new(HashMap::new()),
            in_flight_workspace_evidence: StdMutex::new(HashMap::new()),
            workspace_evidence_capture_sequence: AtomicU64::new(0),
            #[cfg(test)]
            workspace_evidence_capture_count: AtomicU64::new(0),
            #[cfg(test)]
            next_workspace_evidence_capture_pause: StdMutex::new(None),
            #[cfg(test)]
            workspace_evidence_waiter_joined: tokio::sync::Notify::new(),
            #[cfg(test)]
            root_resolution_count: AtomicU64::new(0),
        });
        if let Some(mut receiver) = receiver
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let weak_cache = Arc::downgrade(&cache);
            runtime.spawn(async move {
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
    /// Watcher delivery is asynchronous, so an unchanged watcher generation
    /// cannot prove that an external edit is not still waiting in the event
    /// queue. Authoritative evidence therefore never reuses watcher-backed
    /// metadata caches.
    pub(crate) async fn workspace_evidence_identity(
        &self,
        cwd: &Path,
    ) -> Option<WorkspaceEvidenceIdentity> {
        self.workspace_evidence_identity_with_attribution(cwd)
            .await
            .identity
    }

    pub(crate) async fn workspace_evidence_identity_with_attribution(
        &self,
        cwd: &Path,
    ) -> WorkspaceEvidenceCapture {
        let Some(repo_root) = get_git_repo_root(cwd)
            .as_deref()
            .map(canonical_workspace_evidence_root)
        else {
            return WorkspaceEvidenceCapture::default();
        };
        let key = WorkspaceEvidenceCaptureKey {
            repo_root: repo_root.clone(),
            watcher_generation: self.watcher_generation.load(Ordering::Acquire),
            host_mutation_generation: self.host_mutation_generation.load(Ordering::Acquire),
        };
        let (in_flight_capture, coalesced) = {
            let mut in_flight = self
                .in_flight_workspace_evidence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A newer local observation epoch must never join an older
            // capture. Dropping the map's old shared future is cancellation
            // safe because every active caller owns its own clone, and keeps
            // abandoned epochs bounded to one entry per repository.
            in_flight.retain(|existing_key, _| {
                existing_key.repo_root != repo_root || existing_key == &key
            });
            match in_flight.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => (entry.get().clone(), true),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let capture_sequence = self
                        .workspace_evidence_capture_sequence
                        .fetch_add(1, Ordering::AcqRel)
                        .saturating_add(1);
                    #[cfg(test)]
                    self.workspace_evidence_capture_count
                        .fetch_add(1, Ordering::AcqRel);
                    #[cfg(test)]
                    let pause = self
                        .next_workspace_evidence_capture_pause
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    let capture_repo_root = repo_root.clone();
                    let future = async move {
                        #[cfg(test)]
                        if let Some(pause) = pause {
                            pause.started.notify_one();
                            pause.release.notified().await;
                        }
                        capture_workspace_evidence_identity_with_attribution(&capture_repo_root)
                            .await
                    }
                    .boxed()
                    .shared();
                    let capture = InFlightWorkspaceEvidenceCapture {
                        capture_sequence,
                        future,
                    };
                    entry.insert(capture.clone());
                    (capture, false)
                }
            }
        };
        #[cfg(test)]
        if coalesced {
            self.workspace_evidence_waiter_joined.notify_one();
        }
        #[cfg(not(test))]
        let _ = coalesced;
        let capture = in_flight_capture.future.await;
        {
            let mut in_flight = self
                .in_flight_workspace_evidence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if in_flight.get(&key).is_some_and(|current| {
                current.capture_sequence == in_flight_capture.capture_sequence
            }) {
                in_flight.remove(&key);
            }
        }
        let capture_sequence = in_flight_capture.capture_sequence;
        let mut latest = self
            .latest_workspace_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if latest
            .get(&repo_root)
            .is_none_or(|cached| cached.capture_sequence <= capture_sequence)
        {
            latest.insert(
                repo_root,
                CachedWorkspaceEvidenceIdentity {
                    capture_sequence,
                    identity: capture.identity.clone(),
                },
            );
        }
        capture
    }

    /// Returns the most recent authoritative identity already captured for a
    /// repository. Sampling refreshes this cache before the model can issue
    /// tools, so proven read-only children can reuse it without launching a
    /// second set of Git subprocesses at dispatch time.
    pub(crate) fn latest_workspace_evidence_identity(
        &self,
        repo_root: &Path,
    ) -> Option<WorkspaceEvidenceIdentity> {
        let repo_root = canonical_workspace_evidence_root(repo_root);
        self.latest_workspace_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&repo_root)
            .and_then(|cached| cached.identity.clone())
    }

    #[cfg(test)]
    pub(crate) fn workspace_evidence_capture_count(&self) -> u64 {
        self.workspace_evidence_capture_count
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn pause_next_workspace_evidence_capture(&self) -> Arc<WorkspaceEvidenceCapturePause> {
        let pause = Arc::new(WorkspaceEvidenceCapturePause {
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self
            .next_workspace_evidence_capture_pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&pause));
        pause
    }

    pub(crate) async fn snapshot(
        self: &Arc<Self>,
        environments: &TurnEnvironmentSnapshot,
    ) -> GitWorkspaceSnapshot {
        self.snapshot_with_project_discovery(environments, None)
            .await
    }

    pub(crate) async fn snapshot_with_project_discovery(
        self: &Arc<Self>,
        environments: &TurnEnvironmentSnapshot,
        project_discovery: Option<&ProjectDiscoveryContext>,
    ) -> GitWorkspaceSnapshot {
        let metrics = codex_otel::global();
        self.snapshot_with_project_discovery_and_metrics(
            environments,
            project_discovery,
            metrics.as_ref(),
        )
        .await
    }

    async fn snapshot_with_project_discovery_and_metrics(
        self: &Arc<Self>,
        environments: &TurnEnvironmentSnapshot,
        project_discovery: Option<&ProjectDiscoveryContext>,
        metrics: Option<&MetricsClient>,
    ) -> GitWorkspaceSnapshot {
        let environment_candidates = environments
            .turn_environments
            .iter()
            .enumerate()
            .filter_map(|(index, environment)| {
                Some((
                    index,
                    EnvironmentWorkspaceKey {
                        environment_id: environment.environment_id.clone(),
                        cwd: environment.cwd().to_abs_path().ok()?,
                        remote: environment.environment.is_remote(),
                    },
                ))
            })
            .collect::<Vec<_>>();
        let key = RootCacheKey {
            environment_generation: environments.generation,
            environments: environment_candidates
                .iter()
                .map(|(_, environment)| environment.clone())
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

        let mut root_resolutions = Vec::with_capacity(environment_candidates.len());
        for (environment_index, key_environment) in environment_candidates.iter().cloned() {
            let _cache = Arc::clone(self);
            let environment = environments.turn_environments[environment_index].clone();
            let project_discovery = project_discovery.cloned();
            let metrics = metrics.cloned();
            root_resolutions.push(async move {
                let fs = environment.environment.get_filesystem();
                let repo_root = if let Some(repo_root) = matching_checkout_root(
                    project_discovery.as_ref(),
                    &key_environment,
                    fs.as_ref(),
                    metrics.as_ref(),
                )
                .await
                {
                    Some(repo_root)
                } else {
                    #[cfg(test)]
                    _cache.root_resolution_count.fetch_add(1, Ordering::AcqRel);
                    get_git_repo_root_with_fs(fs.as_ref(), &key_environment.cwd).await
                };
                GitWorkspaceEntry {
                    environment_id: key_environment.environment_id.clone(),
                    cwd: key_environment.cwd.clone(),
                    repo_root,
                    remote: key_environment.remote,
                }
            });
        }
        let entries = resolve_roots_in_order(root_resolutions).await;

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

    #[cfg(test)]
    pub(crate) fn root_resolution_count(&self) -> u64 {
        self.root_resolution_count.load(Ordering::Acquire)
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
        let changed_paths = changed_paths.map(|paths| {
            paths
                .into_iter()
                .filter(|path| !is_generated_codex_eval_path(path))
                .collect::<Vec<_>>()
        });
        self.record_filtered_source_change_event(changed_paths)
    }

    fn record_filtered_source_change_event(&self, changed_paths: Option<Vec<PathBuf>>) -> u64 {
        if changed_paths.as_ref().is_some_and(Vec::is_empty) {
            return self.watcher_generation.load(Ordering::Acquire);
        }
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
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
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
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let paths = changed_paths
            .iter()
            .map(|path| {
                let path = Path::new(path);
                let absolute = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    repo_root.join(path)
                };
                dunce::canonicalize(&absolute).unwrap_or(absolute)
            })
            .filter(|path| !is_generated_codex_eval_path(path))
            .collect::<Vec<_>>();
        if paths.is_empty() && !changed_paths.is_empty() {
            return;
        }
        self.host_mutation_generation.fetch_add(1, Ordering::AcqRel);
        self.record_filtered_source_change_event((!paths.is_empty()).then_some(paths));
    }
}

async fn resolve_roots_in_order<T>(
    resolutions: impl IntoIterator<Item = impl Future<Output = T> + Send>,
) -> Vec<T> {
    futures::stream::iter(resolutions)
        .buffered(ROOT_DISCOVERY_CONCURRENCY)
        .collect()
        .await
}

async fn matching_checkout_root(
    project_discovery: Option<&ProjectDiscoveryContext>,
    environment: &EnvironmentWorkspaceKey,
    fs: &dyn ExecutorFileSystem,
    metrics: Option<&MetricsClient>,
) -> Option<AbsolutePathBuf> {
    if environment.remote {
        record_project_discovery_reuse(metrics, "git", "miss", "remote_environment");
        return None;
    }
    let Some(discovery) = project_discovery else {
        record_project_discovery_reuse(metrics, "git", "miss", "context_unavailable");
        return None;
    };
    if !discovery.matches_cwd(&environment.cwd) {
        record_project_discovery_reuse(metrics, "git", "miss", "cwd_mismatch");
        return None;
    }
    let Some(checkout_root) = discovery.git_checkout_root() else {
        record_project_discovery_reuse(metrics, "git", "miss", "checkout_root_unavailable");
        return None;
    };
    let marker = PathUri::from_abs_path(&checkout_root.join(".git"));
    let Ok(metadata) = fs.get_metadata(&marker, /*sandbox*/ None).await else {
        record_project_discovery_reuse(metrics, "git", "miss", "git_marker_unavailable");
        return None;
    };
    if metadata.is_directory || metadata.is_file {
        record_project_discovery_reuse(metrics, "git", "hit", "matched");
        Some(checkout_root.clone())
    } else {
        record_project_discovery_reuse(metrics, "git", "miss", "git_marker_invalid");
        None
    }
}

fn record_project_discovery_reuse(
    metrics: Option<&MetricsClient>,
    consumer: &'static str,
    result: &'static str,
    reason: &'static str,
) {
    let Some(metrics) = metrics else {
        return;
    };
    if let Err(err) = metrics.counter(
        PROJECT_DISCOVERY_REUSE_METRIC,
        /*inc*/ 1,
        &[
            ("consumer", consumer),
            ("result", result),
            ("reason", reason),
        ],
    ) {
        warn!("project discovery reuse metric failed: {err}");
    }
}

fn is_generated_codex_eval_path(path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let normalized = normalized.to_ascii_lowercase();
    normalized == ".codex/evals"
        || normalized.starts_with(".codex/evals/")
        || normalized.ends_with("/.codex/evals")
        || normalized.contains("/.codex/evals/")
}

fn canonical_workspace_evidence_root(repo_root: &Path) -> PathBuf {
    dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf())
}

fn path_is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path_is_same_or_descendant_with_case_sensitivity(path, ancestor, !cfg!(windows))
}

fn path_is_same_or_descendant_with_case_sensitivity(
    path: &Path,
    ancestor: &Path,
    case_sensitive: bool,
) -> bool {
    let normalize = |path: &Path| {
        let value = path.to_string_lossy().replace('\\', "/");
        if case_sensitive {
            value
        } else {
            value.to_lowercase()
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
    Windows { volume: u64, index: [u8; 16] },
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
            let state = file_dependency_state(File::open(&path).ok()?, hash_contents)?;
            Some(DependencyFingerprint { path, state })
        }
        Ok(_) => None,
        Err(err) if err.kind() == ErrorKind::NotFound => Some(DependencyFingerprint {
            path,
            state: DependencyState::Missing,
        }),
        Err(_) => None,
    }
}

fn file_dependency_state(mut file: File, hash_contents: bool) -> Option<DependencyState> {
    let metadata = file.metadata().ok()?;
    let stable_id = stable_file_identity(&file);
    let digest = if hash_contents {
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).ok()?;
        Some(Sha256::digest(contents).into())
    } else {
        None
    };
    Some(DependencyState::File {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
        stable_id,
        digest,
    })
}

fn stable_file_identity(file: &File) -> Option<StableFileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

    let mut info = MaybeUninit::<FILE_ID_INFO>::zeroed();
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
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

#[cfg(test)]
#[path = "git_workspace_tests.rs"]
mod tests;
