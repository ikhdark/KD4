//! Git roots and ordinary display metadata.
//! Historical evidence types remain deserializable, but no filesystem
//! generations, tree hashes, source watches, or freshness attestations are captured.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::future::Future;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use codex_config::ProjectDiscoveryContext;
use codex_file_system::ExecutorFileSystem;
use codex_git_utils::{get_git_remote_urls_result, get_git_repo_root_with_fs, get_has_changes, get_head_commit_hash_result};
#[cfg(test)]
use codex_git_utils::get_git_repo_root;
use codex_otel::MetricsClient;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;
use crate::environment_selection::TurnEnvironmentSnapshot;
pub(crate) use codex_command_analysis::git_dirs::resolve_git_dirs;
const RETAINED_REPOSITORY_CAPACITY: usize = 64;
const PROJECT_DISCOVERY_REUSE_METRIC: &str = "codex.project_discovery_reuse";
const ROOT_DISCOVERY_CONCURRENCY: usize = 4;

#[cfg(test)]
#[path = "git_workspace_tests.rs"]
mod tests;

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
    primary_environment_id: Option<String>,
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
        self.primary_entry().map(|entry| entry.repo_root.is_some())
    }

    pub(crate) fn primary_local_metadata_source(&self) -> Option<GitWorkspaceMetadataSource> {
        let entry = self.primary_entry()?;
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

    fn primary_entry(&self) -> Option<&GitWorkspaceEntry> {
        let id = self.primary_environment_id.as_ref()?;
        self.entries
            .iter()
            .find(|entry| &entry.environment_id == id)
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkspaceEvidenceIdentity {
    // These fingerprints combine independent observations. Equality is useful
    // for invalidation, but never proves current bytes or one coherent snapshot
    // against arbitrary external writers. Consumers must retain that scope.
    /// Repository discovery or capture failed. Unlike a proven non-Git `None`, this
    /// value never authorizes reuse, even when two failed captures are equal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) unavailable: bool,
    #[serde(default)]
    pub(crate) repository_root: Option<String>,
    pub(crate) head_identity: Option<String>,
    pub(crate) index_identity: Option<String>,
    pub(crate) worktree_identity: Option<String>,
    /// Capture-local detail, never serialized into model history or cache keys.
    #[serde(skip)]
    pub(crate) path_fingerprints: Option<BTreeMap<String, String>>,
}

impl PartialEq for WorkspaceEvidenceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.unavailable == other.unavailable
            && self.repository_root == other.repository_root
            && self.head_identity == other.head_identity
            && self.index_identity == other.index_identity
            && self.worktree_identity == other.worktree_identity
    }
}

impl Eq for WorkspaceEvidenceIdentity {}

impl WorkspaceEvidenceIdentity {
    fn unavailable(repository_root: Option<&Path>) -> Self {
        Self {
            unavailable: true,
            repository_root: repository_root.map(|root| root.to_string_lossy().into_owned()),
            head_identity: None,
            index_identity: None,
            worktree_identity: None,
            path_fingerprints: None,
        }
    }

    pub(crate) fn changed_paths_since(&self, before: &Self) -> Option<BTreeSet<PathBuf>> {
        if self.unavailable
            || before.unavailable
            || self.repository_root != before.repository_root
            || self.head_identity != before.head_identity
        {
            return None;
        }
        let root = Path::new(self.repository_root.as_deref()?);
        let before = before.path_fingerprints.as_ref()?;
        let after = self.path_fingerprints.as_ref()?;
        Some(
            before
                .keys()
                .chain(after.keys())
                .filter(|path| before.get(*path) != after.get(*path))
                .map(|path| root.join(path))
                .collect(),
        )
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

type WorkspaceRootCacheEntry = (Option<PathBuf>, Vec<DependencyFingerprint>);

#[derive(Debug, Default)]
pub(crate) struct WorkspaceRootCache {
    entries: StdMutex<HashMap<PathBuf, WorkspaceRootCacheEntry>>,
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
                dependency_fingerprint(path, true)
                    .filter(|dependency| !matches!(dependency.state, DependencyState::Missing))
                    .ok_or_else(|| std::io::Error::other("cannot inspect repository marker"))?
            } else {
                DependencyFingerprint {
                    path,
                    state: DependencyState::Missing,
                }
            };
            dependencies.push(dependency);
            if exists {
                break;
            }
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((root, previous)) = entries.get(base)
            && *previous == dependencies
        {
            return Ok(root.clone());
        }
        let root = dependencies.iter().find_map(|dependency| {
            (!matches!(dependency.state, DependencyState::Missing))
                .then(|| dependency.path.parent().map(Path::to_path_buf))
                .flatten()
        });
        if entries.len() >= RETAINED_REPOSITORY_CAPACITY && !entries.contains_key(base) {
            entries.clear();
        }
        entries.insert(base.to_path_buf(), (root.clone(), dependencies));
        Ok(root)
    }
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SourcePathChangeObservation {
    watcher_epoch: u64,
    // Numeric epochs can repeat after PID reuse. Legacy observations remain
    // recoverable, but cannot prove continuity with a newly created watcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watcher_incarnation: Option<uuid::Uuid>,
    watcher_generation: u64,
    #[serde(default)]
    registration_generation: u64,
    repo_root: PathBuf,
    path: PathBuf,
    #[serde(default)]
    recursive: bool,
    // The pathname used by the caller is a dependency too: replacing a link
    // changes the read without touching its previously resolved target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<Box<SourcePathChangeObservation>>,
}

impl SourcePathChangeObservation {
    pub(crate) fn source_dependency(&self) -> crate::tool_history::SourceDependencyV1 {
        let path = self.alias.as_ref().map_or(&self.path, |alias| &alias.path);
        crate::tool_history::SourceDependencyV1::new(path, self.recursive)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceFreshness {
    /// The live journal has no delivered relevant change. Events may be pending.
    NoObservedChange,
    Changed,
    Unknown,
}

#[derive(Default)]
pub(crate) struct GitWorkspaceCache {
    pub(crate) roots: Arc<WorkspaceRootCache>,
    state: Mutex<RootState>,
    #[cfg(test)]
    root_resolution_count: AtomicU64,
}
#[derive(Default)]
struct RootState { root: Option<RootCacheEntry> }
struct RootCacheEntry {
    key: RootCacheKey,
    dependencies: Vec<DependencyFingerprint>,
    entries: Vec<GitWorkspaceEntry>,
}

impl GitWorkspaceCache {
    pub(crate) fn new() -> Arc<Self> { Arc::new(Self::default()) }

    #[cfg(test)]
    pub(crate) fn with_noop_watcher_for_tests() -> Arc<Self> { Self::new() }

    pub(crate) async fn resolve_workspace_root(&self, cwd: &Path) -> std::io::Result<Option<PathBuf>> {
        let roots = Arc::clone(&self.roots);
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || roots.resolve(&cwd))
            .await.map_err(std::io::Error::other)?
    }

    pub(crate) async fn snapshot(self: &Arc<Self>, environments: &TurnEnvironmentSnapshot) -> GitWorkspaceSnapshot {
        self.snapshot_with_project_discovery(environments, None).await
    }

    pub(crate) async fn snapshot_with_project_discovery(
        self: &Arc<Self>, environments: &TurnEnvironmentSnapshot,
        discovery: Option<&ProjectDiscoveryContext>,
    ) -> GitWorkspaceSnapshot {
        self.snapshot_with_project_discovery_and_metrics(environments, discovery, codex_otel::global().as_ref()).await
    }

    async fn snapshot_with_project_discovery_and_metrics(
        self: &Arc<Self>,
        environments: &TurnEnvironmentSnapshot,
        project_discovery: Option<&ProjectDiscoveryContext>,
        metrics: Option<&MetricsClient>,
    ) -> GitWorkspaceSnapshot {
        // Read the role from this same selection snapshot, never from the
        // ready/path-convertible subset below (including on a root-cache hit).
        let primary_environment_id = environments
            .primary()
            .map(|environment| environment.environment_id.clone());
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
                .any(|environment| environment.remote);
        let dependencies = cacheable
            .then(|| root_dependencies(&key.environments))
            .flatten();

        if let Some(dependencies) = dependencies.as_ref() {
            let state = self.state.lock().await;
            if let Some(entry) = state.root.as_ref()
                && entry.key == key
                && entry.dependencies == *dependencies
            {
                return GitWorkspaceSnapshot {
                    environment_generation: key.environment_generation,
                    primary_environment_id,
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
            {
                self.state.lock().await.root = Some(RootCacheEntry {
                    key: key.clone(),
                    dependencies: before_dependencies,
                    entries: entries.clone(),
                });
            }
        }

        GitWorkspaceSnapshot {
            environment_generation: key.environment_generation,
            primary_environment_id,
            entries,
            cache: Arc::clone(self),
        }
    }

    async fn stable_metadata(&self, source: &GitWorkspaceMetadataSource) -> StableGitMetadata {
        let (head, remotes) = tokio::join!(
            get_head_commit_hash_result(source.cwd.as_path()),
            get_git_remote_urls_result(source.cwd.as_path()),
        );
        StableGitMetadata {
            associated_remote_urls: remotes.ok().flatten(),
            latest_git_commit_hash: head.ok().flatten().map(|sha| sha.0),
        }
    }

    // Compatibility boundaries for historical consumers during reconstruction.
    // These never certify source freshness or perform workspace I/O.
    pub(crate) async fn workspace_evidence_for_environment(
        &self, _environments: &TurnEnvironmentSnapshot, _host_cwd: &Path, _cwd: &Path,
    ) -> WorkspaceEvidenceCapture { WorkspaceEvidenceCapture::default() }
    pub(crate) async fn workspace_evidence_for_turn(
        &self, _turn: &crate::session::turn_context::TurnContext, _cwd: &Path,
    ) -> WorkspaceEvidenceCapture { WorkspaceEvidenceCapture::default() }
    pub(crate) async fn workspace_evidence_for_uri(
        &self, _turn: &crate::session::turn_context::TurnContext, _cwd: &PathUri, _environment_id: &str,
    ) -> Option<WorkspaceEvidenceIdentity> { None }
    pub(crate) async fn workspace_evidence_identity(&self, _cwd: &Path) -> Option<WorkspaceEvidenceIdentity> { None }
    pub(crate) async fn workspace_root_identity(&self, _cwd: &Path) -> Option<WorkspaceEvidenceIdentity> { None }
    pub(crate) async fn workspace_evidence_identity_with_attribution(&self, _cwd: &Path) -> WorkspaceEvidenceCapture {
        WorkspaceEvidenceCapture::default()
    }
    pub(crate) async fn latest_workspace_evidence_identity(&self, _cwd: &Path) -> Option<WorkspaceEvidenceIdentity> { None }
    pub(crate) async fn begin_source_path_change_observations(
        &self, _root: &Path, _paths: &[(PathBuf, bool)],
    ) -> Option<Vec<SourcePathChangeObservation>> { None }
    pub(crate) fn source_path_change_observation_is_current(&self, _observation: &SourcePathChangeObservation) -> bool { false }
    pub(crate) fn source_path_freshness(&self, _observation: &SourcePathChangeObservation) -> SourceFreshness { SourceFreshness::Unknown }
    pub(crate) fn begin_host_workspace_mutation(self: &Arc<Self>) -> PendingSourceMutation { PendingSourceMutation }
    pub(crate) fn note_host_workspace_mutation(&self) {}
    pub(crate) async fn note_host_workspace_mutation_paths(self: &Arc<Self>, _root: &Path, _paths: &[String]) {}
    #[cfg(test)]
    pub(crate) fn root_resolution_count(&self) -> u64 { self.root_resolution_count.load(Ordering::Acquire) }
    #[cfg(test)]
    pub(crate) fn workspace_evidence_capture_count(&self) -> u64 { 0 }
    #[cfg(test)]
    pub(crate) fn workspace_evidence_capture_sequence(&self) -> u64 { 0 }
    #[cfg(test)]
    pub(crate) fn take_source_change_freshness_lookup_count_for_test(&self) -> usize { 0 }
    #[cfg(test)]
    pub(crate) async fn begin_source_path_change_observation(&self, _root: &Path, _path: &Path, _recursive: bool) -> Option<SourcePathChangeObservation> { None }
}
pub(crate) struct PendingSourceMutation;
impl PendingSourceMutation { pub(crate) fn settle(self) {} }
pub(crate) async fn capture_workspace_evidence_identity(_cwd: &Path) -> Option<WorkspaceEvidenceIdentity> { None }
pub(crate) async fn git_ignored_paths(_root: &Path, _paths: &[PathBuf]) -> BTreeSet<PathBuf> { BTreeSet::new() }
pub(crate) async fn git_ignores_all_changed_paths(_root: &Path, _paths: &[PathBuf]) -> bool { false }
async fn collect_workspace_work_in_order<T>(
    work: impl IntoIterator<Item = impl Future<Output = T> + Send>,
    concurrency: usize,
) -> Vec<T> {
    // Delivery order is not an execution dependency. An ordered buffer keeps
    // completed siblings in its admission window until its oldest item ends.
    // No tasks are spawned here: cancellation still drops owned futures, and
    // callers retain their existing blocking-worker cancellation controls.
    let mut results = futures::stream::iter(
        work.into_iter()
            .enumerate()
            .map(|(index, future)| async move { (index, future.await) }),
    )
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;
    results.sort_unstable_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, value)| value).collect()
}

async fn resolve_roots_in_order<T>(
    resolutions: impl IntoIterator<Item = impl Future<Output = T> + Send>,
) -> Vec<T> {
    collect_workspace_work_in_order(resolutions, ROOT_DISCOVERY_CONCURRENCY).await
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
    if !environment
        .cwd
        .as_path()
        .starts_with(checkout_root.as_path())
    {
        return None;
    }
    for ancestor in environment.cwd.as_path().ancestors() {
        if ancestor == checkout_root.as_path() {
            break;
        }
        let marker = AbsolutePathBuf::from_absolute_path(ancestor.join(".git")).ok()?;
        match fs
            .get_metadata(&PathUri::from_abs_path(&marker), None)
            .await
        {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            _ => {
                record_project_discovery_reuse(
                    metrics,
                    "git",
                    "miss",
                    "nearer_marker_or_unavailable",
                );
                return None;
            }
        }
    }
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

fn root_dependencies(
    environments: &[EnvironmentWorkspaceKey],
) -> Option<Vec<DependencyFingerprint>> {
    let mut dependencies = Vec::new();
    let mut observed = HashMap::new();
    for environment in environments {
        for ancestor in environment.cwd.ancestors() {
            let path = ancestor.join(".git").into_path_buf();
            let exists = if let Some(exists) = observed.get(&path) {
                *exists
            } else {
                let dependency = dependency_fingerprint(path.clone(), true)?;
                let exists = !matches!(dependency.state, DependencyState::Missing);
                observed.insert(path, exists);
                dependencies.push(dependency);
                exists
            };
            if exists {
                break;
            }
        }
    }
    Some(dependencies)
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
