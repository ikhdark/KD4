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
        #[cfg(test)]
        assert!(
            !CACHE_LOOKUP_RUNTIME_THREAD.get(),
            "connector cache I/O must not run on the async runtime thread"
        );
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
        .filter_map(|connector_id| connectors_by_id.remove(connector_id.0.as_str()))
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
            .map(|connector| connector.id.as_str())
            .collect();
        accessible_connectors
            .into_iter()
            .filter(|connector| connector_ids.contains(connector.id.as_str()))
            .collect()
    } else {
        accessible_connectors
    };
    merge_connectors(connectors, accessible_connectors)
}

#[cfg(test)]
thread_local! {
    // The normal-boundary regression marks its current-thread runtime before lookup.
    static CACHE_LOOKUP_RUNTIME_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_connectors::metadata::connector_install_url;
    use codex_plugin::AppConnectorId;
    use pretty_assertions::assert_eq;

    #[tokio::test(flavor = "current_thread")]
    async fn listing_preserves_disabled_uncached_and_cached_results() {
        use codex_core::config::ConfigBuilder;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::path;

        let home = tempfile::tempdir().expect("Codex home");
        std::fs::write(home.path().join("config.toml"), "[features]\napps = true\n")
            .expect("apps config");
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .build()
            .await
            .expect("config");
        let server = MockServer::start().await;
        config.chatgpt_base_url = server.uri();
        CACHE_LOOKUP_RUNTIME_THREAD.set(true);
        assert_eq!(
            list_cached_all_connectors(&config, &[]).await,
            Some(Vec::new())
        );
        assert_eq!(
            list_all_connectors(&config)
                .await
                .expect("disabled listing"),
            Vec::new()
        );
        codex_login::auth::login_with_chatgpt_auth_tokens(
            home.path(),
            "e30.e30.signature",
            "connector-test-account",
            Some("plus"),
        )
        .expect("auth");
        assert_eq!(list_cached_all_connectors(&config, &[]).await, None);
        Mock::given(path("/connectors/directory/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "apps": [{"id": "alpha", "name": "alpha"}], "next_token": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let expected = vec![merged_app("alpha", false)];
        assert_eq!(
            list_all_connectors(&config)
                .await
                .expect("directory listing"),
            expected
        );
        assert_eq!(
            list_cached_all_connectors(&config, &[]).await,
            Some(expected.clone())
        );
        // Replace the one-entry in-memory cache through its normal refresh API.
        // The original identity can now be restored only from its on-disk cache.
        let other_context = ConnectorDirectoryCacheContext::new(
            home.path().to_path_buf(),
            ConnectorDirectoryCacheKey::new(
                format!("{}/other", config.chatgpt_base_url),
                None,
                None,
                false,
            ),
        );
        let other =
            codex_connectors::list_all_connectors_with_options(other_context, true, |_| async {
                Ok(serde_json::from_value(serde_json::json!({
                    "apps": [], "next_token": null
                }))?)
            })
            .await
            .expect("replace in-memory cache");
        assert!(other.is_empty());
        assert_eq!(
            list_cached_all_connectors(&config, &[]).await,
            Some(expected)
        );
        CACHE_LOOKUP_RUNTIME_THREAD.set(false);
        server.verify().await;
    }

    fn app(id: &str) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            icon_assets: None,
            icon_dark_assets: None,
            distribution_channel: None,
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: None,
            is_accessible: false,
            is_enabled: true,
            plugin_display_names: Vec::new(),
        }
    }

    fn merged_app(id: &str, is_accessible: bool) -> AppInfo {
        AppInfo {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            icon_assets: None,
            icon_dark_assets: None,
            distribution_channel: None,
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: Some(connector_install_url(id, id)),
            is_accessible,
            is_enabled: true,
            plugin_display_names: Vec::new(),
        }
    }

    #[test]
    fn excludes_accessible_connectors_not_in_all_when_all_loaded() {
        let merged = merge_connectors_with_accessible(
            vec![app("alpha")],
            vec![app("alpha"), app("beta")],
            /*all_connectors_loaded*/ true,
        );
        assert_eq!(merged, vec![merged_app("alpha", /*is_accessible*/ true)]);
    }

    #[test]
    fn keeps_accessible_connectors_not_in_all_while_all_loading() {
        let merged = merge_connectors_with_accessible(
            vec![app("alpha")],
            vec![app("alpha"), app("beta")],
            /*all_connectors_loaded*/ false,
        );
        assert_eq!(
            merged,
            vec![
                merged_app("alpha", /*is_accessible*/ true),
                merged_app("beta", /*is_accessible*/ true)
            ]
        );
    }

    #[test]
    fn connectors_for_plugin_apps_returns_only_requested_plugin_apps() {
        let connectors = connectors_for_plugin_apps(
            vec![app("alpha"), app("beta")],
            &[
                AppConnectorId("gmail".to_string()),
                AppConnectorId("alpha".to_string()),
                AppConnectorId("gmail".to_string()),
            ],
        );
        assert_eq!(
            connectors,
            vec![merged_app("gmail", /*is_accessible*/ false), app("alpha")]
        );
    }

    #[test]
    fn connectors_for_plugin_apps_preserves_formerly_disallowed_plugin_apps() {
        let connector_id = "asdk_app_6938a94a61d881918ef32cb999ff937c";
        let connectors =
            connectors_for_plugin_apps(Vec::new(), &[AppConnectorId(connector_id.to_string())]);
        assert_eq!(
            connectors,
            vec![merged_app(connector_id, /*is_accessible*/ false)]
        );
    }
}
