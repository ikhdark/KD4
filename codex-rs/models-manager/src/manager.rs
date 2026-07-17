use super::cache::ModelsCacheManager;
use crate::collaboration_mode_presets::builtin_collaboration_mode_presets;
use crate::config::ModelsManagerConfig;
use crate::model_info;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use std::fmt;
use std::future::Future;
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tokio::sync::watch;
use tracing::Instrument as _;
use tracing::error;
use tracing::info;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Remote endpoint used by the OpenAI-compatible model manager.
///
/// Implementations own provider-specific auth and transport details. The model
/// manager owns refresh policy, cache behavior, and catalog merging; it calls
/// this endpoint only when it decides a remote refresh should happen.
pub trait ModelsEndpointClient: fmt::Debug + Send + Sync {
    /// Returns whether this provider can authenticate command-scoped requests.
    fn has_command_auth(&self) -> bool;

    /// Returns whether the currently resolved auth can use Codex backend-only models.
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool>;

    /// Fetches the latest remote model catalog and optional ETag.
    fn list_models<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>>;
}

pub type ModelsEndpointFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from the network, ignoring cache.
    Online,
    /// Only use cached data, never fetch from the network.
    Offline,
    /// Use cache if available and fresh, otherwise fetch from the network.
    OnlineIfUncached,
}

impl RefreshStrategy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::OnlineIfUncached => "online_if_uncached",
        }
    }
}

impl fmt::Display for RefreshStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

type SharedModelsEndpointClient = Arc<dyn ModelsEndpointClient>;

/// Coordinates model discovery plus cached metadata on disk.
pub trait ModelsManager: fmt::Debug + Send + Sync {
    /// List all available models, refreshing according to the specified strategy.
    ///
    /// Returns model presets sorted by priority and filtered by auth mode and visibility.
    fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        Box::pin(
            async move {
                self.list_models_snapshot(refresh_strategy, http_client_factory)
                    .await
                    .as_ref()
                    .to_vec()
            }
            .instrument(tracing::info_span!(
                "list_models",
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    /// Return the picker-ready models in the immutable catalog snapshot.
    fn list_models_snapshot(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<[ModelPreset]>> {
        Box::pin(async move {
            self.catalog_snapshot(refresh_strategy, http_client_factory)
                .await
                .available_models(self.uses_codex_backend())
        })
    }

    /// Return one immutable catalog generation after applying the requested refresh policy.
    fn catalog_snapshot(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(async move {
            let catalog = self
                .raw_model_catalog(refresh_strategy, http_client_factory)
                .await;
            Arc::new(ModelCatalogSnapshot::new(0, catalog.models, None))
        })
    }

    /// Return the active raw model catalog, refreshing according to the specified strategy.
    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse>;

    /// Return the current in-memory remote model catalog without refreshing or loading cache state.
    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>>;

    /// Attempt to return the current in-memory remote model catalog without blocking.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError>;

    /// Return the current immutable, indexed catalog snapshot.
    fn current_catalog_snapshot(&self) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(async move {
            Arc::new(ModelCatalogSnapshot::new(
                0,
                self.get_remote_models().await,
                None,
            ))
        })
    }

    /// Attempt to return the current immutable catalog snapshot without blocking.
    fn try_current_catalog_snapshot(&self) -> Result<Arc<ModelCatalogSnapshot>, TryLockError> {
        Ok(Arc::new(ModelCatalogSnapshot::new(
            0,
            self.try_get_remote_models()?,
            None,
        )))
    }

    /// Return the auth manager used for picker filtering.
    fn auth_manager(&self) -> Option<&AuthManager>;

    /// Build picker-ready presets from the active catalog snapshot.
    fn build_available_models(&self, remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        build_available_models_for_auth(remote_models, self.uses_codex_backend())
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask>;

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        Ok(self.try_list_models_snapshot()?.as_ref().to_vec())
    }

    /// Attempt to share picker-ready models from the current catalog snapshot.
    fn try_list_models_snapshot(&self) -> Result<Arc<[ModelPreset]>, TryLockError> {
        let snapshot = self.try_current_catalog_snapshot()?;
        Ok(snapshot.available_models(self.uses_codex_backend()))
    }

    // todo(aibrahim): should be visible to core only and sent on session_configured event
    /// Get the model identifier to use, refreshing according to the specified strategy.
    ///
    /// If `model` is provided, preserves it unless the implementation supports and the policy
    /// allows provider fallback. Otherwise selects the default based on auth mode and available
    /// models.
    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        allow_provider_model_fallback: bool,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'a, String> {
        Box::pin(
            async move {
                if let Some(model) = model.as_ref() {
                    return model.to_string();
                }
                default_model_from_available(
                    self.list_models(refresh_strategy, http_client_factory)
                        .await,
                )
            }
            .instrument(tracing::info_span!(
                "get_default_model",
                model.provided = model.is_some(),
                allow_provider_model_fallback,
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    // todo(aibrahim): look if we can tighten it to pub(crate)
    /// Look up model metadata, applying remote overrides and config adjustments.
    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(
            async move {
                self.current_catalog_snapshot().await.model_info(model, config)
            }
            .instrument(tracing::info_span!("get_model_info", model = model)),
        )
    }

    fn uses_codex_backend(&self) -> bool {
        self.auth_manager()
            .is_some_and(AuthManager::current_auth_uses_codex_backend)
    }

    /// Enqueue a catalog refresh if the provided ETag differs from the cached ETag.
    ///
    /// Implementations must return without waiting for network or cache I/O. Async catalog reads
    /// started after this call observe the queued refresh through [`Self::refresh_barrier`].
    fn enqueue_refresh_if_new_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: HttpClientFactory,
    );

    /// Wait until every ETag refresh queued before this call has either published or failed.
    fn refresh_barrier(&self) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub type ModelsManagerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shared model manager handle used across runtime services.
pub type SharedModelsManager = Arc<dyn ModelsManager>;

/// Immutable, indexed model catalog published as one atomic generation.
#[derive(Debug)]
pub struct ModelCatalogSnapshot {
    generation: u64,
    models: Arc<[ModelInfo]>,
    etag: Option<String>,
    exact_model_index: HashMap<String, usize>,
    codex_backend_presets: Arc<[ModelPreset]>,
    api_presets: Arc<[ModelPreset]>,
}

impl ModelCatalogSnapshot {
    fn new(generation: u64, models: Vec<ModelInfo>, etag: Option<String>) -> Self {
        let exact_model_index = models
            .iter()
            .enumerate()
            .map(|(index, model)| (model.slug.clone(), index))
            .collect();
        let codex_backend_presets = Arc::from(build_available_models_for_auth(
            models.clone(),
            /*uses_codex_backend*/ true,
        ));
        let api_presets = Arc::from(build_available_models_for_auth(
            models.clone(),
            /*uses_codex_backend*/ false,
        ));
        Self {
            generation,
            models: Arc::from(models),
            etag,
            exact_model_index,
            codex_backend_presets,
            api_presets,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn models(&self) -> &[ModelInfo] {
        self.models.as_ref()
    }

    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    pub fn available_models(&self, uses_codex_backend: bool) -> Arc<[ModelPreset]> {
        if uses_codex_backend {
            Arc::clone(&self.codex_backend_presets)
        } else {
            Arc::clone(&self.api_presets)
        }
    }

    pub fn model_info(&self, model: &str, config: &ModelsManagerConfig) -> ModelInfo {
        let remote = self
            .exact_model_index
            .get(model)
            .and_then(|index| self.models.get(*index))
            .cloned()
            .or_else(|| find_model_by_longest_prefix(model, &self.models))
            .or_else(|| find_model_by_namespaced_suffix(model, &self.models));
        let model_info = if let Some(remote) = remote {
            ModelInfo {
                slug: model.to_string(),
                used_fallback_model_metadata: false,
                ..remote
            }
        } else {
            model_info::model_info_from_slug(model)
        };
        model_info::with_config_overrides(model_info, config)
    }
}

/// OpenAI-compatible model manager backed by bundled models, cache, and `/models`.
#[derive(Debug)]
pub struct OpenAiModelsManager {
    catalog: RwLock<Arc<ModelCatalogSnapshot>>,
    catalog_generation: AtomicU64,
    cache_manager: ModelsCacheManager,
    endpoint_client: SharedModelsEndpointClient,
    auth_manager: Option<Arc<AuthManager>>,
    refresh_coordinator: ModelRefreshCoordinator,
    refresh_execution: tokio::sync::Mutex<()>,
}

#[derive(Debug)]
struct ModelRefreshCoordinator {
    state: Mutex<ModelRefreshCoordinatorState>,
    completed_generation: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct ModelRefreshCoordinatorState {
    next_generation: u64,
    worker_running: bool,
    active: Option<(u64, String)>,
    pending: Option<PendingModelRefresh>,
}

struct PendingModelRefresh {
    generation: u64,
    etag: String,
    http_client_factory: HttpClientFactory,
}

impl fmt::Debug for PendingModelRefresh {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingModelRefresh")
            .field("generation", &self.generation)
            .field("etag", &self.etag)
            .finish_non_exhaustive()
    }
}

/// Static model manager backed by an authoritative in-process catalog.
#[derive(Debug)]
pub struct StaticModelsManager {
    catalog: Arc<ModelCatalogSnapshot>,
    auth_manager: Option<Arc<AuthManager>>,
}

impl OpenAiModelsManager {
    /// Construct an OpenAI-compatible remote model manager.
    pub fn new(
        codex_home: PathBuf,
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        let remote_models = load_remote_models_from_file().unwrap_or_default();
        let (completed_generation, _) = watch::channel(0);
        Self {
            catalog: RwLock::new(Arc::new(ModelCatalogSnapshot::new(
                0,
                remote_models,
                None,
            ))),
            catalog_generation: AtomicU64::new(0),
            cache_manager,
            endpoint_client,
            auth_manager,
            refresh_coordinator: ModelRefreshCoordinator {
                state: Mutex::new(ModelRefreshCoordinatorState::default()),
                completed_generation,
            },
            refresh_execution: tokio::sync::Mutex::new(()),
        }
    }
}

impl StaticModelsManager {
    /// Construct a static model manager from an authoritative catalog.
    pub fn new(auth_manager: Option<Arc<AuthManager>>, model_catalog: ModelsResponse) -> Self {
        Self {
            catalog: Arc::new(ModelCatalogSnapshot::new(0, model_catalog.models, None)),
            auth_manager,
        }
    }
}

impl ModelsManager for OpenAiModelsManager {
    fn catalog_snapshot(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(self.refresh_catalog_snapshot(refresh_strategy, http_client_factory))
    }

    fn list_models_snapshot(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<[ModelPreset]>> {
        Box::pin(async move {
            self.refresh_catalog_snapshot(refresh_strategy, http_client_factory)
                .await
                .available_models(self.uses_codex_backend())
        })
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(OpenAiModelsManager::raw_model_catalog(
            self,
            refresh_strategy,
            http_client_factory,
        ))
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move { self.current_catalog_snapshot().await.models().to_vec() })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self.try_current_catalog_snapshot()?.models().to_vec())
    }

    fn current_catalog_snapshot(&self) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(async move {
            self.wait_for_refresh_barrier().await;
            Arc::clone(&*self.catalog.read().await)
        })
    }

    fn try_current_catalog_snapshot(&self) -> Result<Arc<ModelCatalogSnapshot>, TryLockError> {
        Ok(Arc::clone(&*self.catalog.try_read()?))
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn enqueue_refresh_if_new_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) {
        self.enqueue_model_refresh(etag, http_client_factory);
    }

    fn refresh_barrier(&self) -> ModelsManagerFuture<'_, ()> {
        Box::pin(self.wait_for_refresh_barrier())
    }
}

impl OpenAiModelsManager {
    async fn refresh_catalog_snapshot(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> Arc<ModelCatalogSnapshot> {
        self.wait_for_refresh_barrier().await;
        if let Err(err) = self
            .refresh_available_models(refresh_strategy, &http_client_factory)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        Arc::clone(&*self.catalog.read().await)
    }

    async fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsResponse {
        let snapshot = self
            .refresh_catalog_snapshot(refresh_strategy, http_client_factory)
            .await;
        ModelsResponse {
            models: snapshot.models().to_vec(),
        }
    }

    fn enqueue_model_refresh(
        self: Arc<Self>,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) {
        let should_start_worker = {
            let mut state = self
                .refresh_coordinator
                .state
                .lock()
                .expect("model refresh coordinator lock should not be poisoned");
            if let Some(pending) = state.pending.as_mut()
                && pending.etag == etag
            {
                pending.http_client_factory = http_client_factory;
                return;
            }
            if state
                .active
                .as_ref()
                .is_some_and(|(_, active_etag)| active_etag == &etag)
                && state.pending.is_none()
            {
                return;
            }

            state.next_generation = state
                .next_generation
                .checked_add(1)
                .expect("model refresh generation should not overflow");
            let generation = state.next_generation;
            state.pending = Some(PendingModelRefresh {
                generation,
                etag,
                http_client_factory,
            });
            if state.worker_running {
                false
            } else {
                state.worker_running = true;
                true
            }
        };

        if should_start_worker {
            tokio::spawn(async move {
                self.run_model_refresh_worker().await;
            });
        }
    }

    async fn run_model_refresh_worker(self: Arc<Self>) {
        loop {
            let refresh = {
                let mut state = self
                    .refresh_coordinator
                    .state
                    .lock()
                    .expect("model refresh coordinator lock should not be poisoned");
                let Some(refresh) = state.pending.take() else {
                    state.worker_running = false;
                    return;
                };
                state.active = Some((refresh.generation, refresh.etag.clone()));
                refresh
            };

            self.refresh_if_new_etag_now(refresh.etag, refresh.http_client_factory)
                .await;

            let _ = self
                .refresh_coordinator
                .completed_generation
                .send_replace(refresh.generation);
            self.refresh_coordinator
                .state
                .lock()
                .expect("model refresh coordinator lock should not be poisoned")
                .active = None;
        }
    }

    async fn wait_for_refresh_barrier(&self) {
        let target_generation = self
            .refresh_coordinator
            .state
            .lock()
            .expect("model refresh coordinator lock should not be poisoned")
            .next_generation;
        let mut completed = self.refresh_coordinator.completed_generation.subscribe();
        while *completed.borrow_and_update() < target_generation {
            if completed.changed().await.is_err() {
                return;
            }
        }
    }

    async fn refresh_if_new_etag_now(
        &self,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) {
        let current_etag = self.get_etag().await;
        if current_etag.clone().is_some() && current_etag.as_deref() == Some(etag.as_str()) {
            if let Err(err) = self.cache_manager.renew_cache_ttl().await {
                error!("failed to renew cache TTL: {err}");
            }
            return;
        }
        if let Err(err) = self
            .refresh_available_models(RefreshStrategy::Online, &http_client_factory)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: &HttpClientFactory,
    ) -> CoreResult<()> {
        let _refresh_guard = self.refresh_execution.lock().await;
        if !self.should_refresh_models().await {
            if matches!(
                refresh_strategy,
                RefreshStrategy::Offline | RefreshStrategy::OnlineIfUncached
            ) {
                self.try_load_cache().await;
            }
            return Ok(());
        }

        match refresh_strategy {
            RefreshStrategy::Offline => {
                // Only try to load from cache, never fetch
                self.try_load_cache().await;
                Ok(())
            }
            RefreshStrategy::OnlineIfUncached => {
                // Try cache first, fall back to online if unavailable
                if self.try_load_cache().await {
                    info!("models cache: using cached models for OnlineIfUncached");
                    return Ok(());
                }
                info!("models cache: cache miss, fetching remote models");
                self.fetch_and_update_models(http_client_factory).await
            }
            RefreshStrategy::Online => {
                // Always fetch from network
                self.fetch_and_update_models(http_client_factory).await
            }
        }
    }

    async fn fetch_and_update_models(
        &self,
        http_client_factory: &HttpClientFactory,
    ) -> CoreResult<()> {
        let client_version = crate::client_version_to_whole();
        let (models, etag) = self
            .endpoint_client
            .list_models(&client_version, http_client_factory.clone())
            .await?;
        let published_models = self.merge_remote_models(models.clone());
        self.publish_catalog(published_models, etag.clone()).await;
        self.cache_manager
            .persist_cache(&models, etag, client_version)
            .await;
        Ok(())
    }

    async fn should_refresh_models(&self) -> bool {
        self.endpoint_client.uses_codex_backend().await || self.endpoint_client.has_command_auth()
    }

    async fn get_etag(&self) -> Option<String> {
        self.catalog.read().await.etag.clone()
    }

    async fn publish_catalog(
        &self,
        models: Vec<ModelInfo>,
        etag: Option<String>,
    ) -> Arc<ModelCatalogSnapshot> {
        {
            let current = self.catalog.read().await;
            if current.models() == models.as_slice() && current.etag() == etag.as_deref() {
                return Arc::clone(&current);
            }
        }
        let generation = self
            .catalog_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let snapshot = Arc::new(ModelCatalogSnapshot::new(generation, models, etag));
        *self.catalog.write().await = Arc::clone(&snapshot);
        snapshot
    }

    /// Merge fetched models into the bundled catalog before atomically publishing the snapshot.
    fn merge_remote_models(&self, models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        // Use the remote models list as the source of truth if it contains at least one
        // non-hidden model and the user is using ChatGPT auth.
        let should_use_remote_models_only = !models.is_empty()
            && models
                .iter()
                .any(|model| model.visibility == ModelVisibility::List)
            && self.auth_manager.as_ref().is_some_and(|auth_manager| {
                auth_manager
                    .auth_mode()
                    .is_some_and(AuthMode::has_chatgpt_account)
            });
        if should_use_remote_models_only {
            return models;
        }

        let mut existing_models = load_remote_models_from_file().unwrap_or_default();
        for model in models {
            if let Some(existing_index) = existing_models
                .iter()
                .position(|existing| existing.slug == model.slug)
            {
                existing_models[existing_index] = model;
            } else {
                existing_models.push(model);
            }
        }
        existing_models
    }

    /// Attempt to satisfy the refresh from the cache when it matches the provider and TTL.
    async fn try_load_cache(&self) -> bool {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let client_version = crate::client_version_to_whole();
        info!(client_version, "models cache: evaluating cache eligibility");
        // TODO(celia-oai): Include provider identity in cache eligibility so switching
        // providers does not reuse a fresh models_cache.json entry from another provider.
        let cache = match self.cache_manager.load_fresh(&client_version).await {
            Some(cache) => cache,
            None => {
                info!("models cache: no usable cache entry");
                return false;
            }
        };
        let models = cache.models.clone();
        let published_models = self.merge_remote_models(models.clone());
        self.publish_catalog(published_models, cache.etag.clone())
            .await;
        info!(
            models_count = models.len(),
            etag = ?cache.etag,
            "models cache: cache entry applied"
        );
        true
    }
}

impl ModelsManager for StaticModelsManager {
    fn catalog_snapshot(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(async { Arc::clone(&self.catalog) })
    }

    fn list_models_snapshot(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<[ModelPreset]>> {
        Box::pin(async { self.catalog.available_models(self.uses_codex_backend()) })
    }

    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        allow_provider_model_fallback: bool,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'a, String> {
        Box::pin(
            async move {
                let available_models = self
                    .list_models(refresh_strategy, http_client_factory)
                    .await;
                let requested_model = model.as_deref();

                if allow_provider_model_fallback {
                    if requested_model_is_available(requested_model, &available_models)
                        && let Some(requested_model) = requested_model
                    {
                        return requested_model.to_string();
                    }
                    return default_model_from_available(available_models);
                }

                model
                    .clone()
                    .unwrap_or_else(|| default_model_from_available(available_models))
            }
            .instrument(tracing::info_span!(
                "get_default_model",
                model.provided = model.is_some(),
                allow_provider_model_fallback,
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    fn raw_model_catalog(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(async move {
            ModelsResponse {
                models: self.get_remote_models().await,
            }
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async { self.catalog.models().to_vec() })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self.catalog.models().to_vec())
    }

    fn current_catalog_snapshot(&self) -> ModelsManagerFuture<'_, Arc<ModelCatalogSnapshot>> {
        Box::pin(async { Arc::clone(&self.catalog) })
    }

    fn try_current_catalog_snapshot(&self) -> Result<Arc<ModelCatalogSnapshot>, TryLockError> {
        Ok(Arc::clone(&self.catalog))
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn enqueue_refresh_if_new_etag(
        self: Arc<Self>,
        _etag: String,
        _http_client_factory: HttpClientFactory,
    ) {
    }
}

fn load_remote_models_from_file() -> Result<Vec<ModelInfo>, std::io::Error> {
    Ok(crate::bundled_models_response()?.models)
}

fn build_available_models_for_auth(
    mut remote_models: Vec<ModelInfo>,
    uses_codex_backend: bool,
) -> Vec<ModelPreset> {
    remote_models.sort_by_key(|model| model.priority);
    let presets = remote_models.into_iter().map(Into::into).collect();
    let mut presets = ModelPreset::filter_by_auth(presets, uses_codex_backend);
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    presets
}

fn default_model_from_available(available: Vec<ModelPreset>) -> String {
    available
        .iter()
        .find(|model| model.is_default)
        .or_else(|| available.first())
        .map(|model| model.model.clone())
        .unwrap_or_default()
}

fn requested_model_is_available(
    requested_model: Option<&str>,
    available_models: &[ModelPreset],
) -> bool {
    requested_model.is_some_and(|requested_model| {
        available_models
            .iter()
            .any(|available_model| available_model.model == requested_model)
    })
}

fn find_model_by_longest_prefix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    let mut best: Option<ModelInfo> = None;
    for candidate in candidates {
        if !model.starts_with(&candidate.slug) {
            continue;
        }
        let is_better_match = if let Some(current) = best.as_ref() {
            candidate.slug.len() > current.slug.len()
        } else {
            true
        };
        if is_better_match {
            best = Some(candidate.clone());
        }
    }
    best
}

fn find_model_by_namespaced_suffix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    // Retry metadata lookup for a single namespaced slug like `namespace/model-name`.
    //
    // This only strips one leading namespace segment and only when the namespace looks
    // like a simple provider id to avoid broadly matching arbitrary aliases.
    let (namespace, suffix) = model.split_once('/')?;
    if suffix.contains('/') {
        return None;
    }
    if namespace.is_empty()
        || !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    find_model_by_longest_prefix(suffix, candidates)
}

pub(crate) fn construct_model_info_from_candidates(
    model: &str,
    candidates: &[ModelInfo],
    config: &ModelsManagerConfig,
) -> ModelInfo {
    // First use the normal longest-prefix match. If that misses, allow a narrowly scoped
    // retry for namespaced slugs like `custom/gpt-5.3-codex`.
    let remote = find_model_by_longest_prefix(model, candidates)
        .or_else(|| find_model_by_namespaced_suffix(model, candidates));
    let model_info = if let Some(remote) = remote {
        ModelInfo {
            slug: model.to_string(),
            used_fallback_model_metadata: false,
            ..remote
        }
    } else {
        model_info::model_info_from_slug(model)
    };
    model_info::with_config_overrides(model_info, config)
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
