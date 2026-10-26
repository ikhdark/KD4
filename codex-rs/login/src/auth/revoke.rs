//! Best-effort OAuth token revocation used during logout.
//!
//! Managed ChatGPT auth stores OAuth tokens locally. Logout attempts to revoke the
//! refresh token, falling back to the access token when no refresh token is
//! available, and callers still remove local auth if the revoke request fails.

use serde::Serialize;
use std::time::Duration;

use codex_http_client::HttpClient;
use codex_protocol::auth::AuthMode;

use super::manager::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use super::manager::REVOKE_TOKEN_URL;
use super::manager::REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR;
use super::manager::oauth_client_id;
use super::storage::AuthDotJson;
use super::util::try_parse_error_message;
use crate::default_client::create_default_auth_client;
use crate::outbound_proxy::AuthRouteConfig;
use crate::token_data::TokenData;

const REVOKE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevokeTokenKind {
    Access,
    Refresh,
}

impl RevokeTokenKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access_token",
            Self::Refresh => "refresh_token",
        }
    }

}

#[derive(Serialize)]
struct RevokeTokenRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
}

pub(super) async fn revoke_auth_tokens(
    auth_dot_json: Option<&AuthDotJson>,
    auth_route_config: &AuthRouteConfig,
) -> Result<(), std::io::Error> {
    let Some((token, kind)) = auth_dot_json.and_then(revocable_token) else {
        return Ok(());
    };

    let context = auth_dot_json.and_then(|auth| auth.oauth_context.as_ref());
    let endpoint = match context {
        Some(context) => context.endpoint("/oauth/revoke")?,
        None => revoke_token_endpoint(),
    };
    let client_id = match kind {
        RevokeTokenKind::Refresh => Some(
            context
                .map(|context| context.client_id.clone())
                .unwrap_or_else(oauth_client_id),
        ),
        RevokeTokenKind::Access => None,
    };
    let client = create_default_auth_client(&endpoint, auth_route_config).await?;
    revoke_oauth_token(
        &client,
        endpoint.as_str(),
        token,
        kind,
        REVOKE_HTTP_TIMEOUT,
        client_id,
    )
    .await
}

fn revocable_token(auth_dot_json: &AuthDotJson) -> Option<(&str, RevokeTokenKind)> {
    let tokens = managed_chatgpt_tokens(auth_dot_json)?;
    if !tokens.refresh_token.is_empty() {
        Some((tokens.refresh_token.as_str(), RevokeTokenKind::Refresh))
    } else if !tokens.access_token.is_empty() {
        Some((tokens.access_token.as_str(), RevokeTokenKind::Access))
    } else {
        None
    }
}

fn managed_chatgpt_tokens(auth_dot_json: &AuthDotJson) -> Option<&TokenData> {
    if resolved_auth_mode(auth_dot_json) == AuthMode::Chatgpt {
        auth_dot_json.tokens.as_ref()
    } else {
        None
    }
}

fn resolved_auth_mode(auth_dot_json: &AuthDotJson) -> AuthMode {
    if let Some(mode) = auth_dot_json.auth_mode {
        return mode;
    }
    if auth_dot_json.openai_api_key.is_some() {
        return AuthMode::ApiKey;
    }
    AuthMode::Chatgpt
}

async fn revoke_oauth_token(
    client: &HttpClient,
    endpoint: &str,
    token: &str,
    kind: RevokeTokenKind,
    timeout: Duration,
    client_id: Option<String>,
) -> Result<(), std::io::Error> {
    let request = RevokeTokenRequest {
        token,
        token_type_hint: kind.as_str(),
        client_id,
    };

    let response = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .json(&request)
        .send()
        .await
        .map_err(std::io::Error::other)?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let body = response.text().await.unwrap_or_default();
    let message = try_parse_error_message(&body);
    Err(std::io::Error::other(format!(
        "failed to revoke {}: {}: {}",
        kind.as_str(),
        status,
        message
    )))
}

fn revoke_token_endpoint() -> String {
    if let Ok(endpoint) = std::env::var(REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR) {
        return endpoint;
    }

    if let Ok(refresh_endpoint) = std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        && let Some(endpoint) = derive_revoke_token_endpoint(&refresh_endpoint)
    {
        return endpoint;
    }

    REVOKE_TOKEN_URL.to_string()
}

fn derive_revoke_token_endpoint(refresh_endpoint: &str) -> Option<String> {
    let mut url = url::Url::parse(refresh_endpoint).ok()?;
    url.set_path("/oauth/revoke");
    url.set_query(None);
    Some(url.to_string())
}

