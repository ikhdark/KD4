#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;
use crate::npm_registry;
use crate::npm_registry::NpmPackageInfo;
use crate::update_action;
use crate::update_action::UpdateAction;
use crate::updates_cache::VersionInfo;
use crate::updates_cache::read_version_info;
use crate::updates_cache::version_filepath;
use chrono::Duration;
use chrono::Utc;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_install_context::is_newer_version;
use codex_install_context::is_source_build_version;
use codex_install_context::version_from_release_tag;
use codex_login::default_client::create_client_pool;
use serde::Deserialize;
use std::path::Path;

use crate::version::CODEX_CLI_VERSION;

pub(crate) use crate::updates_cache::dismiss_version;

pub fn get_upgrade_version(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let action = update_action::get_update_action();
    let version_file = version_filepath(config);
    let info = read_version_info(&version_file).ok();

    if match &info {
        None => true,
        Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
    } {
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        let http_clients = create_client_pool(config.http_client_factory(), ClientRouteClass::Api);
        tokio::spawn(async move {
            check_for_update(&version_file, action, &http_clients)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"))
        });
    }

    info.and_then(|info| {
        if is_newer_version(&info.latest_version, CODEX_CLI_VERSION).unwrap_or(false) {
            Some(info.latest_version)
        } else {
            None
        }
    })
}

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

async fn check_for_update(
    version_file: &Path,
    action: Option<UpdateAction>,
    http_clients: &RouteAwareClientPool,
) -> anyhow::Result<()> {
    let latest_version = match action {
        Some(UpdateAction::NpmGlobalLatest)
        | Some(UpdateAction::BunGlobalLatest)
        | Some(UpdateAction::PnpmGlobalLatest) => {
            let latest_version = fetch_latest_github_release_version(http_clients).await?;
            let package_info = http_clients
                .get(npm_registry::PACKAGE_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<NpmPackageInfo>()
                .await?;
            npm_registry::ensure_version_ready(&package_info, &latest_version)?;
            latest_version
        }
        Some(UpdateAction::StandaloneWindows) | None => {
            fetch_latest_github_release_version(http_clients).await?
        }
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

async fn fetch_latest_github_release_version(
    http_clients: &RouteAwareClientPool,
) -> anyhow::Result<String> {
    fetch_github_release_version(http_clients, LATEST_RELEASE_URL).await
}

async fn fetch_github_release_version(
    http_clients: &RouteAwareClientPool,
    release_url: &str,
) -> anyhow::Result<String> {
    let ReleaseInfo {
        tag_name: latest_tag_name,
    } = http_clients
        .get(release_url)
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;
    version_from_release_tag(&latest_tag_name)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse latest tag name '{latest_tag_name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use codex_http_client::cache_system_proxy_route_for_test;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    #[tokio::test]
    async fn github_update_check_uses_effective_proxy_route() {
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"tag_name":"rust-v9.9.9"}"#),
            )
            .mount(&proxy)
            .await;
        let release_url = "http://tui-update-check.test/releases/latest";
        cache_system_proxy_route_for_test(release_url, proxy.uri());
        let http_clients = create_client_pool(
            HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
            ClientRouteClass::Api,
        );

        let version = fetch_github_release_version(&http_clients, release_url)
            .await
            .expect("update request should use the configured proxy route");

        assert_eq!(version, "9.9.9");
    }
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}
