use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

use crate::chatgpt_client::chatgpt_get_request_with_timeout;
use crate::chatgpt_client::chatgpt_http_clients;

use codex_connectors::AppInfo;
use codex_connectors::ConnectorDirectoryCacheContext;
use codex_connectors::ConnectorDirectoryCacheKey;
use codex_connectors::DirectoryListResponse;
use codex_connectors::merge::merge_connectors;
use codex_connectors::merge::merge_plugin_connectors;
use codex_core::config::Config;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_environment_manager;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_mcp_manager;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_options;
pub use codex_core::connectors::list_accessible_connectors_from_mcp_tools_with_options_and_status;
pub use codex_core::connectors::list_cached_accessible_connectors_from_mcp_tools;
pub use codex_core::connectors::list_cached_accessible_connectors_from_mcp_tools_with_mcp_manager;
pub use codex_core::connectors::with_app_enabled_state;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_plugin::AppConnectorId;

const DIRECTORY_CONNECTORS_TIMEOUT: Duration = Duration::from_secs(60);

fn apps_enabled(config: &Config, auth: Option<&CodexAuth>) -> bool {
    config
        .features
        .apps_enabled_for_auth(auth.is_some_and(CodexAuth::uses_codex_backend))
}

fn connector_auth(auth: Option<CodexAuth>) -> anyhow::Result<CodexAuth> {
    let auth = auth.ok_or_else(|| anyhow::anyhow!("ChatGPT auth not available"))?;
    anyhow::ensure!(
        auth.uses_codex_backend(),
        "ChatGPT connectors require Codex backend auth"
    );
    Ok(auth)
}

pub async fn list_all_connectors(config: &Config) -> anyhow::Result<Vec<AppInfo>> {
    list_all_connectors_with_options(config, /*force_refetch*/ false, &[]).await
}

pub async fn list_cached_all_connectors(
    config: &Config,
    plugin_apps: &[AppConnectorId],
) -> Option<Vec<AppInfo>> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager.auth().await;
    if !apps_enabled(config, auth.as_ref()) {
        return Some(Vec::new());
    }

    let auth = connector_auth(auth).ok()?;
    let cache_context = connector_directory_cache_context(config, &auth);
    // A cold lookup reads and parses the disk cache before promoting it to memory.
    let connectors = match tokio::task::spawn_blocking(move || {
        codex_connectors::cached_directory_connectors(&cache_context)
    })
    .await
    {
        Ok(connectors) => connectors?,
        Err(error) => {
            tracing::warn!(%error, "connector directory cache lookup task failed");
            return None;
        }
    };
    Some(merge_directory_and_plugin_connectors(
        connectors,
        plugin_apps,
    ))
}

pub async fn list_all_connectors_with_options(
    config: &Config,
    force_refetch: bool,
    plugin_apps: &[AppConnectorId],
) -> anyhow::Result<Vec<AppInfo>> {
    let auth_manager =
        AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ false).await;
    let auth = auth_manager.auth().await;
    if !apps_enabled(config, auth.as_ref()) {
        return Ok(Vec::new());
    }
    let auth = connector_auth(auth)?;
    let http_clients = chatgpt_http_clients(config);
    let chatgpt_base_url = config.chatgpt_base_url.clone();
    let cache_context = connector_directory_cache_context(config, &auth);
    let connectors = codex_connectors::list_all_connectors_with_options(
        cache_context,
        force_refetch,
        move |path| {
            let auth = auth.clone();
            let http_clients = http_clients.clone();
            let chatgpt_base_url = chatgpt_base_url.clone();
            async move {
                chatgpt_get_request_with_timeout::<DirectoryListResponse>(
                    &chatgpt_base_url,
                    &auth,
                    &http_clients,
                    path,
                    Some(DIRECTORY_CONNECTORS_TIMEOUT),
                )
                .await
            }
        },
    )
    .await?;
    Ok(merge_directory_and_plugin_connectors(
        connectors,
        plugin_apps,
    ))
}

fn connector_directory_cache_context(
    config: &Config,
    auth: &CodexAuth,
) -> ConnectorDirectoryCacheContext {
    ConnectorDirectoryCacheContext::new(
        config.codex_home.to_path_buf(),
        ConnectorDirectoryCacheKey::new(
            config.chatgpt_base_url.clone(),
            auth.get_account_id(),
            auth.get_chatgpt_user_id(),
            auth.is_workspace_account(),
        ),
    )
}

fn merge_directory_and_plugin_connectors(
    connectors: Vec<AppInfo>,
    plugin_apps: &[AppConnectorId],
) -> Vec<AppInfo> {
    merge_plugin_connectors(
        connectors,
        plugin_apps
            .iter()
            .map(|connector_id| connector_id.0.clone()),
    )
}

pub fn connectors_for_plugin_apps(
    connectors: Vec<AppInfo>,
    plugin_apps: &[AppConnectorId],
) -> Vec<AppInfo> {
    let connectors = merge_plugin_connectors(
        connectors,
        plugin_apps
            .iter()
            .map(|connector_id| connector_id.0.clone()),
    );
    let mut connectors_by_id = connectors
        .into_iter()
        .map(|connector| (connector.id.clone(), connector))
        .collect::<HashMap<_, _>>();

    plugin_apps
        .iter()
        .filter_map(|connector_id| codex_connectors::canonical_connector_id(&connector_id.0))
        .filter_map(|connector_id| connectors_by_id.remove(connector_id))
        .collect()
}

pub fn merge_connectors_with_accessible(
    connectors: Vec<AppInfo>,
    accessible_connectors: Vec<AppInfo>,
    all_connectors_loaded: bool,
) -> Vec<AppInfo> {
    let accessible_connectors = if all_connectors_loaded {
        let connector_ids: HashSet<&str> = connectors
            .iter()
            .filter_map(|connector| codex_connectors::canonical_connector_id(&connector.id))
            .collect();
        accessible_connectors
            .into_iter()
            .filter(|connector| {
                codex_connectors::canonical_connector_id(&connector.id)
                    .is_some_and(|id| connector_ids.contains(id))
            })
            .collect()
    } else {
        accessible_connectors
    };
    merge_connectors(connectors, accessible_connectors)
}


