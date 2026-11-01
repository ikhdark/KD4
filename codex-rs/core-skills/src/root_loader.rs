use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_plugin::PluginSkillRoot;

use crate::SkillLoadOutcome;
use crate::loader::SkillRoot;
use crate::loader::SkillRootSnapshot;
use crate::loader::load_skill_root;
use crate::model::SkillFileSystemsByPath;

static NEXT_PLUGIN_SKILL_SNAPSHOT_IDENTITY: AtomicU64 = AtomicU64::new(1);

/// Parsed plugin skill-root snapshots produced by one plugin load.
///
/// Clones share the same snapshots. The plugins manager stores them with the corresponding loaded
/// plugins and passes a clone to skill loading as an optional preload.
#[derive(Clone)]
pub struct PluginSkillSnapshots {
    cache_identity: u64,
    installations: Arc<Mutex<HashMap<codex_utils_absolute_path::AbsolutePathBuf, (String, Arc<dyn Fn() -> Option<Box<dyn Send + Sync>> + Send + Sync>)>>>,
    snapshots_by_root: Arc<
        Mutex<HashMap<(PluginSkillRoot, crate::service::FileSystemIdentity), SkillRootSnapshot>>,
    >,
}

impl PluginSkillSnapshots {
    /// Creates an empty snapshot collection for a plugin load to populate.
    pub fn for_plugin_load() -> Self {
        Self {
            cache_identity: NEXT_PLUGIN_SKILL_SNAPSHOT_IDENTITY.fetch_add(1, Ordering::Relaxed),
            installations: Arc::default(),
            snapshots_by_root: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The publication owner supplies checked read leases without coupling skills to its store.
    pub fn bind_installation(
        &self,
        root: codex_utils_absolute_path::AbsolutePathBuf,
        plugin_id: String,
        acquire_read: Arc<dyn Fn() -> Option<Box<dyn Send + Sync>> + Send + Sync>,
    ) {
        self.installations.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root, (plugin_id, acquire_read));
    }

    pub fn is_current(&self) -> bool {
        let validators = self.installations.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values().map(|(_, acquire_read)| Arc::clone(acquire_read)).collect::<Vec<_>>();
        validators.into_iter().all(|acquire_read| acquire_read().is_some())
    }

    fn root_is_current(&self, root: &codex_utils_absolute_path::AbsolutePathBuf) -> bool {
        let validate = self.installations.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).get(root)
            .map(|(_, acquire_read)| Arc::clone(acquire_read));
        validate.is_none_or(|acquire_read| acquire_read().is_some())
    }

    pub(crate) fn read_guard_for_plugin(
        &self,
        plugin_id: &str,
    ) -> std::io::Result<Option<Box<dyn Send + Sync>>> {
        let acquire_read = self.installations.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|(id, _)| id == plugin_id)
            .map(|(_, acquire_read)| Arc::clone(acquire_read));
        acquire_read.map(|acquire_read| acquire_read().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "plugin skill installation changed or is busy; reload the skills catalog",
            )
        })).transpose()
    }

    pub(crate) fn cache_identity(&self) -> u64 {
        self.cache_identity
    }
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
) -> SkillLoadOutcome
where
    I: IntoIterator<Item = SkillRoot>,
{
    let mut root_snapshots = Vec::new();
    for root in roots {
        if root.plugin_root.as_ref().is_some_and(|root|
            plugin_skill_snapshots.is_some_and(|snapshots| !snapshots.root_is_current(root)))
        {
            // Never reinterpret a replacement using the old plugin namespace/policy.
            continue;
        }
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
                crate::service::FileSystemIdentity(Arc::clone(&root.file_system)),
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
                let snapshot = load_skill_root(root).await;
                if let Some(plugin_skill_snapshots) = plugin_skill_snapshots
                    && let Some(cache_key) = cache_key
                {
                    plugin_skill_snapshots
                        .snapshots_by_root
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(cache_key, snapshot.clone());
                }
                snapshot
            }
        };
        root_snapshots.push(snapshot);
    }

    let mut outcome = merge_skill_root_snapshots(root_snapshots);
    outcome.plugin_installations = plugin_skill_snapshots.cloned();
    outcome
}

fn merge_skill_root_snapshots(snapshots: Vec<SkillRootSnapshot>) -> SkillLoadOutcome {
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
        } = snapshot;
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
