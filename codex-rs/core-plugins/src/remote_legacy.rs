use crate::remote::RemotePluginServiceConfig;
use codex_http_client::RouteAwareRequestError;
use codex_login::CodexAuth;
use codex_protocol::protocol::Product;
use std::time::Duration;

const REMOTE_FEATURED_PLUGIN_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum RemotePluginFetchError {
    #[error("failed to build remote featured plugin HTTP client: {0}")]
    HttpClient(#[from] codex_login::BuildLoginHttpClientError),

    #[error("failed to send remote featured plugin request to {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: RouteAwareRequestError,
    },

    #[error("remote featured plugin request to {url} failed with status {status}: {body}")]
    UnexpectedStatus {
        url: String,
        status: http::StatusCode,
        body: String,
    },

    #[error("failed to parse remote featured plugin response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
}

pub async fn fetch_remote_featured_plugin_ids(
    config: &RemotePluginServiceConfig,
    auth: Option<&CodexAuth>,
    product: Option<Product>,
) -> Result<Vec<String>, RemotePluginFetchError> {
    let url = format!(
        "{}/plugins/featured",
        config.chatgpt_base_url.trim_end_matches('/')
    );
    let mut request = config
        .http_clients()
        .get(&url)
        .query(&[(
            "platform",
            product.unwrap_or(Product::Codex).to_app_platform(),
        )])
        .timeout(REMOTE_FEATURED_PLUGIN_FETCH_TIMEOUT);

    if let Some(auth) = auth.filter(|auth| auth.uses_codex_backend()) {
        request =
            request.headers(codex_model_provider::auth_provider_from_auth(auth).to_auth_headers());
    }

    let response = request
        .send()
        .await
        .map_err(|source| RemotePluginFetchError::Request {
            url: url.clone(),
            source,
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(RemotePluginFetchError::UnexpectedStatus { url, status, body });
    }

    serde_json::from_str(&body).map_err(|source| RemotePluginFetchError::Decode {
        url: url.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_http_client::cache_system_proxy_route_for_test;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    #[tokio::test]
    async fn featured_plugin_fetch_uses_configured_proxy_route() {
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"["plugin-a"]"#))
            .mount(&proxy)
            .await;
        let base_url = "http://featured-plugins.test";
        let request_url = format!(
            "{base_url}/plugins/featured?platform={}",
            Product::Codex.to_app_platform()
        );
        cache_system_proxy_route_for_test(&request_url, proxy.uri());
        let config = RemotePluginServiceConfig::new(
            base_url.to_string(),
            HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
        );

        let featured = fetch_remote_featured_plugin_ids(&config, None, None)
            .await
            .expect("featured plugins should use the configured proxy route");

        assert_eq!(featured, vec!["plugin-a"]);
    }
}
