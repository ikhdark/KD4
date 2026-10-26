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

    #[error("failed to read remote featured plugin response from {url}: {source}")]
    Body {
        url: String,
        #[source]
        source: codex_http_client::HttpError,
    },

    #[error("remote featured plugin response from {url} exceeds {max_bytes} bytes")]
    TooLarge { url: String, max_bytes: usize },

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
    let body =
        crate::remote::read_plugin_http_response(response, crate::remote::MAX_PLUGIN_JSON_BYTES)
            .await
            .map_err(|error| match error {
                crate::remote::PluginHttpResponseError::Status { status, body } => {
                    RemotePluginFetchError::UnexpectedStatus {
                        url: url.clone(),
                        status,
                        body,
                    }
                }
                crate::remote::PluginHttpResponseError::Body(source) => {
                    RemotePluginFetchError::Body {
                        url: url.clone(),
                        source,
                    }
                }
                crate::remote::PluginHttpResponseError::TooLarge { max_bytes } => {
                    RemotePluginFetchError::TooLarge {
                        url: url.clone(),
                        max_bytes,
                    }
                }
            })?;

    serde_json::from_slice(&body).map_err(|source| RemotePluginFetchError::Decode {
        url: url.clone(),
        source,
    })
}
