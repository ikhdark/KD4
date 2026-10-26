use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use codex_http_client::HttpClientBuilder;
use codex_protocol::protocol::McpAuthStatus;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use http::header::AUTHORIZATION;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use tracing::debug;

use crate::oauth::StoredOAuthTokenStatus;
use crate::oauth::oauth_token_status;
use crate::oauth_http_client::MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct DiscoveryHttpClient {
    follow_redirects: codex_http_client::HttpClient,
    stop_redirects: codex_http_client::HttpClient,
}

impl DiscoveryHttpClient {
    fn new() -> Result<Self> {
        let builder = HttpClientBuilder::new().timeout(DISCOVERY_TIMEOUT);
        Ok(Self {
            follow_redirects: builder.clone().build_direct()?,
            stop_redirects: builder.without_redirects().build_direct()?,
        })
    }
}

impl HttpClient for DiscoveryHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let client = match params.redirect_policy {
                HttpRedirectPolicy::Follow => &self.follow_redirects,
                HttpRedirectPolicy::Stop => &self.stop_redirects,
            };
            let method = Method::from_bytes(params.method.as_bytes())
                .map_err(|error| ExecServerError::HttpRequest(error.to_string()))?;
            let mut headers = HeaderMap::new();
            for header in params.headers {
                let name = HeaderName::from_bytes(header.name.as_bytes())
                    .map_err(|error| ExecServerError::HttpRequest(error.to_string()))?;
                let value = HeaderValue::from_bytes(header.value.as_bytes())
                    .map_err(|error| ExecServerError::HttpRequest(error.to_string()))?;
                headers.append(name, value);
            }
            let mut request = client.request(method, params.url).headers(headers);
            if let Some(timeout_ms) = params.timeout_ms {
                request = request.timeout(Duration::from_millis(timeout_ms).min(DISCOVERY_TIMEOUT));
            }
            if let Some(body) = params.body {
                request = request.body(body.into_inner());
            }
            let response = request
                .send()
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.to_string()))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some(HttpHeader {
                        name: name.as_str().to_string(),
                        value: value.to_str().ok()?.to_string(),
                    })
                })
                .collect();
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| ExecServerError::HttpRequest(error.to_string()))?;
                if chunk.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES - body.len() {
                    return Err(ExecServerError::HttpRequest(format!(
                        "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(HttpRequestResponse {
                status,
                headers,
                body: body.into(),
            })
        }
        .boxed()
    }

    fn http_request_stream(
        &self,
        _params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async {
            Err(ExecServerError::Protocol(
                "OAuth discovery client only supports buffered requests".to_string(),
            ))
        }
        .boxed()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamableHttpOAuthDiscovery {
    pub scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoginRequirement {
    Login,
    Reauthentication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthState {
    Unsupported,
    LoggedOut(McpLoginRequirement),
    BearerToken,
    OAuth,
}

impl From<McpAuthState> for McpAuthStatus {
    fn from(value: McpAuthState) -> Self {
        match value {
            McpAuthState::Unsupported => Self::Unsupported,
            McpAuthState::LoggedOut(_) => Self::NotLoggedIn,
            McpAuthState::BearerToken => Self::BearerToken,
            McpAuthState::OAuth => Self::OAuth,
        }
    }
}

enum AuthStatusCheck {
    Complete(McpAuthState),
    Discover(HeaderMap),
}

/// Determine the authentication status for a streamable HTTP MCP server.
// Keep this compatibility entrypoint aligned with the HTTP-client variant below.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<McpAuthState> {
    let default_headers = match auth_status_before_discovery(
        codex_home,
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )
    .await?
    {
        AuthStatusCheck::Complete(status) => return Ok(status),
        AuthStatusCheck::Discover(default_headers) => default_headers,
    };

    determine_auth_status_from_discovery(
        server_name,
        url,
        discover_streamable_http_oauth_with_headers(url, &default_headers).await,
    )
}

/// Determine authentication status while routing OAuth discovery through the
/// provided HTTP client.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status_with_http_client(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_client: Arc<dyn HttpClient>,
) -> Result<McpAuthState> {
    let default_headers = match auth_status_before_discovery(
        codex_home,
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )
    .await?
    {
        AuthStatusCheck::Complete(status) => return Ok(status),
        AuthStatusCheck::Discover(default_headers) => default_headers,
    };
    determine_auth_status_from_discovery(
        server_name,
        url,
        discover_streamable_http_oauth_with_headers_and_http_client(
            url,
            default_headers,
            http_client,
        )
        .await,
    )
}

/// Returns locally known auth state without making OAuth discovery requests.
/// `None` means the server's OAuth support still needs network discovery.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status_without_discovery(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<McpAuthState>> {
    Ok(
        match auth_status_before_discovery(
            codex_home,
            server_name,
            url,
            bearer_token_env_var,
            http_headers,
            env_http_headers,
            store_mode,
            keyring_backend_kind,
        )
        .await?
        {
            AuthStatusCheck::Complete(status) => Some(status),
            AuthStatusCheck::Discover(_) => None,
        },
    )
}

// These arguments mirror the persisted MCP configuration fields and are kept
// separate so callers do not need to construct a second public configuration type.
#[allow(clippy::too_many_arguments)]
async fn auth_status_before_discovery(
    codex_home: &Path,
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<AuthStatusCheck> {
    if bearer_token_env_var.is_some() {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    if default_headers.contains_key(AUTHORIZATION) {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    let codex_home = codex_home.to_path_buf();
    let server_name = server_name.to_owned();
    let url = url.to_owned();
    // Credential reads can wait on a cross-process file lock or the OS keyring.
    // Keep those waits off the executor used by both discovery entrypoints.
    let status = tokio::task::spawn_blocking(move || {
        oauth_token_status(
            &codex_home,
            &server_name,
            &url,
            store_mode,
            keyring_backend_kind,
        )
    })
    .await??;
    match status {
        StoredOAuthTokenStatus::Usable => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::OAuth));
        }
        StoredOAuthTokenStatus::AuthorizationRequired => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::LoggedOut(
                McpLoginRequirement::Reauthentication,
            )));
        }
        StoredOAuthTokenStatus::Missing => {}
    }

    Ok(AuthStatusCheck::Discover(default_headers))
}

fn determine_auth_status_from_discovery(
    server_name: &str,
    url: &str,
    discovery: Result<Option<StreamableHttpOAuthDiscovery>>,
) -> Result<McpAuthState> {
    match discovery {
        Ok(Some(_)) => Ok(McpAuthState::LoggedOut(McpLoginRequirement::Login)),
        Ok(None) => Ok(McpAuthState::Unsupported),
        Err(error) => {
            debug!(
                "failed to detect OAuth support for MCP server `{server_name}` at {url}: {error:?}"
            );
            Err(error)
        }
    }
}

/// Attempt to determine whether a streamable HTTP MCP server advertises OAuth login.
pub async fn supports_oauth_login(url: &str) -> Result<bool> {
    Ok(discover_streamable_http_oauth(
        url, /*http_headers*/ None, /*env_http_headers*/ None,
    )
    .await?
    .is_some())
}

pub async fn discover_streamable_http_oauth(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers(url, &default_headers).await
}

pub async fn discover_streamable_http_oauth_with_http_client(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    http_client: Arc<dyn HttpClient>,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers_and_http_client(url, default_headers, http_client)
        .await
}

async fn discover_streamable_http_oauth_with_headers(
    url: &str,
    default_headers: &HeaderMap,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    // Use no_proxy to avoid a bug in the system-configuration crate that
    // can result in a panic. See #8912.
    let default_headers = default_headers.clone();
    let http_client = tokio::task::spawn_blocking(DiscoveryHttpClient::new).await??;
    let mut authorization_manager = AuthorizationManager::new_with_oauth_http_client(
        url,
        Arc::new(
            OAuthHttpClientAdapter::new(Arc::new(http_client), url, default_headers)
                .map_err(anyhow::Error::from_boxed)?
                .with_buffered_responses(),
        ),
    )
    .await?;
    authorization_manager.set_allow_missing_issuer(true);
    discover_streamable_http_oauth_with_manager(&authorization_manager).await
}

async fn discover_streamable_http_oauth_with_headers_and_http_client(
    url: &str,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    // Discovery probes several well-known URLs in turn, and rmcp suggests 30s
    // for each. Apply the direct discovery client's per-probe bound so a server
    // that accepts connections but never answers cannot stall callers for minutes.
    let mut authorization_manager = AuthorizationManager::new_with_oauth_http_client(
        url,
        Arc::new(
            OAuthHttpClientAdapter::new(http_client, url, default_headers)
                .map_err(anyhow::Error::from_boxed)?
                .with_timeout_cap(DISCOVERY_TIMEOUT),
        ),
    )
    .await?;
    authorization_manager.set_allow_missing_issuer(true);
    discover_streamable_http_oauth_with_manager(&authorization_manager).await
}

async fn discover_streamable_http_oauth_with_manager(
    authorization_manager: &AuthorizationManager,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    match authorization_manager.resolve_metadata().boxed().await {
        // rmcp 3 synthesizes legacy endpoints when nothing is published; that is not OAuth support.
        Ok(resolution) if !resolution.source.is_discovered() => Ok(None),
        Ok(resolution) => Ok(Some(StreamableHttpOAuthDiscovery {
            scopes_supported: normalize_scopes(resolution.metadata.scopes_supported),
        })),
        Err(AuthError::NoAuthorizationSupport) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn normalize_scopes(scopes_supported: Option<Vec<String>>) -> Option<Vec<String>> {
    let scopes_supported = scopes_supported?;

    let mut normalized = Vec::new();
    for scope in scopes_supported {
        let scope = scope.trim();
        if scope.is_empty() {
            continue;
        }
        let scope = scope.to_string();
        if !normalized.contains(&scope) {
            normalized.push(scope);
        }
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

