use codex_app_server_transport::RemoteControlHandle;
use codex_app_server_transport::RemoteControlPolicy;
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
use tokio::sync::Notify;
use toml::Value as TomlValue;
use tracing::instrument;
use tracing::warn;

/// Coalesce the clusters of identical config reads made by one app-server RPC
/// without turning the manager into a long-lived authority over config files.
const CONFIG_LOAD_CACHE_TTL: Duration = Duration::from_millis(250);
const CONFIG_LOAD_CACHE_ENTRIES: usize = 8;
const CONFIG_LOAD_TRACKED_FLIGHTS: usize = 32;

#[derive(Debug, thiserror::Error)]
#[error("configuration changed before managed policy admission; retry the request")]
pub struct ConfigGenerationChanged;

#[derive(Debug, Clone, PartialEq)]
struct ConfigLoadCacheKey {
    cli_overrides: Vec<(String, TomlValue)>,
    typesafe_overrides: String,
    fallback_cwd: Option<PathBuf>,
    ignore_project_config: bool,
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

#[derive(Clone)]
struct CloudConfigLoader {
    bundle: CloudConfigBundleLoader,
    // An embedder can supply a bootstrap loader owned by another auth manager.
    // Keep it for startup, but do not use it for a later runtime identity.
    supplied_at_startup: bool,
}

/// Shared app-server entry point for loading effective Codex configuration.
#[derive(Clone)]
pub struct ConfigManager {
    codex_home: PathBuf,
    // CLI overrides are immutable after construction.
    cli_overrides: Arc<Vec<(String, TomlValue)>>,
    // Recoverable locks contain only complete published values. Writers stage
    // updates first and move previous loader destructors outside the lock.
    runtime_feature_enablement: Arc<RwLock<BTreeMap<String, bool>>>,
    loader_overrides: LoaderOverrides,
    strict_config: bool,
    cloud_config_bundle: Arc<RwLock<CloudConfigLoader>>,
    arg0_paths: Arg0DispatchPaths,
    thread_config_loader: Arc<RwLock<Arc<dyn ThreadConfigLoader>>>,
    load_cache: Arc<RwLock<ConfigLoadCache>>,
    load_generation: Arc<AtomicU64>,
    // One publication owner across plugin/account/MCP and explicit user-layer
    // refreshes. Acquire before capturing any per-thread configuration.
    runtime_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    remote_control_handle: Arc<RwLock<Option<RemoteControlHandle>>>,
    requirements_sync_retry: Arc<Notify>,
}

impl ConfigManager {
    pub fn new(
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
            cloud_config_bundle: Arc::new(RwLock::new(CloudConfigLoader {
                bundle: cloud_config_bundle,
                supplied_at_startup: true,
            })),
            arg0_paths,
            thread_config_loader: Arc::new(RwLock::new(thread_config_loader)),
            load_cache: Arc::new(RwLock::new(ConfigLoadCache::default())),
            load_generation: Arc::new(AtomicU64::new(0)),
            runtime_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            remote_control_handle: Arc::new(RwLock::new(None)),
            requirements_sync_retry: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn codex_home(&self) -> &Path {
        self.codex_home.as_path()
    }

    pub async fn lock_runtime_refresh(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.runtime_refresh_lock.lock().await
    }

    pub(crate) fn user_config_path(&self) -> std::io::Result<AbsolutePathBuf> {
        self.loader_overrides.user_config_path(self.codex_home())
    }

    pub fn current_cli_overrides(&self) -> Vec<(String, TomlValue)> {
        self.cli_overrides.as_ref().clone()
    }

    fn current_cloud_config_bundle(&self) -> CloudConfigBundleLoader {
        self.current_cloud_config_loader().bundle
    }

    fn current_cloud_config_loader(&self) -> CloudConfigLoader {
        self.cloud_config_bundle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn extend_runtime_feature_enablement<I>(&self, enablement: I) -> Result<(), ()>
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        self.extend_runtime_feature_enablement_inner(None, enablement)
            .map(|_| ())
            .map_err(|_| ())
    }

    pub fn extend_runtime_feature_enablement_for_generation<I>(
        &self,
        generation: u64,
        enablement: I,
    ) -> std::io::Result<bool>
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        self.extend_runtime_feature_enablement_inner(Some(generation), enablement)
    }

    fn extend_runtime_feature_enablement_inner<I>(
        &self,
        generation: Option<u64>,
        enablement: I,
    ) -> std::io::Result<bool>
    where
        I: IntoIterator<Item = (String, bool)>,
    {
        // Evaluate caller-controlled iteration before taking the publication lock.
        // If it panics, neither the published map nor the cache generation changes.
        let enablement = enablement.into_iter().collect::<BTreeMap<_, _>>();
        let _policy = self.cloud_config_bundle.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if generation.is_some_and(|generation| generation != self.policy_generation()) {
            return Err(std::io::Error::other(ConfigGenerationChanged));
        }
        let mut runtime_feature_enablement = self
            .runtime_feature_enablement
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep the published value complete even if staging allocation unwinds.
        let mut next = runtime_feature_enablement.clone();
        next.extend(enablement);
        if next == *runtime_feature_enablement {
            return Ok(false);
        }
        *runtime_feature_enablement = next;
        drop(runtime_feature_enablement);
        self.invalidate_load_cache_locked("runtime_feature_enablement");
        Ok(true)
    }

    pub fn replace_cloud_config_bundle_loader(
        &self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) {
        let mut guard = self
            .cloud_config_bundle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.install_cloud_config_bundle_loader(
            &mut guard,
            auth_manager,
            chatgpt_base_url,
            http_client_factory,
        );
        drop(guard);
        // Loader-owned values may have destructors; never run them while a
        // publication lock is held or before the new value invalidates the cache.
        drop(previous);
    }

    pub fn replace_thread_config_loader(
        &self,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) {
        let policy = self.cloud_config_bundle.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut guard = self
            .thread_config_loader
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::replace(&mut *guard, thread_config_loader);
        self.invalidate_load_cache_locked("thread_config_loader");
        drop(guard);
        drop(policy);
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

    async fn sync_default_client_residency_requirement(&self) -> bool {
        let generation = self.policy_generation();
        let policy_handle = self
            .remote_control_handle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let policy_target = match policy_handle {
            Some(handle) => match handle.policy_auth_epoch() {
                Ok(epoch) => Some((handle, epoch)),
                Err(_) => return false,
            },
            None => None,
        };
        match self.load_latest_config(/*fallback_cwd*/ None).await {
            Ok(config) => {
                if self
                    .with_current_policy(generation, || {
                        set_default_client_residency_requirement(config.enforce_residency.value());
                    })
                    .is_err()
                {
                    return false;
                }
                if let Some((handle, epoch)) = policy_target {
                    // update_policy owns its auth-epoch fence. Hold only the
                    // configuration publication fence here, avoiding recursive
                    // acquisition of the auth lock inside publish_if_current.
                    let _policy = self
                        .cloud_config_bundle
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if self.policy_generation() != generation {
                        return false;
                    }
                    let denied = config
                        .config_layer_stack
                        .requirements()
                        .allow_remote_control
                        .as_ref()
                        .is_some_and(|requirement| !requirement.value);
                    return handle.update_policy(
                        epoch,
                        if denied {
                            RemoteControlPolicy::DisabledByRequirements
                        } else {
                            RemoteControlPolicy::Allowed
                        },
                    );
                }
                true
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "failed to recheck managed app-server requirements"
                );
                false
            }
        }
    }

    pub fn bind_remote_control_handle(&self, handle: RemoteControlHandle) {
        *self
            .remote_control_handle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    pub fn recheck_remote_policy_after_config_mutation(&self) {
        self.requirements_sync_retry.notify_one();
    }

    pub fn policy_generation(&self) -> u64 {
        self.load_generation.load(Ordering::Acquire)
    }

    pub fn validate_policy_generation(&self, generation: u64) -> std::io::Result<()> {
        self.with_current_policy(generation, || ())
    }

    /// Replaces a cloud-config loader that an auth change has retired. A change
    /// published outside an explicit auth transition (a credential reload, a
    /// load retry, a policy or persistent-failure logout) leaves the loader
    /// bound to the previous policy epoch, and every load through it then fails
    /// with a relogin error for good. The check and the replacement share the
    /// loader lock, so a loader an explicit transition already installed is
    /// never overwritten. Returns whether the loader was replaced.
    fn rebind_retired_cloud_config_bundle_loader(
        &self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
        startup_identity_changed: bool,
    ) -> bool {
        let needs_rebind = |loader: &CloudConfigLoader| {
            if loader.supplied_at_startup {
                startup_identity_changed
            } else {
                loader.bundle.validate_current().is_err()
            }
        };
        if !needs_rebind(&self.current_cloud_config_loader()) {
            return false;
        }
        let mut guard = self
            .cloud_config_bundle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !needs_rebind(&guard) {
            return false;
        }
        // Built only once the replacement is certain: constructing a loader
        // spawns its first bundle fetch. Construction itself awaits nothing.
        let previous = self.install_cloud_config_bundle_loader(
            &mut guard,
            auth_manager,
            chatgpt_base_url,
            http_client_factory,
        );
        drop(guard);
        // As in `replace_cloud_config_bundle_loader`: run loader destructors
        // only after the publication lock is released.
        drop(previous);
        true
    }

    // Cloud-config publication acquires the loader lock before the auth lock.
    // Build and install the new loader in that same order, so an older loader
    // cannot overwrite a newer one merely because it was delayed before locking.
    fn install_cloud_config_bundle_loader(
        &self,
        slot: &mut CloudConfigLoader,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> CloudConfigLoader {
        let bundle = cloud_config_bundle_loader(
            auth_manager,
            chatgpt_base_url,
            self.codex_home.clone(),
            http_client_factory,
        );
        let previous = std::mem::replace(
            slot,
            CloudConfigLoader {
                bundle,
                supplied_at_startup: false,
            },
        );
        self.invalidate_load_cache_locked("cloud_config_loader");
        previous
    }

    pub(crate) fn with_current_policy<T>(
        &self,
        generation: u64,
        publish: impl FnOnce() -> T,
    ) -> std::io::Result<T> {
        let loader = self
            .cloud_config_bundle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.policy_generation() != generation {
            return Err(std::io::Error::other(ConfigGenerationChanged));
        }
        loader
            .bundle
            .publish_if_current(publish)
            .map_err(std::io::Error::other)
    }

    pub async fn complete_auth_transition(
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
        if !self.sync_default_client_residency_requirement().await {
            self.requirements_sync_retry.notify_one();
        }
    }

    /// Re-checks managed requirements whenever the current login completes a missing account
    /// id. Cloud requirements are fetched per account, yet such a completion runs no explicit
    /// auth transition, so remote control holds backend contact for the completed account until
    /// this re-check records a decision for it, and a denial disables remote control.
    ///
    /// The same holds for an identity change that a credential reload publishes on its own
    /// (another process replaced the store, a policy denial or a persistent load failure logged
    /// out, or a failed load recovered): it retires the cloud-config loader, so this task also
    /// rebinds a retired loader and re-checks requirements for the identity that replaced it.
    pub fn recheck_requirements_after_identity_completion(
        self,
        auth_manager: Arc<AuthManager>,
        chatgpt_base_url: String,
        http_client_factory: codex_http_client::HttpClientFactory,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> impl std::future::Future<Output = ()> + Send {
        // Capture both before spawning, not on the first poll: the embedded
        // processor may publish an auth change as soon as it starts.
        let mut auth_changes = auth_manager.auth_change_receiver();
        let startup_epoch = auth_manager
            .with_policy_identity(|identity| identity.current_epoch)
            .ok();
        async move {
            let mut retry_delay = Duration::from_secs(1);
            let mut requirements_sync_pending = false;
            while !shutdown.is_cancelled() {
                let startup_identity_changed = auth_manager
                    .with_policy_identity(|identity| identity.current_epoch)
                    .is_ok_and(|epoch| Some(epoch) != startup_epoch);
                requirements_sync_pending |= self.rebind_retired_cloud_config_bundle_loader(
                    Arc::clone(&auth_manager),
                    chatgpt_base_url.clone(),
                    http_client_factory.clone(),
                    startup_identity_changed,
                );
                if requirements_sync_pending || self.remote_control_policy_recheck_pending() {
                    requirements_sync_pending = !tokio::select! {
                        biased;
                        () = shutdown.cancelled() => return,
                        published = self.sync_default_client_residency_requirement() => published,
                    };
                }
                let pending =
                    requirements_sync_pending || self.remote_control_policy_recheck_pending();
                if !pending {
                    retry_delay = Duration::from_secs(1);
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = self.requirements_sync_retry.notified() => {
                        requirements_sync_pending = true;
                    }
                    () = tokio::time::sleep(retry_delay), if pending => {
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
                    }
                    changed = auth_changes.changed() => {
                        if changed.is_err() {
                            return;
                        }
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
            .is_some_and(RemoteControlHandle::policy_recheck_pending)
    }

    /// Loads the latest config for `fallback_cwd`. Without a cwd the request is
    /// threadless/global, so project layers are not discovered from the
    /// server process's launch directory.
    pub async fn load_latest_config(
        &self,
        fallback_cwd: Option<PathBuf>,
    ) -> std::io::Result<Config> {
        let ignore_project_config = fallback_cwd.is_none();
        self.load_with_cli_overrides_inner(
            &self.current_cli_overrides(),
            /*request_overrides*/ None,
            ConfigOverrides::default(),
            fallback_cwd,
            ignore_project_config,
        )
        .await
    }

    /// Catalog-wide settings must not inherit the server process's repository.
    pub async fn load_non_project_config(&self) -> std::io::Result<Config> {
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

    pub async fn load_latest_config_for_thread(
        &self,
        thread_config: &Config,
    ) -> std::io::Result<Config> {
        let generation = self.policy_generation();
        let refreshed_config = self
            .load_latest_config(Some(thread_config.cwd.to_path_buf()))
            .await?;
        let mut config = thread_config
            .rebuild_preserving_session_layers(&refreshed_config)
            .await?;
        self.apply_runtime_feature_enablement(&mut config);
        self.apply_arg0_paths(&mut config);
        self.with_current_policy(generation, || config)
    }

    pub async fn load_default_config(&self) -> std::io::Result<Config> {
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

    pub async fn load_with_overrides(
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

    pub async fn load_for_cwd(
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

    pub async fn load_with_cli_overrides(
        &self,
        cli_overrides: &[(String, TomlValue)],
        request_overrides: Option<HashMap<String, serde_json::Value>>,
        typesafe_overrides: ConfigOverrides,
        fallback_cwd: Option<PathBuf>,
    ) -> std::io::Result<Config> {
        self.load_with_cli_overrides_inner(
            cli_overrides,
            request_overrides,
            typesafe_overrides,
            fallback_cwd,
            /*ignore_project_config*/ false,
        )
        .await
    }

    #[instrument(level = "trace", skip_all)]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Single-flight gate: equal concurrent loads wait for the first build and reuse its cached config"
    )]
    async fn load_with_cli_overrides_inner(
        &self,
        cli_overrides: &[(String, TomlValue)],
        request_overrides: Option<HashMap<String, serde_json::Value>>,
        mut typesafe_overrides: ConfigOverrides,
        fallback_cwd: Option<PathBuf>,
        ignore_project_config: bool,
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
        let reserved_key = keys.iter().find_map(|(key, path)| {
            (path.as_slice() == ["bypass_hook_trust"]).then(|| (*key).clone())
        });
        if let Some(value) = reserved_key.and_then(|key| request_overrides.remove(&key)) {
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
            ignore_project_config,
        };
        let generation = self.load_generation.load(Ordering::Acquire);
        self.validate_policy_generation(generation)?;
        if let Some(config) = self.cached_config(&cache_key, generation) {
            return self.with_current_policy(generation, || config);
        }

        let flight = self.with_current_policy(generation, || {
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
        })?;
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
            .ignore_project_config(ignore_project_config)
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

    pub async fn load_config_layers_for_cwd(
        &self,
        cwd: AbsolutePathBuf,
    ) -> std::io::Result<ConfigLayerStack> {
        self.load_config_layers(Some(cwd)).await
    }

    pub fn plugin_config_reload_for_cwd(
        &self,
        cwd: AbsolutePathBuf,
    ) -> codex_core_plugins::marketplace_upgrade::ConfigLayerReload {
        let manager = self.clone();
        let runtime = tokio::runtime::Handle::current();
        Arc::new(move || runtime
            .block_on(manager.load_uncached_config(Some(cwd.to_path_buf()), false))
            .map(|config| config.config_layer_stack.as_ref().clone()))
    }

    /// Reloads config layers without deriving project context from the server
    /// process's working directory, for threadless/global plugin operations.
    pub fn plugin_config_reload_global(
        &self,
    ) -> codex_core_plugins::marketplace_upgrade::ConfigLayerReload {
        let manager = self.clone();
        let runtime = tokio::runtime::Handle::current();
        Arc::new(move || runtime
            .block_on(manager.load_non_project_config())
            .map(|config| config.config_layer_stack.as_ref().clone()))
    }

    pub async fn load_config_layers(
        &self,
        cwd: Option<AbsolutePathBuf>,
    ) -> std::io::Result<ConfigLayerStack> {
        let generation = self.policy_generation();
        self.validate_policy_generation(generation)?;
        let thread_config_loader = self.current_thread_config_loader();
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

    pub fn apply_runtime_feature_enablement_to_features(
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

    #[track_caller]
    pub fn invalidate_load_cache(&self) {
        let _policy = self.cloud_config_bundle.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.invalidate_load_cache_locked("explicit_policy_invalidation");
    }

    /// Drops the coalesced config reads without advancing the policy generation.
    ///
    /// For background plugin refreshes only. A `Config` never embeds plugin
    /// state (plugins resolve at use time through the plugins manager), so such
    /// an event changes nothing that thread/start, resume, fork, config writes
    /// or auth transitions admitted against. Advancing the generation would make
    /// any of them roll back and retry. Anything that can change policy inputs
    /// must use [`Self::invalidate_load_cache`] instead.
    pub fn discard_cached_loads(&self) {
        // Policy -> cache is the lock order; this takes the cache lock alone.
        self.load_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready
            .clear();
    }

    // Caller holds the policy publication write lock. Match the reader's
    // policy -> cache lock order, publishing the generation with its cleanup.
    #[track_caller]
    fn invalidate_load_cache_locked(&self, reason: &'static str) {
        let mut cache = self.load_cache.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = self.load_generation.fetch_add(1, Ordering::AcqRel);
        cache.ready.clear();
        cache.flights.clear();
        drop(cache);
        if codex_protocol::timing::observations::enabled() {
            let location = std::panic::Location::caller();
            let caller = location.file().rsplit(['/', '\\']).next().unwrap_or("unknown");
            let _ = codex_protocol::timing::observations::record(
                "app_server_config_generation_published", "", "", "",
                serde_json::json!({
                    "reason": reason,
                    "previous_generation": previous,
                    "generation": previous.wrapping_add(1),
                    "source_file": caller,
                    "source_line": location.line(),
                }),
            );
        }
    }



}

fn protected_feature_keys(config_layer_stack: &ConfigLayerStack) -> BTreeSet<String> {
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

fn apply_runtime_feature_enablement(
    config: &mut Config,
    runtime_feature_enablement: &BTreeMap<String, bool>,
) {
    if runtime_feature_enablement.is_empty() {
        return;
    }
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
    if runtime_feature_enablement.is_empty() {
        return;
    }
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

