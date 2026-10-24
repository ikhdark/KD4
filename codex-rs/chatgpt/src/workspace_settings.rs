use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;
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
const MAX_ACTIVE_REFRESH_KEYS: usize = 128;

#[derive(Debug, Deserialize)]
struct WorkspaceSettingsResponse {
    #[serde(default)]
    beta_settings: HashMap<String, bool>,
}

#[derive(Debug, Default)]
pub struct WorkspaceSettingsCache {
    entry: RwLock<Option<CachedWorkspaceSettings>>,
    /// Only active callers own refresh outcomes. A different key cannot erase
    /// the outcome shared by existing waiters; later calls can retry failures.
    refreshes: Mutex<HashMap<WorkspaceSettingsCacheKey, Weak<WorkspaceRefresh>>>,
}

type WorkspaceRefresh = tokio::sync::Mutex<Option<Result<bool, String>>>;

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
    fn active_refresh(&self, key: &WorkspaceSettingsCacheKey) -> Option<Arc<WorkspaceRefresh>> {
        let mut refreshes = self.refreshes.lock().unwrap_or_else(|error| error.into_inner());
        refreshes.retain(|_, refresh| refresh.strong_count() != 0);
        if let Some(refresh) = refreshes.get(key).and_then(Weak::upgrade) {
            return Some(refresh);
        }
        if refreshes.len() >= MAX_ACTIVE_REFRESH_KEYS {
            return None;
        }
        let refresh = Arc::new(tokio::sync::Mutex::new(None));
        refreshes.insert(key.clone(), Arc::downgrade(&refresh));
        Some(refresh)
    }

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
    anyhow::ensure!(!matches!(account_id.as_str(), "." | ".."), "invalid workspace account ID");

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

    let Some(shared_refresh) = cache.active_refresh(&cache_key) else {
        return fetch_codex_plugins_enabled(config, auth, &account_id).await;
    };
    let mut refresh = shared_refresh.lock().await;
    if let Some(enabled) = cache.get_codex_plugins_enabled(&cache_key) {
        return Ok(enabled);
    }
    if let Some(result) = refresh.as_ref() {
        return result.clone().map_err(anyhow::Error::msg);
    }

    let result = fetch_codex_plugins_enabled(config, auth, &account_id).await;
    if let Ok(enabled) = &result {
        cache.set_codex_plugins_enabled(cache_key, *enabled);
    }
    *refresh = Some(result.as_ref().map(|value| *value).map_err(|error| format!("{error:#}")));
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
