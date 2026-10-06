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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use codex_config::ProjectDiscoveryContext;
use codex_file_system::ExecutorFileSystem;
use codex_file_watcher::DebouncedWatchReceiver;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_git_utils::DISABLED_HOOKS_PATH;
use codex_git_utils::get_git_remote_urls_assume_git_repo;
#[cfg(test)]
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
// Status collection and metadata capture share this aggregate allowance. A
// single Git dependency can consume five seconds before metadata work starts.
const WORKSPACE_GENERATION_DEADLINE: Duration = Duration::from_secs(15);
// Coalesce bursts of transient failures, but do not make an unchanged watcher
// epoch prevent recovery from process/resource failures indefinitely.
const WORKSPACE_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const WORKSPACE_GENERATION_MAX_DECLARED_BYTES: u64 = 64 * 1024 * 1024;
const WORKSPACE_WATCHER_DEBOUNCE: Duration = Duration::from_millis(50);
const SOURCE_CHANGE_JOURNAL_CAPACITY: usize = 4_096;
const RETAINED_REPOSITORY_CAPACITY: usize = 64;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkspaceEvidenceIdentity {
    /// Repository discovery or capture failed. Unlike a proven non-Git `None`, this
    /// value never authorizes reuse, even when two failed captures are equal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) unavailable: bool,
    #[serde(default)]
    pub(crate) repository_root: Option<String>,
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
}

impl WorkspaceEvidenceIdentity {
    fn unavailable(repository_root: Option<&Path>) -> Self {
        Self {
            unavailable: true,
            repository_root: repository_root.map(|root| root.to_string_lossy().into_owned()),
            head_identity: None,
            index_identity: None,
            worktree_identity: None,
        }
    }
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

pub(crate) async fn capture_workspace_evidence_identity(
    cwd: &Path,
) -> Option<WorkspaceEvidenceIdentity> {
    capture_workspace_evidence_identity_with_attribution(cwd)
        .await
        .identity
}

/// Portable content identity, unlike the status/metadata freshness marker.
/// Hash every indexed and non-ignored untracked path, including clean files.
/// No partial hash is returned on drift, unsupported entries, or timeout.
pub(crate) async fn capture_checkout_snapshot(cwd: &Path) -> Option<String> {
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    timeout(WORKSPACE_GENERATION_DEADLINE, async {
        let root = resolve_workspace_evidence_root(cwd).await.ok()??;
        let list = || async {
            let output = Command::new(codex_git_utils::git_executable_async().await)
                .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
                .env("GIT_OPTIONAL_LOCKS", "0")
                .current_dir(&root)
                .kill_on_drop(true)
                .output().await.ok()?;
            if !output.status.success() {
                return None;
            }
            let paths = output.stdout.split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(|path| std::str::from_utf8(path).map(str::to_owned))
                .collect::<Result<BTreeSet<_>, _>>().ok()?;
            Some(paths)
        };
        let paths = list().await?;
        let captured_paths = paths.clone();
        let captured_root = root.clone();
        let control = WorkspaceCaptureControl {
            deadline: Instant::now() + WORKSPACE_GENERATION_DEADLINE,
            cancellation,
        };
        let hash = tokio::task::spawn_blocking(move || {
            let mut hash = Sha256::new();
            hash.update(b"codex-checkout-content-v1\0");
            let mut revisions = Vec::new();
            let mut buffer = [0_u8; 64 * 1024];
            for name in captured_paths {
                if !control.active() {
                    return None;
                }
                let relative = Path::new(&name);
                if relative.components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
                    return None;
                }
                let path = captured_root.join(relative);
                hash.update((name.len() as u64).to_be_bytes());
                hash.update(name.as_bytes());
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        hash.update(b"deleted\0");
                        revisions.push((path, None));
                        continue;
                    }
                    Err(_) => return None,
                };
                if metadata.file_type().is_symlink() {
                    hash.update(b"symlink\0");
                    let target = std::fs::read_link(&path).ok()?;
                    hash.update(Sha256::digest(target.to_str()?.as_bytes()));
                } else if metadata.is_file() {
                    hash.update(b"file\0");
                    let mut file = File::open(&path).ok()?;
                    let (content_hash, bytes) = hash_workspace_content(
                        &mut file, metadata.len(), &mut buffer, &control,
                    )?;
                    if bytes != metadata.len() {
                        return None;
                    }
                    hash.update(content_hash.as_bytes());
                } else {
                    // A gitlink directory does not prove its recursive content.
                    return None;
                }
                revisions.push((path, Some((metadata.len(), metadata.modified().ok()?, metadata.file_type()))));
            }
            for (path, before) in revisions {
                let after = match std::fs::symlink_metadata(path) {
                    Ok(metadata) => Some((metadata.len(), metadata.modified().ok()?, metadata.file_type())),
                    Err(error) if error.kind() == ErrorKind::NotFound => None,
                    Err(_) => return None,
                };
                if before != after || !control.active() {
                    return None;
                }
            }
            Some(format!("{:x}", hash.finalize()))
        }).await.ok()??;
        (list().await? == paths).then_some(hash)
    }).await.ok().flatten()
}

async fn capture_workspace_evidence_identity_with_attribution(
    cwd: &Path,
) -> WorkspaceEvidenceCapture {
    let repo_root = match resolve_workspace_evidence_root(cwd).await {
        Ok(Some(root)) => root,
        result => {
            return WorkspaceEvidenceCapture {
                identity: result
                    .err()
                    .map(|_| WorkspaceEvidenceIdentity::unavailable(None)),
                timed_out_git_dependencies: Vec::new(),
            };
        }
    };
    capture_workspace_evidence_identity_for_repo_root_with_attribution(repo_root).await
}

async fn capture_workspace_evidence_identity_for_repo_root_with_attribution(
    repo_root: PathBuf,
) -> WorkspaceEvidenceCapture {
    capture_workspace_evidence_with_cancellation(repo_root, CancellationToken::new(), Arc::default()).await
}

async fn capture_workspace_evidence_with_cancellation(
    repo_root: PathBuf,
    cancellation: CancellationToken,
    content_cache: Arc<StdMutex<WorkspaceContentCache>>,
) -> WorkspaceEvidenceCapture {
    let unavailable = WorkspaceEvidenceIdentity::unavailable(Some(&repo_root));
    match within_workspace_generation_deadline(
        WORKSPACE_GENERATION_DEADLINE,
        capture_workspace_generation_marker(repo_root, cancellation, content_cache),
    )
    .await
    {
        Ok(identity) => WorkspaceEvidenceCapture {
            identity: Some(identity.unwrap_or(unavailable)),
            timed_out_git_dependencies: Vec::new(),
        },
        Err(_) => WorkspaceEvidenceCapture {
            identity: Some(unavailable),
            timed_out_git_dependencies: vec![
                WorkspaceEvidenceGitDependency::Head,
                WorkspaceEvidenceGitDependency::Index,
                WorkspaceEvidenceGitDependency::Worktree,
                WorkspaceEvidenceGitDependency::Untracked,
            ],
        },
    }
}

async fn resolve_workspace_evidence_root(cwd: &Path) -> std::io::Result<Option<PathBuf>> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || WorkspaceRootCache::default().resolve(&cwd))
        .await.map_err(std::io::Error::other)?
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceRootCache {
    entries: StdMutex<HashMap<PathBuf, (Option<PathBuf>, Vec<DependencyFingerprint>)>>,
}

impl WorkspaceRootCache {
    #[cfg(test)]
    pub(crate) fn with_lock_for_test<T>(&self, f: impl FnOnce() -> T) -> T {
        let _guard = self.entries.lock().expect("workspace root cache");
        f()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub(crate) fn resolve(&self, cwd: &Path) -> std::io::Result<Option<PathBuf>> {
        // Only an exhaustive, successful ancestor walk proves absence. Missing
        // cwd, unreadable metadata and worker failure must not look like non-Git.
        let cwd = dunce::canonicalize(cwd)?;
        let base = if std::fs::metadata(&cwd)?.is_dir() {
            cwd.as_path()
        } else {
            cwd.parent()
                .ok_or_else(|| std::io::Error::other("cwd has no parent"))?
        };
        let mut dependencies = Vec::new();
        for ancestor in base.ancestors() {
            let path = ancestor.join(".git");
            let exists = match std::fs::symlink_metadata(&path) {
                Ok(_) => true,
                Err(error) if error.kind() == ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            let dependency = if exists {
                dependency_fingerprint(path, true).filter(|dependency| {
                    !matches!(dependency.state, DependencyState::Missing)
                }).ok_or_else(|| std::io::Error::other("cannot inspect repository marker"))?
            } else {
                DependencyFingerprint { path, state: DependencyState::Missing }
            };
            dependencies.push(dependency);
            if exists { break; }
        }
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((root, previous)) = entries.get(base)
            && *previous == dependencies
        {
            return Ok(root.clone());
        }
        let root = dependencies.iter().find_map(|dependency| {
            (!matches!(dependency.state, DependencyState::Missing))
                .then(|| dependency.path.parent().map(Path::to_path_buf)).flatten()
        });
        if entries.len() >= RETAINED_REPOSITORY_CAPACITY && !entries.contains_key(base) {
            entries.clear();
        }
        entries.insert(base.to_path_buf(), (root.clone(), dependencies));
        Ok(root)
    }
}

async fn git_ignores_all_changed_paths(root: &Path, paths: &[PathBuf]) -> bool {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    if paths.is_empty() {
        return true;
    }
    let mut input = Vec::new();
    for path in paths {
        let Some(path) = path.to_str() else { return false; };
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    // Bound both command-line size and ignore-query output. Overflow and any
    // inability to prove exclusion remain capture-relevant.
    if input.len() > 8 * 1024 {
        return false;
    }
    timeout(Duration::from_secs(2), async {
        // An ignored directory can still contain force-added tracked files.
        let mut tracked = Command::new(codex_git_utils::git_executable_async().await)
            .args(["--literal-pathspecs", "ls-files", "--cached", "-z", "--"])
            .args(paths).current_dir(root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().ok()?;
        let mut byte = [0];
        if tracked.stdout.take()?.read(&mut byte).await.ok()? != 0
            || !tracked.wait().await.ok()?.success()
        {
            return Some(false);
        }
        let mut ignored = Command::new(codex_git_utils::git_executable_async().await)
            .args(["check-ignore", "-z", "--stdin"]).current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()).kill_on_drop(true).spawn().ok()?;
        let mut stdin = ignored.stdin.take()?;
        let (written, output) = tokio::join!(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        }, ignored.wait_with_output());
        written.ok()?;
        let output = output.ok()?;
        Some(output.status.success()
            && output.stdout.iter().filter(|byte| **byte == 0).count() == paths.len())
    }).await.ok().flatten().unwrap_or(false)
}

async fn remote_workspace_evidence_capture(
    environment: &codex_exec_server::Environment,
    cwd: &PathUri,
) -> WorkspaceEvidenceCapture {
    match timeout(
        WORKSPACE_GENERATION_DEADLINE,
        capture_remote_workspace_evidence(environment, cwd),
    )
    .await
    {
        Ok(Some(identity)) => WorkspaceEvidenceCapture {
            identity,
            timed_out_git_dependencies: Vec::new(),
        },
        result => WorkspaceEvidenceCapture {
            identity: Some(WorkspaceEvidenceIdentity::unavailable(None)),
            timed_out_git_dependencies: if result.is_err() {
                vec![
                    WorkspaceEvidenceGitDependency::Head,
                    WorkspaceEvidenceGitDependency::Index,
                    WorkspaceEvidenceGitDependency::Worktree,
                    WorkspaceEvidenceGitDependency::Untracked,
                ]
            } else {
                Vec::new()
            },
        },
    }
}

async fn capture_remote_workspace_evidence(
    environment: &codex_exec_server::Environment,
    cwd: &PathUri,
) -> Option<Option<WorkspaceEvidenceIdentity>> {
    let fs = environment.get_filesystem();
    let cwd = fs.canonicalize(cwd, None).await.ok()?;
    if !fs.get_metadata(&cwd, None).await.ok()?.is_directory {
        return None;
    }
    let mut root = cwd;
    loop {
        match fs.get_metadata(&root.join(".git").ok()?, None).await {
            Ok(metadata) if metadata.is_file || metadata.is_directory => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(parent) = root.parent() else {
                    return Some(None);
                };
                root = parent;
            }
            _ => return None,
        }
    }
    let deadline = tokio::time::Instant::now() + WORKSPACE_GENERATION_DEADLINE;
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        format!("core.hooksPath={DISABLED_HOOKS_PATH}"),
        "-c".to_string(),
        "core.fsmonitor=false".to_string(),
    ];
    argv.extend(
        workspace_generation_status_args()
            .iter()
            .map(|arg| (*arg).to_string()),
    );
    let status = crate::shell_snapshot::run_remote_snapshot_process_before(
        environment.get_exec_backend(),
        codex_exec_server::ExecParams {
            process_id: format!("workspace-evidence-{}", uuid::Uuid::new_v4()).into(),
            argv,
            cwd: root.clone(),
            env_policy: None,
            env: HashMap::from([("GIT_OPTIONAL_LOCKS".to_string(), "0".to_string())]),
            tty: false,
            pipe_stdin: false,
            arg0: None,
            sandbox: None,
            enforce_managed_network: false,
            managed_network: None,
        },
        deadline,
        "git workspace evidence",
    )
    .await
    .ok()?;
    let mut reader = WorkspaceStatusReader::new();
    reader.push(status.as_bytes())?;
    let (status, paths) = reader.finish()?;
    let head_identity = workspace_head_identity(&status)?;
    let mut manifest = format!("total_paths={}\n", paths.len()).into_bytes();
    let mut remaining = WORKSPACE_GENERATION_MAX_DECLARED_BYTES as usize;
    let mut deletions = Vec::new();
    for observation in paths {
        let path = root.join(&observation.path).ok()?;
        let metadata = match fs.get_metadata(&path, None).await {
            Ok(metadata) if !observation.deleted => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound && observation.deleted => {
                manifest.extend_from_slice(observation.path.as_bytes());
                manifest.extend_from_slice(b"\0deleted\0\n");
                deletions.push(path);
                continue;
            }
            _ => return None,
        };
        // The filesystem protocol cannot read symlink targets without following them.
        // Never publish a complete identity when that evidence is unavailable.
        if metadata.is_symlink || !metadata.is_file {
            return None;
        }
        manifest.extend_from_slice(observation.path.as_bytes());
        manifest.push(0);
        manifest.extend_from_slice(if metadata.is_file {
            b"file"
        } else {
            b"directory"
        });
        manifest.push(0);
        manifest.extend_from_slice(metadata.size.to_string().as_bytes());
        manifest.push(0);
        if metadata.is_file {
            let contents = fs.read_file_bounded(&path, remaining, None).await.ok()??;
            remaining = remaining.checked_sub(contents.len())?;
            if contents.len() as u64 != metadata.size
                || fs.get_metadata(&path, None).await.ok()? != metadata
            {
                return None;
            }
            manifest.extend_from_slice(format!("{:x}", Sha256::digest(&contents)).as_bytes());
        }
        manifest.push(b'\n');
    }
    for path in deletions {
        if !matches!(fs.get_metadata(&path, None).await, Err(error) if error.kind() == ErrorKind::NotFound)
        {
            return None;
        }
    }
    let mut index = Sha256::new();
    index.update(&status);
    let mut worktree = Sha256::new();
    worktree.update(&status);
    worktree.update(&manifest);
    Some(Some(WorkspaceEvidenceIdentity {
        unavailable: false,
        repository_root: Some(format!("{}:{root}", environment.approval_scope_id())),
        head_identity,
        index_identity: Some(format!("{:x}", index.finalize())),
        worktree_identity: Some(format!("{:x}", worktree.finalize())),
    }))
}

async fn within_workspace_generation_deadline<T, Capture>(
    deadline: Duration,
    capture: Capture,
) -> Result<T, tokio::time::error::Elapsed>
where
    Capture: Future<Output = T>,
{
    timeout(deadline, capture).await
}

fn workspace_generation_status_args() -> &'static [&'static str] {
    &[
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=all",
        "--",
        ".",
    ]
}

async fn capture_workspace_generation_marker(
    repo_root: PathBuf,
    cancellation: CancellationToken,
    content_cache: Arc<StdMutex<WorkspaceContentCache>>,
) -> Option<WorkspaceEvidenceIdentity> {
    let capture_started = SystemTime::now();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let control = WorkspaceCaptureControl {
        deadline: Instant::now() + WORKSPACE_GENERATION_DEADLINE,
        cancellation,
    };
    let (status, paths) = workspace_generation_status(&repo_root).await?;
    let head_identity = workspace_head_identity(&status)?;
    let metadata = capture_workspace_metadata_cached(repo_root.clone(), paths, control, content_cache, capture_started).await?;

    let mut index_hasher = Sha256::new();
    index_hasher.update(b"KD4_WORKSPACE_INDEX_GENERATION_V1\n");
    index_hasher.update(&status);
    let mut worktree_hasher = Sha256::new();
    worktree_hasher.update(b"KD4_WORKSPACE_WORKTREE_GENERATION_V1\n");
    worktree_hasher.update(&status);
    worktree_hasher.update(&metadata.manifest);

    Some(WorkspaceEvidenceIdentity {
        unavailable: false,
        // Both capture entry points obtain this canonical root from the
        // blocking discovery worker. Reuse that same captured repository.
        repository_root: Some(repo_root.to_string_lossy().into_owned()),
        head_identity,
        index_identity: Some(format!("{:x}", index_hasher.finalize())),
        worktree_identity: Some(format!("{:x}", worktree_hasher.finalize())),
    })
}

#[derive(Debug)]
struct WorkspaceGenerationPath {
    path: String,
    deleted: bool,
}

// Porcelain v2 rename records have a second NUL-delimited pathname, which is
// not another status record (even when that pathname starts with "? " or "1 ").
struct WorkspaceStatusReader {
    bytes: Vec<u8>,
    record_start: usize,
    rename_source: bool,
    paths: BTreeMap<String, bool>,
}

impl WorkspaceStatusReader {
    const MAX_STATUS_BYTES: usize = 2 * 1024 * 1024;

    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            record_start: 0,
            rename_source: false,
            paths: BTreeMap::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Option<()> {
        if self.bytes.len().checked_add(chunk.len())? > Self::MAX_STATUS_BYTES {
            warn!(
                collected_bytes = self.bytes.len(),
                incoming_bytes = chunk.len(),
                max_bytes = Self::MAX_STATUS_BYTES,
                "Git status output exceeded the byte limit; workspace evidence is unavailable"
            );
            return None;
        }
        let start = self.bytes.len();
        self.bytes.extend_from_slice(chunk);
        for end in start..self.bytes.len() {
            if self.bytes[end] != 0 {
                continue;
            }
            let record = &self.bytes[self.record_start..end];
            self.record_start = end + 1;
            if self.rename_source {
                self.rename_source = false;
                continue;
            }
            let (path, deleted) = match record.first().copied()? {
                b'#' if record.get(1) == Some(&b' ') => continue,
                b'?' if record.get(1) == Some(&b' ') => (record.get(2..)?, false),
                kind @ (b'1' | b'2' | b'u') => {
                    let count = match kind {
                        b'1' => 9,
                        b'2' => 10,
                        _ => 11,
                    };
                    let fields = record
                        .splitn(count, |byte| *byte == b' ')
                        .collect::<Vec<_>>();
                    if fields.len() != count {
                        return None;
                    }
                    self.rename_source = kind == b'2';
                    let deleted =
                        kind == b'1' && fields[1].contains(&b'D') && fields[5] == b"000000";
                    (fields[count - 1], deleted)
                }
                _ => return None,
            };
            let path = std::str::from_utf8(path).ok()?;
            if path.is_empty() {
                return None;
            }
            if let Some(previous) = self.paths.insert(path.to_owned(), deleted)
                && previous != deleted
            {
                return None;
            }
        }
        Some(())
    }

    fn finish(self) -> Option<(Vec<u8>, Vec<WorkspaceGenerationPath>)> {
        if self.record_start != self.bytes.len() || self.rename_source {
            return None;
        }
        Some((
            self.bytes,
            self.paths
                .into_iter()
                .map(|(path, deleted)| WorkspaceGenerationPath { path, deleted })
                .collect(),
        ))
    }
}

async fn workspace_generation_status(
    repo_root: &Path,
) -> Option<(Vec<u8>, Vec<WorkspaceGenerationPath>)> {
    let _timer = codex_otel::start_global_timer("codex.workspace.git_status.duration_ms", &[]);
    let mut command = Command::new(codex_git_utils::git_executable_async().await);
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(workspace_generation_status_args())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // Status may run clean filters when stat information alone cannot establish
    // whether a tracked file changed. Keep their tree owned across the outer
    // workspace-capture deadline, cancellation, and rejected partial output.
    #[cfg(windows)]
    let (_managed, mut child) = {
        command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
        let managed = codex_utils_pty::ManagedRootProcess::reserve_with_reclaim()
            .await
            .ok()?;
        managed.require_descendant_containment().ok()?;
        // Carry both owners through the blocking operation. If capture is
        // cancelled while spawning, the abandoned result still owns the job
        // and kill-on-drop child, including before it reaches this await.
        tokio::task::spawn_blocking(move || {
            let mut child = command.spawn()?;
            let pid = child
                .id()
                .ok_or_else(|| std::io::Error::other("missing Git process id"))?;
            if let Err(error) = managed.attach_and_resume(pid) {
                let _ = child.start_kill();
                return Err(error);
            }
            Ok((managed, child))
        })
        .await
        .ok()?
        .ok()?
    };
    #[cfg(not(windows))]
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut reader = WorkspaceStatusReader::new();
    let mut buffer = [0; 8192];
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) if reader.push(&buffer[..count]).is_some() => {}
            _ => {
                // Explicitly reap rejected captures; never publish a partial identity.
                let _ = child.kill().await;
                let _ = child.wait().await;
                return None;
            }
        }
    }
    child.wait().await.ok()?.success().then_some(())?;
    reader.finish()
}

fn workspace_head_identity(status: &[u8]) -> Option<Option<String>> {
    let head = status
        .split(|byte| *byte == 0)
        .find_map(|record| record.strip_prefix(b"# branch.oid "))?;
    if head == b"(initial)" {
        return Some(None);
    }
    let head = std::str::from_utf8(head).ok()?;
    (!head.is_empty()).then(|| Some(head.to_string()))
}

struct WorkspaceGenerationMetadata {
    manifest: Vec<u8>,
}

#[derive(Clone)]
struct WorkspaceCaptureControl {
    deadline: Instant,
    cancellation: CancellationToken,
}

impl WorkspaceCaptureControl {
    fn active(&self) -> bool {
        Instant::now() < self.deadline && !self.cancellation.is_cancelled()
    }
}

#[cfg(test)]
async fn workspace_generation_metadata(
    repo_root: PathBuf,
    paths: Vec<String>,
) -> Option<WorkspaceGenerationMetadata> {
    capture_workspace_metadata(
        repo_root,
        paths
            .into_iter()
            .map(|path| WorkspaceGenerationPath {
                path,
                deleted: false,
            })
            .collect(),
        WorkspaceCaptureControl {
            deadline: Instant::now() + WORKSPACE_GENERATION_DEADLINE,
            cancellation: CancellationToken::new(),
        },
    )
    .await
}

fn hash_workspace_content(
    reader: &mut impl Read,
    remaining: u64,
    buffer: &mut [u8],
    control: &WorkspaceCaptureControl,
) -> Option<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut bytes_read = 0_u64;
    loop {
        if !control.active() {
            return None;
        }
        let limit = usize::try_from((remaining + 1 - bytes_read).min(buffer.len() as u64)).ok()?;
        let count = reader.read(&mut buffer[..limit]).ok()?;
        if count == 0 {
            break;
        }
        bytes_read += count as u64;
        if bytes_read > remaining {
            return None;
        }
        hasher.update(&buffer[..count]);
    }
    Some((format!("{:x}", hasher.finalize()), bytes_read))
}

#[derive(Default)]
struct WorkspaceContentCache {
    files: HashMap<PathBuf, CachedWorkspaceContent>,
    #[cfg(test)]
    hits: AtomicUsize,
}

struct CachedWorkspaceContent {
    size: u64,
    modified: SystemTime,
    file_id: StableFileIdentity,
    capture_started: SystemTime,
    hash: String,
}

#[cfg(test)]
async fn capture_workspace_metadata(
    repo_root: PathBuf,
    paths: Vec<WorkspaceGenerationPath>,
    control: WorkspaceCaptureControl,
) -> Option<WorkspaceGenerationMetadata> {
    capture_workspace_metadata_cached(repo_root, paths, control, Arc::default(), SystemTime::now()).await
}

async fn capture_workspace_metadata_cached(
    repo_root: PathBuf,
    paths: Vec<WorkspaceGenerationPath>,
    control: WorkspaceCaptureControl,
    content_cache: Arc<StdMutex<WorkspaceContentCache>>,
    capture_started: SystemTime,
) -> Option<WorkspaceGenerationMetadata> {
    let total_paths = paths.len();
    let mut manifest = format!("total_paths={total_paths}\n").into_bytes();
    let observed_bytes = Arc::new(AtomicU64::new(0));
    let hashing_concurrency = std::thread::available_parallelism().map_or(4, usize::from);
    // Ordered buffering bounds blocking workers and preserves the status
    // reader's sorted manifest order regardless of hash completion order.
    let results = futures::stream::iter(paths.into_iter().map(|observation| {
        let repo_root = repo_root.clone();
        let control = control.clone();
        let content_cache = Arc::clone(&content_cache);
        let observed_bytes = Arc::clone(&observed_bytes);
        async move { tokio::task::spawn_blocking(move || {
            let mut manifest = Vec::new();
            if !control.active() { return None; }
            let path = observation.path;
            let absolute = repo_root.join(&path);
            let metadata = match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) if !observation.deleted => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound && observation.deleted => {
                    manifest.extend_from_slice(path.as_bytes());
                    manifest.extend_from_slice(b"\0deleted\0\n");
                    return Some((manifest, Some(absolute)));
                }
                _ => return None,
            };
            // Git reports dirty submodules as directories, without identifying
            // their contents. Such an entry cannot certify workspace freshness.
            if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return None;
            }
            let declared_bytes = if metadata.is_file() { metadata.len() } else { 0 };
            observed_bytes.fetch_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                total.checked_add(declared_bytes).filter(|total| *total <= WORKSPACE_GENERATION_MAX_DECLARED_BYTES)
            }).ok()?;
            let kind = if metadata.file_type().is_symlink() { "symlink" }
                else if metadata.is_file() { "file" }
                else if metadata.is_dir() { "directory" }
                else { "other" };
            manifest.extend_from_slice(path.as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(kind.as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(declared_bytes.to_string().as_bytes());
            manifest.push(0);
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&absolute).ok()?;
                manifest.extend_from_slice(format!("{:x}", Sha256::digest(target.to_string_lossy().as_bytes())).as_bytes());
            } else if metadata.is_file() {
                let mut file = File::open(&absolute).ok()?;
                let modified = metadata.modified().ok()?;
                let file_id = stable_file_identity(&file);
                let cached_hash = {
                    let cache = content_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let hash = cache.files.get(&absolute).filter(|entry| {
                        entry.size == declared_bytes && entry.modified == modified
                            && Some(&entry.file_id) == file_id.as_ref()
                            && modified < entry.capture_started
                    }).map(|entry| entry.hash.clone());
                    #[cfg(test)]
                    if hash.is_some() { cache.hits.fetch_add(1, Ordering::Relaxed); }
                    hash
                };
                let (hash, actual_bytes) = match cached_hash {
                    Some(hash) => (hash, declared_bytes),
                    None => hash_workspace_content(&mut file, declared_bytes, &mut [0_u8; 64 * 1024], &control)?,
                };
                let after = file.metadata().ok()?;
                if actual_bytes != declared_bytes || after.len() != declared_bytes || after.modified().ok()? != metadata.modified().ok()? {
                    return None;
                }
                manifest.extend_from_slice(hash.as_bytes());
                if let Some(file_id) = file_id {
                    let mut cache = content_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Bound cross-repository retention; eviction only costs a rehash.
                    if cache.files.len() >= 4096 && !cache.files.contains_key(&absolute) { cache.files.clear(); }
                    if cache.files.get(&absolute).is_none_or(|entry| entry.capture_started <= capture_started) {
                        cache.files.insert(absolute, CachedWorkspaceContent { size: declared_bytes, modified, file_id, capture_started, hash });
                    }
                }
            }
            manifest.push(b'\n');
            Some((manifest, None))
        }).await.ok()? }
    })).buffered(hashing_concurrency).collect::<Vec<_>>().await;
    let mut deletions = Vec::new();
    for result in results {
        let (entry, deletion) = result?;
        manifest.extend(entry);
        deletions.extend(deletion);
    }
    tokio::task::spawn_blocking(move || {
        for path in deletions {
            if !control.active() || !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == ErrorKind::NotFound) {
                return None;
            }
        }
        control.active().then_some(WorkspaceGenerationMetadata { manifest })
    }).await.ok()?
}

#[cfg(test)]
async fn workspace_generation_git_output(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo_root)
        .kill_on_drop(true);
    let output = command.output().await.ok()?;
    output.status.success().then_some(output.stdout)
}

impl GitWorkspaceMetadataSource {
    #[cfg(test)]
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
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StableGitMetadata {
    associated_remote_urls: Option<BTreeMap<String, String>>,
    latest_git_commit_hash: Option<String>,
}

// Only this worker owns raw watcher registrations/subscribers. Requests and
// returned leases contain no native resource, including abandoned replies.
const GIT_WATCH_REQUEST_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
enum GitWatchKind {
    Metadata,
    Source,
}

struct GitWatchWorker {
    sender: tokio::sync::mpsc::Sender<GitWatchCommand>,
    #[cfg(test)]
    probe: Arc<GitWatchWorkerProbe>,
}

enum GitWatchCommand {
    Register {
        kind: GitWatchKind,
        paths: Vec<WatchPath>,
        sender: tokio::sync::mpsc::Sender<GitWatchCommand>,
        reply: tokio::sync::oneshot::Sender<Result<GitWatchLease, String>>,
    },
    Sweep,
}

#[derive(Default)]
struct GitWatchLease {
    alive: Option<Arc<()>>,
    sender: Option<tokio::sync::mpsc::Sender<GitWatchCommand>>,
}

impl Drop for GitWatchLease {
    fn drop(&mut self) {
        // Release before waking: otherwise the worker can observe a live lease,
        // go idle, and never receive another command for this retirement.
        drop(self.alive.take());
        if let Some(sender) = self.sender.take() {
            // Full means a queued command already guarantees a subsequent sweep.
            // Closed means the worker is already retiring its owned registrations.
            let _ = sender.try_send(GitWatchCommand::Sweep);
        }
    }
}

// Test controls pause immediately before the real native-operation boundary;
// they retain the actual WatchRegistration and do not replace registration logic.
#[cfg(test)]
#[derive(Default)]
struct GitWatchWorkerProbe {
    registered: std::sync::atomic::AtomicUsize,
    retired: std::sync::atomic::AtomicUsize,
    stopped: AtomicBool,
    thread: StdMutex<Option<std::thread::ThreadId>>,
    changed: tokio::sync::Notify,
    before_reply: StdMutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
    before_retire: StdMutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        )>,
    >,
}

#[cfg(test)]
impl GitWatchWorkerProbe {
    fn pause(
        slot: &StdMutex<
            Option<(
                tokio::sync::oneshot::Sender<()>,
                std::sync::mpsc::Receiver<()>,
            )>,
        >,
    ) {
        let pause = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((started, release)) = pause {
            let _ = started.send(());
            // Dropping the test's release sender also releases a failed test.
            let _ = release.recv();
        }
    }

    fn arm(
        slot: &StdMutex<
            Option<(
                tokio::sync::oneshot::Sender<()>,
                std::sync::mpsc::Receiver<()>,
            )>,
        >,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started, started_receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((started, release_receiver));
        (started_receiver, release)
    }

    async fn wait_for(&self, retired: usize, stopped: bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let changed = self.changed.notified();
                if self.retired.load(Ordering::Acquire) >= retired
                    && (!stopped || self.stopped.load(Ordering::Acquire))
                {
                    return;
                }
                changed.await;
            }
        })
        .await
        .expect("watch worker must finish the owned native retirement");
    }
}

impl GitWatchWorker {
    fn start(
        metadata: Option<FileWatcherSubscriber>,
        source: Option<FileWatcherSubscriber>,
    ) -> std::io::Result<Self> {
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<GitWatchCommand>(GIT_WATCH_REQUEST_CAPACITY);
        #[cfg(test)]
        let probe = Arc::new(GitWatchWorkerProbe::default());
        #[cfg(test)]
        let worker_probe = Arc::clone(&probe);
        std::thread::Builder::new()
            .name("codex-git-watches".to_string())
            .spawn(move || {
                #[cfg(test)]
                {
                    *worker_probe
                        .thread
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(std::thread::current().id());
                }
                let mut registrations: Vec<(std::sync::Weak<()>, Option<WatchRegistration>)> =
                    Vec::new();
                let retire_dead =
                    |registrations: &mut Vec<(std::sync::Weak<()>, Option<WatchRegistration>)>| {
                        registrations.retain_mut(|(alive, registration)| {
                            if alive.strong_count() != 0 {
                                return true;
                            }
                            #[cfg(test)]
                            GitWatchWorkerProbe::pause(&worker_probe.before_retire);
                            drop(registration.take());
                            #[cfg(test)]
                            {
                                worker_probe.retired.fetch_add(1, Ordering::AcqRel);
                                worker_probe.changed.notify_one();
                            }
                            false
                        });
                    };
                while let Some(command) = receiver.blocking_recv() {
                    // Drop dead guards on this worker before accepting new work.
                    retire_dead(&mut registrations);
                    match command {
                        GitWatchCommand::Register {
                            kind,
                            paths,
                            sender,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let subscriber = match kind {
                                GitWatchKind::Metadata => metadata.as_ref(),
                                GitWatchKind::Source => source.as_ref(),
                            };
                            let result = subscriber
                                .ok_or_else(|| "Git watch subscriber unavailable".to_string())
                                .and_then(|subscriber| {
                                    subscriber
                                        .register_paths(paths)
                                        .map_err(|error| error.to_string())
                                })
                                .map(|registration| {
                                    let alive = Arc::new(());
                                    registrations
                                        .push((Arc::downgrade(&alive), Some(registration)));
                                    #[cfg(test)]
                                    {
                                        worker_probe.registered.fetch_add(1, Ordering::AcqRel);
                                    }
                                    GitWatchLease {
                                        alive: Some(alive),
                                        sender: Some(sender),
                                    }
                                });
                            // If cancellation won, the returned lease is dropped
                            // here. The native guard never leaves registrations.
                            #[cfg(test)]
                            GitWatchWorkerProbe::pause(&worker_probe.before_reply);
                            let _ = reply.send(result);
                        }
                        GitWatchCommand::Sweep => {}
                    }
                    retire_dead(&mut registrations);
                }
                // Field order at cache destruction is irrelevant: no native
                // owner remains in the cache, and pending leases keep this
                // channel alive until their final retirement request is sent.
                retire_dead(&mut registrations);
                drop(registrations);
                drop(metadata);
                drop(source);
                #[cfg(test)]
                {
                    worker_probe.stopped.store(true, Ordering::Release);
                    worker_probe.changed.notify_one();
                }
            })?;
        Ok(Self {
            sender,
            #[cfg(test)]
            probe,
        })
    }

    async fn register(
        &self,
        kind: GitWatchKind,
        paths: Vec<WatchPath>,
    ) -> Result<GitWatchLease, String> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        self.sender
            .send(GitWatchCommand::Register {
                kind,
                paths,
                sender: self.sender.clone(),
                reply,
            })
            .await
            .map_err(|_| "Git watch worker unavailable".to_string())?;
        receiver
            .await
            .map_err(|_| "Git watch registration interrupted".to_string())?
    }
}

struct RootCacheEntry {
    key: RootCacheKey,
    dependencies: Vec<DependencyFingerprint>,
    watcher_generation: u64,
    entries: Vec<GitWorkspaceEntry>,
    _registration: GitWatchLease,
}

struct MetadataCacheEntry {
    dependencies: StableMetadataDependencies,
    watcher_generation: u64,
    metadata: StableGitMetadata,
    _registration: GitWatchLease,
}

#[derive(Default)]
struct GitWorkspaceCacheState {
    root: Option<RootCacheEntry>,
    metadata: HashMap<PathBuf, MetadataCacheEntry>,
}

struct IgnoredDirectoryProof {
    root: PathBuf,
    directory: PathBuf,
    dependencies: Vec<DependencyFingerprint>,
}

impl IgnoredDirectoryProof {
    fn is_current(&self) -> bool {
        self.directory.ancestors().take_while(|path| path.starts_with(&self.root)).all(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        }) && self.dependencies.iter().all(|dependency| {
            dependency_fingerprint(dependency.path.clone(), dependency.path.file_name().is_some_and(|name| name != "index"))
                .as_ref() == Some(dependency)
        })
    }
}

fn ignore_directory_dependencies(root: &Path, directory: &Path) -> Option<Vec<DependencyFingerprint>> {
    let absolute = AbsolutePathBuf::from_absolute_path(root).ok()?;
    let (git_dir, common_dir, _) = resolve_git_dirs(&absolute)?;
    let mut paths = vec![(git_dir.join("index"), false), (git_dir.join("config.worktree"), true),
        (common_dir.join("config"), true), (root.join(".git"), true)];
    for ancestor in directory.ancestors() {
        if !ancestor.starts_with(root) { break; }
        // Never cache a proof through a symlink or a removed directory.
        let metadata = std::fs::symlink_metadata(ancestor).ok()?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() { return None; }
        paths.push((ancestor.join(".gitignore"), true));
    }
    paths.into_iter().map(|(path, hash)| dependency_fingerprint(path, hash)).collect()
}

// Learn only exclusions from repository .gitignore files. Global excludes/config
// can change outside our watches; they remain on the uncached fallback path.
async fn ignored_directory_proof(root: &Path, path: &Path) -> Option<IgnoredDirectoryProof> {
    use tokio::io::AsyncWriteExt;
    let parent = if path.is_dir() { path } else { path.parent()? };
    let dependencies = ignore_directory_dependencies(root, parent)?;
    let candidates = parent.ancestors().take_while(|dir| *dir != root && dir.starts_with(root))
        .map(Path::to_path_buf).collect::<Vec<_>>();
    let mut input = Vec::new();
    for directory in &candidates {
        input.extend_from_slice(directory.strip_prefix(root).ok()?.to_str()?.as_bytes());
        input.push(0);
    }
    if input.is_empty() || input.len() > 8 * 1024 { return None; }
    let output = timeout(Duration::from_secs(2), async {
        let mut child = Command::new(codex_git_utils::git_executable_async().await).args(["check-ignore", "--verbose", "-z", "--stdin"])
            .current_dir(root).stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
            .kill_on_drop(true).spawn().ok()?;
        let mut stdin = child.stdin.take()?;
        let (written, output) = tokio::join!(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        }, child.wait_with_output());
        written.ok()?;
        let output = output.ok()?;
        output.status.success().then_some(output.stdout)
    }).await.ok()??;
    let fields = output.strip_suffix(&[0])?.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.len() % 4 != 0 { return None; }
    let directory = fields.chunks_exact(4).filter_map(|fields| {
        let source = root.join(std::str::from_utf8(fields[0]).ok()?);
        let directory = root.join(std::str::from_utf8(fields[3]).ok()?);
        (fields[2].first() != Some(&b'!')
            && source.file_name().is_some_and(|name| name == ".gitignore")
            && source.strip_prefix(root).ok()?.components().all(|part| matches!(part, std::path::Component::Normal(_)))
            && directory.starts_with(source.parent()?)
            && candidates.contains(&directory)).then_some(directory)
    }).min_by_key(|directory| directory.components().count())?;
    if !git_ignores_all_changed_paths(root, std::slice::from_ref(&directory)).await { return None; }
    let proof = IgnoredDirectoryProof { root: root.to_path_buf(), directory, dependencies };
    proof.is_current().then_some(proof)
}

#[derive(Clone)]
struct CachedWorkspaceEvidenceIdentity {
    capture_sequence: u64,
    host_mutation_generation: u64,
    identity: Option<WorkspaceEvidenceIdentity>,
    failed_capture: Option<(WorkspaceEvidenceCaptureKey, WorkspaceEvidenceCapture, Instant)>,
}

struct RetainedSourceWatchRegistration {
    generation: u64,
    _registration: GitWatchLease,
}

#[derive(Default)]
struct RepositoryRetention {
    source_watch_registrations: HashMap<PathBuf, RetainedSourceWatchRegistration>,
    latest_workspace_evidence: HashMap<PathBuf, CachedWorkspaceEvidenceIdentity>,
    access_order: VecDeque<PathBuf>,
    next_registration_generation: u64,
}

impl RepositoryRetention {
    fn touch(&mut self, repo_root: &Path) {
        if let Some(index) = self
            .access_order
            .iter()
            .position(|retained| retained == repo_root)
        {
            self.access_order.remove(index);
        }
        self.access_order.push_back(repo_root.to_path_buf());
        while self.access_order.len() > RETAINED_REPOSITORY_CAPACITY {
            let Some(evicted) = self.access_order.pop_front() else {
                break;
            };
            self.source_watch_registrations.remove(&evicted);
            self.latest_workspace_evidence.remove(&evicted);
        }
    }

    fn allocate_registration_generation(&mut self) -> u64 {
        self.next_registration_generation = self.next_registration_generation.saturating_add(1);
        self.next_registration_generation
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkspaceEvidenceCaptureKey {
    repo_root: PathBuf,
    watcher_generation: u64,
    source_watcher_generation: u64,
    host_mutation_generation: u64,
}

#[derive(Clone)]
struct InFlightWorkspaceEvidenceCapture {
    capture_sequence: u64,
    future: Shared<BoxFuture<'static, WorkspaceEvidenceCapture>>,
    interest: Arc<WorkspaceCaptureInterest>,
}

struct WorkspaceCaptureInterest {
    waiters: AtomicUsize,
    cancellation: CancellationToken,
}

struct WorkspaceCaptureWaiter<'a> {
    interest: Arc<WorkspaceCaptureInterest>,
    cache: &'a GitWorkspaceCache,
    key: WorkspaceEvidenceCaptureKey,
    sequence: u64,
}

impl Drop for WorkspaceCaptureWaiter<'_> {
    fn drop(&mut self) {
        let mut captures = self
            .cache
            .in_flight_workspace_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.interest.waiters.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.interest.cancellation.cancel();
            if captures
                .get(&self.key)
                .is_some_and(|capture| capture.capture_sequence == self.sequence)
            {
                captures.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
pub(crate) struct WorkspaceEvidenceCapturePause {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(test)]
impl WorkspaceEvidenceCapturePause {
    pub(crate) async fn wait_until_started(&self) {
        self.started.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

pub(crate) struct GitWorkspaceCache {
    state: Mutex<GitWorkspaceCacheState>,
    pub(crate) roots: Arc<WorkspaceRootCache>,
    content_cache: Arc<StdMutex<WorkspaceContentCache>>,
    watcher_epoch: u64,
    watcher_generation: AtomicU64,
    host_mutation_generation: AtomicU64,
    watcher_reliable: AtomicBool,
    watcher_worker: Option<GitWatchWorker>,
    source_watcher_generation: AtomicU64,
    source_capture_generation: AtomicU64,
    source_event_gate: tokio::sync::RwLock<()>,
    ignored_directories: Mutex<HashMap<PathBuf, Vec<IgnoredDirectoryProof>>>,
    source_watcher_reliable: AtomicBool,
    repository_retention: StdMutex<RepositoryRetention>,
    source_change_journal: StdMutex<SourceChangeJournal>,
    in_flight_workspace_evidence:
        StdMutex<HashMap<WorkspaceEvidenceCaptureKey, InFlightWorkspaceEvidenceCapture>>,
    workspace_evidence_capture_sequence: AtomicU64,
    #[cfg(test)]
    host_mutation_worker_hook: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    workspace_evidence_capture_count: AtomicU64,
    #[cfg(test)]
    next_workspace_evidence_capture_pause: StdMutex<Option<Arc<WorkspaceEvidenceCapturePause>>>,
    #[cfg(test)]
    workspace_evidence_waiter_joined: tokio::sync::Notify,
    #[cfg(test)]
    root_resolution_count: AtomicU64,
    #[cfg(test)]
    ignore_query_count: AtomicU64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SourcePathChangeObservation {
    watcher_epoch: u64,
    watcher_generation: u64,
    #[serde(default)]
    registration_generation: u64,
    repo_root: PathBuf,
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
}

impl SourcePathChangeObservation {
    pub(crate) fn source_dependency(&self) -> crate::tool_history::SourceDependencyV1 {
        crate::tool_history::SourceDependencyV1::new(&self.path, self.recursive)
    }
}

#[derive(Clone, Debug)]
struct SourceChangeEvent {
    generation: u64,
    exact_path_keys: Option<Vec<String>>,
    subtree_keys: Option<Vec<String>>,
}

#[derive(Default)]
struct SourceChangeJournal {
    retained_floor: u64,
    latest_generation: u64,
    events: VecDeque<SourceChangeEvent>,
    exact_path_generations: HashMap<String, VecDeque<u64>>,
    subtree_generations: HashMap<String, VecDeque<u64>>,
    coarse_generations: VecDeque<u64>,
    #[cfg(test)]
    freshness_lookup_count: usize,
}

impl SourceChangeJournal {
    fn record(&mut self, generation: u64, changed_paths: Option<Vec<PathBuf>>) {
        let (exact_path_keys, subtree_keys) = if let Some(changed_paths) = changed_paths {
            let exact_path_keys = changed_paths
                .iter()
                .map(|path| source_change_path_key(path))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let subtree_keys = changed_paths
                .iter()
                .flat_map(|path| path.ancestors())
                .map(source_change_path_key)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            for key in &exact_path_keys {
                self.exact_path_generations
                    .entry(key.clone())
                    .or_default()
                    .push_back(generation);
            }
            for key in &subtree_keys {
                self.subtree_generations
                    .entry(key.clone())
                    .or_default()
                    .push_back(generation);
            }
            (Some(exact_path_keys), Some(subtree_keys))
        } else {
            self.coarse_generations.push_back(generation);
            (None, None)
        };
        self.latest_generation = generation;
        self.events.push_back(SourceChangeEvent {
            generation,
            exact_path_keys,
            subtree_keys,
        });
        while self.events.len() > SOURCE_CHANGE_JOURNAL_CAPACITY {
            if let Some(removed) = self.events.pop_front() {
                self.remove(&removed);
                self.retained_floor = removed.generation;
            }
        }
    }

    fn remove(&mut self, event: &SourceChangeEvent) {
        if let Some(keys) = &event.exact_path_keys {
            for key in keys {
                remove_indexed_generation(&mut self.exact_path_generations, key, event.generation);
            }
        } else if self.coarse_generations.front() == Some(&event.generation) {
            self.coarse_generations.pop_front();
        }
        if let Some(keys) = &event.subtree_keys {
            for key in keys {
                remove_indexed_generation(&mut self.subtree_generations, key, event.generation);
            }
        }
    }

    fn path_changed_since(&mut self, observation: &SourcePathChangeObservation) -> bool {
        self.record_freshness_lookup();
        if self
            .coarse_generations
            .back()
            .is_some_and(|generation| *generation > observation.watcher_generation)
        {
            return true;
        }
        for ancestor in observation.path.ancestors() {
            let key = source_change_path_key(ancestor);
            self.record_freshness_lookup();
            if index_has_generation_after(
                &self.exact_path_generations,
                &key,
                observation.watcher_generation,
            ) {
                return true;
            }
        }
        if observation.recursive {
            let key = source_change_path_key(&observation.path);
            self.record_freshness_lookup();
            if index_has_generation_after(
                &self.subtree_generations,
                &key,
                observation.watcher_generation,
            ) {
                return true;
            }
        }
        false
    }

    fn record_freshness_lookup(&mut self) {
        #[cfg(test)]
        {
            self.freshness_lookup_count += 1;
        }
    }
}

fn remove_indexed_generation(
    index: &mut HashMap<String, VecDeque<u64>>,
    key: &str,
    generation: u64,
) {
    let remove_entry = if let Some(generations) = index.get_mut(key) {
        if generations.front() == Some(&generation) {
            generations.pop_front();
        }
        generations.is_empty()
    } else {
        false
    };
    if remove_entry {
        index.remove(key);
    }
}

fn index_has_generation_after(
    index: &HashMap<String, VecDeque<u64>>,
    key: &str,
    generation: u64,
) -> bool {
    index
        .get(key)
        .and_then(|generations| generations.back())
        .is_some_and(|indexed_generation| *indexed_generation > generation)
}

impl GitWorkspaceCache {
    /// Capture in the selected executor, keeping foreign paths out of host discovery.
    pub(crate) async fn workspace_evidence_for_environment(
        &self,
        environments: &TurnEnvironmentSnapshot,
        host_cwd: &Path,
        cwd: &Path,
    ) -> WorkspaceEvidenceCapture {
        let Some(selected) = environments
            .primary()
            .filter(|selected| selected.environment.is_remote())
        else {
            return self.workspace_evidence_identity_with_attribution(cwd).await;
        };
        let remote_cwd = if let Ok(relative) = cwd.strip_prefix(host_cwd) {
            selected
                .cwd()
                .join(&relative.to_string_lossy().replace('\\', "/"))
                .ok()
        } else {
            AbsolutePathBuf::from_absolute_path(cwd)
                .ok()
                .map(|path| PathUri::from_abs_path(&path))
        };
        match remote_cwd {
            Some(cwd) => remote_workspace_evidence_capture(&selected.environment, &cwd).await,
            None => WorkspaceEvidenceCapture {
                identity: Some(WorkspaceEvidenceIdentity::unavailable(None)),
                timed_out_git_dependencies: Vec::new(),
            },
        }
    }

    pub(crate) async fn workspace_evidence_for_uri(
        &self,
        turn: &crate::session::turn_context::TurnContext,
        cwd: &PathUri,
        environment_id: &str,
    ) -> Option<WorkspaceEvidenceIdentity> {
        match turn
            .environments
            .turn_environments
            .iter()
            .find(|selected| selected.environment_id == environment_id)
        {
            Some(selected) if selected.environment.is_remote() => {
                remote_workspace_evidence_capture(&selected.environment, cwd)
                    .await
                    .identity
            }
            Some(_) => match cwd.to_abs_path() {
                Ok(cwd) => {
                    self.workspace_evidence_for_turn(turn, cwd.as_path())
                        .await
                        .identity
                }
                Err(_) => Some(WorkspaceEvidenceIdentity::unavailable(None)),
            },
            None => Some(WorkspaceEvidenceIdentity::unavailable(None)),
        }
    }

    pub(crate) async fn workspace_evidence_for_turn(
        &self,
        turn: &crate::session::turn_context::TurnContext,
        cwd: &Path,
    ) -> WorkspaceEvidenceCapture {
        self.workspace_evidence_for_environment(&turn.environments, turn.config.cwd.as_path(), cwd)
            .await
    }
    pub(crate) fn new() -> Arc<Self> {
        if tokio::runtime::Handle::try_current().is_err() {
            warn!("Git workspace cache disabled because no Tokio runtime is available");
            return Self::with_watcher(None);
        }
        // Sessions share one watcher, so sessions in the same repository share
        // its backend watches instead of each watching and reconciling them.
        match FileWatcher::shared() {
            Ok(watcher) => Self::with_watcher(Some(watcher)),
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
        let (watcher_subscriber, receiver, source_watcher_subscriber, source_receiver) =
            match watcher {
                Some(watcher) => {
                    let (subscriber, receiver) = watcher.add_subscriber();
                    let (source_subscriber, source_receiver) = watcher.add_subscriber();
                    (
                        Some(subscriber),
                        Some(receiver),
                        Some(source_subscriber),
                        Some(source_receiver),
                    )
                }
                None => (None, None, None, None),
            };
        let watcher_worker = if watcher_subscriber.is_some() || source_watcher_subscriber.is_some()
        {
            match GitWatchWorker::start(watcher_subscriber, source_watcher_subscriber) {
                Ok(worker) => Some(worker),
                Err(error) => {
                    warn!(%error, "Git workspace cache disabled because watch worker could not start");
                    None
                }
            }
        } else {
            None
        };
        let watcher_available = watcher_worker.is_some();
        let watcher_epoch = (u64::from(std::process::id()) << 32)
            | NEXT_WATCHER_EPOCH.fetch_add(1, Ordering::Relaxed);
        let cache = Arc::new(Self {
            state: Mutex::new(GitWorkspaceCacheState::default()),
            roots: Arc::default(),
            content_cache: Arc::default(),
            watcher_epoch,
            watcher_generation: AtomicU64::new(0),
            host_mutation_generation: AtomicU64::new(0),
            watcher_reliable: AtomicBool::new(watcher_available),
            watcher_worker,
            source_watcher_generation: AtomicU64::new(0),
            source_capture_generation: AtomicU64::new(0),
            source_event_gate: tokio::sync::RwLock::new(()),
            ignored_directories: Mutex::new(HashMap::new()),
            source_watcher_reliable: AtomicBool::new(watcher_available),
            repository_retention: StdMutex::new(RepositoryRetention::default()),
            source_change_journal: StdMutex::new(SourceChangeJournal::default()),
            in_flight_workspace_evidence: StdMutex::new(HashMap::new()),
            workspace_evidence_capture_sequence: AtomicU64::new(0),
            #[cfg(test)]
            host_mutation_worker_hook: StdMutex::new(None),
            #[cfg(test)]
            workspace_evidence_capture_count: AtomicU64::new(0),
            #[cfg(test)]
            next_workspace_evidence_capture_pause: StdMutex::new(None),
            #[cfg(test)]
            workspace_evidence_waiter_joined: tokio::sync::Notify::new(),
            #[cfg(test)]
            root_resolution_count: AtomicU64::new(0),
            #[cfg(test)]
            ignore_query_count: AtomicU64::new(0),
        });
        if let Some(receiver) = receiver
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let weak_cache = Arc::downgrade(&cache);
            runtime.spawn(async move {
                let mut receiver =
                    DebouncedWatchReceiver::new(receiver, WORKSPACE_WATCHER_DEBOUNCE);
                while let Some(_event) = receiver.recv().await {
                    let Some(cache) = weak_cache.upgrade() else {
                        return;
                    };
                    // One generation per debounced filesystem burst keeps an authoritative
                    // capture key stable while an editor or build emits many adjacent events.
                    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);
                }
                if let Some(cache) = weak_cache.upgrade() {
                    cache.invalidate_for_watcher_failure().await;
                }
            });
        }
        if let Some(source_receiver) = source_receiver
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let weak_cache = Arc::downgrade(&cache);
            runtime.spawn(async move {
                let mut source_receiver =
                    DebouncedWatchReceiver::new(source_receiver, WORKSPACE_WATCHER_DEBOUNCE);
                while let Some(event) = source_receiver.recv().await {
                    let Some(cache) = weak_cache.upgrade() else {
                        return;
                    };
                    let changed_paths = (!event.rescan_required).then_some(event.paths);
                    cache.record_watched_source_change_event(changed_paths).await;
                }
                if let Some(cache) = weak_cache.upgrade() {
                    cache.invalidate_source_watcher();
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
    }

    fn invalidate_source_watcher(&self) {
        self.source_watcher_reliable.store(false, Ordering::Release);
        self.record_source_change_event(None);
    }

    /// Returns a freshly captured content-based workspace identity.
    ///
    /// Watcher delivery is asynchronous, so an unchanged watcher generation
    /// cannot prove that an external edit is not still waiting in the event
    /// queue. Authoritative evidence therefore never reuses watcher-backed
    /// metadata caches.
    #[cfg(test)]
    pub(crate) async fn workspace_evidence_identity(
        &self,
        cwd: &Path,
    ) -> Option<WorkspaceEvidenceIdentity> {
        self.workspace_evidence_identity_with_attribution(cwd)
            .await
            .identity
    }

    pub(crate) async fn resolve_workspace_root(&self, cwd: &Path) -> std::io::Result<Option<PathBuf>> {
        let roots = Arc::clone(&self.roots);
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || roots.resolve(&cwd))
            .await.map_err(std::io::Error::other)?
    }

    pub(crate) async fn workspace_evidence_identity_with_attribution(
        &self,
        cwd: &Path,
    ) -> WorkspaceEvidenceCapture {
        let discovery = self.resolve_workspace_root(cwd).await;
        let repo_root = match discovery {
            Ok(Some(root)) => root,
            result => {
                let identity = result
                    .err()
                    .map(|_| WorkspaceEvidenceIdentity::unavailable(None));
                let fallback_cwd = cwd.to_path_buf();
                let cwd = tokio::task::spawn_blocking(move || {
                    canonical_workspace_evidence_root(&fallback_cwd)
                })
                .await
                .unwrap_or_else(|_| cwd.to_path_buf());
                let mut retention = self
                    .repository_retention
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let sequence = self
                    .workspace_evidence_capture_sequence
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                for (root, cached) in &mut retention.latest_workspace_evidence {
                    if cwd.starts_with(root) {
                        cached.identity = identity.clone();
                        cached.capture_sequence = sequence;
                    }
                }
                // A later successful discovery must not rejoin a capture from before this failure.
                self.host_mutation_generation.fetch_add(1, Ordering::AcqRel);
                return WorkspaceEvidenceCapture {
                    identity,
                    timed_out_git_dependencies: Vec::new(),
                };
            }
        };
        let source_event_guard = self.source_event_gate.read().await;
        let key = WorkspaceEvidenceCaptureKey {
            repo_root: repo_root.clone(),
            watcher_generation: self.watcher_generation.load(Ordering::Acquire),
            source_watcher_generation: self.source_capture_generation.load(Ordering::Acquire),
            host_mutation_generation: self.host_mutation_generation.load(Ordering::Acquire),
        };
        drop(source_event_guard);
        let (in_flight_capture, coalesced) = {
            let mut in_flight = self
                .in_flight_workspace_evidence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut retention = self
                .repository_retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((failed_key, capture, failed_at)) = retention
                .latest_workspace_evidence
                .get(&repo_root)
                .and_then(|cached| cached.failed_capture.as_ref())
                && failed_key == &key
                && failed_at.elapsed() < WORKSPACE_FAILURE_RETRY_INTERVAL
            {
                let capture = capture.clone();
                retention.touch(&repo_root);
                return capture;
            }
            drop(retention);
            // A newer local observation epoch must never join an older
            // capture. Dropping the map's old shared future is cancellation
            // safe because every active caller owns its own clone, and keeps
            // abandoned epochs bounded to one entry per repository.
            in_flight.retain(|existing_key, capture| {
                !capture.interest.cancellation.is_cancelled()
                    && (existing_key.repo_root != repo_root || existing_key == &key)
            });
            match in_flight.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    entry.get().interest.waiters.fetch_add(1, Ordering::AcqRel);
                    (entry.get().clone(), true)
                }
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
                    let content_cache = Arc::clone(&self.content_cache);
                    let cancellation = CancellationToken::new();
                    let interest = Arc::new(WorkspaceCaptureInterest {
                        waiters: AtomicUsize::new(1),
                        cancellation: cancellation.clone(),
                    });
                    let future = async move {
                        #[cfg(test)]
                        if let Some(pause) = pause {
                            pause.started.notify_one();
                            pause.release.notified().await;
                        }
                        capture_workspace_evidence_with_cancellation(
                            capture_repo_root,
                            cancellation,
                            content_cache,
                        )
                        .await
                    }
                    .boxed()
                    .shared();
                    let capture = InFlightWorkspaceEvidenceCapture {
                        capture_sequence,
                        future,
                        interest,
                    };
                    entry.insert(capture.clone());
                    (capture, false)
                }
            }
        };
        let _waiter = WorkspaceCaptureWaiter {
            interest: Arc::clone(&in_flight_capture.interest),
            cache: self,
            key: key.clone(),
            sequence: in_flight_capture.capture_sequence,
        };
        #[cfg(test)]
        if coalesced {
            self.workspace_evidence_waiter_joined.notify_one();
        }
        #[cfg(not(test))]
        let _ = coalesced;
        let capture = in_flight_capture.future.await;
        // Publish before retiring the shared future, so a sibling cannot slip
        // between completion and negative-cache insertion and start a new scan.
        let mut in_flight = self
            .in_flight_workspace_evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let capture_sequence = in_flight_capture.capture_sequence;
        let mut retention = self
            .repository_retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if retention
            .latest_workspace_evidence
            .get(&repo_root)
            .is_none_or(|cached| cached.capture_sequence <= capture_sequence)
        {
            retention.latest_workspace_evidence.insert(
                repo_root.clone(),
                CachedWorkspaceEvidenceIdentity {
                    capture_sequence,
                    host_mutation_generation: key.host_mutation_generation,
                    identity: capture.identity.clone(),
                    failed_capture: capture.identity.as_ref()
                        .is_some_and(|identity| identity.unavailable)
                        .then(|| (key.clone(), capture.clone(), Instant::now())),
                },
            );
        }
        retention.touch(&repo_root);
        if in_flight.get(&key).is_some_and(|current| {
            current.capture_sequence == in_flight_capture.capture_sequence
        }) {
            in_flight.remove(&key);
        }
        drop(retention);
        drop(in_flight);
        capture
    }

    /// Returns the most recent authoritative identity already captured for a
    /// repository. Sampling refreshes this cache before the model can issue
    /// tools, so proven read-only children can reuse it without launching a
    /// second set of Git subprocesses at dispatch time.
    pub(crate) async fn latest_workspace_evidence_identity(
        &self,
        repo_root: &Path,
    ) -> Option<WorkspaceEvidenceIdentity> {
        let repo_root = repo_root.to_path_buf();
        let repo_root =
            tokio::task::spawn_blocking(move || canonical_workspace_evidence_root(&repo_root))
                .await
                .ok()?;
        let mut retention = self
            .repository_retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = retention.latest_workspace_evidence.get(&repo_root).cloned();
        if cached.is_some() {
            retention.touch(&repo_root);
        }
        cached
            // The first observer in a batch may follow a known writer that did
            // not need a baseline. Never lend it the pre-mutation sampling scan.
            .filter(|cached| {
                cached.host_mutation_generation
                    == self.host_mutation_generation.load(Ordering::Acquire)
            })
            .and_then(|cached| cached.identity)
            .filter(|identity| !identity.unavailable)
    }

    #[cfg(test)]
    pub(crate) fn workspace_evidence_capture_count(&self) -> u64 {
        self.workspace_evidence_capture_count
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn pause_next_workspace_evidence_capture(
        &self,
    ) -> Arc<WorkspaceEvidenceCapturePause> {
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
                let registration = self.register_dependencies(&before_dependencies).await;
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
                let registration = self.register_dependencies(&before_dependencies.files).await;
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

    async fn register_dependencies(&self, dependencies: &[DependencyFingerprint]) -> GitWatchLease {
        let Some(worker) = self.watcher_worker.as_ref() else {
            return GitWatchLease::default();
        };
        match worker
            .register(
                GitWatchKind::Metadata,
                dependencies
                    .iter()
                    .map(|dependency| WatchPath {
                        path: dependency.path.clone(),
                        recursive: false,
                    })
                    .collect(),
            )
            .await
        {
            Ok(registration) => registration,
            Err(err) => {
                warn!("Git workspace cache disabled after watch registration failed: {err}");
                self.watcher_reliable.store(false, Ordering::Release);
                self.watcher_generation.fetch_add(1, Ordering::AcqRel);
                GitWatchLease::default()
            }
        }
    }

    fn reliable_source_watcher_generation(&self) -> Option<u64> {
        self.source_watcher_reliable
            .load(Ordering::Acquire)
            .then(|| self.source_watcher_generation.load(Ordering::Acquire))
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
        self.record_source_change_with_capture_effect(changed_paths, true)
    }

    async fn record_watched_source_change_event(&self, changed_paths: Option<Vec<PathBuf>>) {
        let _guard = self.source_event_gate.write().await;
        let changed_paths = changed_paths.map(|paths| paths.into_iter()
            .filter(|path| !is_generated_codex_eval_path(path) && !is_git_internal_source_event(path))
            .collect::<Vec<_>>());
        if changed_paths.as_ref().is_some_and(Vec::is_empty) { return; }
        if changed_paths.is_none() { self.ignored_directories.lock().await.clear(); }
        let roots = self.repository_retention.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .source_watch_registrations.keys().cloned().collect::<Vec<_>>();
        let mut groups = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
        let mut ignored = changed_paths.is_some();
        if let Some(paths) = &changed_paths {
            for path in paths {
                let root = roots.iter().filter(|root| path.starts_with(root))
                    .max_by_key(|root| root.components().count());
                match root {
                    Some(root) => groups.entry(root.clone()).or_default().push(path.clone()),
                    None => { ignored = false; break; }
                }
            }
        }
        if ignored {
            let mut cache = self.ignored_directories.lock().await;
            cache.retain(|root, _| roots.contains(root));
            for (root, paths) in groups {
                let proofs = cache.entry(root.clone()).or_default();
                proofs.retain(IgnoredDirectoryProof::is_current);
                let pending = paths.into_iter().filter(|path| {
                    !proofs.iter().any(|proof| path.starts_with(&proof.directory))
                }).collect::<Vec<_>>();
                if pending.is_empty() { continue; }
                #[cfg(test)]
                self.ignore_query_count.fetch_add(1, Ordering::Relaxed);
                if !git_ignores_all_changed_paths(&root, &pending).await {
                    ignored = false;
                    break;
                }
                // Learn at most one directory per batch; never turn a source edit
                // burst into one Git invocation per path.
                if let Some(proof) = ignored_directory_proof(&root, &pending[0]).await {
                    if proofs.len() >= RETAINED_REPOSITORY_CAPACITY { proofs.clear(); }
                    proofs.push(proof);
                }
            }
        }
        // Ignored files remain observable dependencies. Only the git-status
        // capture identity is unaffected; never omit them from the journal.
        self.record_source_change_with_capture_effect(changed_paths, !ignored);
    }

    fn record_source_change_with_capture_effect(
        &self, changed_paths: Option<Vec<PathBuf>>, affects_capture: bool,
    ) -> u64 {
        if changed_paths.as_ref().is_some_and(Vec::is_empty) {
            return self.source_watcher_generation.load(Ordering::Acquire);
        }
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = self
            .source_watcher_generation
            .load(Ordering::Acquire)
            .saturating_add(1);
        journal.record(generation, changed_paths);
        if affects_capture {
            self.source_capture_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.source_watcher_generation
            .store(generation, Ordering::Release);
        generation
    }

    /// Starts a path-scoped observation backed by a retained recursive watch.
    /// The returned token is safe to persist in model-visible tool output: it
    /// is accepted only by this live watcher epoch and only while the bounded
    /// journal proves that the path and its ancestors were untouched.
    ///
    /// Production registers a dependency set in one call; this single-path
    /// shape is retained for tests that observe one path at a time.
    #[cfg(test)]
    pub(crate) async fn begin_source_path_change_observation(
        &self,
        repo_root: &Path,
        path: &Path,
        recursive: bool,
    ) -> Option<SourcePathChangeObservation> {
        self.begin_source_path_change_observations(repo_root, &[(path.to_path_buf(), recursive)])
            .await?
            .pop()
    }

    /// Share one repository watch, retaining narrow watches for external inputs.
    /// Canonicalization finishes before the observation generation is captured.
    pub(crate) async fn begin_source_path_change_observations(
        &self,
        repo_root: &Path,
        paths: &[(PathBuf, bool)],
    ) -> Option<Vec<SourcePathChangeObservation>> {
        if !self.source_watcher_reliable.load(Ordering::Acquire)
            || paths
                .iter()
                .any(|(path, _)| is_generated_codex_eval_path(path))
        {
            return None;
        }
        let repo_root = repo_root.to_path_buf();
        let paths = paths.to_vec();
        let paths = tokio::task::spawn_blocking(move || {
            let repo_root = dunce::canonicalize(&repo_root).unwrap_or(repo_root);
            paths
                .into_iter()
                .map(|(path, recursive)| {
                    let path = dunce::canonicalize(&path).unwrap_or(path);
                    let watch_root = if path_is_same_or_descendant(&path, &repo_root) {
                        repo_root.clone()
                    } else {
                        // FileWatcher handles files and missing paths without
                        // recursively watching their entire parent directory.
                        path.clone()
                    };
                    (watch_root, path, recursive)
                })
                .collect::<Vec<_>>()
        })
        .await
        .ok()?;
        let mut registrations = BTreeMap::new();
        for (watch_root, _, _) in &paths {
            if !registrations.contains_key(watch_root) {
                let generation = self.retain_source_watch(watch_root).await?;
                registrations.insert(watch_root.clone(), generation);
            }
        }
        let watcher_generation = self.reliable_source_watcher_generation()?;
        Some(
            paths
                .into_iter()
                .map(|(watch_root, path, recursive)| SourcePathChangeObservation {
                    watcher_epoch: self.watcher_epoch,
                    watcher_generation,
                    registration_generation: registrations[&watch_root],
                    repo_root: watch_root,
                    path,
                    recursive,
                })
                .collect(),
        )
    }

    async fn retain_source_watch(&self, repo_root: &Path) -> Option<u64> {
        let existing_generation = {
            let mut retention = self
                .repository_retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation = retention
                .source_watch_registrations
                .get(repo_root)
                .map(|registration| registration.generation);
            if generation.is_some() {
                retention.touch(repo_root);
            }
            generation
        };
        let registration_generation = if let Some(generation) = existing_generation {
            generation
        } else {
            // Never wait for backend registration with the retention mutex held.
            // The candidate is a lease; cancellation and a losing insertion race
            // retire its raw registration on the worker.
            let registration = self
                .watcher_worker
                .as_ref()?
                .register(
                    GitWatchKind::Source,
                    vec![WatchPath {
                        path: repo_root.to_path_buf(),
                        recursive: true,
                    }],
                )
                .await
                .ok()?;
            let mut retention = self
                .repository_retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation =
                if let Some(existing) = retention.source_watch_registrations.get(repo_root) {
                    existing.generation
                } else {
                    let generation = retention.allocate_registration_generation();
                    retention.source_watch_registrations.insert(
                        repo_root.to_path_buf(),
                        RetainedSourceWatchRegistration {
                            generation,
                            _registration: registration,
                        },
                    );
                    generation
                };
            retention.touch(repo_root);
            generation
        };
        Some(registration_generation)
    }

    pub(crate) fn source_path_change_observation_is_current(
        &self,
        observation: &SourcePathChangeObservation,
    ) -> bool {
        if observation.watcher_epoch != self.watcher_epoch
            || is_generated_codex_eval_path(&observation.path)
            || !path_is_same_or_descendant(&observation.path, &observation.repo_root)
        {
            return false;
        }
        let Some(current_generation) = self.reliable_source_watcher_generation() else {
            return false;
        };
        if current_generation < observation.watcher_generation {
            return false;
        }
        {
            let mut retention = self
                .repository_retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let registration_is_current = retention
                .source_watch_registrations
                .get(&observation.repo_root)
                .is_some_and(|registration| {
                    registration.generation == observation.registration_generation
                });
            if !registration_is_current {
                return false;
            }
            retention.touch(&observation.repo_root);
        }
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if (journal.retained_floor != 0 && observation.watcher_generation <= journal.retained_floor)
            || journal.latest_generation != current_generation
        {
            return false;
        }
        !journal.path_changed_since(observation)
    }

    #[cfg(test)]
    pub(crate) fn take_source_change_freshness_lookup_count_for_test(&self) -> usize {
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut journal.freshness_lookup_count)
    }

    pub(crate) fn note_host_workspace_mutation(&self) {
        self.host_mutation_generation.fetch_add(1, Ordering::AcqRel);
        self.watcher_generation.fetch_add(1, Ordering::AcqRel);
        self.record_source_change_event(None);
    }

    pub(crate) async fn note_host_workspace_mutation_paths(
        self: &Arc<Self>,
        repo_root: &Path,
        changed_paths: &[String],
    ) {
        let cache = Arc::clone(self);
        let repo_root = repo_root.to_path_buf();
        let changed_paths = changed_paths.to_vec();
        // The worker owns publication as well as filesystem observation: a
        // cancelled caller must not discard a completed mutation invalidation.
        if let Err(error) = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let hook = cache
                .host_mutation_worker_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            cache.record_host_workspace_mutation_paths(&repo_root, &changed_paths);
        })
        .await
        {
            warn!(%error, "Host mutation path worker failed; invalidating coarse workspace evidence");
            self.note_host_workspace_mutation();
        }
    }

    fn record_host_workspace_mutation_paths(&self, repo_root: &Path, changed_paths: &[String]) {
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

fn is_git_internal_source_event(path: &Path) -> bool {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git") {
            // Metadata has its own watch. Only staging and ignore-rule changes
            // under .git must also invalidate the workspace source capture.
            // Keep changes to the .git marker itself observable as well.
            let relative = components.as_path();
            let relative = relative.to_string_lossy().replace('\\', "/");
            return !relative.is_empty()
                && !relative.eq_ignore_ascii_case("index")
                && !relative.eq_ignore_ascii_case("info/exclude");
        }
    }
    false
}

fn canonical_workspace_evidence_root(repo_root: &Path) -> PathBuf {
    dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf())
}

fn path_is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path_is_same_or_descendant_with_case_sensitivity(path, ancestor, false)
}

fn source_change_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
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
        let repo_root = source.repo_root.clone();
        let (executable, files) = run_blocking_git_metadata(move || {
            let executable = which::which(codex_git_utils::git_executable()).ok()?;
            let executable = executable.canonicalize().unwrap_or(executable);
            let (git_dir, common_dir, head_ref) = resolve_git_dirs(&repo_root)?;
            let mut paths = vec![
                (executable.clone(), false),
                (repo_root.join(".git").into_path_buf(), true),
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
            Some((executable, files))
        })
        .await?;
        let config_signature = git_config_signature(&executable, source.cwd.as_path()).await?;
        Some(Self {
            files,
            config_signature,
        })
    }
}

async fn run_blocking_git_metadata<T, F>(capture: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    tokio::task::spawn_blocking(capture).await.ok().flatten()
}

pub(crate) fn resolve_git_dirs(repo_root: &AbsolutePathBuf) -> Option<(PathBuf, PathBuf, Option<PathBuf>)> {
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, async move {
        // Own the entire process tree until output collection completes. A Git
        // wrapper may launch helpers even for a read-only configuration probe.
        #[cfg(windows)]
        let (_managed, child) = {
            command.creation_flags(codex_utils_pty::WINDOWS_CREATE_SUSPENDED);
            let managed = codex_utils_pty::ManagedRootProcess::reserve_with_reclaim()
                .await
                .ok()?;
            managed.require_descendant_containment().ok()?;
            tokio::task::spawn_blocking(move || {
                let mut child = command.spawn()?;
                let pid = child
                    .id()
                    .ok_or_else(|| std::io::Error::other("missing Git process id"))?;
                if let Err(error) = managed.attach_and_resume(pid) {
                    let _ = child.start_kill();
                    return Err(error);
                }
                Ok((managed, child))
            })
            .await
            .ok()?
            .ok()?
        };
        #[cfg(not(windows))]
        let child = command.spawn().ok()?;
        child.wait_with_output().await.ok()
    })
    .await
    .ok()??;
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
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => hasher.update(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
        Some(hasher.finalize().into())
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
    // SAFETY: `file` owns a live handle for this call, and `info` is an aligned
    // FILE_ID_INFO buffer whose exact size is supplied to the matching query class.
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
    // SAFETY: the successful FileIdInfo query initialized the complete buffer above.
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
