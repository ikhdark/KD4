use codex_core::config::Config;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_login::CodexAuth;
use codex_login::default_client::create_client_pool;

use anyhow::Context;
use serde::de::DeserializeOwned;
use std::time::Duration;

const OAI_PRODUCT_SKU_HEADER: &str = "OAI-Product-Sku";
const CODEX_PRODUCT_SKU: &str = "codex";

/// Make a GET request to the ChatGPT backend API.
pub(crate) async fn chatgpt_get_request<T: DeserializeOwned>(
    chatgpt_base_url: &str,
    auth: &CodexAuth,
    http_clients: &RouteAwareClientPool,
    path: String,
) -> anyhow::Result<T> {
    chatgpt_get_request_with_timeout(
        chatgpt_base_url,
        auth,
        http_clients,
        path,
        /*timeout*/ None,
    )
    .await
}

pub(crate) async fn chatgpt_get_request_with_timeout<T: DeserializeOwned>(
    chatgpt_base_url: &str,
    auth: &CodexAuth,
    http_clients: &RouteAwareClientPool,
    path: String,
    timeout: Option<Duration>,
) -> anyhow::Result<T> {
    anyhow::ensure!(
        auth.uses_codex_backend(),
        "ChatGPT backend requests require Codex backend auth"
    );
    anyhow::ensure!(
        auth.get_account_id().is_some(),
        "ChatGPT account ID not available, please re-run `codex login`"
    );

    let url = format!("{}/{}", chatgpt_base_url, path.trim_start_matches('/'));
    let mut request = http_clients
        .get(&url)
        .headers(codex_model_provider::auth_provider_from_auth(auth).to_auth_headers())
        .header(OAI_PRODUCT_SKU_HEADER, CODEX_PRODUCT_SKU)
        .header("Content-Type", "application/json");

    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }

    let response = request.send().await.context("Failed to send request")?;

    if response.status().is_success() {
        let result: T = response
            .json()
            .await
            .context("Failed to parse JSON response")?;
        Ok(result)
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Request failed with status {status}: {body}")
    }
}

pub(crate) fn chatgpt_http_clients(config: &Config) -> RouteAwareClientPool {
    create_client_pool(config.http_client_factory(), ClientRouteClass::Api)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::config::ConfigBuilder;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_http_client::cache_system_proxy_route_for_test;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    #[tokio::test]
    async fn chatgpt_requests_retain_the_effective_proxy_policy() {
        let codex_home = tempfile::tempdir().expect("temporary Codex home");
        std::fs::write(
            codex_home.path().join("config.toml"),
            "[features]\nrespect_system_proxy = true\n",
        )
        .expect("write config");
        let config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load config");

        assert_eq!(
            chatgpt_http_clients(&config).outbound_proxy_policy(),
            OutboundProxyPolicy::RespectSystemProxy
        );
    }

    #[tokio::test]
    async fn caller_owned_pool_routes_repeated_chatgpt_gets_through_configured_proxy() {
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&proxy)
            .await;
        let base_url = "http://chatgpt-get-helper.test";
        let request_url = format!("{base_url}/backend-api/test");
        cache_system_proxy_route_for_test(&request_url, proxy.uri());
        let http_clients = create_client_pool(
            HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
            ClientRouteClass::Api,
        );
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();

        for _ in 0..2 {
            let response: serde_json::Value = chatgpt_get_request(
                base_url,
                &auth,
                &http_clients,
                "/backend-api/test".to_string(),
            )
            .await
            .expect("ChatGPT GET should use the configured proxy route");
            assert_eq!(response, serde_json::json!({"ok": true}));
        }

        assert_eq!(
            proxy
                .received_requests()
                .await
                .expect("proxy requests")
                .len(),
            2
        );
    }
}
