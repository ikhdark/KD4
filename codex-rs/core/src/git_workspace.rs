use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
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
use sha2::Digest;
use sha2::Sha256;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::warn;

use crate::environment_selection::TurnEnvironmentSnapshot;

const GIT_DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(5);
const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

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
                    entry
                        .repo_root
                        .as_ref()
                        .unwrap_or(&entry.cwd)
                        .to_path_buf(),
                )
            })
            .collect()
    }

    pub(crate) fn primary_is_git(&self) -> Option<bool> {
        self.entries
            .first()
            .map(|entry| entry.repo_root.is_some())
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

impl GitWorkspaceMetadataSource {
    pub(crate) fn discover_local(cwd: AbsolutePathBuf) -> Option<Self> {
        let repo_root = AbsolutePathBuf::from_absolute_path(get_git_repo_root(cwd.as_path())?)
            .ok()?;
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

#[derive(Default)]
struct GitWorkspaceCacheState {
    root: Option<RootCacheEntry>,
    metadata: HashMap<PathBuf, MetadataCacheEntry>,
}

pub(crate) struct GitWorkspaceCache {
    state: Mutex<GitWorkspaceCacheState>,
    watcher_generation: AtomicU64,
    watcher_reliable: AtomicBool,
    watcher_subscriber: Option<FileWatcherSubscriber>,
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

    fn with_watcher(watcher: Option<Arc<FileWatcher>>) -> Arc<Self> {
        let (watcher_subscriber, receiver) = match watcher {
            Some(watcher) => {
                let (subscriber, receiver) = watcher.add_subscriber();
                (Some(subscriber), Some(receiver))
            }
            None => (None, None),
        };
        let cache = Arc::new(Self {
            state: Mutex::new(GitWorkspaceCacheState::default()),
            watcher_generation: AtomicU64::new(0),
            watcher_reliable: AtomicBool::new(watcher_subscriber.is_some()),
            watcher_subscriber,
        });
        if let Some(mut receiver) = receiver {
            let weak_cache = Arc::downgrade(&cache);
            tokio::spawn(async move {
                while receiver.recv().await.is_some() {
                    let Some(cache) = weak_cache.upgrade() else {
                        return;
                    };
                    cache.watcher_generation.fetch_add(1, Ordering::AcqRel);
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
            && !key.environments.iter().any(|environment| environment.remote)
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
        .and_then(|head| head.trim().strip_prefix("ref:").map(str::trim).map(PathBuf::from));
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
    output.status.success().then(|| Sha256::digest(output.stdout).into())
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
        digest: Option<[u8; 32]>,
    },
}

fn dependency_fingerprint(
    path: PathBuf,
    hash_contents: bool,
) -> Option<DependencyFingerprint> {
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Some(DependencyFingerprint {
            path,
            state: DependencyState::Directory {
                modified: metadata.modified().ok(),
                created: metadata.created().ok(),
            },
        }),
        Ok(metadata) if metadata.is_file() => {
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

#[cfg(test)]
#[path = "git_workspace_tests.rs"]
mod tests;
