use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;

use codex_config::ConfigLayerStack;
use codex_exec_server::ExecutorFileSystem;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::Product;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginSkillRoot;
use tracing::info;
use tracing::instrument;
use tracing::warn;
use tokio::sync::OnceCell;

use crate::HostSkillsSnapshot;
use crate::PluginSkillSnapshots;
use crate::SkillLoadOutcome;
use crate::build_implicit_skill_path_indexes;
use crate::config_rules::SkillConfigRules;
use crate::config_rules::resolve_disabled_skill_paths;
use crate::config_rules::skill_config_rules_from_stack;
use crate::loader::SkillRoot;
use crate::loader::SkillRootLayout;
use crate::loader::load_skills_from_roots;
use crate::loader::resolve_skill_root_layout;
use crate::loader::skill_root_layout;
use crate::loader::skill_roots;
use crate::system::install_system_skills;
use crate::system::uninstall_system_skills;
use codex_config::SkillsConfig;

const MAX_SKILLS_CACHE_ENTRIES: usize = 64;

#[derive(Debug, Clone)]
pub struct SkillsLoadInput {
    pub cwd: AbsolutePathBuf,
    pub effective_skill_roots: Vec<PluginSkillRoot>,
    pub config_layer_stack: ConfigLayerStack,
    pub bundled_skills_enabled: bool,
    environment_generation: u64,
    watch_local_filesystem: bool,
    plugin_skill_snapshots: Option<PluginSkillSnapshots>,
}

impl SkillsLoadInput {
    pub fn new(
        cwd: AbsolutePathBuf,
        effective_skill_roots: Vec<PluginSkillRoot>,
        config_layer_stack: ConfigLayerStack,
        bundled_skills_enabled: bool,
    ) -> Self {
        Self {
            cwd,
            effective_skill_roots,
            config_layer_stack,
            bundled_skills_enabled,
            environment_generation: 0,
            watch_local_filesystem: false,
            plugin_skill_snapshots: None,
        }
    }

    /// Attaches plugin skill snapshots parsed during plugin loading, when available.
    pub fn with_plugin_skill_snapshots(
        mut self,
        plugin_skill_snapshots: Option<PluginSkillSnapshots>,
    ) -> Self {
        self.plugin_skill_snapshots = plugin_skill_snapshots;
        self
    }

    /// Associates this load with the captured turn-environment generation.
    pub fn with_environment_generation(mut self, environment_generation: u64) -> Self {
        self.environment_generation = environment_generation;
        self
    }

    /// Enables event-driven invalidation for host-local skill and project-discovery paths.
    pub fn with_local_file_watching(mut self, enabled: bool) -> Self {
        self.watch_local_filesystem = enabled;
        self
    }
}

/// Owns host skill discovery, immutable snapshots, cache invalidation, and extra roots.
///
/// Source-specific model exposure remains the responsibility of the skills extension.
pub struct SkillsService {
    codex_home: AbsolutePathBuf,
    restriction_product: Option<Product>,
    extra_roots: RwLock<Vec<AbsolutePathBuf>>,
    cache: Arc<RwLock<SkillsCache>>,
    local_watcher: Option<FileWatcherSubscriber>,
}

#[derive(Default)]
struct SkillsCache {
    filesystem_generation: u64,
    project_layouts: VecDeque<ProjectLayoutCacheEntry>,
    entries: VecDeque<SkillsCacheEntry>,
    flights: Vec<SkillsCacheFlight>,
}

struct ProjectLayoutCacheEntry {
    key: ProjectLayoutCacheKey,
    roots: Vec<SkillRoot>,
    _watch_registration: WatchRegistration,
}

#[derive(Clone)]
struct SkillsCacheEntry {
    key: ConfigSkillsCacheKey,
    snapshot: HostSkillsSnapshot,
}

struct SkillsCacheFlight {
    filesystem_generation: u64,
    key: ConfigSkillsCacheKey,
    snapshot: Weak<OnceCell<HostSkillsSnapshot>>,
}

impl SkillsService {
    pub fn new(codex_home: AbsolutePathBuf, bundled_skills_enabled: bool) -> Self {
        Self::new_with_restriction_product(codex_home, bundled_skills_enabled, Some(Product::Codex))
    }

    pub fn new_with_restriction_product(
        codex_home: AbsolutePathBuf,
        bundled_skills_enabled: bool,
        restriction_product: Option<Product>,
    ) -> Self {
        let cache = Arc::new(RwLock::new(SkillsCache::default()));
        let service = Self {
            codex_home,
            restriction_product,
            extra_roots: RwLock::new(Vec::new()),
            local_watcher: Self::start_local_watcher(Arc::clone(&cache)),
            cache,
        };
        if !bundled_skills_enabled {
            // The loader caches bundled skills under `skills/.system`. Clearing that directory is
            // best-effort cleanup; root selection still enforces the config even if removal fails.
            uninstall_system_skills(&service.codex_home);
        } else if let Err(err) = install_system_skills(&service.codex_home) {
            tracing::error!("failed to install system skills: {err}");
        }
        service
    }

    fn start_local_watcher(
        cache: Arc<RwLock<SkillsCache>>,
    ) -> Option<FileWatcherSubscriber> {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skills file watcher disabled because no Tokio runtime is available");
            return None;
        };
        let watcher = match FileWatcher::new() {
            Ok(watcher) => Arc::new(watcher),
            Err(err) => {
                warn!("skills file watcher disabled because file watching is unavailable: {err}");
                return None;
            }
        };
        let (subscriber, mut receiver) = watcher.add_subscriber();
        handle.spawn(async move {
            while let Some(event) = receiver.recv().await {
                invalidate_cache_paths(cache.as_ref(), &event.paths);
            }
        });
        Some(subscriber)
    }

    pub fn set_extra_roots(&self, extra_roots: Vec<AbsolutePathBuf>) -> bool {
        let changed = {
            let mut roots = self
                .extra_roots
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *roots == extra_roots {
                false
            } else {
                *roots = extra_roots;
                true
            }
        };
        if changed {
            self.clear_cache();
        }
        changed
    }

    /// Load skills for an already-constructed [`Config`], avoiding any additional config-layer
    /// loading.
    ///
    /// This path uses a cache keyed by the effective skill-relevant config state rather than just
    /// cwd so role-local and session-local skill overrides cannot bleed across sessions that happen
    /// to share a directory.
    #[instrument(
        name = "skills_for_config",
        level = "info",
        skip_all,
        fields(otel.name = "skills_for_config")
    )]
    pub async fn snapshot_for_config(
        &self,
        input: &SkillsLoadInput,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
    ) -> HostSkillsSnapshot {
        let skill_config_rules = skill_config_rules_from_stack(&input.config_layer_stack);
        loop {
            let (layout_generation, roots) = self
                .skill_roots_for_config_inner(input, fs.clone(), /*force_reload*/ false)
                .await;
            if let Some(snapshot) = self
                .snapshot_for_roots(
                    input,
                    roots,
                    &skill_config_rules,
                    /*use_cache*/ true,
                    /*force_reload*/ false,
                    Some(layout_generation),
                )
                .await
            {
                return snapshot;
            }
        }
    }

    pub async fn skill_roots_for_config(
        &self,
        input: &SkillsLoadInput,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
    ) -> Vec<SkillRoot> {
        self.skill_roots_for_config_inner(input, fs, /*force_reload*/ false)
            .await
            .1
    }

    async fn skill_roots_for_config_inner(
        &self,
        input: &SkillsLoadInput,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
        force_reload: bool,
    ) -> (u64, Vec<SkillRoot>) {
        let layout = skill_root_layout(
            fs,
            &input.config_layer_stack,
            &input.cwd,
            input.effective_skill_roots.clone(),
            self.extra_roots(),
        );
        let cache_key = project_layout_cache_key(input, &layout);
        loop {
            let generation = self.filesystem_generation();
            if !force_reload
                && let Some(snapshot) = self.cached_project_layout(&cache_key)
            {
                return snapshot;
            }
            let watch_registration = self.register_local_watch_paths(input, &layout);
            if self.filesystem_generation() != generation {
                continue;
            }
            let mut roots = resolve_skill_root_layout(layout.clone()).await;
            if !input.bundled_skills_enabled {
                roots.retain(|root| root.scope != SkillScope::System);
            }
            if self.publish_project_layout(
                generation,
                &cache_key,
                roots.clone(),
                watch_registration,
            ) {
                return (generation, roots);
            }
        }
    }

    fn register_local_watch_paths(
        &self,
        input: &SkillsLoadInput,
        layout: &SkillRootLayout,
    ) -> WatchRegistration {
        if !input.watch_local_filesystem {
            return WatchRegistration::default();
        }
        let Some(subscriber) = self.local_watcher.as_ref() else {
            return WatchRegistration::default();
        };

        let mut paths = layout
            .roots
            .iter()
            // Plugin roots are refreshed by the owning plugin lifecycle.
            .filter(|root| root.plugin_id.is_none())
            .map(|root| WatchPath {
                path: root.path.to_path_buf(),
                recursive: true,
            })
            .collect::<Vec<_>>();
        for ancestor in layout.cwd.ancestors() {
            // Missing paths are intentional: FileWatcher temporarily watches an existing ancestor
            // and migrates the registration when `.agents/skills` or a marker is created.
            paths.push(WatchPath {
                path: ancestor.join(".agents").join("skills").to_path_buf(),
                recursive: true,
            });
            paths.extend(layout.project_root_markers.iter().map(|marker| WatchPath {
                path: ancestor.join(marker).to_path_buf(),
                recursive: false,
            }));
        }
        paths.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.recursive.cmp(&right.recursive))
        });
        paths.dedup();
        subscriber.register_paths(paths)
    }

    pub async fn snapshot_for_cwd(
        &self,
        input: &SkillsLoadInput,
        force_reload: bool,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
    ) -> HostSkillsSnapshot {
        let use_cwd_cache = fs.is_some();
        let skill_config_rules = skill_config_rules_from_stack(&input.config_layer_stack);
        loop {
            let (layout_generation, mut roots) = if use_cwd_cache {
                self.skill_roots_for_config_inner(input, fs.clone(), force_reload)
                    .await
            } else {
                (
                    self.filesystem_generation(),
                    skill_roots(
                        fs.clone(),
                        &input.config_layer_stack,
                        &input.cwd,
                        input.effective_skill_roots.clone(),
                        self.extra_roots(),
                    )
                    .await,
                )
            };
            if !bundled_skills_enabled_from_stack(&input.config_layer_stack) {
                roots.retain(|root| root.scope != SkillScope::System);
            }
            if let Some(snapshot) = self
                .snapshot_for_roots(
                    input,
                    roots,
                    &skill_config_rules,
                    use_cwd_cache,
                    force_reload,
                    use_cwd_cache.then_some(layout_generation),
                )
                .await
            {
                return snapshot;
            }
        }
    }

    async fn snapshot_for_roots(
        &self,
        input: &SkillsLoadInput,
        roots: Vec<SkillRoot>,
        skill_config_rules: &SkillConfigRules,
        use_cache: bool,
        force_reload: bool,
        expected_generation: Option<u64>,
    ) -> Option<HostSkillsSnapshot> {
        if !use_cache {
            return Some(HostSkillsSnapshot::new(Arc::new(
                self.build_skill_outcome(input, roots, skill_config_rules)
                    .await,
            )));
        }

        let cache_key =
            config_skills_cache_key(input.environment_generation, &roots, skill_config_rules);
        if !force_reload && let Some(snapshot) = self.cached_snapshot(&cache_key) {
            return Some(snapshot);
        }
        if expected_generation.is_some_and(|expected| self.filesystem_generation() != expected) {
            return None;
        }
        let (generation, load) = self.skills_cache_flight(&cache_key);
        if expected_generation.is_some_and(|expected| generation != expected) {
            return None;
        }
        let snapshot = load
            .get_or_init(|| async {
                HostSkillsSnapshot::new(Arc::new(
                    self.build_skill_outcome(input, roots, skill_config_rules)
                        .await,
                ))
            })
            .await
            .clone();
        self.publish_skills_cache_flight(generation, &cache_key, &load, snapshot.clone())
            .then_some(snapshot)
    }

    #[instrument(level = "trace", skip_all)]
    async fn build_skill_outcome(
        &self,
        input: &SkillsLoadInput,
        roots: Vec<SkillRoot>,
        skill_config_rules: &SkillConfigRules,
    ) -> SkillLoadOutcome {
        let outcome = load_skills_from_roots(roots, input.plugin_skill_snapshots.as_ref()).await;
        let outcome =
            crate::filter_skill_load_outcome_for_product(outcome, self.restriction_product);
        let disabled_paths = resolve_disabled_skill_paths(&outcome.skills, skill_config_rules);
        finalize_skill_outcome(outcome, disabled_paths)
    }

    pub fn clear_cache(&self) {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cleared = cache.entries.len();
        cache.filesystem_generation = cache.filesystem_generation.wrapping_add(1);
        cache.project_layouts.clear();
        cache.entries.clear();
        cache.flights.clear();
        info!("skills cache cleared ({cleared} entries)");
    }

    pub fn invalidate_paths(&self, paths: &[PathBuf]) {
        invalidate_cache_paths(self.cache.as_ref(), paths);
    }

    fn filesystem_generation(&self) -> u64 {
        self.cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .filesystem_generation
    }

    fn cached_project_layout(
        &self,
        cache_key: &ProjectLayoutCacheKey,
    ) -> Option<(u64, Vec<SkillRoot>)> {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = cache
            .project_layouts
            .iter()
            .position(|entry| entry.key == *cache_key)?;
        let entry = cache.project_layouts.remove(position)?;
        let roots = entry.roots.clone();
        cache.project_layouts.push_back(entry);
        Some((cache.filesystem_generation, roots))
    }

    fn publish_project_layout(
        &self,
        generation: u64,
        cache_key: &ProjectLayoutCacheKey,
        roots: Vec<SkillRoot>,
        watch_registration: WatchRegistration,
    ) -> bool {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.filesystem_generation != generation {
            return false;
        }
        if let Some(position) = cache
            .project_layouts
            .iter()
            .position(|entry| entry.key == *cache_key)
        {
            cache.project_layouts.remove(position);
        }
        if cache.project_layouts.len() >= MAX_SKILLS_CACHE_ENTRIES {
            cache.project_layouts.pop_front();
        }
        cache.project_layouts.push_back(ProjectLayoutCacheEntry {
            key: cache_key.clone(),
            roots,
            _watch_registration: watch_registration,
        });
        true
    }

    fn cached_snapshot(&self, cache_key: &ConfigSkillsCacheKey) -> Option<HostSkillsSnapshot> {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = cache
            .entries
            .iter()
            .position(|entry| entry.key == *cache_key)?;
        let entry = cache.entries.remove(position)?;
        let snapshot = entry.snapshot.clone();
        cache.entries.push_back(entry);
        Some(snapshot)
    }

    fn skills_cache_flight(
        &self,
        cache_key: &ConfigSkillsCacheKey,
    ) -> (u64, Arc<OnceCell<HostSkillsSnapshot>>) {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = cache.filesystem_generation;
        cache
            .flights
            .retain(|flight| flight.snapshot.strong_count() > 0);
        if let Some(snapshot) = cache
            .flights
            .iter()
            .find(|flight| {
                flight.filesystem_generation == generation && flight.key == *cache_key
            })
            .and_then(|flight| flight.snapshot.upgrade())
        {
            return (generation, snapshot);
        }
        let snapshot = Arc::new(OnceCell::new());
        cache.flights.push(SkillsCacheFlight {
            filesystem_generation: generation,
            key: cache_key.clone(),
            snapshot: Arc::downgrade(&snapshot),
        });
        (generation, snapshot)
    }

    fn publish_skills_cache_flight(
        &self,
        generation: u64,
        cache_key: &ConfigSkillsCacheKey,
        load: &Arc<OnceCell<HostSkillsSnapshot>>,
        snapshot: HostSkillsSnapshot,
    ) -> bool {
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.flights.retain(|flight| {
            flight.snapshot.upgrade().is_some_and(|snapshot| {
                !(flight.filesystem_generation == generation
                    && flight.key == *cache_key
                    && Arc::ptr_eq(&snapshot, load))
            })
        });
        if cache.filesystem_generation != generation {
            return false;
        }
        if let Some(position) = cache
            .entries
            .iter()
            .position(|entry| entry.key == *cache_key)
        {
            cache.entries.remove(position);
        }
        if cache.entries.len() >= MAX_SKILLS_CACHE_ENTRIES {
            cache.entries.pop_front();
        }
        cache.entries.push_back(SkillsCacheEntry {
            key: cache_key.clone(),
            snapshot,
        });
        true
    }

    fn extra_roots(&self) -> Vec<AbsolutePathBuf> {
        match self.extra_roots.read() {
            Ok(roots) => roots.clone(),
            Err(err) => err.into_inner().clone(),
        }
    }
}

fn invalidate_cache_paths(cache: &RwLock<SkillsCache>, paths: &[PathBuf]) {
    if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
        let mut cache = cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cleared = cache.entries.len();
        cache.filesystem_generation = cache.filesystem_generation.wrapping_add(1);
        cache.project_layouts.clear();
        cache.entries.clear();
        cache.flights.clear();
        info!("skills cache cleared ({cleared} entries)");
        return;
    }

    let mut cache = cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let before_entries = cache.entries.len();
    let before_flights = cache.flights.len();
    // Root discovery depends on missing marker and `.agents/skills` paths that cannot be
    // represented by the previously discovered roots, so any filesystem notification makes
    // project-layout snapshots stale.
    cache.project_layouts.clear();
    cache
        .entries
        .retain(|entry| !entry.key.matches_any_path(paths));
    cache
        .flights
        .retain(|flight| !flight.key.matches_any_path(paths));
    let mut cleared = before_entries - cache.entries.len();
    let invalidated_flight = before_flights != cache.flights.len();
    if cleared == 0 && !invalidated_flight {
        cleared = cache.entries.len();
        cache.entries.clear();
        cache.flights.clear();
    }
    cache.filesystem_generation = cache.filesystem_generation.wrapping_add(1);
    info!("skills cache invalidated for changed paths ({cleared} entries)");
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConfigSkillsCacheKey {
    environment_generation: u64,
    roots: Vec<SkillRootCacheKey>,
    skill_config_rules: SkillConfigRules,
}

impl ConfigSkillsCacheKey {
    fn matches_any_path(&self, paths: &[PathBuf]) -> bool {
        self.roots.iter().any(|root| {
            paths
                .iter()
                .any(|path| paths_overlap(root.path.as_path(), path.as_path()))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectLayoutCacheKey {
    environment_generation: u64,
    cwd: AbsolutePathBuf,
    repo_filesystem_identity: Option<usize>,
    project_root_markers: Vec<String>,
    roots: Vec<SkillRootCacheKey>,
    bundled_skills_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SkillRootCacheKey {
    path: AbsolutePathBuf,
    scope_rank: u8,
    filesystem_identity: usize,
    plugin_id: Option<String>,
    plugin_namespace: Option<String>,
    plugin_root: Option<AbsolutePathBuf>,
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub fn bundled_skills_enabled_from_stack(
    config_layer_stack: &codex_config::ConfigLayerStack,
) -> bool {
    let effective_config = config_layer_stack.effective_config();
    let Some(skills_value) = effective_config
        .as_table()
        .and_then(|table| table.get("skills"))
    else {
        return true;
    };

    let skills: SkillsConfig = match skills_value.clone().try_into() {
        Ok(skills) => skills,
        Err(err) => {
            warn!("invalid skills config: {err}");
            return true;
        }
    };

    skills.bundled.unwrap_or_default().enabled
}

fn config_skills_cache_key(
    environment_generation: u64,
    roots: &[SkillRoot],
    skill_config_rules: &SkillConfigRules,
) -> ConfigSkillsCacheKey {
    ConfigSkillsCacheKey {
        environment_generation,
        roots: roots.iter().map(skill_root_cache_key).collect(),
        skill_config_rules: skill_config_rules.clone(),
    }
}

fn project_layout_cache_key(
    input: &SkillsLoadInput,
    layout: &SkillRootLayout,
) -> ProjectLayoutCacheKey {
    ProjectLayoutCacheKey {
        environment_generation: input.environment_generation,
        cwd: layout.cwd.clone(),
        repo_filesystem_identity: layout.repo_fs.as_ref().map(file_system_identity),
        project_root_markers: layout.project_root_markers.clone(),
        roots: layout.roots.iter().map(skill_root_cache_key).collect(),
        bundled_skills_enabled: input.bundled_skills_enabled,
    }
}

fn skill_root_cache_key(root: &SkillRoot) -> SkillRootCacheKey {
    let scope_rank = match root.scope {
        SkillScope::Repo => 0,
        SkillScope::User => 1,
        SkillScope::System => 2,
        SkillScope::Admin => 3,
    };
    SkillRootCacheKey {
        path: root.path.clone(),
        scope_rank,
        filesystem_identity: file_system_identity(&root.file_system),
        plugin_id: root.plugin_id.clone(),
        plugin_namespace: root.plugin_namespace.clone(),
        plugin_root: root.plugin_root.clone(),
    }
}

fn file_system_identity(file_system: &Arc<dyn ExecutorFileSystem>) -> usize {
    Arc::as_ptr(file_system) as *const () as usize
}

fn finalize_skill_outcome(
    mut outcome: SkillLoadOutcome,
    disabled_paths: HashSet<AbsolutePathBuf>,
) -> SkillLoadOutcome {
    outcome.disabled_paths = disabled_paths;
    // Usage-event detection should see any enabled skill file/script read, even when the
    // skill is not model-routable through implicit invocation.
    let (by_scripts_dir, by_doc_path) = build_implicit_skill_path_indexes(
        outcome
            .skills
            .iter()
            .filter(|skill| outcome.is_skill_enabled(skill))
            .cloned()
            .collect(),
    );
    outcome.implicit_skills_by_scripts_dir = Arc::new(by_scripts_dir);
    outcome.implicit_skills_by_doc_path = Arc::new(by_doc_path);
    outcome
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
