use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_core::config::Config;
use codex_login::CodexAuth;
use serde::Deserialize;

use crate::chatgpt_client::chatgpt_get_request_with_timeout;
use crate::chatgpt_client::chatgpt_http_clients;

const WORKSPACE_SETTINGS_TIMEOUT: Duration = Duration::from_secs(10);
const WORKSPACE_SETTINGS_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CODEX_PLUGINS_BETA_SETTING: &str = "enable_plugins";

#[derive(Debug, Deserialize)]
struct WorkspaceSettingsResponse {
    #[serde(default)]
    beta_settings: HashMap<String, bool>,
}

#[derive(Debug, Default)]
pub struct WorkspaceSettingsCache {
    entry: RwLock<Option<CachedWorkspaceSettings>>,
    /// Serializes refreshes and holds the latest attempt's failure, if any.
    refresh: tokio::sync::Mutex<Option<FailedRefresh>>,
    /// Finished refresh attempts, read before waiting on `refresh`.
    refresh_attempts: AtomicU64,
}

/// A failure shared only with callers that were waiting when it happened;
/// later callers fetch again rather than reuse the fail-open result.
#[derive(Debug)]
struct FailedRefresh {
    key: WorkspaceSettingsCacheKey,
    error: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkspaceSettingsCacheKey {
    chatgpt_base_url: String,
    account_id: String,
}

#[derive(Clone, Debug)]
struct CachedWorkspaceSettings {
    key: WorkspaceSettingsCacheKey,
    expires_at: Instant,
    codex_plugins_enabled: bool,
}

impl WorkspaceSettingsCache {
    fn get_codex_plugins_enabled(&self, key: &WorkspaceSettingsCacheKey) -> Option<bool> {
        let entry = match self.entry.read() {
            Ok(entry) => entry,
            Err(err) => err.into_inner(),
        };
        let cached = entry.as_ref()?;
        (cached.key == *key && Instant::now() < cached.expires_at)
            .then_some(cached.codex_plugins_enabled)
    }

    fn set_codex_plugins_enabled(&self, key: WorkspaceSettingsCacheKey, enabled: bool) {
        let mut entry = match self.entry.write() {
            Ok(entry) => entry,
            Err(err) => err.into_inner(),
        };
        *entry = Some(CachedWorkspaceSettings {
            key,
            expires_at: Instant::now() + WORKSPACE_SETTINGS_CACHE_TTL,
            codex_plugins_enabled: enabled,
        });
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "Single-flight refresh: waiters reuse one outcome instead of each paying the timeout"
)]
pub async fn codex_plugins_enabled_for_workspace(
    config: &Config,
    auth: Option<&CodexAuth>,
    cache: Option<&WorkspaceSettingsCache>,
) -> anyhow::Result<bool> {
    let Some(auth) = auth else {
        return Ok(true);
    };
    if !auth.is_chatgpt_auth() {
        return Ok(true);
    }

    if !auth.is_workspace_account() {
        return Ok(true);
    }

    let Some(account_id) = auth.get_account_id().filter(|id| !id.is_empty()) else {
        return Ok(true);
    };

    let cache_key = WorkspaceSettingsCacheKey {
        chatgpt_base_url: config.chatgpt_base_url.clone(),
        account_id: account_id.clone(),
    };
    let Some(cache) = cache else {
        return fetch_codex_plugins_enabled(config, auth, &account_id).await;
    };
    if let Some(enabled) = cache.get_codex_plugins_enabled(&cache_key) {
        return Ok(enabled);
    }

    // Catalog operations can request this setting concurrently. Serialize
    // refreshes and reuse the outcome of one that finished while waiting, so
    // an unavailable backend costs one timeout rather than one per waiter.
    let attempts_before_wait = cache.refresh_attempts.load(Ordering::Acquire);
    let mut refresh = cache.refresh.lock().await;
    if let Some(enabled) = cache.get_codex_plugins_enabled(&cache_key) {
        return Ok(enabled);
    }
    if cache.refresh_attempts.load(Ordering::Acquire) != attempts_before_wait
        && let Some(failed) = refresh.as_ref().filter(|failed| failed.key == cache_key)
    {
        anyhow::bail!("{}", failed.error);
    }

    let result = fetch_codex_plugins_enabled(config, auth, &account_id).await;
    *refresh = match &result {
        Ok(enabled) => {
            cache.set_codex_plugins_enabled(cache_key, *enabled);
            None
        }
        Err(error) => Some(FailedRefresh {
            key: cache_key,
            error: format!("{error:#}"),
        }),
    };
    cache.refresh_attempts.fetch_add(1, Ordering::Release);
    result
}

async fn fetch_codex_plugins_enabled(
    config: &Config,
    auth: &CodexAuth,
    account_id: &str,
) -> anyhow::Result<bool> {
    let encoded_account_id = encode_path_segment(account_id);
    let http_clients = chatgpt_http_clients(config);
    let settings: WorkspaceSettingsResponse = chatgpt_get_request_with_timeout(
        &config.chatgpt_base_url,
        auth,
        &http_clients,
        format!("/accounts/{encoded_account_id}/settings"),
        Some(WORKSPACE_SETTINGS_TIMEOUT),
    )
    .await?;

    Ok(settings
        .beta_settings
        .get(CODEX_PLUGINS_BETA_SETTING)
        .copied()
        .unwrap_or(true))
}

/// Reads the workspace setting while preserving the product's fail-open behavior.
pub async fn codex_plugins_enabled_for_workspace_or_default(
    config: &Config,
    auth: Option<&CodexAuth>,
    cache: Option<&WorkspaceSettingsCache>,
) -> bool {
    workspace_setting_or_default(codex_plugins_enabled_for_workspace(config, auth, cache).await)
}

fn workspace_setting_or_default(result: anyhow::Result<bool>) -> bool {
    match result {
        Ok(enabled) => enabled,
        Err(err) => {
            tracing::warn!(
                "failed to fetch workspace Codex plugins setting; allowing Codex plugins: {err:#}"
            );
            true
        }
    }
}

pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
#[path = "workspace_settings_tests.rs"]
mod tests;
