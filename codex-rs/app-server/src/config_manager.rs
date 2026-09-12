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
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use toml::Value as TomlValue;
use tracing::instrument;
use tracing::warn;

/// Coalesce the clusters of identical config reads made by one app-server RPC
/// without turning the manager into a long-lived authority over config files.
const CONFIG_LOAD_CACHE_TTL: Duration = Duration::from_millis(250);

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
    load_cache: Arc<RwLock<Option<ConfigLoadCacheEntry>>>,
    load_generation: Arc<AtomicU64>,
    #[cfg(test)]
    config_build_count: Arc<AtomicUsize>,
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
            load_cache: Arc::new(RwLock::new(None)),
            load_generation: Arc::new(AtomicU64::new(0)),
            #[cfg(test)]
            config_build_count: Arc::new(AtomicUsize::new(0)),
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
        drop(guard);
        self.invalidate_load_cache();
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
        match self.load_latest_config(/*fallback_cwd*/ None).await {
            Ok(config) => {
                set_default_client_residency_requirement(config.enforce_residency.value());
            }
            Err(err) => warn!(
                error = %err,
                "failed to sync default client residency requirement after auth refresh"
            ),
        }
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
        // Match build_cli_overrides_layer's literal-dot path splitting: quoting
        // and escaping do not change which dots are segment separators.
        let keys = request_overrides.keys().collect::<BTreeSet<_>>();
        for key in keys {
            for (separator, _) in key.match_indices('.') {
                let ancestor = &key[..separator];
                if request_overrides.contains_key(ancestor) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "ambiguous configuration overrides `{ancestor}` and `{key}`: use a single nested value or non-overlapping dotted paths"
                        ),
                    ));
                }
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
                    .map(|(key, value)| (key, json_to_toml(value))),
            )
            .collect::<Vec<_>>();

        let cache_key = ConfigLoadCacheKey {
            cli_overrides: merged_cli_overrides.clone(),
            typesafe_overrides: format!("{typesafe_overrides:?}"),
            fallback_cwd: fallback_cwd.clone(),
        };
        let generation = self.load_generation.load(Ordering::Acquire);
        if let Ok(cache) = self.load_cache.read()
            && let Some(entry) = cache.as_ref()
            && entry.generation == generation
            && entry.key == cache_key
            && entry.loaded_at.elapsed() <= CONFIG_LOAD_CACHE_TTL
        {
            return Ok(entry.config.clone());
        }

        #[cfg(test)]
        self.config_build_count.fetch_add(1, Ordering::Relaxed);
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
        if let Ok(mut cache) = self.load_cache.write() {
            *cache = Some(ConfigLoadCacheEntry {
                key: cache_key,
                generation,
                loaded_at: Instant::now(),
                config: config.clone(),
            });
        }
        Ok(config)
    }

    pub(crate) async fn load_config_layers_for_cwd(
        &self,
        cwd: AbsolutePathBuf,
    ) -> std::io::Result<ConfigLayerStack> {
        self.load_config_layers(Some(cwd)).await
    }

    pub(crate) async fn load_config_layers(
        &self,
        cwd: Option<AbsolutePathBuf>,
    ) -> std::io::Result<ConfigLayerStack> {
        let thread_config_loader = self.current_thread_config_loader();
        load_config_layers_state(
            LOCAL_FS.as_ref(),
            &self.codex_home,
            cwd,
            &self.current_cli_overrides(),
            codex_config::ConfigLoadOptions {
                loader_overrides: self.loader_overrides.clone(),
                strict_config: self.strict_config,
                cloud_config_bundle: self.current_cloud_config_bundle(),
            },
            thread_config_loader.as_ref(),
        )
        .await
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
            *cache = None;
        }
    }

    #[cfg(test)]
    fn config_build_count(&self) -> usize {
        self.config_build_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(
        codex_home: PathBuf,
        cli_overrides: Vec<(String, TomlValue)>,
        loader_overrides: LoaderOverrides,
        cloud_config_bundle: CloudConfigBundleLoader,
    ) -> Self {
        Self::new(
            codex_home,
            cli_overrides,
            loader_overrides,
            /*strict_config*/ false,
            cloud_config_bundle,
            Arg0DispatchPaths::default(),
            Arc::new(codex_config::NoopThreadConfigLoader),
        )
    }

    #[cfg(test)]
    pub(crate) fn without_managed_config_for_tests(codex_home: PathBuf) -> Self {
        Self::new_for_tests(
            codex_home,
            Vec::new(),
            LoaderOverrides::without_managed_config_for_tests(),
            CloudConfigBundleLoader::default(),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn identical_config_loads_within_ttl_reuse_built_config() -> std::io::Result<()> {
        let codex_home = TempDir::new()?;
        let manager =
            ConfigManager::without_managed_config_for_tests(codex_home.path().to_path_buf());
        let fallback_cwd = Some(codex_home.path().to_path_buf());

        manager.load_latest_config(fallback_cwd.clone()).await?;
        manager.load_latest_config(fallback_cwd).await?;

        assert_eq!(manager.config_build_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rpc_config_rejects_overlapping_paths_before_build_or_cache_changes()
    -> std::io::Result<()> {
        let codex_home = TempDir::new()?;
        let config_path = codex_home.path().join("config.toml");
        let original =
            "model = \"baseline-model\"\n[shell_environment_policy]\ninherit = \"core\"\n";
        std::fs::write(&config_path, original)?;
        let manager =
            ConfigManager::without_managed_config_for_tests(codex_home.path().to_path_buf());
        let overrides = ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        };
        let before = manager.load_with_overrides(None, overrides.clone()).await?;
        assert_eq!(before.model.as_deref(), Some("baseline-model"));
        assert_eq!(
            before.permissions.shell_environment_policy.inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core
        );
        let cache_before = manager.load_cache.read().expect("cache readable").clone();
        let builds_before = manager.config_build_count();
        let generation_before = manager.load_generation.load(Ordering::Acquire);
        for reverse in [false, true] {
            let mut pairs = vec![
                (
                    "shell_environment_policy".to_string(),
                    serde_json::json!({"inherit": "all"}),
                ),
                (
                    "shell_environment_policy.inherit".to_string(),
                    serde_json::json!("none"),
                ),
                ("model".to_string(), serde_json::json!("must-not-publish")),
            ];
            if reverse {
                pairs.reverse();
            }
            let error = manager
                .load_with_overrides(Some(pairs.into_iter().collect()), overrides.clone())
                .await
                .expect_err("an unordered request cannot choose parent versus child precedence");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(
                error.to_string(),
                "ambiguous configuration overrides `shell_environment_policy` and `shell_environment_policy.inherit`: use a single nested value or non-overlapping dotted paths"
            );
            assert_eq!(manager.config_build_count(), builds_before);
            assert_eq!(
                manager.load_generation.load(Ordering::Acquire),
                generation_before
            );
            let cache = manager.load_cache.read().expect("cache readable");
            let cache = cache.as_ref().expect("previous successful cache remains");
            let previous = cache_before.as_ref().expect("warm cache");
            assert_eq!(cache.key, previous.key);
            assert_eq!(cache.loaded_at, previous.loaded_at);
            assert_eq!(cache.config.model.as_deref(), Some("baseline-model"));
            assert_eq!(std::fs::read_to_string(&config_path)?, original);
        }
        let after = manager.load_with_overrides(None, overrides).await?;
        assert_eq!(after.model.as_deref(), Some("baseline-model"));
        assert_eq!(
            after.permissions.shell_environment_policy.inherit,
            codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core
        );
        Ok(())
    }

    #[tokio::test]
    async fn rpc_config_preserves_ordered_cli_and_nonoverlapping_request_precedence()
    -> std::io::Result<()> {
        use codex_protocol::config_types::ShellEnvironmentPolicyInherit;

        let codex_home = TempDir::new()?;
        let manager = ConfigManager::new_for_tests(
            codex_home.path().to_path_buf(),
            vec![
                (
                    "shell_environment_policy".to_string(),
                    json_to_toml(serde_json::json!({"inherit": "all", "set": {"CLI": "kept"}})),
                ),
                (
                    "shell_environment_policy.inherit".to_string(),
                    TomlValue::String("none".to_string()),
                ),
                (
                    "model".to_string(),
                    TomlValue::String("cli-model".to_string()),
                ),
            ],
            LoaderOverrides::without_managed_config_for_tests(),
            CloudConfigBundleLoader::default(),
        );
        let overrides = ConfigOverrides {
            cwd: Some(codex_home.path().to_path_buf()),
            ..Default::default()
        };
        let cli_only = manager.load_with_overrides(None, overrides.clone()).await?;
        assert_eq!(
            cli_only.permissions.shell_environment_policy.inherit,
            ShellEnvironmentPolicyInherit::None
        );
        assert_eq!(cli_only.model.as_deref(), Some("cli-model"));
        // PATH and PATH_EXTRA share a byte prefix but are distinct path segments.
        let pairs = [
            (
                "shell_environment_policy.inherit",
                serde_json::json!("core"),
            ),
            (
                "shell_environment_policy.set.PATH",
                serde_json::json!("request-path"),
            ),
            (
                "shell_environment_policy.set.PATH_EXTRA",
                serde_json::json!("request-extra"),
            ),
            ("model", serde_json::json!("request-model")),
            ("model_context_window", serde_json::json!(123456)),
        ];
        for reverse in [false, true] {
            let mut pairs = pairs.clone().to_vec();
            if reverse {
                pairs.reverse();
            }
            let request = pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect();
            let actual = manager
                .load_with_overrides(Some(request), overrides.clone())
                .await?;
            assert_eq!(actual.model.as_deref(), Some("request-model"));
            assert_eq!(actual.model_context_window, Some(123456));
            assert_eq!(
                actual.permissions.shell_environment_policy.inherit,
                ShellEnvironmentPolicyInherit::Core
            );
            assert_eq!(
                actual.permissions.shell_environment_policy.r#set,
                HashMap::from([
                    ("CLI".to_string(), "kept".to_string()),
                    ("PATH".to_string(), "request-path".to_string()),
                    ("PATH_EXTRA".to_string(), "request-extra".to_string()),
                ])
            );
        }
        Ok(())
    }

    fn manager_with_published_authorities(codex_home: &Path) -> ConfigManager {
        let manager = ConfigManager::new_for_tests(
            codex_home.to_path_buf(),
            vec![(
                "model".to_string(),
                TomlValue::String("poison-cli-model".to_string()),
            )],
            LoaderOverrides::without_managed_config_for_tests(),
            CloudConfigBundleLoader::new(async {
                Ok(Some(codex_config::CloudConfigBundle {
                    config_toml: codex_config::CloudConfigTomlBundle {
                        enterprise_managed: vec![codex_config::CloudConfigFragment {
                            id: "cloud-config".to_string(),
                            name: "cloud config".to_string(),
                            contents: "model_context_window = 123456".to_string(),
                        }],
                    },
                    requirements_toml: codex_config::CloudRequirementsTomlBundle {
                        enterprise_managed: vec![codex_config::CloudRequirementsFragment {
                            id: "cloud-requirement".to_string(),
                            name: "cloud requirement".to_string(),
                            contents: "allowed_approval_policies = [\"on-request\"]".to_string(),
                        }],
                    },
                }))
            }),
        );
        manager.replace_thread_config_loader(Arc::new(
            codex_config::StaticThreadConfigLoader::new(vec![
                codex_config::ThreadConfigSource::Session(codex_config::SessionThreadConfig {
                    features: BTreeMap::from([("tool_suggest".to_string(), false)]),
                    ..Default::default()
                }),
            ]),
        ));
        manager
            .extend_runtime_feature_enablement([("auth_elicitation".to_string(), false)])
            .expect("publish runtime feature");
        manager
    }

    fn assert_published_authorities(config: &Config) {
        assert_eq!(config.model.as_deref(), Some("poison-cli-model"));
        assert_eq!(config.model_context_window, Some(123456));
        assert_eq!(
            config
                .config_layer_stack
                .requirements_toml()
                .allowed_approval_policies,
            Some(vec![codex_protocol::protocol::AskForApproval::OnRequest]),
        );
        assert!(
            !config
                .features
                .enabled(codex_features::Feature::AuthElicitation)
        );
        assert!(
            !config
                .features
                .enabled(codex_features::Feature::ToolSuggest)
        );
    }

    #[tokio::test]
    async fn poisoned_publication_locks_preserve_effective_config_authorities()
    -> std::io::Result<()> {
        let codex_home = TempDir::new()?;
        let manager = manager_with_published_authorities(codex_home.path());
        let cwd = Some(codex_home.path().to_path_buf());
        assert_published_authorities(&manager.load_latest_config(cwd.clone()).await?);

        // Fault injection poisons an unchanged published value; normal loading
        // below must still apply its authority instead of silently defaulting it.
        fn poison_published<T>(lock: &RwLock<T>) {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _guard = lock.write().expect("initially unpoisoned writer");
                    panic!("publication writer interrupted without changing its value");
                }))
                .is_err()
            );
        }
        poison_published(&manager.cloud_config_bundle);
        poison_published(&manager.runtime_feature_enablement);
        poison_published(&manager.thread_config_loader);
        manager.invalidate_load_cache();
        assert_published_authorities(&manager.load_latest_config(cwd.clone()).await?);
        assert_eq!(
            manager.config_build_count(),
            2,
            "the cached config must not hide poisoned reads"
        );

        manager
            .extend_runtime_feature_enablement([("auth_elicitation".to_string(), true)])
            .expect("a later complete update recovers publication");
        let updated = manager.load_latest_config(cwd).await?;
        assert!(
            updated
                .features
                .enabled(codex_features::Feature::AuthElicitation)
        );
        assert_eq!(updated.model.as_deref(), Some("poison-cli-model"));
        assert_eq!(updated.model_context_window, Some(123456));
        Ok(())
    }

    #[tokio::test]
    async fn panicking_runtime_feature_iterator_does_not_publish_partial_config()
    -> std::io::Result<()> {
        let codex_home = TempDir::new()?;
        let manager = manager_with_published_authorities(codex_home.path());
        let generation = manager.load_generation.load(Ordering::Acquire);
        let mut yielded = false;
        let enablement = std::iter::from_fn(move || {
            if yielded {
                panic!("caller-controlled iterator interrupted after its first update");
            }
            yielded = true;
            Some(("auth_elicitation".to_string(), true))
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = manager.extend_runtime_feature_enablement(enablement);
            }))
            .is_err()
        );
        assert!(!manager.runtime_feature_enablement.is_poisoned());
        assert_eq!(manager.load_generation.load(Ordering::Acquire), generation);
        // First actual load has no cached value to conceal a partial update.
        assert_published_authorities(
            &manager
                .load_latest_config(Some(codex_home.path().to_path_buf()))
                .await?,
        );
        Ok(())
    }
}
