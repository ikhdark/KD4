use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

pub mod accessible;
mod app_info;
mod app_tool_policy;
mod directory_cache;
pub mod filter;
pub mod merge;
pub mod metadata;
mod plugin_config;
mod runtime_projection;
mod snapshot;

pub use app_info::AppBranding;
pub use app_info::AppInfo;
pub use app_info::AppMetadata;
pub use app_info::AppReview;
pub use app_info::AppScreenshot;
pub use app_tool_policy::AppToolPolicy;
pub use app_tool_policy::AppToolPolicyEvaluator;
pub use app_tool_policy::AppToolPolicyInput;
pub use app_tool_policy::app_is_enabled;
pub use app_tool_policy::apps_config_from_layer_stack;
pub use directory_cache::ConnectorDirectoryCacheContext;
pub use plugin_config::parse_plugin_app_config;
pub use plugin_config::parse_plugin_app_config_value;
pub use runtime_projection::ConnectorRuntimeTool;
pub use runtime_projection::InstalledConnectorRuntime;
pub use runtime_projection::connector_tool_is_synthetic;
pub use runtime_projection::installed_connector_runtime;
pub use snapshot::ConnectorSnapshot;
pub use snapshot::PluginConnectorSource;

/// Connector identity is case-sensitive; whitespace is never part of an ID.
pub fn canonical_connector_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty()).then_some(id)
}

pub const CONNECTORS_CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectorDirectoryCacheKey {
    chatgpt_base_url: String,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

impl ConnectorDirectoryCacheKey {
    pub fn new(
        chatgpt_base_url: String,
        account_id: Option<String>,
        chatgpt_user_id: Option<String>,
        is_workspace_account: bool,
    ) -> Self {
        Self {
            chatgpt_base_url,
            account_id,
            chatgpt_user_id,
            is_workspace_account,
        }
    }
}

#[derive(Clone)]
struct CachedConnectorDirectory {
    expires_at: Instant,
    connectors: Vec<AppInfo>,
}

const MAX_DIRECTORY_CACHE_SCOPES: usize = 16;
static CONNECTOR_DIRECTORY_CACHE: LazyLock<
    StdMutex<HashMap<ConnectorDirectoryCacheKey, CachedConnectorDirectory>>,
> = LazyLock::new(|| StdMutex::new(HashMap::new()));

fn insert_directory_cache(
    cache: &mut HashMap<ConnectorDirectoryCacheKey, CachedConnectorDirectory>,
    key: ConnectorDirectoryCacheKey,
    entry: CachedConnectorDirectory,
) {
    if !cache.contains_key(&key)
        && cache.len() >= MAX_DIRECTORY_CACHE_SCOPES
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.expires_at)
            .map(|(key, _)| key.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, entry);
}

type DirectoryRefresh = tokio::sync::Mutex<Option<Result<Vec<AppInfo>, String>>>;

static DIRECTORY_REFRESH_LOCKS: LazyLock<
    StdMutex<HashMap<ConnectorDirectoryCacheKey, Weak<DirectoryRefresh>>>,
> = LazyLock::new(|| StdMutex::new(HashMap::new()));

fn directory_refresh_lock(cache_key: &ConnectorDirectoryCacheKey) -> Arc<DirectoryRefresh> {
    let mut locks = DIRECTORY_REFRESH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(cache_key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(tokio::sync::Mutex::new(None));
    locks.insert(cache_key.clone(), Arc::downgrade(&lock));
    lock
}

#[derive(Debug, Deserialize)]
pub struct DirectoryListResponse {
    apps: Vec<DirectoryApp>,
    #[serde(alias = "nextToken")]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DirectoryApp {
    id: String,
    name: String,
    description: Option<String>,
    #[serde(alias = "appMetadata")]
    app_metadata: Option<AppMetadata>,
    branding: Option<AppBranding>,
    labels: Option<HashMap<String, String>>,
    #[serde(alias = "logoUrl")]
    logo_url: Option<String>,
    #[serde(alias = "logoUrlDark")]
    logo_url_dark: Option<String>,
    #[serde(alias = "iconAssets")]
    icon_assets: Option<HashMap<String, String>>,
    #[serde(alias = "iconDarkAssets")]
    icon_dark_assets: Option<HashMap<String, String>>,
    #[serde(alias = "distributionChannel")]
    distribution_channel: Option<String>,
    visibility: Option<String>,
}

pub fn cached_directory_connectors(
    cache_context: &ConnectorDirectoryCacheContext,
) -> Option<Vec<AppInfo>> {
    if let Some(cached_connectors) = cached_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Some(cached_connectors);
    }

    let directory_cache::CachedConnectorDirectoryDiskLoad::Hit { connectors } =
        directory_cache::load_cached_directory_connectors_from_disk(cache_context)
    else {
        return None;
    };
    Some(promote_disk_directory_connectors(
        &cache_context.cache_key,
        connectors,
    ))
}

fn promote_disk_directory_connectors(
    cache_key: &ConnectorDirectoryCacheKey,
    connectors: Vec<AppInfo>,
) -> Vec<AppInfo> {
    let entry = CachedConnectorDirectory {
        expires_at: Instant::now(),
        connectors: connectors.clone(),
    };
    let mut cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache_guard.get(cache_key) {
        return cached.connectors.clone();
    }
    insert_directory_cache(&mut cache_guard, cache_key.clone(), entry);
    connectors
}

fn cached_directory_connectors_in_memory(
    cache_key: &ConnectorDirectoryCacheKey,
) -> Option<Vec<AppInfo>> {
    let cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache_guard
        .get(cache_key)
        .map(|cached| cached.connectors.clone())
}

fn unexpired_directory_connectors_in_memory(
    cache_key: &ConnectorDirectoryCacheKey,
) -> Option<Vec<AppInfo>> {
    let cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = cache_guard.get(cache_key)?;
    if Instant::now() < cached.expires_at {
        return Some(cached.connectors.clone());
    }
    None
}

/// Lists and caches the directory for the scope in `cache_context`.
/// `fetch_page` must use the backend and authenticated identity represented by its key.
pub async fn list_all_connectors_with_options<F, Fut>(
    cache_context: ConnectorDirectoryCacheContext,
    force_refetch: bool,
    mut fetch_page: F,
) -> anyhow::Result<Vec<AppInfo>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<DirectoryListResponse>>,
{
    if !force_refetch
        && let Some(cached_connectors) =
            unexpired_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Ok(cached_connectors);
    }

    // Serialize refreshes for this identity through memory and disk publication.
    let mut refresh_guard = directory_refresh_lock(&cache_context.cache_key)
        .lock_owned()
        .await;
    if !force_refetch
        && let Some(cached_connectors) =
            unexpired_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Ok(cached_connectors);
    }

    // Share only this active cohort's failure. Once its last waiter drops,
    // the weak registry entry cannot suppress a later independent retry.
    if !force_refetch && let Some(result) = refresh_guard.as_ref() {
        return result.clone().map_err(anyhow::Error::msg);
    }
    let apps: anyhow::Result<_> = async {
        if cache_context.cache_key.is_workspace_account {
            let workspace_page =
                fetch_page("/connectors/directory/list_workspace?external_logos=true".to_string());
            // Share only synchronous future construction, never a lock across await.
            // Both pagination chains run in this task and are dropped together on error
            // or cancellation. The mutex preserves Send for mutable fetch callbacks.
            let fetch_page = StdMutex::new(&mut fetch_page);
            let fetch_page = |path| {
                fetch_page
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)(path)
            };
            let workspace_listing = async {
                list_directory_pages(
                    &mut &fetch_page,
                    "/connectors/directory/list_workspace",
                    workspace_page.await?,
                )
                .await
            };
            let mut public_fetch = &fetch_page;
            let (mut apps, workspace_apps) = tokio::try_join!(
                list_directory_connectors(&mut public_fetch),
                workspace_listing
            )?;
            // Keep public-before-workspace precedence regardless of completion order.
            apps.extend(workspace_apps);
            Ok(apps)
        } else {
            list_directory_connectors(&mut fetch_page).await
        }
    }
    .await;
    let apps = match apps {
        Ok(apps) => apps,
        Err(error) => {
            *refresh_guard = Some(Err(format!("{error:#}")));
            return Err(error);
        }
    };

    let mut connectors = merge_directory_apps(apps)
        .into_iter()
        .map(directory_app_to_app_info)
        .collect::<Vec<_>>();
    for connector in &mut connectors {
        let install_url = match connector.install_url.take() {
            Some(install_url) => install_url,
            None => connector_install_url(&connector.name, &connector.id),
        };
        connector.name = normalize_connector_name(&connector.name, &connector.id);
        connector.description = normalize_connector_value(connector.description.as_deref());
        connector.install_url = Some(install_url);
        connector.is_accessible = false;
    }
    connectors.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    tokio::task::spawn_blocking(move || {
        // A cancelled caller must not release refresh ownership while this worker writes.
        write_cached_directory_connectors(&cache_context, &connectors);
        *refresh_guard = Some(Ok(connectors.clone()));
        connectors
    })
    .await
    .map_err(anyhow::Error::from)
}

fn write_cached_directory_connectors(
    cache_context: &ConnectorDirectoryCacheContext,
    connectors: &[AppInfo],
) {
    write_cached_directory_connectors_in_memory(
        cache_context.cache_key.clone(),
        connectors,
        CONNECTORS_CACHE_TTL,
    );
    directory_cache::write_cached_directory_connectors_to_disk(cache_context, connectors);
}

fn write_cached_directory_connectors_in_memory(
    cache_key: ConnectorDirectoryCacheKey,
    connectors: &[AppInfo],
    ttl: Duration,
) {
    let entry = CachedConnectorDirectory {
        expires_at: Instant::now() + ttl,
        connectors: connectors.to_vec(),
    };
    let mut cache_guard = CONNECTOR_DIRECTORY_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    insert_directory_cache(&mut cache_guard, cache_key, entry);
}

async fn list_directory_connectors<F, Fut>(fetch_page: &mut F) -> anyhow::Result<Vec<DirectoryApp>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<DirectoryListResponse>>,
{
    let response = fetch_page("/connectors/directory/list?external_logos=true".to_string()).await?;
    list_directory_pages(fetch_page, "/connectors/directory/list", response).await
}

async fn list_directory_pages<F, Fut>(
    fetch_page: &mut F,
    directory_path: &str,
    mut response: DirectoryListResponse,
) -> anyhow::Result<Vec<DirectoryApp>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = anyhow::Result<DirectoryListResponse>>,
{
    let mut apps = Vec::new();
    let mut seen_tokens = HashSet::new();
    loop {
        apps.extend(
            response
                .apps
                .into_iter()
                .filter(|app| !is_hidden_directory_app(app)),
        );
        let next_token = response
            .next_token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty());
        let Some(token) = next_token.as_ref() else {
            break;
        };
        anyhow::ensure!(
            seen_tokens.insert(token.clone()),
            "connector directory returned a repeated pagination token"
        );
        let encoded_token = urlencoding::encode(token);
        response = fetch_page(format!(
            "{directory_path}?token={encoded_token}&external_logos=true"
        ))
        .await?;
    }
    Ok(apps)
}

fn merge_directory_apps(apps: Vec<DirectoryApp>) -> Vec<DirectoryApp> {
    let mut merged: HashMap<String, DirectoryApp> = HashMap::new();
    for mut app in apps {
        let Some(id) = normalize_connector_value(Some(&app.id)) else {
            continue;
        };
        app.id = id;
        if let Some(existing) = merged.get_mut(&app.id) {
            merge_directory_app(existing, app);
        } else {
            merged.insert(app.id.clone(), app);
        }
    }
    merged.into_values().collect()
}

fn merge_directory_app(existing: &mut DirectoryApp, incoming: DirectoryApp) {
    let DirectoryApp {
        id: _,
        name,
        description,
        app_metadata,
        branding,
        labels,
        logo_url,
        logo_url_dark,
        icon_assets,
        icon_dark_assets,
        distribution_channel,
        visibility: _,
    } = incoming;

    let incoming_name_is_empty = name.trim().is_empty();
    if existing.name.trim().is_empty() && !incoming_name_is_empty {
        existing.name = name;
    }

    let incoming_description_present = description
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if incoming_description_present {
        existing.description = description;
    }

    if existing.logo_url.is_none() && logo_url.is_some() {
        existing.logo_url = logo_url;
    }
    if existing.logo_url_dark.is_none() && logo_url_dark.is_some() {
        existing.logo_url_dark = logo_url_dark;
    }
    if existing.icon_assets.as_ref().is_none_or(HashMap::is_empty)
        && icon_assets
            .as_ref()
            .is_some_and(|assets| !assets.is_empty())
    {
        existing.icon_assets = icon_assets;
    }
    if existing
        .icon_dark_assets
        .as_ref()
        .is_none_or(HashMap::is_empty)
        && icon_dark_assets
            .as_ref()
            .is_some_and(|assets| !assets.is_empty())
    {
        existing.icon_dark_assets = icon_dark_assets;
    }
    if existing.distribution_channel.is_none() && distribution_channel.is_some() {
        existing.distribution_channel = distribution_channel;
    }

    if let Some(incoming_branding) = branding {
        if let Some(existing_branding) = existing.branding.as_mut() {
            if existing_branding.category.is_none() && incoming_branding.category.is_some() {
                existing_branding.category = incoming_branding.category;
            }
            if existing_branding.developer.is_none() && incoming_branding.developer.is_some() {
                existing_branding.developer = incoming_branding.developer;
            }
            if existing_branding.website.is_none() && incoming_branding.website.is_some() {
                existing_branding.website = incoming_branding.website;
            }
            if existing_branding.privacy_policy.is_none()
                && incoming_branding.privacy_policy.is_some()
            {
                existing_branding.privacy_policy = incoming_branding.privacy_policy;
            }
            if existing_branding.terms_of_service.is_none()
                && incoming_branding.terms_of_service.is_some()
            {
                existing_branding.terms_of_service = incoming_branding.terms_of_service;
            }
            if !existing_branding.is_discoverable_app && incoming_branding.is_discoverable_app {
                existing_branding.is_discoverable_app = true;
            }
        } else {
            existing.branding = Some(incoming_branding);
        }
    }

    if let Some(incoming_app_metadata) = app_metadata {
        if let Some(existing_app_metadata) = existing.app_metadata.as_mut() {
            if existing_app_metadata.review.is_none() && incoming_app_metadata.review.is_some() {
                existing_app_metadata.review = incoming_app_metadata.review;
            }
            if existing_app_metadata.categories.is_none()
                && incoming_app_metadata.categories.is_some()
            {
                existing_app_metadata.categories = incoming_app_metadata.categories;
            }
            if existing_app_metadata.sub_categories.is_none()
                && incoming_app_metadata.sub_categories.is_some()
            {
                existing_app_metadata.sub_categories = incoming_app_metadata.sub_categories;
            }
            if existing_app_metadata.seo_description.is_none()
                && incoming_app_metadata.seo_description.is_some()
            {
                existing_app_metadata.seo_description = incoming_app_metadata.seo_description;
            }
            if existing_app_metadata.screenshots.is_none()
                && incoming_app_metadata.screenshots.is_some()
            {
                existing_app_metadata.screenshots = incoming_app_metadata.screenshots;
            }
            if existing_app_metadata.developer.is_none()
                && incoming_app_metadata.developer.is_some()
            {
                existing_app_metadata.developer = incoming_app_metadata.developer;
            }
            if existing_app_metadata.version.is_none() && incoming_app_metadata.version.is_some() {
                existing_app_metadata.version = incoming_app_metadata.version;
            }
            if existing_app_metadata.version_id.is_none()
                && incoming_app_metadata.version_id.is_some()
            {
                existing_app_metadata.version_id = incoming_app_metadata.version_id;
            }
            if existing_app_metadata.version_notes.is_none()
                && incoming_app_metadata.version_notes.is_some()
            {
                existing_app_metadata.version_notes = incoming_app_metadata.version_notes;
            }
            if existing_app_metadata.first_party_type.is_none()
                && incoming_app_metadata.first_party_type.is_some()
            {
                existing_app_metadata.first_party_type = incoming_app_metadata.first_party_type;
            }
            if existing_app_metadata.first_party_requires_install.is_none()
                && incoming_app_metadata.first_party_requires_install.is_some()
            {
                existing_app_metadata.first_party_requires_install =
                    incoming_app_metadata.first_party_requires_install;
            }
            if existing_app_metadata
                .show_in_composer_when_unlinked
                .is_none()
                && incoming_app_metadata
                    .show_in_composer_when_unlinked
                    .is_some()
            {
                existing_app_metadata.show_in_composer_when_unlinked =
                    incoming_app_metadata.show_in_composer_when_unlinked;
            }
        } else {
            existing.app_metadata = Some(incoming_app_metadata);
        }
    }

    if existing.labels.is_none() && labels.is_some() {
        existing.labels = labels;
    }
}

fn is_hidden_directory_app(app: &DirectoryApp) -> bool {
    matches!(app.visibility.as_deref(), Some("HIDDEN"))
}

fn directory_app_to_app_info(app: DirectoryApp) -> AppInfo {
    AppInfo {
        id: app.id,
        name: app.name,
        description: app.description,
        logo_url: app.logo_url,
        logo_url_dark: app.logo_url_dark,
        icon_assets: app.icon_assets,
        icon_dark_assets: app.icon_dark_assets,
        distribution_channel: app.distribution_channel,
        branding: app.branding,
        app_metadata: app.app_metadata,
        labels: app.labels,
        install_url: None,
        is_accessible: false,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

fn connector_install_url(name: &str, connector_id: &str) -> String {
    let slug = connector_name_slug(name);
    let connector_id = urlencoding::encode(connector_id);
    format!("https://chatgpt.com/apps/{slug}/{connector_id}")
}

fn connector_name_slug(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            normalized.push(character.to_ascii_lowercase());
        } else {
            normalized.push('-');
        }
    }
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "app".to_string()
    } else {
        normalized.to_string()
    }
}

fn normalize_connector_name(name: &str, connector_id: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        connector_id.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_connector_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
