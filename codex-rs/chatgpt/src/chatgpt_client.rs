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
const CHATGPT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;

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
        Some(CHATGPT_REQUEST_TIMEOUT),
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

    let mut response = request.send().await.context("Failed to send request")?;

    if response.status().is_success() {
        let result: T = response
            .json()
            .await
            .context("Failed to parse JSON response")?;
        Ok(result)
    } else {
        let status = response.status();
        let mut body = Vec::new();
        // Stop reading once the diagnostic prefix is full, even if the server
        // keeps sending data or never finishes its error response.
        while body.len() < MAX_ERROR_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY_BYTES - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                Ok(None) | Err(_) => break,
            }
        }
        let truncated = body.len() == MAX_ERROR_BODY_BYTES;
        let mut body = String::from_utf8_lossy(&body).into_owned();
        if truncated {
            body.push_str(" [truncated]");
        }
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
    async fn error_body_read_stops_at_the_limit_without_waiting_for_eof() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let base_url = format!("http://{}", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.expect("request header"));
            }
            // Advertise another byte, but never send it or close the response.
            let headers = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\n\r\n",
                MAX_ERROR_BODY_BYTES + 1
            );
            stream.write_all(headers.as_bytes()).await.expect("headers");
            stream
                .write_all(&vec![b'x'; MAX_ERROR_BODY_BYTES])
                .await
                .expect("body prefix");
            std::future::pending::<()>().await;
        });
        let pool = create_client_pool(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Api,
        );
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            chatgpt_get_request::<serde_json::Value>(&base_url, &auth, &pool, "/test".to_string()),
        )
        .await;
        server.abort();
        let error = result
            .expect("must not wait for the rest of the error body")
            .expect_err("HTTP 503");
        assert_eq!(
            error.to_string(),
            format!(
                "Request failed with status 503 Service Unavailable: {} [truncated]",
                "x".repeat(MAX_ERROR_BODY_BYTES)
            )
        );
    }

    #[tokio::test]
    async fn short_errors_keep_diagnostics_and_large_successes_are_not_truncated() {
        let server = MockServer::start().await;
        let pool = create_client_pool(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Api,
        );
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("settings unavailable"))
            .expect(1)
            .mount(&server)
            .await;
        let error = chatgpt_get_request::<serde_json::Value>(
            &server.uri(),
            &auth,
            &pool,
            "/test".to_string(),
        )
        .await
        .expect_err("HTTP 503");
        assert_eq!(
            error.to_string(),
            "Request failed with status 503 Service Unavailable: settings unavailable"
        );
        server.verify().await;
        server.reset().await;
        let expected = serde_json::json!({"diff": "x".repeat(MAX_ERROR_BODY_BYTES * 2)});
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&expected))
            .expect(1)
            .mount(&server)
            .await;
        let response: serde_json::Value =
            chatgpt_get_request(&server.uri(), &auth, &pool, "/test".to_string())
                .await
                .expect("large successful response");
        assert_eq!(response, expected);
        server.verify().await;
    }

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
