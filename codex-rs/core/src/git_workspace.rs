use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use codex_config::ProjectDiscoveryContext;
use codex_file_system::ExecutorFileSystem;
use codex_file_watcher::DebouncedWatchReceiver;
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
const WORKSPACE_GENERATION_DEADLINE: Duration = Duration::from_secs(5);
const WORKSPACE_GENERATION_MAX_PATHS: usize = 256;
const WORKSPACE_GENERATION_MAX_DECLARED_BYTES: u64 = 64 * 1024 * 1024;
const WORKSPACE_GENERATION_MAX_INDEX_BYTES: usize = 128 * 1024 * 1024;
const WORKSPACE_GENERATION_MAX_INDEX_PATHS: usize = 1_000_000;
const WORKSPACE_WATCHER_DEBOUNCE: Duration = Duration::from_millis(50);
const SOURCE_WATCHER_BARRIER_TIMEOUT: Duration = Duration::from_secs(2);
const SOURCE_WATCHER_BARRIER_POLL: Duration = Duration::from_millis(10);
const SOURCE_WATCHER_BARRIER_PREFIX: &str = ".completion-proof-watcher-barrier-";
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

pub(crate) async fn capture_workspace_evidence_identity(
    cwd: &Path,
) -> Option<WorkspaceEvidenceIdentity> {
    capture_workspace_evidence_identity_with_attribution(cwd)
        .await
        .identity
}

async fn capture_workspace_evidence_identity_with_attribution(
    cwd: &Path,
) -> WorkspaceEvidenceCapture {
    let Some(repo_root) = resolve_workspace_evidence_root(cwd).await else {
        return WorkspaceEvidenceCapture::default();
    };
    capture_workspace_evidence_identity_for_repo_root_with_attribution(repo_root).await
}

async fn capture_workspace_evidence_identity_for_repo_root_with_attribution(
    repo_root: PathBuf,
) -> WorkspaceEvidenceCapture {
    match within_workspace_generation_deadline(
        WORKSPACE_GENERATION_DEADLINE,
        capture_workspace_generation_marker(repo_root),
    )
    .await
    {
        Ok(identity) => WorkspaceEvidenceCapture {
            identity,
            timed_out_git_dependencies: Vec::new(),
        },
        Err(_) => WorkspaceEvidenceCapture {
            identity: None,
            timed_out_git_dependencies: vec![
                WorkspaceEvidenceGitDependency::Head,
                WorkspaceEvidenceGitDependency::Index,
                WorkspaceEvidenceGitDependency::Worktree,
                WorkspaceEvidenceGitDependency::Untracked,
            ],
        },
    }
}

async fn resolve_workspace_evidence_root(cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        get_git_repo_root(&cwd)
            .as_deref()
            .map(canonical_workspace_evidence_root)
    })
    .await
    .ok()
    .flatten()
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
        "-z",
        "--untracked-files=all",
        "--",
        ".",
    ]
}

fn workspace_generation_index_args() -> &'static [&'static str] {
    &[
        "ls-files", "--cached", "--stage", "-v", "-f", "-z", "--", ".",
    ]
}

async fn capture_workspace_generation_marker(
    repo_root: PathBuf,
) -> Option<WorkspaceEvidenceIdentity> {
    let (head, index, status) = tokio::join!(
        workspace_generation_git_output(&repo_root, &["rev-parse", "--verify", "HEAD"]),
        workspace_generation_git_output(&repo_root, workspace_generation_index_args()),
        workspace_generation_git_output(&repo_root, workspace_generation_status_args()),
    );
    let index = index?;
    workspace_generation_index(&index)?;
    let status = status?;
    let paths = workspace_generation_paths(&status)?;
    let metadata = workspace_generation_metadata(repo_root.clone(), paths).await?;

    let mut index_hasher = Sha256::new();
    index_hasher.update(b"KD4_WORKSPACE_INDEX_GENERATION_V2\n");
    index_hasher.update(&index);
    let mut worktree_hasher = Sha256::new();
    worktree_hasher.update(b"KD4_WORKSPACE_WORKTREE_GENERATION_V2\n");
    worktree_hasher.update(&index);
    worktree_hasher.update(b"\0status\0");
    worktree_hasher.update(&status);
    worktree_hasher.update(b"\0manifest\0");
    worktree_hasher.update(&metadata.manifest);

    Some(WorkspaceEvidenceIdentity {
        repository_root: Some(
            dunce::canonicalize(&repo_root)
                .unwrap_or(repo_root)
                .to_string_lossy()
                .into_owned(),
        ),
        head_identity: workspace_head_identity(head),
        index_identity: Some(format!("{:x}", index_hasher.finalize())),
        worktree_identity: Some(format!("{:x}", worktree_hasher.finalize())),
    })
}

fn workspace_generation_index(index: &[u8]) -> Option<()> {
    if index.len() > WORKSPACE_GENERATION_MAX_INDEX_BYTES {
        return None;
    }
    if index.is_empty() {
        return Some(());
    }
    if index.last() != Some(&0) {
        return None;
    }

    let mut paths = BTreeSet::new();
    let mut casefolded_paths = BTreeMap::new();
    let mut path_count = 0_usize;
    for record in index[..index.len().saturating_sub(1)].split(|byte| *byte == 0) {
        if record.is_empty() {
            return None;
        }
        path_count = path_count.checked_add(1)?;
        if path_count > WORKSPACE_GENERATION_MAX_INDEX_PATHS {
            return None;
        }

        let tab = record.iter().position(|byte| *byte == b'\t')?;
        let header = record.get(..tab)?;
        let raw_path = record.get(tab.checked_add(1)?..)?;
        if raw_path.is_empty() {
            return None;
        }
        let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
        let [tag, mode, object_id, stage] = fields.as_slice() else {
            return None;
        };
        if tag.len() != 1
            || mode.len() != 6
            || !mode.iter().all(|byte| matches!(byte, b'0'..=b'7'))
            || !matches!(object_id.len(), 40 | 64)
            || !object_id
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            || stage.len() != 1
            || !matches!(stage[0], b'0'..=b'3')
        {
            return None;
        }
        let path = strict_workspace_generation_path(raw_path)?;
        if *tag != b"H" || *stage != b"0" {
            return None;
        }
        match *mode {
            b"100644" | b"100755" => {}
            b"120000" if !workspace_generation_test_surface_candidate(&path) => {}
            b"160000" => return None,
            _ => return None,
        }
        if !paths.insert(path.clone()) {
            return None;
        }
        let casefolded = path.to_lowercase();
        if casefolded_paths
            .insert(casefolded, path.clone())
            .is_some_and(|previous| previous != path)
        {
            return None;
        }
    }
    Some(())
}

fn workspace_generation_test_surface_candidate(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let python_test =
        name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"));
    let javascript_config = ["jest", "playwright", "vitest"].iter().any(|runner| {
        ["cjs", "cts", "js", "mjs", "mts", "ts"]
            .iter()
            .any(|suffix| name == format!("{runner}.config.{suffix}"))
    });
    let javascript_test = ["test", "spec"].iter().any(|kind| {
        ["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"]
            .iter()
            .any(|suffix| name.ends_with(&format!(".{kind}.{suffix}")))
    });
    path == "justfile"
        || name == "Cargo.toml"
        || python_test
        || matches!(
            name,
            "package.json" | "pyproject.toml" | "pytest.ini" | "setup.cfg" | "tox.ini"
        )
        || javascript_config
        || javascript_test
        || name.ends_with("_test.go")
        || name.ends_with(".bats")
}

fn workspace_generation_paths(status: &[u8]) -> Option<Vec<String>> {
    if status.is_empty() {
        return Some(Vec::new());
    }
    if status.last() != Some(&0) {
        return None;
    }

    let records = status[..status.len().saturating_sub(1)]
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            return None;
        }
        match record.first().copied() {
            Some(b'1') => {
                paths.insert(strict_workspace_generation_path(porcelain_record_path(
                    record, 8,
                )?)?);
            }
            Some(b'2') => {
                paths.insert(strict_workspace_generation_path(porcelain_record_path(
                    record, 9,
                )?)?);
                index = index.checked_add(1)?;
                paths.insert(strict_workspace_generation_path(*records.get(index)?)?);
            }
            Some(b'u') => {
                paths.insert(strict_workspace_generation_path(porcelain_record_path(
                    record, 10,
                )?)?);
            }
            Some(b'?') if record.get(1) == Some(&b' ') => {
                paths.insert(strict_workspace_generation_path(record.get(2..)?)?);
            }
            _ => return None,
        }
        index = index.checked_add(1)?;
    }
    Some(paths.into_iter().collect())
}

fn porcelain_record_path(record: &[u8], field_index: usize) -> Option<&[u8]> {
    if record.get(1) != Some(&b' ') {
        return None;
    }
    record
        .splitn(field_index.saturating_add(1), |byte| *byte == b' ')
        .nth(field_index)
}

fn strict_workspace_generation_path(raw_path: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(raw_path).ok()?;
    let windows_drive = path.as_bytes().get(1) == Some(&b':')
        && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    if path.is_empty()
        || path.contains('\\')
        || Path::new(path).is_absolute()
        || path.starts_with('/')
        || windows_drive
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || path.chars().any(|character| character.is_control())
        || path.eq_ignore_ascii_case(".git")
        || path
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".git/"))
    {
        return None;
    }
    Some(path.to_string())
}

fn workspace_head_identity(head: Option<Vec<u8>>) -> Option<String> {
    head.and_then(|head| {
        String::from_utf8(head)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

struct WorkspaceGenerationMetadata {
    manifest: Vec<u8>,
}

async fn workspace_generation_metadata(
    repo_root: PathBuf,
    paths: Vec<String>,
) -> Option<WorkspaceGenerationMetadata> {
    tokio::task::spawn_blocking(move || {
        if paths.len() > WORKSPACE_GENERATION_MAX_PATHS {
            return None;
        }
        let total_paths = paths.len();
        let mut manifest = format!("total_paths={total_paths}\n").into_bytes();
        let mut observed_declared_bytes = 0_u64;

        for path in paths {
            let absolute = repo_root.join(&path);
            let metadata = match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => return None,
            };
            let declared_bytes = metadata
                .as_ref()
                .filter(|metadata| metadata.is_file())
                .map_or(0, std::fs::Metadata::len);
            if observed_declared_bytes.saturating_add(declared_bytes)
                > WORKSPACE_GENERATION_MAX_DECLARED_BYTES
            {
                return None;
            }
            let kind = if metadata.is_none() {
                "missing"
            } else if metadata
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                "symlink"
            } else if metadata.as_ref().is_some_and(std::fs::Metadata::is_file) {
                "file"
            } else if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
                "directory"
            } else {
                "other"
            };
            manifest.extend_from_slice(path.as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(kind.as_bytes());
            manifest.push(0);
            manifest.extend_from_slice(declared_bytes.to_string().as_bytes());
            manifest.push(0);
            if metadata
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                let target = std::fs::read_link(&absolute).ok()?;
                manifest.extend_from_slice(
                    format!("{:x}", Sha256::digest(target.to_string_lossy().as_bytes())).as_bytes(),
                );
            } else if metadata.as_ref().is_some_and(std::fs::Metadata::is_file) {
                let remaining =
                    WORKSPACE_GENERATION_MAX_DECLARED_BYTES.saturating_sub(observed_declared_bytes);
                let mut content = Vec::new();
                File::open(&absolute)
                    .ok()?
                    .take(remaining.saturating_add(1))
                    .read_to_end(&mut content)
                    .ok()?;
                if u64::try_from(content.len()).ok()? > remaining {
                    return None;
                }
                manifest.extend_from_slice(format!("{:x}", Sha256::digest(&content)).as_bytes());
            }
            manifest.push(b'\n');
            observed_declared_bytes = observed_declared_bytes.saturating_add(declared_bytes);
        }
        Some(WorkspaceGenerationMetadata { manifest })
    })
    .await
    .ok()?
}

async fn workspace_generation_git_output(repo_root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn()).ok()?;
    let output = child.wait_with_output().await.ok()?;
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

#[derive(Clone)]
struct CachedWorkspaceEvidenceIdentity {
    capture_sequence: u64,
    identity: Option<WorkspaceEvidenceIdentity>,
}

struct RetainedSourceWatchRegistration {
    generation: u64,
    _registration: WatchRegistration,
    barrier: SourceWatcherBarrier,
}

struct SourceWatcherBarrier {
    path: PathBuf,
    file: StdMutex<Option<File>>,
}

impl SourceWatcherBarrier {
    fn establish(repo_root: &Path) -> Option<Self> {
        let barrier_parent = AbsolutePathBuf::from_absolute_path(repo_root)
            .ok()
            .and_then(|repo_root| resolve_git_dirs(&repo_root).map(|(git_dir, _, _)| git_dir))
            .filter(|git_dir| git_dir.is_dir())
            .unwrap_or_else(|| repo_root.to_path_buf());
        let path = barrier_parent.join(format!(
            "{SOURCE_WATCHER_BARRIER_PREFIX}{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        file.write_all(&0_u64.to_le_bytes()).ok()?;
        Some(Self {
            path,
            file: StdMutex::new(Some(file)),
        })
    }

    fn signal(&self, sequence: u64) -> bool {
        let mut retained_file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = match retained_file.take() {
            Some(file) => file,
            None => {
                let Ok(file) = std::fs::OpenOptions::new().write(true).open(&self.path) else {
                    return false;
                };
                file
            }
        };
        let signaled = file.seek(SeekFrom::Start(0)).is_ok()
            && file.write_all(&sequence.to_le_bytes()).is_ok();
        drop(file);
        signaled
    }
}

impl Drop for SourceWatcherBarrier {
    fn drop(&mut self) {
        let file = self
            .file
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(file);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
struct RepositoryRetention {
    source_watch_registrations: HashMap<PathBuf, Arc<RetainedSourceWatchRegistration>>,
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
    watcher_epoch: u64,
    watcher_generation: AtomicU64,
    host_mutation_generation: AtomicU64,
    watcher_reliable: AtomicBool,
    watcher_subscriber: Option<FileWatcherSubscriber>,
    source_watcher_generation: AtomicU64,
    source_watcher_barrier_sequence: AtomicU64,
    source_watcher_reliable: AtomicBool,
    source_watcher_subscriber: Option<FileWatcherSubscriber>,
    repository_retention: StdMutex<RepositoryRetention>,
    source_change_journal: StdMutex<SourceChangeJournal>,
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

#[derive(Clone, Debug)]
pub(crate) struct SourcePathChangeHandoff {
    pub(crate) exact_paths: Vec<PathBuf>,
    pub(crate) had_non_runtime_change: bool,
    pub(crate) continuation: SourcePathChangeObservation,
}

impl SourcePathChangeHandoff {
    pub(crate) fn classifiable_exact_paths(&self) -> Option<Vec<PathBuf>> {
        (!self.had_non_runtime_change || !self.exact_paths.is_empty())
            .then(|| self.exact_paths.clone())
    }
}

#[derive(Clone, Debug)]
struct SourceChangeEvent {
    generation: u64,
    exact_paths: Option<Vec<PathBuf>>,
    exact_path_keys: Option<Vec<String>>,
    runtime_owned_path_keys: BTreeSet<String>,
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
    active_runtime_owned_exact_path_keys: BTreeSet<String>,
    #[cfg(test)]
    freshness_lookup_count: usize,
}

impl SourceChangeJournal {
    fn record(&mut self, generation: u64, changed_paths: Option<Vec<PathBuf>>) {
        let (exact_paths, exact_path_keys, runtime_owned_path_keys, subtree_keys) =
            if let Some(changed_paths) = changed_paths {
                let exact_paths = changed_paths
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let exact_path_keys = exact_paths
                    .iter()
                    .map(|path| source_change_path_key(path))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let runtime_owned_exact_paths = exact_paths
                    .iter()
                    .filter(|path| {
                        self.active_runtime_owned_exact_path_keys
                            .contains(&source_change_path_key(path))
                    })
                    .collect::<Vec<_>>();
                let runtime_owned_path_keys = exact_paths
                    .iter()
                    .filter(|path| {
                        runtime_owned_exact_paths
                            .iter()
                            .any(|runtime_owned| path_is_same_or_descendant(runtime_owned, path))
                    })
                    .map(|path| source_change_path_key(path))
                    .collect::<BTreeSet<_>>();
                let evidence_paths = exact_paths.iter().filter(|path| {
                    !runtime_owned_path_keys.contains(&source_change_path_key(path))
                });
                let subtree_keys = evidence_paths
                    .flat_map(|path| path.ancestors())
                    .map(source_change_path_key)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                for key in exact_path_keys
                    .iter()
                    .filter(|key| !runtime_owned_path_keys.contains(*key))
                {
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
                (
                    Some(exact_paths),
                    Some(exact_path_keys),
                    runtime_owned_path_keys,
                    Some(subtree_keys),
                )
            } else {
                self.coarse_generations.push_back(generation);
                (None, None, BTreeSet::new(), None)
            };
        self.latest_generation = generation;
        self.events.push_back(SourceChangeEvent {
            generation,
            exact_paths,
            exact_path_keys,
            runtime_owned_path_keys,
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

    fn exact_path_reported_since(&self, generation: u64, path: &Path) -> bool {
        let key = source_change_path_key(path);
        self.events.iter().any(|event| {
            event.generation > generation
                && event
                    .exact_path_keys
                    .as_ref()
                    .is_some_and(|paths| paths.iter().any(|path| path == &key))
        })
    }

    fn exact_paths_since(&self, observation: &SourcePathChangeObservation) -> Option<Vec<PathBuf>> {
        if self.retained_floor != 0 && observation.watcher_generation <= self.retained_floor {
            return None;
        }
        let mut changed = BTreeSet::new();
        for event in self
            .events
            .iter()
            .filter(|event| event.generation > observation.watcher_generation)
        {
            let paths = event.exact_paths.as_ref()?;
            for path in paths {
                if event
                    .runtime_owned_path_keys
                    .contains(&source_change_path_key(path))
                    // Windows may report the watched directory and its parents
                    // while delivering a concrete child-path event. Those
                    // coarse paths do not identify changed repository content;
                    // retain only exact descendants of the repository root.
                    || path_is_same_or_descendant(&observation.repo_root, path)
                {
                    continue;
                }
                let affects_observation = path_is_same_or_descendant(&observation.path, path)
                    || (observation.recursive
                        && path_is_same_or_descendant(path, &observation.path));
                if affects_observation {
                    changed.insert(path.clone());
                }
            }
        }
        Some(changed.into_iter().collect())
    }

    fn record_freshness_lookup(&mut self) {
        #[cfg(test)]
        {
            self.freshness_lookup_count += 1;
        }
    }
}

struct RuntimeOwnedSourcePathRegistration<'a> {
    journal: &'a StdMutex<SourceChangeJournal>,
    path_key: String,
}

impl<'a> RuntimeOwnedSourcePathRegistration<'a> {
    fn register(
        journal: &'a StdMutex<SourceChangeJournal>,
        path: &Path,
    ) -> Option<RuntimeOwnedSourcePathRegistration<'a>> {
        let path_key = source_change_path_key(path);
        let inserted = journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_runtime_owned_exact_path_keys
            .insert(path_key.clone());
        inserted.then_some(RuntimeOwnedSourcePathRegistration { journal, path_key })
    }
}

impl Drop for RuntimeOwnedSourcePathRegistration<'_> {
    fn drop(&mut self) {
        self.journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_runtime_owned_exact_path_keys
            .remove(&self.path_key);
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
        let watcher_epoch = (u64::from(std::process::id()) << 32)
            | NEXT_WATCHER_EPOCH.fetch_add(1, Ordering::Relaxed);
        let cache = Arc::new(Self {
            state: Mutex::new(GitWorkspaceCacheState::default()),
            watcher_epoch,
            watcher_generation: AtomicU64::new(0),
            host_mutation_generation: AtomicU64::new(0),
            watcher_reliable: AtomicBool::new(watcher_subscriber.is_some()),
            watcher_subscriber,
            source_watcher_generation: AtomicU64::new(0),
            source_watcher_barrier_sequence: AtomicU64::new(0),
            source_watcher_reliable: AtomicBool::new(source_watcher_subscriber.is_some()),
            source_watcher_subscriber,
            repository_retention: StdMutex::new(RepositoryRetention::default()),
            source_change_journal: StdMutex::new(SourceChangeJournal::default()),
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
                    cache.record_source_change_event(changed_paths);
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
        state.project_namespaces.clear();
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
        let Some(repo_root) = resolve_workspace_evidence_root(cwd).await else {
            return WorkspaceEvidenceCapture::default();
        };
        let key = WorkspaceEvidenceCaptureKey {
            repo_root: repo_root.clone(),
            watcher_generation: self.watcher_generation.load(Ordering::Acquire),
            source_watcher_generation: self.source_watcher_generation.load(Ordering::Acquire),
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
                        capture_workspace_evidence_identity_for_repo_root_with_attribution(
                            capture_repo_root,
                        )
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
                    identity: capture.identity.clone(),
                },
            );
        }
        retention.touch(&repo_root);
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
        let mut retention = self
            .repository_retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = retention.latest_workspace_evidence.get(&repo_root).cloned();
        if cached.is_some() {
            retention.touch(&repo_root);
        }
        cached.and_then(|cached| cached.identity)
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
        let dependencies = StableMetadataDependencies::capture_project_namespace(source).await;
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
            let after_dependencies =
                StableMetadataDependencies::capture_project_namespace(source).await;
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

    fn reliable_source_watcher_generation(&self) -> Option<u64> {
        self.source_watcher_reliable
            .load(Ordering::Acquire)
            .then(|| self.source_watcher_generation.load(Ordering::Acquire))
    }

    fn record_source_change_event(&self, changed_paths: Option<Vec<PathBuf>>) -> u64 {
        let changed_paths = changed_paths;
        self.record_filtered_source_change_event(changed_paths)
    }

    fn record_filtered_source_change_event(&self, changed_paths: Option<Vec<PathBuf>>) -> u64 {
        if changed_paths.as_ref().is_some_and(Vec::is_empty) {
            return self.source_watcher_generation.load(Ordering::Acquire);
        }
        let generation = self
            .source_watcher_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        journal.record(generation, changed_paths);
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
        if !self.source_watcher_reliable.load(Ordering::Acquire) {
            return None;
        }
        let repo_root = dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !path_is_same_or_descendant(&path, &repo_root) {
            return None;
        }
        let registration_generation = {
            let mut retention = self
                .repository_retention
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let generation =
                if let Some(registration) = retention.source_watch_registrations.get(&repo_root) {
                    registration.generation
                } else {
                    let barrier = SourceWatcherBarrier::establish(&repo_root)?;
                    let registration = self
                        .source_watcher_subscriber
                        .as_ref()?
                        .register_paths(vec![
                            WatchPath {
                                path: repo_root.clone(),
                                recursive: true,
                            },
                            WatchPath {
                                path: barrier.path.clone(),
                                recursive: false,
                            },
                        ])
                        .ok()?;
                    let generation = retention.allocate_registration_generation();
                    retention.source_watch_registrations.insert(
                        repo_root.clone(),
                        Arc::new(RetainedSourceWatchRegistration {
                            generation,
                            _registration: registration,
                            barrier,
                        }),
                    );
                    generation
                };
            retention.touch(&repo_root);
            generation
        };
        let watcher_generation = self.reliable_source_watcher_generation()?;
        Some(SourcePathChangeObservation {
            watcher_epoch: self.watcher_epoch,
            watcher_generation,
            registration_generation,
            repo_root,
            path,
            recursive,
        })
    }

    /// Starts a root-scoped observation only after a watcher barrier has
    /// drained events that predate the interval.
    pub(crate) async fn begin_source_path_change_observation_after_barrier(
        &self,
        repo_root: &Path,
    ) -> Option<SourcePathChangeObservation> {
        let initial = self
            .begin_source_path_change_observation(repo_root, repo_root, /*recursive*/ true)?;
        let handoff = self
            .finish_source_path_change_observation_and_continue(&initial)
            .await?;
        Some(handoff.continuation)
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

    /// Completes a source-path observation with an OS-watcher barrier and
    /// returns every exact path reported during the interval. `None` means the
    /// watcher, journal, or barrier could not provide trustworthy evidence.
    pub(crate) async fn finish_source_path_change_observation(
        &self,
        observation: &SourcePathChangeObservation,
    ) -> Option<Vec<PathBuf>> {
        self.finish_source_path_change_observation_and_continue(observation)
            .await
            .and_then(|handoff| handoff.classifiable_exact_paths())
    }

    /// Completes an observation and returns a continuation token from the same
    /// journal snapshot. This lets callers hand off a long-lived observation
    /// without creating a nested barrier or leaving an unchecked interval.
    pub(crate) async fn finish_source_path_change_observation_and_continue(
        &self,
        observation: &SourcePathChangeObservation,
    ) -> Option<SourcePathChangeHandoff> {
        if observation.watcher_epoch != self.watcher_epoch
            || !observation.recursive
            || observation.path != observation.repo_root
        {
            return None;
        }
        let source_watch = self.source_observation_registration(observation)?;

        let sequence = self
            .source_watcher_barrier_sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let barrier_path = source_watch.barrier.path.clone();
        let _runtime_owned_barrier = RuntimeOwnedSourcePathRegistration::register(
            &self.source_change_journal,
            &barrier_path,
        )?;

        let signal_generation = self.reliable_source_watcher_generation()?;
        let source_watch_for_signal = Arc::clone(&source_watch);
        if !tokio::task::spawn_blocking(move || source_watch_for_signal.barrier.signal(sequence))
            .await
            .ok()?
        {
            return None;
        }
        if !self
            .wait_for_exact_source_path_event(signal_generation, &barrier_path)
            .await
        {
            return None;
        }

        let current_generation = self.reliable_source_watcher_generation()?;
        let mut journal = self
            .source_change_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if journal.latest_generation != current_generation {
            return None;
        }
        let had_non_runtime_change = journal.path_changed_since(observation);
        let exact_paths = journal.exact_paths_since(observation)?;
        let continuation = SourcePathChangeObservation {
            watcher_epoch: observation.watcher_epoch,
            watcher_generation: current_generation,
            registration_generation: observation.registration_generation,
            repo_root: observation.repo_root.clone(),
            path: observation.path.clone(),
            recursive: observation.recursive,
        };
        Some(SourcePathChangeHandoff {
            exact_paths,
            had_non_runtime_change,
            continuation,
        })
    }

    fn source_observation_registration(
        &self,
        observation: &SourcePathChangeObservation,
    ) -> Option<Arc<RetainedSourceWatchRegistration>> {
        let mut retention = self
            .repository_retention
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registration = retention
            .source_watch_registrations
            .get(&observation.repo_root)
            .filter(|registration| registration.generation == observation.registration_generation)?
            .clone();
        retention.touch(&observation.repo_root);
        Some(registration)
    }

    async fn wait_for_exact_source_path_event(&self, generation: u64, path: &Path) -> bool {
        let deadline = tokio::time::Instant::now() + SOURCE_WATCHER_BARRIER_TIMEOUT;
        loop {
            let Some(current_generation) = self.reliable_source_watcher_generation() else {
                return false;
            };
            let observed = {
                let journal = self
                    .source_change_journal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                journal.latest_generation == current_generation
                    && journal.exact_path_reported_since(generation, path)
            };
            if observed {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(SOURCE_WATCHER_BARRIER_POLL).await;
        }
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
            .collect::<Vec<_>>();
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

fn canonical_workspace_evidence_root(repo_root: &Path) -> PathBuf {
    dunce::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf())
}

fn path_is_same_or_descendant(path: &Path, ancestor: &Path) -> bool {
    path_is_same_or_descendant_with_case_sensitivity(path, ancestor, false)
}

fn source_change_path_key(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
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
            let executable = which::which("git").ok()?;
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

    async fn capture_project_namespace(source: &GitWorkspaceMetadataSource) -> Option<Self> {
        let repo_root = source.repo_root.clone();
        run_blocking_git_metadata(move || {
            let executable = which::which("git").ok()?;
            let executable = executable.canonicalize().unwrap_or(executable);
            let git_marker = repo_root.join(".git").into_path_buf();
            let (git_dir, common_dir, head_ref) = resolve_git_dirs(&repo_root)?;
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
        })
        .await
    }
}

async fn run_blocking_git_metadata<T, F>(capture: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    tokio::task::spawn_blocking(capture).await.ok().flatten()
}

async fn collect_project_namespace(cwd: &Path) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(["-c", "core.fsmonitor=false"])
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, async {
        let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn())?;
        child.wait_with_output().await
    })
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(GIT_DEPENDENCY_TIMEOUT, async {
        let child = codex_utils_pty::with_windows_child_creation(|_| command.spawn())?;
        child.wait_with_output().await
    })
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
