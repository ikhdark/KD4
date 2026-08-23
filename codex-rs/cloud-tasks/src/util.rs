use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use reqwest::header::HeaderMap;

use codex_core::config::Config;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManager;
use codex_utils_cli::CliConfigOverrides;
use std::sync::Arc;

const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";

pub struct CloudAuthContext {
    pub auth_manager: Option<Arc<AuthManager>>,
    pub http_client_factory: HttpClientFactory,
    pub chatgpt_base_url: String,
}

pub fn set_user_agent_suffix(suffix: &str) {
    if let Ok(mut guard) = codex_login::default_client::USER_AGENT_SUFFIX.lock() {
        guard.replace(suffix.to_string());
    }
}

pub fn append_error_log(message: impl AsRef<str>) {
    let ts = Utc::now().to_rfc3339();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("error.log")
    {
        use std::io::Write as _;
        let _ = writeln!(f, "[{ts}] {}", message.as_ref());
    }
}

/// Normalize the configured base URL to a canonical form used by the backend client.
/// - trims trailing '/'
/// - appends '/backend-api' for ChatGPT hosts when missing
pub fn normalize_base_url(input: &str) -> String {
    let mut base_url = input.to_string();
    while base_url.ends_with('/') {
        base_url.pop();
    }
    if (base_url.starts_with("https://chatgpt.com")
        || base_url.starts_with("https://chat.openai.com"))
        && !base_url.contains("/backend-api")
    {
        base_url = format!("{base_url}/backend-api");
    }
    base_url
}

pub async fn load_auth_manager(
    config_overrides: &CliConfigOverrides,
    chatgpt_base_url: Option<&str>,
) -> CloudAuthContext {
    let config = match load_config(config_overrides).await {
        Ok(config) => config,
        Err(error) => {
            append_error_log(format!(
                "failed to load auth config; using transport-default proxy handling: {error}"
            ));
            let http_client_factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
            return CloudAuthContext {
                auth_manager: None,
                http_client_factory,
                chatgpt_base_url: normalize_base_url(
                    chatgpt_base_url.unwrap_or(DEFAULT_CHATGPT_BASE_URL),
                ),
            };
        }
    };
    let chatgpt_base_url =
        normalize_base_url(chatgpt_base_url.unwrap_or(config.chatgpt_base_url.as_str()));
    let http_client_factory = config.http_client_factory();
    let auth_manager = AuthManager::new(
        config.codex_home.to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        config.cli_auth_credentials_store_mode,
        config.forced_chatgpt_workspace_id.clone(),
        Some(chatgpt_base_url.clone()),
        config.auth_keyring_backend_kind(),
        config.auth_route_config(),
    )
    .await;
    CloudAuthContext {
        auth_manager: Some(Arc::new(auth_manager)),
        http_client_factory,
        chatgpt_base_url,
    }
}

async fn load_config(config_overrides: &CliConfigOverrides) -> std::io::Result<Config> {
    let overrides = config_overrides
        .parse_overrides()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    Config::load_with_cli_overrides(overrides).await
}

/// Build headers for ChatGPT-backed requests: `User-Agent`, optional `Authorization`,
/// and optional `ChatGPT-Account-Id`.
pub async fn build_chatgpt_headers(auth_manager: Option<&AuthManager>) -> HeaderMap {
    use reqwest::header::HeaderValue;
    use reqwest::header::USER_AGENT;

    set_user_agent_suffix("codex_cloud_tasks_tui");
    let ua = codex_login::default_client::get_codex_user_agent();
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&ua).unwrap_or(HeaderValue::from_static("codex-cli")),
    );
    if let Some(auth_manager) = auth_manager
        && let Some(auth) = auth_manager.auth().await
        && auth.uses_codex_backend()
    {
        headers.extend(codex_model_provider::auth_provider_from_auth(&auth).to_auth_headers());
    }
    headers
}

/// Construct a browser-friendly task URL for the given backend base URL.
pub fn task_url(base_url: &str, task_id: &str) -> String {
    let normalized = normalize_base_url(base_url);
    if let Some(root) = normalized.strip_suffix("/backend-api") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if let Some(root) = normalized.strip_suffix("/api/codex") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if normalized.ends_with("/codex") {
        return format!("{normalized}/tasks/{task_id}");
    }
    format!("{normalized}/codex/tasks/{task_id}")
}

pub fn format_relative_time(reference: DateTime<Utc>, ts: DateTime<Utc>) -> String {
    let mut secs = (reference - ts).num_seconds();
    if secs < 0 {
        secs = 0;
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let local = ts.with_timezone(&Local);
    local.format("%b %e %H:%M").to_string()
}

pub fn format_relative_time_now(ts: DateTime<Utc>) -> String {
    format_relative_time(Utc::now(), ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_context_applies_cloud_cli_base_url_override() {
        let expected = "https://cloud-cli-override.example/backend-api";
        let config_overrides = CliConfigOverrides {
            raw_overrides: vec![
                format!("chatgpt_base_url={expected:?}"),
                "cli_auth_credentials_store=\"file\"".to_string(),
            ],
        };

        let context = load_auth_manager(&config_overrides, None).await;

        assert_eq!(context.chatgpt_base_url, expected);
    }

    #[tokio::test]
    async fn build_chatgpt_headers_uses_provided_auth_manager_snapshot() {
        let auth_manager = AuthManager::from_auth_for_testing(
            codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );

        let headers = build_chatgpt_headers(Some(auth_manager.as_ref())).await;

        assert_eq!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer Access Token")
        );
        assert_eq!(
            headers
                .get("ChatGPT-Account-Id")
                .and_then(|value| value.to_str().ok()),
            Some("account_id")
        );
    }
}
