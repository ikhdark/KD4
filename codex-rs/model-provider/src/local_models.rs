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
                let client = self
                    .pool_for(factory)
                    .client_for_url(&url)
                    .await
                    .map_err(std::io::Error::other)?;
                let response = client
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
    let identity: ModelsCacheIdentity =
        Arc::new(move || crate::provider::model_provider_cache_identity(&id, &identity_info, None));
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
#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::OutboundProxyPolicy;
    use codex_model_provider_info::built_in_model_providers;
    use codex_models_manager::manager::ModelsManager;
    use codex_models_manager::manager::RefreshStrategy;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn discovered_membership_reaches_consumers_and_readiness_is_reused() {
        for (id, api_path, body) in [
            (
                "ollama",
                "/prefix/api/tags",
                json!({"models":[{"name":"qwen3:8b"}]}),
            ),
            (
                "lmstudio",
                "/prefix/v1/models",
                json!({"data":[{"id":"qwen3:8b"}]}),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path(api_path))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            let mut info = built_in_model_providers(None).remove(id).unwrap();
            info.base_url = Some(format!("{}/prefix/v1", server.uri()));
            let home = tempfile::tempdir().unwrap();
            let manager = manager(id, info.clone(), home.path().to_path_buf());
            let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
            let models = manager
                .list_models(RefreshStrategy::Online, factory.clone())
                .await
                .unwrap();
            assert_eq!(
                models
                    .iter()
                    .map(|model| model.model.as_str())
                    .collect::<Vec<_>>(),
                ["qwen3:8b"]
            );
            assert!(models[0].supported_reasoning_efforts.is_empty());
            assert!(models[0].description.contains("unknown"));
            store_discovered_local_models(
                id,
                &info,
                home.path().to_path_buf(),
                vec!["readiness-model".into()],
            )
            .await;
            let another = super::manager(id, info, home.path().to_path_buf());
            let models = another
                .list_models(RefreshStrategy::OnlineIfUncached, factory.clone())
                .await
                .unwrap();
            assert_eq!(models[0].model, "readiness-model");
            assert_eq!(
                another
                    .get_default_model(
                        &Some("removed-active-model".into()),
                        false,
                        RefreshStrategy::Offline,
                        factory
                    )
                    .await
                    .unwrap(),
                "removed-active-model"
            );
        }
    }

    #[tokio::test]
    async fn display_names_do_not_change_custom_routing_or_registered_capabilities() {
        for name in ["OpenAI", "Amazon Bedrock", "Amazon Bedrock Runtime"] {
            let info = ModelProviderInfo {
                name: name.into(),
                base_url: Some("http://127.0.0.1:9999/v1".into()),
                experimental_bearer_token: Some("proxy-token".into()),
                ..Default::default()
            };
            let provider = crate::create_model_provider(info.clone(), None);
            assert!(!provider.info().is_amazon_bedrock());
            assert!(!provider.info().is_openai());
            assert_eq!(provider.runtime_base_url().await.unwrap(), info.base_url);
        }
        for (id, mut info) in built_in_model_providers(None) {
            let expected = crate::create_model_provider(info.clone(), None).capabilities();
            info.name = "renamed".into();
            let actual = crate::create_model_provider(info, None).capabilities();
            assert_eq!(actual, expected, "{id}");
            if matches!(id.as_str(), "ollama" | "lmstudio") {
                assert!(!actual.namespace_tools && !actual.web_search && !actual.image_generation);
            }
        }
    }
}
