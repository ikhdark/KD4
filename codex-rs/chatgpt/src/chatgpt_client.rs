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
        let read_limit_reached = body.len() == MAX_ERROR_BODY_BYTES;
        let mut body = String::from_utf8_lossy(&body).into_owned();
        if read_limit_reached {
            body.push_str(" [body read limit reached]");
        }
        anyhow::bail!("Request failed with status {status}: {body}")
    }
}

pub(crate) fn chatgpt_http_clients(config: &Config) -> RouteAwareClientPool {
    create_client_pool(config.http_client_factory(), ClientRouteClass::Api)
}

