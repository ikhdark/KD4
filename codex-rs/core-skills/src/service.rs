use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::RwLock;

use codex_config::ConfigLayerStack;
use codex_exec_server::ExecutorFileSystem;
use codex_plugin::PluginSkillRoot;
use codex_protocol::protocol::Product;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use tracing::info;
use tracing::instrument;
use tracing::warn;

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

const MAX_CACHED_SKILL_SNAPSHOTS: usize = 64;

struct SnapshotCacheOptions<'a> {
    force_reload: bool,
    cache_result: bool,
    isolate_file_system: bool,
    cwd_cache_key: Option<&'a AbsolutePathBuf>,
}

#[derive(Debug, Clone)]
pub struct SkillsLoadInput {
    pub cwd: AbsolutePathBuf,
    pub effective_skill_roots: Vec<PluginSkillRoot>,
    pub config_layer_stack: Arc<ConfigLayerStack>,
    pub bundled_skills_enabled: bool,
    plugin_skill_snapshots: Option<PluginSkillSnapshots>,
}

impl SkillsLoadInput {
    pub fn new(
        cwd: AbsolutePathBuf,
        effective_skill_roots: Vec<PluginSkillRoot>,
        config_layer_stack: impl Into<Arc<ConfigLayerStack>>,
        bundled_skills_enabled: bool,
    ) -> Self {
        Self {
            cwd,
            effective_skill_roots,
            config_layer_stack: config_layer_stack.into(),
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
    input_snapshot_cache: RwLock<HashMap<SkillsInputCacheKey, HostSkillsSnapshot>>,
    snapshot_cache: RwLock<HashMap<SkillsCacheKey, HostSkillsSnapshot>>,
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
            input_snapshot_cache: RwLock::new(HashMap::new()),
            snapshot_cache: RwLock::new(HashMap::new()),
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
        let input_cache_key = SkillsInputCacheKey::new(input, fs.as_ref());
        if let Some(snapshot) = self.cached_input_snapshot(&input_cache_key) {
            return snapshot;
        }
        let roots = self.skill_roots_for_config(input, fs).await;
        let skill_config_rules = skill_config_rules_from_stack(&input.config_layer_stack);
        let snapshot = self
            .snapshot_for_roots(
                input,
                roots,
                skill_config_rules,
                SnapshotCacheOptions {
                    force_reload: false,
                    cache_result: true,
                    isolate_file_system: true,
                    cwd_cache_key: None,
                },
            )
            .await;
        self.cache_input_snapshot(input_cache_key, snapshot.clone());
        snapshot
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
        let cache_result = true;
        let mut roots = skill_roots(
            fs,
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
            skill_config_rules,
            SnapshotCacheOptions {
                force_reload,
                cache_result,
                isolate_file_system: false,
                cwd_cache_key: Some(&input.cwd),
            },
        )
        .await
    }

    async fn snapshot_for_roots(
        &self,
        input: &SkillsLoadInput,
        roots: Vec<SkillRoot>,
        skill_config_rules: SkillConfigRules,
        cache_options: SnapshotCacheOptions<'_>,
    ) -> HostSkillsSnapshot {
        let SnapshotCacheOptions {
            force_reload,
            cache_result,
            isolate_file_system,
            cwd_cache_key,
        } = cache_options;
        let cache_key = skills_cache_key(
            &roots,
            &skill_config_rules,
            input.plugin_skill_snapshots.as_ref(),
            isolate_file_system,
            cwd_cache_key,
        );
        if cache_result
            && !force_reload
            && let Some(snapshot) = self.cached_snapshot(&cache_key)
        {
            return snapshot;
        }

        let snapshot = HostSkillsSnapshot::new(Arc::new(
            self.build_skill_outcome(input, roots, &skill_config_rules)
                .await,
        ));
        if cache_result {
            let mut cache = self
                .snapshot_cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !cache.contains_key(&cache_key)
                && cache.len() >= MAX_CACHED_SKILL_SNAPSHOTS
                && let Some(evicted_key) = cache.keys().next().cloned()
            {
                cache.remove(&evicted_key);
            }
            cache.insert(cache_key, snapshot.clone());
        }
        snapshot
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
        let mut input_cache = self
            .input_snapshot_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let input_entries = input_cache.len();
        input_cache.clear();
        let mut cache = self
            .snapshot_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cleared = cache.len() + input_entries;
        cache.clear();
        info!("skills cache cleared ({cleared} entries)");
    }

    fn cached_snapshot(&self, cache_key: &SkillsCacheKey) -> Option<HostSkillsSnapshot> {
        match self.snapshot_cache.read() {
            Ok(cache) => cache.get(cache_key).cloned(),
            Err(err) => err.into_inner().get(cache_key).cloned(),
        }
    }

    fn cached_input_snapshot(&self, cache_key: &SkillsInputCacheKey) -> Option<HostSkillsSnapshot> {
        match self.input_snapshot_cache.read() {
            Ok(cache) => cache.get(cache_key).cloned(),
            Err(err) => err.into_inner().get(cache_key).cloned(),
        }
    }

    fn cache_input_snapshot(&self, cache_key: SkillsInputCacheKey, snapshot: HostSkillsSnapshot) {
        let mut cache = self
            .input_snapshot_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cache.contains_key(&cache_key)
            && cache.len() >= MAX_CACHED_SKILL_SNAPSHOTS
            && let Some(evicted_key) = cache.keys().next().cloned()
        {
            cache.remove(&evicted_key);
        }
        cache.insert(cache_key, snapshot);
    }

    fn extra_roots(&self) -> Vec<AbsolutePathBuf> {
        match self.extra_roots.read() {
            Ok(roots) => roots.clone(),
            Err(err) => err.into_inner().clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct ConfigLayerStackIdentity(Arc<ConfigLayerStack>);

impl PartialEq for ConfigLayerStackIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ConfigLayerStackIdentity {}

impl Hash for ConfigLayerStackIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SkillsInputCacheKey {
    config_layer_stack: ConfigLayerStackIdentity,
    cwd: AbsolutePathBuf,
    effective_skill_roots: Vec<PluginSkillRoot>,
    bundled_skills_enabled: bool,
    plugin_skill_snapshots_identity: Option<u64>,
    file_system_identity: usize,
}

impl SkillsInputCacheKey {
    fn new(input: &SkillsLoadInput, fs: Option<&Arc<dyn ExecutorFileSystem>>) -> Self {
        Self {
            config_layer_stack: ConfigLayerStackIdentity(Arc::clone(&input.config_layer_stack)),
            cwd: input.cwd.clone(),
            effective_skill_roots: input.effective_skill_roots.clone(),
            bundled_skills_enabled: input.bundled_skills_enabled,
            plugin_skill_snapshots_identity: input
                .plugin_skill_snapshots
                .as_ref()
                .map(PluginSkillSnapshots::cache_identity),
            file_system_identity: fs
                .map(|fs| Arc::as_ptr(fs).cast::<()>() as usize)
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SkillsCacheKey {
    cwd: Option<AbsolutePathBuf>,
    roots: Vec<SkillRootCacheKey>,
    skill_config_rules: SkillConfigRules,
    plugin_skill_snapshots_identity: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SkillRootCacheKey {
    path: AbsolutePathBuf,
    scope_rank: u8,
    file_system_identity: usize,
    plugin_id: Option<String>,
    plugin_namespace: Option<String>,
    plugin_root: Option<AbsolutePathBuf>,
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

fn skills_cache_key(
    roots: &[SkillRoot],
    skill_config_rules: &SkillConfigRules,
    plugin_skill_snapshots: Option<&PluginSkillSnapshots>,
    isolate_file_system: bool,
    cwd_cache_key: Option<&AbsolutePathBuf>,
) -> SkillsCacheKey {
    SkillsCacheKey {
        cwd: cwd_cache_key.cloned(),
        roots: roots
            .iter()
            .filter(|_| cwd_cache_key.is_none())
            .map(|root| {
                let scope_rank = match root.scope {
                    SkillScope::Repo => 0,
                    SkillScope::User => 1,
                    SkillScope::System => 2,
                    SkillScope::Admin => 3,
                };
                SkillRootCacheKey {
                    path: root.path.clone(),
                    scope_rank,
                    file_system_identity: if isolate_file_system {
                        Arc::as_ptr(&root.file_system).cast::<()>() as usize
                    } else {
                        0
                    },
                    plugin_id: root.plugin_id.clone(),
                    plugin_namespace: root.plugin_namespace.clone(),
                    plugin_root: root.plugin_root.clone(),
                }
            })
            .collect(),
        skill_config_rules: skill_config_rules.clone(),
        plugin_skill_snapshots_identity: plugin_skill_snapshots
            .map(PluginSkillSnapshots::cache_identity),
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
