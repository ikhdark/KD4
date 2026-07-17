use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;

use codex_config::ConfigLayerStack;
use codex_exec_server::ExecutorFileSystem;
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
use crate::loader::load_skills_from_roots;
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
}

/// Owns host skill discovery, immutable snapshots, cache invalidation, and extra roots.
///
/// Source-specific model exposure remains the responsibility of the skills extension.
pub struct SkillsService {
    codex_home: AbsolutePathBuf,
    restriction_product: Option<Product>,
    extra_roots: RwLock<Vec<AbsolutePathBuf>>,
    cache: RwLock<SkillsCache>,
}

#[derive(Default)]
struct SkillsCache {
    generation: u64,
    entries: VecDeque<SkillsCacheEntry>,
    flights: Vec<SkillsCacheFlight>,
}

#[derive(Clone)]
struct SkillsCacheEntry {
    key: ConfigSkillsCacheKey,
    snapshot: HostSkillsSnapshot,
}

struct SkillsCacheFlight {
    generation: u64,
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
        let service = Self {
            codex_home,
            restriction_product,
            extra_roots: RwLock::new(Vec::new()),
            cache: RwLock::new(SkillsCache::default()),
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

    pub fn set_extra_roots(&self, extra_roots: Vec<AbsolutePathBuf>) {
        {
            let mut roots = self
                .extra_roots
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *roots = extra_roots;
        }
        self.clear_cache();
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
        let roots = self.skill_roots_for_config(input, fs).await;
        let skill_config_rules = skill_config_rules_from_stack(&input.config_layer_stack);
        self.snapshot_for_roots(
            input,
            roots,
            &skill_config_rules,
            /*use_cache*/ true,
            /*force_reload*/ false,
        )
        .await
    }

    pub async fn skill_roots_for_config(
        &self,
        input: &SkillsLoadInput,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
    ) -> Vec<SkillRoot> {
        let mut roots = skill_roots(
            fs,
            &input.config_layer_stack,
            &input.cwd,
            input.effective_skill_roots.clone(),
            self.extra_roots(),
        )
        .await;
        if !input.bundled_skills_enabled {
            roots.retain(|root| root.scope != SkillScope::System);
        }
        roots
    }

    pub async fn snapshot_for_cwd(
        &self,
        input: &SkillsLoadInput,
        force_reload: bool,
        fs: Option<Arc<dyn ExecutorFileSystem>>,
    ) -> HostSkillsSnapshot {
        let use_cwd_cache = fs.is_some();
        let mut roots = skill_roots(
            fs.clone(),
            &input.config_layer_stack,
            &input.cwd,
            input.effective_skill_roots.clone(),
            self.extra_roots(),
        )
        .await;
        if !bundled_skills_enabled_from_stack(&input.config_layer_stack) {
            roots.retain(|root| root.scope != SkillScope::System);
        }
        let skill_config_rules = skill_config_rules_from_stack(&input.config_layer_stack);
        self.snapshot_for_roots(
            input,
            roots,
            &skill_config_rules,
            use_cwd_cache,
            force_reload,
        )
        .await
    }

    async fn snapshot_for_roots(
        &self,
        input: &SkillsLoadInput,
        roots: Vec<SkillRoot>,
        skill_config_rules: &SkillConfigRules,
        use_cache: bool,
        force_reload: bool,
    ) -> HostSkillsSnapshot {
        if !use_cache {
            return HostSkillsSnapshot::new(Arc::new(
                self.build_skill_outcome(input, roots, skill_config_rules)
                    .await,
            ));
        }

        let cache_key = config_skills_cache_key(&roots, skill_config_rules);
        loop {
            if !force_reload && let Some(snapshot) = self.cached_snapshot(&cache_key) {
                return snapshot;
            }
            let (generation, load) = self.skills_cache_flight(&cache_key);
            let snapshot = load
                .get_or_init(|| async {
                    HostSkillsSnapshot::new(Arc::new(
                        self.build_skill_outcome(input, roots.clone(), skill_config_rules)
                            .await,
                    ))
                })
                .await
                .clone();
            if self.publish_skills_cache_flight(
                generation,
                &cache_key,
                &load,
                snapshot.clone(),
            ) {
                return snapshot;
            }
        }
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
        cache.generation = cache.generation.wrapping_add(1);
        cache.entries.clear();
        cache.flights.clear();
        info!("skills cache cleared ({cleared} entries)");
    }

    pub fn invalidate_paths(&self, paths: &[PathBuf]) {
        if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
            self.clear_cache();
            return;
        }

        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before_entries = cache.entries.len();
        let before_flights = cache.flights.len();
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
        cache.generation = cache.generation.wrapping_add(1);
        info!("skills cache invalidated for changed paths ({cleared} entries)");
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
        let generation = cache.generation;
        cache
            .flights
            .retain(|flight| flight.snapshot.strong_count() > 0);
        if let Some(snapshot) = cache
            .flights
            .iter()
            .find(|flight| flight.generation == generation && flight.key == *cache_key)
            .and_then(|flight| flight.snapshot.upgrade())
        {
            return (generation, snapshot);
        }
        let snapshot = Arc::new(OnceCell::new());
        cache.flights.push(SkillsCacheFlight {
            generation,
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
                !(flight.generation == generation
                    && flight.key == *cache_key
                    && Arc::ptr_eq(&snapshot, load))
            })
        });
        if cache.generation != generation {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ConfigSkillsCacheKey {
    roots: Vec<(AbsolutePathBuf, u8, Option<String>, Option<String>)>,
    skill_config_rules: SkillConfigRules,
}

impl ConfigSkillsCacheKey {
    fn matches_any_path(&self, paths: &[PathBuf]) -> bool {
        self.roots.iter().any(|(root, _, _, _)| {
            paths
                .iter()
                .any(|path| paths_overlap(root.as_path(), path.as_path()))
        })
    }
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
    roots: &[SkillRoot],
    skill_config_rules: &SkillConfigRules,
) -> ConfigSkillsCacheKey {
    ConfigSkillsCacheKey {
        roots: roots
            .iter()
            .map(|root| {
                let scope_rank = match root.scope {
                    SkillScope::Repo => 0,
                    SkillScope::User => 1,
                    SkillScope::System => 2,
                    SkillScope::Admin => 3,
                };
                (
                    root.path.clone(),
                    scope_rank,
                    root.plugin_id.clone(),
                    root.plugin_namespace.clone(),
                )
            })
            .collect(),
        skill_config_rules: skill_config_rules.clone(),
    }
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
