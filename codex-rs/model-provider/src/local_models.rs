use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use codex_http_client::HttpClientFactory;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::ProviderKind;
use codex_model_provider_info::local::model_list_url;
use codex_model_provider_info::local::parse_model_ids;
use codex_models_manager::manager::ModelsCacheIdentity;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;

#[derive(Debug)]
pub(crate) struct LocalModelsEndpoint {
    info: ModelProviderInfo,
    pool: Mutex<Option<(HttpClientFactory, codex_http_client::RouteAwareClientPool)>>,
}

impl LocalModelsEndpoint {
    fn pool_for(&self, factory: HttpClientFactory) -> codex_http_client::RouteAwareClientPool {
        let mut cached = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((previous, pool)) = cached.as_ref()
            && previous == &factory
        {
            return pool.clone();
        }
        let pool = codex_http_client::RouteAwareClientPool::new(
            factory.clone(),
            codex_http_client::ClientRouteClass::Other,
        );
        *cached = Some((factory, pool.clone()));
        pool
    }
}

// Readiness already owns discovery. Hand off its immutable result without
// blocking startup on optional storage. No lock is held across I/O or awaits.
struct ReadinessCatalog {
    provider_id: String,
    info: ModelProviderInfo,
    home: PathBuf,
    ids: Vec<String>,
    expires: Instant,
}

fn readiness_catalogs() -> &'static Mutex<Vec<ReadinessCatalog>> {
    static CATALOGS: OnceLock<Mutex<Vec<ReadinessCatalog>>> = OnceLock::new();
    CATALOGS.get_or_init(Mutex::default)
}

fn descriptors(ids: Vec<String>) -> Vec<ModelInfo> {
    ids.into_iter().map(|id| {
        let mut info = codex_models_manager::model_info::model_info_from_slug(&id);
        info.base_instructions = codex_models_manager::model_info::BASE_INSTRUCTIONS.to_owned();
        info.model_messages = None;
        info.visibility = ModelVisibility::List;
        info.description = Some("Installed local model; capabilities are unknown unless configured in model_catalog_json.".into());
        info.context_window = None;
        info.input_modalities = vec![InputModality::Text];
        info
    }).collect()
}

impl ModelsEndpointClient for LocalModelsEndpoint {
    fn has_authoritative_catalog(&self) -> bool {
        true
    }
    fn has_command_auth(&self) -> bool {
        false
    }
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }
    fn list_models<'a>(
        &'a self,
        _version: &'a str,
        factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<(Vec<ModelInfo>, Option<String>)>> {
        Box::pin(async move {
            let base = self
                .info
                .base_url
                .as_deref()
                .ok_or_else(|| std::io::Error::other("local provider has no base URL"))?;
            let url = model_list_url(self.info.kind, base)?;
            let kind = self.info.kind;
            let ids = tokio::time::timeout(Duration::from_secs(10), async {
                let response = self.pool_for(factory)
                    .get(url)
                    .send()
                    .await
                    .map_err(std::io::Error::other)?;
                if !response.status().is_success() {
                    return Err(std::io::Error::other(format!(
                        "failed to list local models: HTTP {}",
                        response.status()
                    )));
                }
                let value = response.json().await.map_err(std::io::Error::other)?;
                parse_model_ids(kind, &value)
            })
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "local catalog discovery timed out",
                )
            })??;
            Ok((descriptors(ids), None))
        })
    }
}

pub(crate) fn manager(
    provider_id: &str,
    info: ModelProviderInfo,
    home: PathBuf,
) -> OpenAiModelsManager {
    let seed = readiness_catalogs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|entry| {
            entry.provider_id == provider_id
                && entry.info == info
                && entry.home == home
                && Instant::now() < entry.expires
        })
        .map(|entry| {
            (
                entry.ids.clone(),
                entry.expires.saturating_duration_since(Instant::now()),
            )
        });
    let id = provider_id.to_owned();
    let identity_info = info.clone();
    let components = OnceLock::new();
    let identity: ModelsCacheIdentity = Arc::new(move || {
        components
            .get_or_init(|| crate::provider::ModelProviderCacheIdentity::new(&id, &identity_info))
            .for_auth_identity(&identity_info, crate::auth::provider_cache_auth_identity(None))
    });
    let manager = OpenAiModelsManager::new(
        home,
        Arc::new(LocalModelsEndpoint {
            info,
            pool: Mutex::new(None),
        }),
        None,
        identity,
    );
    if let Some((ids, remaining)) = seed {
        manager.with_discovered_catalog(descriptors(ids), remaining)
    } else {
        manager
    }
}

/// Hand readiness discovery to catalog consumers, bounded to eight endpoint scopes.
pub async fn store_discovered_local_models(
    provider_id: &str,
    info: &ModelProviderInfo,
    home: PathBuf,
    ids: Vec<String>,
) {
    if !matches!(info.kind, ProviderKind::Ollama | ProviderKind::LmStudio) {
        return;
    }
    let mut entries = readiness_catalogs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|entry| {
        Instant::now() < entry.expires
            && !(entry.provider_id == provider_id && entry.info == *info && entry.home == home)
    });
    if entries.len() == 8 {
        entries.remove(0);
    }
    entries.push(ReadinessCatalog {
        provider_id: provider_id.into(),
        info: info.clone(),
        home,
        ids,
        expires: Instant::now() + Duration::from_secs(300),
    });
}
