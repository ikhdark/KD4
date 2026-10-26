use codex_arg0::Arg0DispatchPaths;
use codex_cloud_config::cloud_config_bundle_loader;
use codex_config::CloudConfigBundleLoader;
use codex_config::ConfigLayerStack;
use codex_config::LoaderOverrides;
use codex_config::ThreadConfigLoader;
use codex_config::json_to_toml;
use codex_config::loader::load_config_layers_state;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_exec_server::LOCAL_FS;
use codex_features::Features;
use codex_features::user_settable_feature_for_key;
use codex_login::AuthManager;
use codex_login::default_client::set_default_client_residency_requirement;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use toml::Value as TomlValue;
use tracing::instrument;
use tracing::warn;

/// Coalesce the clusters of identical config reads made by one app-server RPC
/// without turning the manager into a long-lived authority over config files.
const CONFIG_LOAD_CACHE_TTL: Duration = Duration::from_millis(250);
const CONFIG_LOAD_CACHE_ENTRIES: usize = 8;
const CONFIG_LOAD_TRACKED_FLIGHTS: usize = 32;

#[derive(Debug, Clone, PartialEq)]
struct ConfigLoadCacheKey {
    cli_overrides: Vec<(String, TomlValue)>,
    typesafe_overrides: String,
    fallback_cwd: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct ConfigLoadCacheEntry {
    key: ConfigLoadCacheKey,
    generation: u64,
    loaded_at: Instant,
    config: Config,
}

#[derive(Default)]
struct ConfigLoadCache {
    ready: VecDeque<ConfigLoadCacheEntry>,
    // A canceled builder releases its gate. A waiting caller can then retry;
    // there is no detached producer or failed result retained indefinitely.
    flights: Vec<(ConfigLoadCacheKey, u64, Weak<tokio::sync::Mutex<()>>)>,
}

#[derive(Default)]
struct OverridePathNode {
    children: BTreeMap<String, OverridePathNode>,
    terminal: Option<usize>,
    first: Option<usize>,
}

impl OverridePathNode {
    fn insert(&mut self, path: &[String], index: usize) -> Option<usize> {
        if let Some(previous) = self.terminal {
            return Some(previous);
        }
        if path.is_empty() {
            if let Some(previous) = self.first {
                return Some(previous);
            }
            self.terminal = Some(index);
        } else if let Some(previous) = self
            .children
            .entry(path[0].clone())
            .or_default()
            .insert(&path[1..], index)
        {
            return Some(previous);
        }
        self.first.get_or_insert(index);
        None
    }
}

/// Shared app-server entry point for loading effective Codex configuration.
#[derive(Clone)]
pub(crate) struct ConfigManager {
    codex_home: PathBuf,
    // CLI overrides are immutable after construction.
    cli_overrides: Arc<Vec<(String, TomlValue)>>,
    // Recoverable locks contain only complete published values. Writers stage
    // updates first and move previous loader destructors outside the lock.
    runtime_feature_enablement: Arc<RwLock<BTreeMap<String, bool>>>,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    cloud_config_bundle: Arc<RwLock<CloudConfigBundleLoader>>,
    arg0_paths: Arg0DispatchPaths,
    thread_config_loader: Arc<RwLock<Arc<dyn ThreadConfigLoader>>>,
    load_cache: Arc<RwLock<ConfigLoadCache>>,
    load_generation: Arc<AtomicU64>,
    remote_control_handle: Arc<RwLock<Option<crate::transport::RemoteControlHandle>>>,
}

impl ConfigManager {
    pub(crate) fn new(
        codex_home: PathBuf,
        cli_overrides: Vec<(String, TomlValue)>,
        loader_overrides: LoaderOverrides,
        strict_config: bool,
        cloud_config_bundle: CloudConfigBundleLoader,
        arg0_paths: Arg0DispatchPaths,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) -> Self {
        Self {
            codex_home,
            cli_overrides: Arc::new(cli_overrides),
            runtime_feature_enablement: Arc::new(RwLock::new(BTreeMap::new())),
            loader_overrides,
            strict_config,
            cloud_config_bundle: Arc::new(RwLock::new(cloud_config_bundle)),
            arg0_paths,
            thread_config_loader: Arc::new(RwLock::new(thread_config_loader)),
            load_cache: Arc::new(RwLock::new(ConfigLoadCache::default())),
            load_generation: Arc::new(AtomicU64::new(0)),
            remote_control_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub(crate) fn codex_home(&self) -> &Path {
        self.codex_home.as_path()
    }

    pub(crate) fn user_config_path(&self) -> std::io::Result<AbsolutePathBuf> {
        self.loader_overrides.user_config_path(self.codex_home())
    }

    pub(crate) fn current_cli_overrides(&self) -> Vec<(String, TomlValue)> {
        self.cli_overrides.as_ref().clone()
    }

    pub(crate) fn current_cloud_config_bundle(&self) -> CloudConfigBundleLoader {
        self.cloud_config_bundle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn extend_runtime_feature_enablement<I>(&self, enablement: I) -> Result<(), ()>
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        // Evaluate caller-controlled iteration before taking the publication lock.
        // If it panics, neither the published map nor the cache generation changes.
        let enablement = enablement.into_iter().collect::<BTreeMap<_, _>>();
        let mut runtime_feature_enablement = self
            .runtime_feature_enablement
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep the published value complete even if staging allocation unwinds.
        let mut next = runtime_feature_enablement.clone();
        next.extend(enablement);
        if next == *runtime_feature_enablement {
            return Ok(());
        }
        *runtime_feature_enablement = next;
        drop(runtime_feature_enablement);
        self.invalidate_load_cache();
        Ok(())
    }

    pub(crate) fn replace_cloud_config_bundle_loader(
        &self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) {
        let loader = cloud_config_bundle_loader(
            auth_manager,
            chatgpt_base_url,
            self.codex_home.clone(),
            http_client_factory,
        );
        let mut guard = self
            .cloud_config_bundle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, loader);
        self.invalidate_load_cache();
        drop(guard);
        // Loader-owned values may have destructors; never run them while a
        // publication lock is held or before the new value invalidates the cache.
        drop(previous);
    }

    pub(crate) fn replace_thread_config_loader(
        &self,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) {
        let mut guard = self
            .thread_config_loader
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, thread_config_loader);
        drop(guard);
        self.invalidate_load_cache();
        drop(previous);
    }

    fn current_thread_config_loader(&self) -> Arc<dyn ThreadConfigLoader> {
        Arc::clone(
            &*self
                .thread_config_loader
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub(crate) async fn sync_default_client_residency_requirement(&self) {
        let generation = self.policy_generation();
        let policy_target = self
            .remote_control_handle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .and_then(|handle| handle.policy_auth_epoch().ok().map(|epoch| (handle, epoch)));
        match self.load_latest_config(/*fallback_cwd*/ None).await {
            Ok(config) => {
                let _ = self.with_current_policy(generation, || {
                    set_default_client_residency_requirement(config.enforce_residency.value());
                });
                if self.validate_policy_generation(generation).is_ok()
                    && let Some((handle, epoch)) = policy_target
                {
                    let denied = config
                        .config_layer_stack
                        .requirements()
                        .allow_remote_control
                        .as_ref()
                        .is_some_and(|requirement| !requirement.value);
                    handle.update_policy(
                        epoch,
                        if denied {
                            crate::transport::RemoteControlPolicy::DisabledByRequirements
                        } else {
                            crate::transport::RemoteControlPolicy::Allowed
                        },
                    );
                }
            }
            Err(err) => warn!(
                error = %err,
                "failed to sync default client residency requirement after auth refresh"
            ),
        }
    }

    pub(crate) fn bind_remote_control_handle(&self, handle: crate::transport::RemoteControlHandle) {
        *self
            .remote_control_handle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    pub(crate) fn policy_generation(&self) -> u64 {
        self.load_generation.load(Ordering::Acquire)
    }

    pub(crate) fn validate_policy_generation(&self, generation: u64) -> std::io::Result<()> {
        self.with_current_policy(generation, || ())
    }

    fn with_current_policy<T>(
        &self,
        generation: u64,
        publish: impl FnOnce() -> T,
    ) -> std::io::Result<T> {
        let loader = self
            .cloud_config_bundle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.policy_generation() != generation {
            return Err(std::io::Error::other(
                "configuration changed before managed policy admission; retry the request",
            ));
        }
        loader
            .publish_if_current(publish)
            .map_err(std::io::Error::other)
    }

    pub(crate) async fn complete_auth_transition(
        &self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) {
        self.replace_cloud_config_bundle_loader(
            auth_manager,
            chatgpt_base_url,
            http_client_factory,
        );
        self.sync_default_client_residency_requirement().await;
    }

    /// Re-checks managed requirements whenever the current login completes a missing account
    /// id. Cloud requirements are fetched per account, yet such a completion runs no explicit
    /// auth transition, so remote control holds backend contact for the completed account until
    /// this re-check records a decision for it, and a denial disables remote control.
    pub(crate) async fn recheck_requirements_after_identity_completion(
        self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        let mut auth_changes = auth_manager.auth_change_receiver();
        loop {
            if self.remote_control_policy_recheck_pending() {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = self.complete_auth_transition(
                        Arc::clone(&auth_manager),
                        chatgpt_base_url.clone(),
                        http_client_factory.clone(),
                    ) => {}
                }
            }
            // A failed re-check leaves remote control held; the next auth change retries it.
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = auth_changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn remote_control_policy_recheck_pending(&self) -> bool {
        self.remote_control_handle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(crate::transport::RemoteControlHandle::policy_recheck_pending)
    }

    pub(crate) async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> std::io::Result<Config> {
        self.load_with_cli_overrides(
            &self.current_cli_overrides(),
            /*request_overrides*/ None,
            ConfigOverrides::default(),
            fallback_cwd,
        )
        .await
    }

    /// Catalog-wide settings must not inherit the server process's repository.
    pub(crate) async fn load_non_project_config(&self) -> std::io::Result<Config> {
        self.load_uncached_config(None, true).await
    }

    async fn load_uncached_config(
        &self,
        cwd: Option<PathBuf>,
        ignore_project_config: bool,
    ) -> std::io::Result<Config> {
        let generation = self.policy_generation();
        self.validate_policy_generation(generation)?;
        let mut config = codex_core::config::ConfigBuilder::default()
            .codex_home(self.codex_home.clone())
            .cli_overrides(self.current_cli_overrides())
            .loader_overrides(self.loader_overrides.clone())
            .strict_config(self.strict_config)
            .cloud_config_bundle(self.current_cloud_config_bundle())
            .thread_config_loader(self.current_thread_config_loader())
            .fallback_cwd(cwd)
            .ignore_project_config(ignore_project_config)
            .build()
            .await?;
        self.apply_runtime_feature_enablement(&mut config);
        self.apply_arg0_paths(&mut config);
        self.with_current_policy(generation, || config)
    }

    pub(crate) async fn load_latest_config_for_thread(
        &self,
        thread_config: &Config,
    ) -> std::io::Result<Config> {
        let refreshed_config = self
            .load_latest_config(Some(thread_config.cwd.to_path_buf()))
            .await?;
        let mut config = thread_config
            .rebuild_preserving_session_layers(&refreshed_config)
            .await?;
        self.apply_runtime_feature_enablement(&mut config);
        self.apply_arg0_paths(&mut config);
        Ok(config)
    }

    pub(crate) async fn load_default_config(&self) -> std::io::Result<Config> {
        let mut config = Config::load_default_with_cli_overrides_for_codex_home(
            self.codex_home.clone(),
            self.current_cli_overrides(),
        )
        .await?;
        if self.loader_overrides.user_config_path.is_some()
            || self.loader_overrides.user_config_profile.is_some()
        {
            let user_config_path = self.loader_overrides.user_config_path(self.codex_home())?;
            config.config_layer_stack = config
                .config_layer_stack
                .with_user_config_profile(
                    &user_config_path,
                    self.loader_overrides.user_config_profile.as_ref(),
                    TomlValue::Table(toml::map::Map::new()),
                )
                .into();
        }
        self.apply_runtime_feature_enablement(&mut config);
        self.apply_arg0_paths(&mut config);
        Ok(config)
    }

    pub(crate) async fn load_with_overrides(
        &self,
        request_overrides: Option<HashMap<String, serde_json::Value>>,
        typesafe_overrides: ConfigOverrides,
    ) -> std::io::Result<Config> {
        self.load_with_cli_overrides(
            &self.current_cli_overrides(),
            request_overrides,
            typesafe_overrides,
            /*fallback_cwd*/ None,
        )
        .await
    }

    pub(crate) async fn load_for_cwd(
        &self,
        request_overrides: Option<HashMap<String, serde_json::Value>>,
        typesafe_overrides: ConfigOverrides,
        cwd: Option<PathBuf>,
    ) -> std::io::Result<Config> {
        self.load_with_cli_overrides(
            &self.current_cli_overrides(),
            request_overrides,
            typesafe_overrides,
            cwd,
        )
        .await
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn load_with_cli_overrides(
        &self,
        cli_overrides: &[(String, TomlValue)],
        request_overrides: Option<HashMap<String, serde_json::Value>>,
        mut typesafe_overrides: ConfigOverrides,
        fallback_cwd: Option<PathBuf>,
    ) -> std::io::Result<Config> {
        let mut request_overrides = request_overrides.unwrap_or_default();
        // RPC maps have no last-write order, unlike the ordered CLI flags below.
        // Compare parsed paths so quoted dots are literal, and equivalent
        // spellings cannot bypass the ambiguity check.
        let mut keys = request_overrides
            .keys()
            .map(|key| Ok((key, codex_config::parse_override_key(key)?)))
            .collect::<std::io::Result<Vec<_>>>()?;
        keys.sort_by_key(|(key, _)| *key);
        let mut paths = OverridePathNode::default();
        for (index, (key, path)) in keys.iter().enumerate() {
            if let Some(previous) = paths.insert(path, index) {
                let ancestor = keys[previous].0;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "ambiguous configuration overrides `{ancestor}` and `{key}`: use a single nested value or non-overlapping dotted paths"
                    ),
                ));
            }
        }
        if let Some(value) = request_overrides.remove("bypass_hook_trust") {
            typesafe_overrides.bypass_hook_trust = Some(value.as_bool().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "`bypass_hook_trust` override must be a boolean",
                )
            })?);
        }
        let merged_cli_overrides = cli_overrides
            .iter()
            .cloned()
            .chain(
                request_overrides
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(key, value)| (key, json_to_toml(value))),
            )
            .collect::<Vec<_>>();

        let cache_key = ConfigLoadCacheKey {
            cli_overrides: merged_cli_overrides.clone(),
            typesafe_overrides: format!("{typesafe_overrides:?}"),
            fallback_cwd: fallback_cwd.clone(),
        };
        let generation = self.load_generation.load(Ordering::Acquire);
        self.validate_policy_generation(generation)?;
        if let Some(config) = self.cached_config(&cache_key, generation) {
            return self.with_current_policy(generation, || config);
        }

        let flight = {
            let mut cache = self
                .load_cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.flights.retain(|(_, entry_generation, gate)| {
                *entry_generation == generation && gate.strong_count() > 0
            });
            if let Some(gate) = cache.flights.iter().find_map(|(key, entry_generation, gate)| {
                (key == &cache_key && *entry_generation == generation)
                    .then(|| gate.upgrade())
                    .flatten()
            }) {
                gate
            } else {
                let gate = Arc::new(tokio::sync::Mutex::new(()));
                if cache.flights.len() < CONFIG_LOAD_TRACKED_FLIGHTS {
                    cache.flights.push((cache_key.clone(), generation, Arc::downgrade(&gate)));
                }
                gate
            }
        };
        let _flight = flight.lock().await;
        self.validate_policy_generation(generation)?;
        if let Some(config) = self.cached_config(&cache_key, generation) {
            return self.with_current_policy(generation, || config);
        }

        let mut config = codex_core::config::ConfigBuilder::default()
            .codex_home(self.codex_home.clone())
            .cli_overrides(merged_cli_overrides)
            .loader_overrides(self.loader_overrides.clone())
            .strict_config(self.strict_config)
            .harness_overrides(typesafe_overrides)
            .fallback_cwd(fallback_cwd)
            .cloud_config_bundle(self.current_cloud_config_bundle())
            .thread_config_loader(self.current_thread_config_loader())
            .build()
            .await?;
        self.apply_runtime_feature_enablement(&mut config);
        self.apply_arg0_paths(&mut config);
        self.with_current_policy(generation, || {
            if let Ok(mut cache) = self.load_cache.write() {
                cache.ready.retain(|entry| {
                    entry.generation == generation
                        && entry.loaded_at.elapsed() <= CONFIG_LOAD_CACHE_TTL
                        && entry.key != cache_key
                });
                cache.ready.push_back(ConfigLoadCacheEntry {
                    key: cache_key,
                    generation,
                    loaded_at: Instant::now(),
                    config: config.clone(),
                });
                while cache.ready.len() > CONFIG_LOAD_CACHE_ENTRIES {
                    cache.ready.pop_front();
                }
            }
            config
        })
    }

    fn cached_config(&self, key: &ConfigLoadCacheKey, generation: u64) -> Option<Config> {
        self.load_cache.read().ok()?.ready.iter().find_map(|entry| {
            (entry.generation == generation
                && &entry.key == key
                && entry.loaded_at.elapsed() <= CONFIG_LOAD_CACHE_TTL)
                .then(|| entry.config.clone())
        })
    }

    pub(crate) async fn load_config_layers_for_cwd(
        &self,
        cwd: AbsolutePathBuf,
    ) -> std::io::Result<ConfigLayerStack> {
        self.load_config_layers(Some(cwd)).await
    }

    pub(crate) fn plugin_config_reload_for_cwd(
        &self,
        cwd: AbsolutePathBuf,
    ) -> codex_core_plugins::marketplace_upgrade::ConfigLayerReload {
        let manager = self.clone();
        let runtime = tokio::runtime::Handle::current();
        Arc::new(move || runtime
            .block_on(manager.load_uncached_config(Some(cwd.to_path_buf()), false))
            .map(|config| config.config_layer_stack.as_ref().clone()))
    }

    pub(crate) async fn load_config_layers(
        &self,
        cwd: Option<AbsolutePathBuf>,
    ) -> std::io::Result<ConfigLayerStack> {
        let thread_config_loader = self.current_thread_config_loader();
        let generation = self.policy_generation();
        self.validate_policy_generation(generation)?;
        let loader = self.current_cloud_config_bundle().for_load();
        let layers = load_config_layers_state(
            LOCAL_FS.as_ref(),
            &self.codex_home,
            cwd,
            &self.current_cli_overrides(),
            codex_config::ConfigLoadOptions {
                loader_overrides: self.loader_overrides.clone(),
                strict_config: self.strict_config,
                cloud_config_bundle: loader.clone(),
            },
            thread_config_loader.as_ref(),
        )
        .await;
        match layers {
            Ok(layers) => self.with_current_policy(generation, || layers),
            Err(err) => {
                loader.reject_loaded_candidate();
                Err(err)
            }
        }
    }

    fn apply_runtime_feature_enablement(&self, config: &mut Config) {
        apply_runtime_feature_enablement(config, &self.current_runtime_feature_enablement());
    }

    pub(crate) fn apply_runtime_feature_enablement_to_features(
        &self,
        features: &mut Features,
        config_layer_stack: &ConfigLayerStack,
    ) {
        apply_runtime_feature_enablement_to_features(
            features,
            config_layer_stack,
            &self.current_runtime_feature_enablement(),
        );
    }

    fn current_runtime_feature_enablement(&self) -> BTreeMap<String, bool> {
        self.runtime_feature_enablement
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn apply_arg0_paths(&self, config: &mut Config) {
        config.codex_self_exe = self.arg0_paths.codex_self_exe.clone();
    }

    pub(crate) fn invalidate_load_cache(&self) {
        self.load_generation.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut cache) = self.load_cache.write() {
            cache.ready.clear();
            cache.flights.clear();
        }
    }



}

pub(crate) fn protected_feature_keys(config_layer_stack: &ConfigLayerStack) -> BTreeSet<String> {
    let mut protected_features = config_layer_stack
        .effective_config()
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|features| features.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();

    if let Some(feature_requirements) = config_layer_stack
        .requirements_toml()
        .feature_requirements
        .as_ref()
    {
        protected_features.extend(feature_requirements.entries.keys().cloned());
    }

    protected_features
}

pub(crate) fn apply_runtime_feature_enablement(
    config: &mut Config,
    runtime_feature_enablement: &BTreeMap<String, bool>,
) {
    let protected_features = protected_feature_keys(&config.config_layer_stack);
    for (name, enabled) in runtime_feature_enablement {
        if protected_features.contains(name) {
            continue;
        }
        let Some(feature) = user_settable_feature_for_key(name) else {
            continue;
        };
        if let Err(err) = config.features.set_enabled(feature, *enabled) {
            warn!(
                feature = name,
                error = %err,
                "failed to apply runtime feature enablement"
            );
        }
    }
}

fn apply_runtime_feature_enablement_to_features(
    features: &mut Features,
    config_layer_stack: &ConfigLayerStack,
    runtime_feature_enablement: &BTreeMap<String, bool>,
) {
    let protected_features = protected_feature_keys(config_layer_stack);
    for (name, enabled) in runtime_feature_enablement {
        if protected_features.contains(name) {
            continue;
        }
        let Some(feature) = user_settable_feature_for_key(name) else {
            continue;
        };
        features.set_enabled(feature, *enabled);
    }
}

