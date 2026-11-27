use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_plugin::PluginSkillRoot;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::SkillLoadOutcome;
use crate::loader::SkillRoot;
use crate::loader::SkillRootSnapshot;
use crate::loader::load_skill_root;
use crate::model::SkillFileSystemsByPath;
use crate::service::FileSystemIdentity;

static NEXT_PLUGIN_SKILL_SNAPSHOT_IDENTITY: AtomicU64 = AtomicU64::new(1);

pub(crate) type InstallationReadLease = Box<dyn Send + Sync>;
type AcquireInstallationRead = Arc<dyn Fn() -> Option<InstallationReadLease> + Send + Sync>;
type InstallationBindings = HashMap<AbsolutePathBuf, (String, AcquireInstallationRead)>;
type RootSnapshots = HashMap<(PluginSkillRoot, FileSystemIdentity), Arc<SkillRootSnapshot>>;

/// Parsed plugin skill-root snapshots produced by one plugin load.
///
/// Clones share the same snapshots. The plugins manager stores them with the corresponding loaded
/// plugins and passes a clone to skill loading as an optional preload.
#[derive(Clone)]
pub struct PluginSkillSnapshots {
    cache_identity: u64,
    installations: Arc<Mutex<InstallationBindings>>,
    snapshots_by_root: Arc<Mutex<RootSnapshots>>,
    preload_had_retryable_read_failures: Arc<AtomicBool>,
}

impl PluginSkillSnapshots {
    /// Creates an empty snapshot collection for a plugin load to populate.
    pub fn for_plugin_load() -> Self {
        Self {
            cache_identity: NEXT_PLUGIN_SKILL_SNAPSHOT_IDENTITY.fetch_add(1, Ordering::Relaxed),
            installations: Arc::default(),
            snapshots_by_root: Arc::new(Mutex::new(HashMap::new())),
            preload_had_retryable_read_failures: Arc::default(),
        }
    }

    /// The publication owner supplies checked read leases without coupling skills to its store.
    pub fn bind_installation(
        &self,
        root: AbsolutePathBuf,
        plugin_id: String,
        acquire_read: Arc<dyn Fn() -> Option<Box<dyn Send + Sync>> + Send + Sync>,
    ) {
        self.installations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root, (plugin_id, acquire_read));
    }

    /// Synchronous publication validation for callers already on a blocking thread.
    pub fn is_current(&self) -> bool {
        self.installation_validators()
            .into_iter()
            .all(|acquire_read| acquire_read().is_some())
    }

    /// Check cache eligibility without running a store validator on an async worker.
    pub(crate) async fn is_current_async(&self) -> bool {
        let validators = self.installation_validators();
        if validators.is_empty() {
            return true;
        }
        tokio::task::spawn_blocking(move || {
            validators
                .into_iter()
                .all(|acquire_read| acquire_read().is_some())
        })
        .await
        .unwrap_or(false)
    }

    fn installation_validators(&self) -> Vec<AcquireInstallationRead> {
        self.installations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|(_, acquire_read)| Arc::clone(acquire_read))
            .collect()
    }

    async fn read_guard_for_root(
        &self,
        root: &AbsolutePathBuf,
    ) -> io::Result<Option<InstallationReadLease>> {
        let acquire_read = self
            .installations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .map(|(_, acquire_read)| Arc::clone(acquire_read));
        acquire_installation_read(acquire_read).await
    }

    pub(crate) async fn read_guard_for_plugin(
        &self,
        plugin_id: &str,
    ) -> io::Result<Option<InstallationReadLease>> {
        let acquire_read = self
            .installations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|(id, _)| id == plugin_id)
            .map(|(_, acquire_read)| Arc::clone(acquire_read));
        acquire_installation_read(acquire_read).await
    }

    /// Whether a load using this preload encountered an incomplete root.
    pub fn had_retryable_read_failures(&self) -> bool {
        self.preload_had_retryable_read_failures.load(Ordering::Relaxed)
    }

    pub(crate) fn cache_identity(&self) -> u64 {
        self.cache_identity
    }
}

async fn acquire_installation_read(
    acquire_read: Option<AcquireInstallationRead>,
) -> io::Result<Option<InstallationReadLease>> {
    let Some(acquire_read) = acquire_read else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || acquire_read())
        .await
        .map_err(io::Error::other)?
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "plugin skill installation changed or is busy; reload the skills catalog",
            )
        })
}

impl fmt::Debug for PluginSkillSnapshots {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginSkillSnapshots")
            .finish_non_exhaustive()
    }
}

pub(crate) async fn load_and_merge_skill_roots<I>(
    roots: I,
    plugin_skill_snapshots: Option<&PluginSkillSnapshots>,
    repository_sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> SkillLoadOutcome
where
    I: IntoIterator<Item = SkillRoot>,
{
    let mut root_snapshots = Vec::new();
    let mut has_retryable_read_failures = false;
    for root in roots {
        // Keep the exact installation leased through the cache lookup or filesystem scan.
        // Never reinterpret a replacement using the old plugin namespace/policy.
        let _installation_read = match (plugin_skill_snapshots, root.plugin_root.as_ref()) {
            (Some(snapshots), Some(root)) => match snapshots.read_guard_for_root(root).await {
                Ok(lease) => lease,
                Err(_) => {
                    has_retryable_read_failures = true;
                    continue;
                }
            },
            _ => None,
        };
        let cache_key = match (
            root.plugin_id.clone(),
            root.plugin_namespace.clone(),
            root.plugin_root.clone(),
        ) {
            (Some(plugin_id), Some(plugin_namespace), Some(plugin_root)) => Some((
                PluginSkillRoot {
                    path: root.path.clone(),
                    plugin_id,
                    plugin_namespace,
                    plugin_root,
                },
                FileSystemIdentity(Arc::clone(&root.file_system)),
            )),
            _ => None,
        };
        let cached_snapshot = cache_key.as_ref().and_then(|cache_key| {
            let plugin_skill_snapshots = plugin_skill_snapshots?;
            plugin_skill_snapshots
                .snapshots_by_root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(cache_key)
                .cloned()
        });
        let snapshot = match cached_snapshot {
            Some(snapshot) => snapshot,
            None => {
                // Plugin-cached roots are never repository-scoped, so the cache stays valid.
                let snapshot = Arc::new(load_skill_root(root, repository_sandbox).await);
                if !snapshot.has_retryable_read_failures
                    && let Some(plugin_skill_snapshots) = plugin_skill_snapshots
                    && let Some(cache_key) = cache_key
                {
                    plugin_skill_snapshots
                        .snapshots_by_root
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(cache_key, Arc::clone(&snapshot));
                }
                snapshot
            }
        };
        has_retryable_read_failures |= snapshot.has_retryable_read_failures;
        root_snapshots.push(snapshot);
    }

    let mut outcome = merge_skill_root_snapshots(root_snapshots);
    outcome.has_retryable_read_failures = has_retryable_read_failures;
    if has_retryable_read_failures && let Some(snapshots) = plugin_skill_snapshots {
        snapshots
            .preload_had_retryable_read_failures
            .store(true, Ordering::Relaxed);
    }
    outcome.plugin_installations = plugin_skill_snapshots.cloned();
    outcome.repository_sandbox = repository_sandbox.cloned();
    outcome
}

fn merge_skill_root_snapshots(snapshots: Vec<Arc<SkillRootSnapshot>>) -> SkillLoadOutcome {
    fn scope_rank(scope: codex_protocol::protocol::SkillScope) -> u8 {
        use codex_protocol::protocol::SkillScope;

        // Higher-priority scopes first (matches root scan order for dedupe).
        match scope {
            SkillScope::Repo => 0,
            SkillScope::User => 1,
            SkillScope::System => 2,
            SkillScope::Admin => 3,
        }
    }

    let mut outcome = SkillLoadOutcome::default();
    let mut skill_roots = Vec::new();
    let mut skill_root_by_path = HashMap::new();
    let mut file_systems_by_skill_path = HashMap::new();

    for snapshot in snapshots {
        let SkillRootSnapshot {
            root,
            skills,
            errors,
            file_system,
            ..
        } = Arc::unwrap_or_clone(snapshot);
        if !skills.is_empty() && !skill_roots.contains(&root) {
            skill_roots.push(root.clone());
        }
        for skill in skills.iter().filter(|skill| skill.source.is_none()) {
            skill_root_by_path
                .entry(skill.path_to_skills_md.clone())
                .or_insert_with(|| root.clone());
            file_systems_by_skill_path
                .entry(skill.path_to_skills_md.clone())
                .or_insert_with(|| Arc::clone(&file_system));
        }
        outcome.skills.extend(skills);
        outcome.errors.extend(errors);
    }

    let mut seen = HashMap::new();
    let mut skills = Vec::<crate::SkillMetadata>::new();
    for skill in outcome.skills.drain(..) {
        let key = (skill.path_to_skills_md.clone(), skill.source.clone());
        if let Some(&index) = seen.get(&key) {
            let existing: &mut crate::SkillMetadata = &mut skills[index];
            for path in skill.discovery_paths {
                if !existing.discovery_paths.contains(&path) {
                    existing.discovery_paths.push(path);
                }
            }
        } else {
            seen.insert(key, skills.len());
            skills.push(skill);
        }
    }
    outcome.skills = skills;
    let retained_skill_paths = outcome
        .skills
        .iter()
        .map(|skill| skill.path_to_skills_md.clone())
        .collect::<HashSet<_>>();
    skill_root_by_path.retain(|path, _| retained_skill_paths.contains(path));
    let used_roots = skill_root_by_path.values().cloned().collect::<HashSet<_>>();
    skill_roots.retain(|root| used_roots.contains(root));
    file_systems_by_skill_path.retain(|path, _| retained_skill_paths.contains(path));
    outcome.skill_roots = skill_roots;
    outcome.skill_root_by_path = Arc::new(skill_root_by_path);
    outcome.file_systems_by_skill_path = SkillFileSystemsByPath::new(file_systems_by_skill_path);

    outcome.skills.sort_by(|a, b| {
        scope_rank(a.scope)
            .cmp(&scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path_to_skills_md.cmp(&b.path_to_skills_md))
    });

    outcome
}
