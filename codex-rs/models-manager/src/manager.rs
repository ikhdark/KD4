use super::cache::ModelsCacheManager;
use crate::ModelsManagerConfig;
use crate::collaboration_mode_presets::builtin_collaboration_mode_presets;
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
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
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

    fn list_models_conditional<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
        _etag: Option<&'a str>,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsFetchResult>> {
        Box::pin(async move {
            let (models, etag) = self
                .list_models(client_version, http_client_factory)
                .await?;
            Ok(ModelsFetchResult::Modified { models, etag })
        })
    }
}

pub type ModelsEndpointFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug)]
pub enum ModelsFetchResult {
    Modified {
        models: Vec<ModelInfo>,
        etag: Option<String>,
    },
    NotModified,
}

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always revalidate over the network, using the current ETag when available.
    /// A `Not Modified` response retains the current catalog and renews its cache TTL.
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

/// Resolves the complete provider/auth/account identity used by the disk cache.
///
/// The resolver is evaluated for every cache operation so an account or
/// workspace switch cannot keep using the identity captured at construction.
pub type ModelsCacheIdentity = Arc<dyn Fn() -> String + Send + Sync>;

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
                let catalog = self
                    .raw_model_catalog(refresh_strategy, http_client_factory)
                    .await;
                self.build_available_models(catalog.models)
            }
            .instrument(tracing::info_span!(
                "list_models",
                refresh_strategy = %refresh_strategy
            )),
        )
    }

    /// List models through a shared immutable snapshot.
    fn list_models_shared(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<Vec<ModelPreset>>> {
        Box::pin(async move {
            Arc::new(
                self.list_models(refresh_strategy, http_client_factory)
                    .await,
            )
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

    /// Return the auth manager used for picker filtering.
    fn auth_manager(&self) -> Option<&AuthManager>;

    /// Build picker-ready presets from the active catalog snapshot.
    fn build_available_models(&self, remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        let uses_codex_backend = self
            .auth_manager()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        build_available_models_for_auth(remote_models, uses_codex_backend)
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask>;

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        Ok(self.try_list_models_shared()?.as_ref().clone())
    }

    /// Attempt to return the current picker projection without copying it.
    fn try_list_models_shared(&self) -> Result<Arc<Vec<ModelPreset>>, TryLockError> {
        let remote_models = self.try_get_remote_models()?;
        Ok(Arc::new(self.build_available_models(remote_models)))
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
                let remote_models = self.get_remote_models().await;
                construct_model_info_from_candidates(model, &remote_models, config)
            }
            .instrument(tracing::info_span!("get_model_info", model = model)),
        )
    }

    /// Refresh models if the provided ETag differs from the cached ETag.
    ///
    /// Uses `Online` strategy to fetch latest models when ETags differ and resolves when the
    /// coalescing refresh worker reaches idle.
    fn notify_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'static, ()>;
}

pub type ModelsManagerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shared model manager handle used across runtime services.
pub type SharedModelsManager = Arc<dyn ModelsManager>;

fn build_available_models_for_auth(
    mut remote_models: Vec<ModelInfo>,
    uses_codex_backend: bool,
) -> Vec<ModelPreset> {
    remote_models.sort_by_key(|model| model.priority);
    if !uses_codex_backend {
        remote_models.retain(|model| model.supported_in_api);
    }
    finalize_available_models(remote_models.into_iter().map(Into::into).collect())
}

fn finalize_available_models(mut presets: Vec<ModelPreset>) -> Vec<ModelPreset> {
    ModelPreset::mark_default_by_picker_visibility(&mut presets);
    presets
}

#[derive(Debug)]
struct AvailableModelPresets {
    api: Arc<Vec<ModelPreset>>,
    codex: Arc<Vec<ModelPreset>>,
}

impl AvailableModelPresets {
    fn new(remote_models: &[ModelInfo]) -> Self {
        let mut sorted_models: Vec<&ModelInfo> = remote_models.iter().collect();
        sorted_models.sort_by_key(|model| model.priority);
        let mut api = Vec::with_capacity(sorted_models.len());
        let mut codex = Vec::with_capacity(sorted_models.len());
        for model in sorted_models {
            let supported_in_api = model.supported_in_api;
            let preset = ModelPreset::from(model);
            if supported_in_api {
                api.push(preset.clone());
            }
            codex.push(preset);
        }
        Self {
            api: Arc::new(finalize_available_models(api)),
            codex: Arc::new(finalize_available_models(codex)),
        }
    }

    fn for_auth(&self, uses_codex_backend: bool) -> Arc<Vec<ModelPreset>> {
        if uses_codex_backend {
            Arc::clone(&self.codex)
        } else {
            Arc::clone(&self.api)
        }
    }
}

/// OpenAI-compatible model manager backed by bundled models, cache, and `/models`.
#[derive(Debug)]
pub struct OpenAiModelsManager {
    state: RwLock<ModelState>,
    cache_manager: ModelsCacheManager,
    endpoint_client: SharedModelsEndpointClient,
    auth_manager: Option<Arc<AuthManager>>,
    // If both locks are needed, acquire `state` before `etag_refresh`. Never hold
    // `etag_refresh` across an await point.
    etag_refresh: Mutex<EtagRefreshState>,
    etag_refresh_idle: Notify,
}

#[derive(Debug)]
struct ModelState {
    remote_models: Vec<ModelInfo>,
    available_models: AvailableModelPresets,
    etag: Option<String>,
    active_cache_identity: String,
}

impl ModelState {
    fn new(
        remote_models: Vec<ModelInfo>,
        etag: Option<String>,
        active_cache_identity: String,
    ) -> Self {
        let available_models = AvailableModelPresets::new(&remote_models);
        Self {
            remote_models,
            available_models,
            etag,
            active_cache_identity,
        }
    }

    fn replace_remote_models(&mut self, remote_models: Vec<ModelInfo>) {
        self.available_models = AvailableModelPresets::new(&remote_models);
        self.remote_models = remote_models;
    }

    fn reset_for_cache_identity(&mut self, cache_identity: String) -> bool {
        if self.active_cache_identity == cache_identity {
            return false;
        }

        self.replace_remote_models(load_bundled_models().unwrap_or_default());
        self.etag = None;
        self.active_cache_identity = cache_identity;
        true
    }
}

#[derive(Debug, Default)]
struct EtagRefreshState {
    generation: u64,
    active_etag: Option<String>,
    pending: Option<EtagRefreshNotice>,
    worker_running: bool,
}

#[derive(Debug)]
struct EtagRefreshNotice {
    generation: u64,
    etag: String,
    http_client_factory: HttpClientFactory,
}

struct EtagRefreshWorkerExitGuard {
    manager: Arc<OpenAiModelsManager>,
    armed: bool,
}

impl EtagRefreshWorkerExitGuard {
    fn new(manager: Arc<OpenAiModelsManager>) -> Self {
        Self {
            manager,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EtagRefreshWorkerExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let mut state = self
            .manager
            .etag_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active_etag = None;
        state.pending = None;
        state.worker_running = false;
        drop(state);
        self.manager.etag_refresh_idle.notify_waiters();
    }
}

/// Static model manager backed by an authoritative in-process catalog.
#[derive(Debug)]
pub struct StaticModelsManager {
    remote_models: Vec<ModelInfo>,
    available_models: AvailableModelPresets,
    auth_manager: Option<Arc<AuthManager>>,
    #[cfg(test)]
    list_models_calls: std::sync::atomic::AtomicUsize,
}

impl OpenAiModelsManager {
    /// Construct an OpenAI-compatible remote model manager.
    pub fn new(
        codex_home: PathBuf,
        endpoint_client: Arc<dyn ModelsEndpointClient>,
        auth_manager: Option<Arc<AuthManager>>,
        cache_identity: ModelsCacheIdentity,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager =
            ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL, cache_identity);
        let active_cache_identity = cache_manager.current_identity();
        let remote_models = load_bundled_models().unwrap_or_default();
        Self {
            state: RwLock::new(ModelState::new(remote_models, None, active_cache_identity)),
            cache_manager,
            endpoint_client,
            auth_manager,
            etag_refresh: Mutex::new(EtagRefreshState::default()),
            etag_refresh_idle: Notify::new(),
        }
    }
}

impl StaticModelsManager {
    /// Construct a static model manager from an authoritative catalog.
    pub fn new(auth_manager: Option<Arc<AuthManager>>, model_catalog: ModelsResponse) -> Self {
        let available_models = AvailableModelPresets::new(&model_catalog.models);
        Self {
            remote_models: model_catalog.models,
            available_models,
            auth_manager,
            #[cfg(test)]
            list_models_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    fn list_models_call_count(&self) -> usize {
        self.list_models_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl ModelsManager for OpenAiModelsManager {
    fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        Box::pin(async move {
            self.list_models_shared(refresh_strategy, http_client_factory)
                .await
                .as_ref()
                .clone()
        })
    }

    fn list_models_shared(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<Vec<ModelPreset>>> {
        Box::pin(async move {
            if let Err(err) = self
                .refresh_available_models(refresh_strategy, &http_client_factory)
                .await
            {
                error!("failed to refresh available models: {err}");
            }
            self.ensure_current_cache_identity().await;
            let uses_codex_backend = self
                .auth_manager()
                .is_some_and(AuthManager::current_auth_uses_codex_backend);
            self.state
                .read()
                .await
                .available_models
                .for_auth(uses_codex_backend)
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
        Box::pin(async move {
            self.ensure_current_cache_identity().await;
            self.state.read().await.remote_models.clone()
        })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.try_ensure_current_cache_identity()?;
        Ok(self.state.try_read()?.remote_models.clone())
    }

    fn try_list_models_shared(&self) -> Result<Arc<Vec<ModelPreset>>, TryLockError> {
        self.try_ensure_current_cache_identity()?;
        let uses_codex_backend = self
            .auth_manager()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        Ok(self
            .state
            .try_read()?
            .available_models
            .for_auth(uses_codex_backend))
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(
            async move {
                self.ensure_current_cache_identity().await;
                let state = self.state.read().await;
                construct_model_info_from_candidates(model, &state.remote_models, config)
            }
            .instrument(tracing::info_span!("get_model_info", model = model)),
        )
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn notify_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'static, ()> {
        Arc::clone(&self).submit_etag_notice(etag, http_client_factory);
        Box::pin(async move { self.wait_for_etag_refresh().await })
    }
}

impl OpenAiModelsManager {
    async fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: HttpClientFactory,
    ) -> ModelsResponse {
        if let Err(err) = self
            .refresh_available_models(refresh_strategy, &http_client_factory)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        ModelsResponse {
            models: self.get_remote_models().await,
        }
    }

    fn submit_etag_notice(self: Arc<Self>, etag: String, http_client_factory: HttpClientFactory) {
        let should_start_worker = {
            let mut state = self
                .etag_refresh
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.active_etag.as_deref() == Some(etag.as_str())
                || state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.etag == etag)
            {
                return;
            }
            state.generation = state.generation.saturating_add(1);
            state.pending = Some(EtagRefreshNotice {
                generation: state.generation,
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
            let exit_guard = EtagRefreshWorkerExitGuard::new(Arc::clone(&self));
            tokio::spawn(async move { self.run_etag_refresh_worker(exit_guard).await });
        }
    }

    async fn wait_for_etag_refresh(&self) {
        loop {
            let idle = self.etag_refresh_idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();

            let worker_running = self
                .etag_refresh
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .worker_running;
            if !worker_running {
                return;
            }

            idle.await;
        }
    }

    async fn run_etag_refresh_worker(self: Arc<Self>, mut exit_guard: EtagRefreshWorkerExitGuard) {
        loop {
            let notice = {
                let mut state = self
                    .etag_refresh
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let notice = state.pending.take();
                if let Some(notice) = &notice {
                    state.active_etag = Some(notice.etag.clone());
                } else {
                    state.active_etag = None;
                    state.worker_running = false;
                    exit_guard.disarm();
                }
                notice
            };
            let Some(notice) = notice else {
                self.etag_refresh_idle.notify_waiters();
                return;
            };

            let refresh_identity = self.ensure_current_cache_identity().await;
            let write_basis = match self
                .cache_manager
                .write_basis_for_identity(&refresh_identity)
                .await
            {
                Ok(basis) => Some(basis),
                Err(err) => {
                    error!("failed to capture models cache revision before refresh: {err}");
                    None
                }
            };

            if self.get_etag().await.as_deref() == Some(notice.etag.as_str()) {
                if let Some(write_basis) = write_basis
                    && let Err(err) = self
                        .cache_manager
                        .renew_cache_ttl_for_identity_if_unchanged(
                            &crate::client_version_to_whole(),
                            &notice.etag,
                            &refresh_identity,
                            &write_basis,
                        )
                        .await
                {
                    error!("failed to renew cache TTL: {err}");
                    self.ensure_current_cache_identity().await;
                }
            } else {
                let current_etag = self.get_etag().await;
                match self
                    .fetch_models(&notice.http_client_factory, current_etag.as_deref())
                    .await
                {
                    Ok(ModelsFetchResult::Modified { models, etag }) => {
                        if !self.cache_manager.identity_is_current(&refresh_identity) {
                            self.ensure_current_cache_identity().await;
                            continue;
                        }
                        let merged_models = self.merged_remote_models(models.clone());
                        let (should_apply, identity_is_current) = {
                            let mut model_state = self.state.write().await;
                            let refresh_state = self
                                .etag_refresh
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let identity_is_current = model_state.active_cache_identity
                                == refresh_identity
                                && self.cache_manager.identity_is_current(&refresh_identity);
                            if refresh_state.generation == notice.generation && identity_is_current
                            {
                                model_state.replace_remote_models(merged_models);
                                model_state.etag = etag.clone();
                                (true, true)
                            } else {
                                (false, identity_is_current)
                            }
                        };
                        if !identity_is_current {
                            self.ensure_current_cache_identity().await;
                            continue;
                        }
                        if should_apply
                            && let Some(write_basis) = write_basis.as_ref()
                            && !self
                                .cache_manager
                                .persist_cache_for_identity_if_unchanged(
                                    &models,
                                    etag,
                                    crate::client_version_to_whole(),
                                    &refresh_identity,
                                    write_basis,
                                )
                                .await
                        {
                            self.ensure_current_cache_identity().await;
                        }
                    }
                    Ok(ModelsFetchResult::NotModified) => {
                        if !self.cache_manager.identity_is_current(&refresh_identity) {
                            self.ensure_current_cache_identity().await;
                            continue;
                        }
                        if let (Some(etag), Some(write_basis)) =
                            (current_etag, write_basis.as_ref())
                            && let Err(err) = self
                                .cache_manager
                                .renew_cache_ttl_for_identity_if_unchanged(
                                    &crate::client_version_to_whole(),
                                    &etag,
                                    &refresh_identity,
                                    write_basis,
                                )
                                .await
                        {
                            error!("failed to renew cache TTL after conditional refresh: {err}");
                        }
                    }
                    Err(err) => error!("failed to refresh available models: {err}"),
                }
            }

            let mut state = self
                .etag_refresh
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active_etag = None;
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: &HttpClientFactory,
    ) -> CoreResult<()> {
        if !self.should_refresh_models().await {
            match refresh_strategy {
                RefreshStrategy::Offline | RefreshStrategy::OnlineIfUncached => {
                    self.try_load_cache().await;
                }
                RefreshStrategy::Online => {
                    // This no-op route has no cache or fetch operation to own
                    // the identity transition.
                    self.ensure_current_cache_identity().await;
                }
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
        let fetch_identity = self.ensure_current_cache_identity().await;
        let client_version = crate::client_version_to_whole();
        let current_etag = self.get_etag().await;
        let write_basis = match self
            .cache_manager
            .write_basis_for_identity(&fetch_identity)
            .await
        {
            Ok(basis) => Some(basis),
            Err(err) => {
                error!("failed to capture models cache revision before fetch: {err}");
                None
            }
        };
        match self
            .fetch_models(http_client_factory, current_etag.as_deref())
            .await?
        {
            ModelsFetchResult::Modified { models, etag } => {
                if !self.cache_manager.identity_is_current(&fetch_identity) {
                    self.ensure_current_cache_identity().await;
                    return Ok(());
                }
                if !self
                    .apply_remote_models_and_etag_for_identity(
                        models.clone(),
                        etag.clone(),
                        &fetch_identity,
                    )
                    .await
                {
                    self.ensure_current_cache_identity().await;
                    return Ok(());
                }
                if let Some(write_basis) = write_basis
                    && !self
                        .cache_manager
                        .persist_cache_for_identity_if_unchanged(
                            &models,
                            etag,
                            client_version,
                            &fetch_identity,
                            &write_basis,
                        )
                        .await
                {
                    self.ensure_current_cache_identity().await;
                }
            }
            ModelsFetchResult::NotModified => {
                if !self.cache_manager.identity_is_current(&fetch_identity) {
                    self.ensure_current_cache_identity().await;
                    return Ok(());
                }
                if let (Some(etag), Some(write_basis)) = (current_etag, write_basis.as_ref())
                    && let Err(err) = self
                        .cache_manager
                        .renew_cache_ttl_for_identity_if_unchanged(
                            &client_version,
                            &etag,
                            &fetch_identity,
                            write_basis,
                        )
                        .await
                {
                    error!("failed to renew cache TTL after conditional refresh: {err}");
                }
            }
        }
        Ok(())
    }

    async fn fetch_models(
        &self,
        http_client_factory: &HttpClientFactory,
        etag: Option<&str>,
    ) -> CoreResult<ModelsFetchResult> {
        self.endpoint_client
            .list_models_conditional(
                &crate::client_version_to_whole(),
                http_client_factory.clone(),
                etag,
            )
            .await
    }

    async fn should_refresh_models(&self) -> bool {
        self.endpoint_client.uses_codex_backend().await || self.endpoint_client.has_command_auth()
    }

    async fn get_etag(&self) -> Option<String> {
        self.state.read().await.etag.clone()
    }

    /// Reset identity-scoped in-memory state when the authoritative auth scope changes.
    async fn ensure_current_cache_identity(&self) -> String {
        let current_identity = self.cache_manager.current_identity();
        let mut state = self.state.write().await;
        if state.reset_for_cache_identity(current_identity.clone()) {
            info!(
                mismatch_category = "provider_cache_identity",
                "models cache: reset identity-scoped in-memory catalog"
            );
        }
        current_identity
    }

    /// Reset identity-scoped state for synchronous callers without waiting for the state lock.
    fn try_ensure_current_cache_identity(&self) -> Result<(), TryLockError> {
        let current_identity = self.cache_manager.current_identity();
        {
            let state = self.state.try_read()?;
            if state.active_cache_identity == current_identity {
                return Ok(());
            }
        }

        let mut state = self.state.try_write()?;
        if state.reset_for_cache_identity(current_identity) {
            info!(
                mismatch_category = "provider_cache_identity",
                "models cache: reset identity-scoped in-memory catalog"
            );
        }
        Ok(())
    }

    /// Replace the identity-scoped catalog and validator as one logical snapshot.
    async fn apply_remote_models_and_etag_for_identity(
        &self,
        models: Vec<ModelInfo>,
        etag: Option<String>,
        expected_identity: &str,
    ) -> bool {
        let merged_models = self.merged_remote_models(models);
        let mut state = self.state.write().await;
        if state.active_cache_identity != expected_identity
            || !self.cache_manager.identity_is_current(expected_identity)
        {
            return false;
        }
        if state.remote_models != merged_models {
            state.replace_remote_models(merged_models);
        }
        state.etag = etag;
        true
    }

    fn merged_remote_models(&self, mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        crate::prompt_resolver::apply_prompt_policy(&mut models);
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
            models
        } else {
            let mut existing_models = load_bundled_models().unwrap_or_default();
            let mut existing_indices: HashMap<String, usize> = existing_models
                .iter()
                .enumerate()
                .map(|(index, model)| (model.slug.clone(), index))
                .collect();
            for model in models {
                if let Some(&existing_index) = existing_indices.get(&model.slug) {
                    existing_models[existing_index] = model;
                } else {
                    existing_indices.insert(model.slug.clone(), existing_models.len());
                    existing_models.push(model);
                }
            }
            existing_models
        }
    }

    /// Attempt to satisfy the refresh from the cache when its complete identity and TTL match.
    async fn try_load_cache(&self) -> bool {
        let load_identity = self.ensure_current_cache_identity().await;
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let client_version = crate::client_version_to_whole();
        info!(client_version, "models cache: evaluating cache eligibility");
        let cache = match self
            .cache_manager
            .load_fresh_for_identity(&client_version, &load_identity)
            .await
        {
            Some(cache) => cache,
            None => {
                info!("models cache: no usable cache entry");
                return false;
            }
        };
        if !self.cache_manager.identity_is_current(&load_identity) {
            self.ensure_current_cache_identity().await;
            return false;
        }
        let models = cache.models.clone();
        if !self
            .apply_remote_models_and_etag_for_identity(
                models.clone(),
                cache.etag.clone(),
                &load_identity,
            )
            .await
        {
            self.ensure_current_cache_identity().await;
            return false;
        }
        if !self.cache_manager.identity_is_current(&load_identity) {
            self.ensure_current_cache_identity().await;
            return false;
        }
        info!(
            models_count = models.len(),
            etag = ?cache.etag,
            "models cache: cache entry applied"
        );
        true
    }
}

impl ModelsManager for StaticModelsManager {
    fn list_models(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        Box::pin(async move {
            #[cfg(test)]
            self.list_models_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let uses_codex_backend = self
                .auth_manager()
                .is_some_and(AuthManager::current_auth_uses_codex_backend);
            self.available_models
                .for_auth(uses_codex_backend)
                .as_ref()
                .clone()
        })
    }

    fn list_models_shared(
        &self,
        _refresh_strategy: RefreshStrategy,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'_, Arc<Vec<ModelPreset>>> {
        Box::pin(async move {
            let uses_codex_backend = self
                .auth_manager()
                .is_some_and(AuthManager::current_auth_uses_codex_backend);
            self.available_models.for_auth(uses_codex_backend)
        })
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
                if !allow_provider_model_fallback && let Some(model) = model.as_ref() {
                    return model.clone();
                }
                let available_models = self
                    .list_models(refresh_strategy, http_client_factory)
                    .await;
                let requested_model = model.as_deref();

                if allow_provider_model_fallback {
                    if (requested_model_is_available(requested_model, &available_models)
                        || requested_model_is_sol(requested_model))
                        && let Some(requested_model) = requested_model
                    {
                        return requested_model.to_string();
                    }
                    return default_model_from_available(available_models);
                }

                default_model_from_available(available_models)
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
        Box::pin(async { self.remote_models.clone() })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        Ok(self.remote_models.clone())
    }

    fn try_list_models_shared(&self) -> Result<Arc<Vec<ModelPreset>>, TryLockError> {
        let uses_codex_backend = self
            .auth_manager()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        Ok(self.available_models.for_auth(uses_codex_backend))
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(
            async move { construct_model_info_from_candidates(model, &self.remote_models, config) }
                .instrument(tracing::info_span!("get_model_info", model = model)),
        )
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.auth_manager.as_deref()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    fn notify_etag(
        self: Arc<Self>,
        _etag: String,
        _http_client_factory: HttpClientFactory,
    ) -> ModelsManagerFuture<'static, ()> {
        Box::pin(async {})
    }
}

fn load_bundled_models() -> Result<Vec<ModelInfo>, std::io::Error> {
    Ok(crate::bundled_models()?.models.clone())
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

fn requested_model_is_sol(requested_model: Option<&str>) -> bool {
    const SOL_MODEL: &str = "gpt-5.6-sol";
    requested_model.is_some_and(|requested_model| {
        requested_model == SOL_MODEL
            || requested_model
                .strip_suffix(SOL_MODEL)
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|provider| !provider.is_empty())
    })
}

fn find_model_by_longest_prefix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
    let mut best: Option<ModelInfo> = None;
    for candidate in candidates {
        let is_exact_match = model == candidate.slug;
        let is_hyphenated_variant = !candidate.slug.is_empty()
            && model
                .strip_prefix(&candidate.slug)
                .is_some_and(|suffix| suffix.starts_with('-'));
        if !is_exact_match && !is_hyphenated_variant {
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
