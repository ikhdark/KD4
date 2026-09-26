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

static DIRECTORY_REFRESH_LOCKS: LazyLock<
    StdMutex<HashMap<ConnectorDirectoryCacheKey, Weak<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(|| StdMutex::new(HashMap::new()));

fn directory_refresh_lock(cache_key: &ConnectorDirectoryCacheKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = DIRECTORY_REFRESH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(cache_key).and_then(Weak::upgrade) {
        return lock;
    }
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
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
    let refresh_guard = directory_refresh_lock(&cache_context.cache_key)
        .lock_owned()
        .await;
    if !force_refetch
        && let Some(cached_connectors) =
            unexpired_directory_connectors_in_memory(&cache_context.cache_key)
    {
        return Ok(cached_connectors);
    }

    let apps = if cache_context.cache_key.is_workspace_account {
        // The workspace page is independent of the paginated public directory.
        // Overlap both request chains; either failure still publishes nothing.
        let workspace_page =
            fetch_page("/connectors/directory/list_workspace?external_logos=true".to_string());
        let (mut apps, workspace_page) =
            tokio::try_join!(list_directory_connectors(&mut fetch_page), workspace_page)?;
        apps.extend(
            workspace_page
                .apps
                .into_iter()
                .filter(|app| !is_hidden_directory_app(app)),
        );
        apps
    } else {
        list_directory_connectors(&mut fetch_page).await?
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
        let _refresh_guard = refresh_guard;
        write_cached_directory_connectors(&cache_context, &connectors);
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
    let mut apps = Vec::new();
    let mut next_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();
    loop {
        let path = match next_token.as_deref() {
            Some(token) => {
                let encoded_token = urlencoding::encode(token);
                format!("/connectors/directory/list?token={encoded_token}&external_logos=true")
            }
            None => "/connectors/directory/list?external_logos=true".to_string(),
        };
        let response = fetch_page(path).await?;
        apps.extend(
            response
                .apps
                .into_iter()
                .filter(|app| !is_hidden_directory_app(app)),
        );
        next_token = response
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
    }
    Ok(apps)
}

fn merge_directory_apps(apps: Vec<DirectoryApp>) -> Vec<DirectoryApp> {
    let mut merged: HashMap<String, DirectoryApp> = HashMap::new();
    for app in apps {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    static CONNECTOR_DIRECTORY_CACHE_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn alternating_scopes_keep_independent_fresh_snapshots_with_bounded_storage() {
        let _guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;
        clear_directory_memory_cache();
        for id in ["account-a", "account-b"] {
            write_cached_directory_connectors_in_memory(
                cache_key(id, false),
                &[],
                CONNECTORS_CACHE_TTL,
            );
        }
        for id in ["account-a", "account-b"] {
            assert_eq!(
                unexpired_directory_connectors_in_memory(&cache_key(id, false)),
                Some(Vec::new())
            );
        }
        for id in 0..MAX_DIRECTORY_CACHE_SCOPES * 2 {
            write_cached_directory_connectors_in_memory(
                cache_key(&format!("bounded-{id}"), false),
                &[],
                CONNECTORS_CACHE_TTL,
            );
        }
        assert_eq!(
            CONNECTOR_DIRECTORY_CACHE.lock().unwrap().len(),
            MAX_DIRECTORY_CACHE_SCOPES
        );
    }

    fn cache_key(id: &str, is_workspace_account: bool) -> ConnectorDirectoryCacheKey {
        ConnectorDirectoryCacheKey::new(
            "https://chatgpt.example".to_string(),
            Some(format!("account-{id}")),
            Some(format!("user-{id}")),
            is_workspace_account,
        )
    }

    fn cache_context(
        codex_home: &TempDir,
        id: &str,
        is_workspace_account: bool,
    ) -> ConnectorDirectoryCacheContext {
        ConnectorDirectoryCacheContext::new(
            codex_home.path().to_path_buf(),
            cache_key(id, is_workspace_account),
        )
    }

    fn clear_directory_memory_cache() {
        let mut cache_guard = CONNECTOR_DIRECTORY_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache_guard.clear();
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes tests that mutate the shared connector directory cache"
    )]
    async fn directory_scope_comes_from_the_cache_key() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;
        clear_directory_memory_cache();
        let home = TempDir::new()?;
        for (index, workspace) in [false, true, false].into_iter().enumerate() {
            let context = cache_context(&home, "scope", workspace);
            let mut paths = Vec::new();
            let connectors = list_all_connectors_with_options(context, false, |path| {
                let is_workspace = path.contains("list_workspace");
                paths.push(path);
                async move {
                    Ok(DirectoryListResponse {
                        apps: vec![if is_workspace {
                            app("workspace", "Workspace")
                        } else {
                            app("global", "Global")
                        }],
                        next_token: None,
                    })
                }
            })
            .await?;
            let ids = connectors
                .iter()
                .map(|app| app.id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                ids,
                if workspace {
                    vec!["global", "workspace"]
                } else {
                    vec!["global"]
                }
            );
            assert_eq!(
                paths.len(),
                if index == 2 {
                    0
                } else if workspace {
                    2
                } else {
                    1
                }
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes tests that mutate the shared connector directory cache"
    )]
    async fn pagination_cycles_fail_without_publishing_partial_results() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;
        let home = TempDir::new()?;
        let context = cache_context(&home, "cycle", false);
        let previous = vec![directory_app_to_app_info(app("complete", "Complete"))];
        write_cached_directory_connectors(&context, &previous);
        for tokens in [vec!["A", "A"], vec!["A", "B", "A"]] {
            let mut calls = 0;
            let result = list_all_connectors_with_options(context.clone(), true, |_| {
                let token = tokens.get(calls).copied();
                calls += 1;
                async move {
                    // Bound the fake even if cycle detection regresses.
                    let token =
                        token.ok_or_else(|| anyhow::anyhow!("fetch exceeded expected pages"))?;
                    Ok(DirectoryListResponse {
                        apps: vec![app("partial", "Partial")],
                        next_token: Some(token.to_string()),
                    })
                }
            })
            .await;
            assert_eq!(
                result.unwrap_err().to_string(),
                "connector directory returned a repeated pagination token"
            );
            assert_eq!(calls, tokens.len());
            assert_eq!(
                cached_directory_connectors(&context),
                Some(previous.clone())
            );
            clear_directory_memory_cache();
            assert_eq!(
                cached_directory_connectors(&context),
                Some(previous.clone())
            );
        }
        Ok(())
    }

    fn app(id: &str, name: &str) -> DirectoryApp {
        DirectoryApp {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            app_metadata: None,
            branding: None,
            labels: None,
            logo_url: None,
            logo_url_dark: None,
            icon_assets: None,
            icon_dark_assets: None,
            distribution_channel: None,
            visibility: None,
        }
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes tests that mutate the shared connector directory cache"
    )]
    async fn concurrent_directory_misses_share_one_refresh() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;
        clear_directory_memory_cache();
        let home = TempDir::new()?;
        let context = cache_context(&home, "concurrent", false);
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let mut signals = Some((started, wait));
        let first = list_all_connectors_with_options(context.clone(), false, |_| {
            let (started, wait) = signals.take().expect("one page");
            async move {
                started.send(()).unwrap();
                wait.await?;
                Ok(DirectoryListResponse {
                    apps: vec![app("alpha", "Alpha")],
                    next_token: None,
                })
            }
        });
        let second = async {
            ready.await?;
            let mut second = Box::pin(list_all_connectors_with_options(
                context.clone(),
                false,
                |_| async { anyhow::bail!("overlapping miss must reuse the completed refresh") },
            ));
            assert!(matches!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx))).await,
                std::task::Poll::Pending
            ));
            release.send(()).unwrap();
            second.await
        };
        let (first, second) = tokio::join!(first, second);
        let first = first?;
        let mut expected = directory_app_to_app_info(app("alpha", "Alpha"));
        expected.install_url = Some(connector_install_url("Alpha", "alpha"));
        assert_eq!(first, vec![expected.clone()]);
        assert_eq!(second?, vec![expected.clone()]);
        clear_directory_memory_cache();
        assert_eq!(cached_directory_connectors(&context), Some(vec![expected]));
        Ok(())
    }

    #[test]
    fn disk_cache_replaces_complete_snapshots_and_retains_invalid_files() -> anyhow::Result<()> {
        let home = TempDir::new()?;
        let context = cache_context(&home, "replace", false);
        let path = context.cache_path();
        directory_cache::write_cached_directory_connectors_to_disk(
            &context,
            &[directory_app_to_app_info(app("old", "Old"))],
        );
        let expected = vec![directory_app_to_app_info(app("new", "New"))];
        directory_cache::write_cached_directory_connectors_to_disk(&context, &expected);
        let directory_cache::CachedConnectorDirectoryDiskLoad::Hit { connectors } =
            directory_cache::load_cached_directory_connectors_from_disk(&context)
        else {
            panic!("replacement should be readable");
        };
        assert_eq!(connectors, expected);
        assert_eq!(std::fs::read_dir(path.parent().unwrap())?.count(), 1);
        std::fs::write(&path, b"invalid json")?;
        assert!(matches!(
            directory_cache::load_cached_directory_connectors_from_disk(&context),
            directory_cache::CachedConnectorDirectoryDiskLoad::Invalid
        ));
        assert_eq!(std::fs::read(&path)?, b"invalid json");
        Ok(())
    }

    #[test]
    fn directory_app_icon_assets_reach_app_info() -> anyhow::Result<()> {
        let response: DirectoryListResponse = serde_json::from_value(serde_json::json!({
            "apps": [{
                "id": "alpha",
                "name": "Alpha",
                "icon_assets": {},
                "icon_dark_assets": {}
            }, {
                "id": "alpha",
                "name": "",
                "icon_assets": {
                    "256_square": "https://example.com/alpha-square.png"
                },
                "icon_dark_assets": {
                    "256_square": "https://example.com/alpha-square-dark.png"
                }
            }],
            "next_token": null
        }))?;

        let app_info = directory_app_to_app_info(merge_directory_apps(response.apps).remove(0));

        assert_eq!(
            serde_json::to_value(app_info)?,
            serde_json::json!({
                "id": "alpha",
                "name": "Alpha",
                "description": null,
                "logoUrl": null,
                "logoUrlDark": null,
                "iconAssets": {
                    "256_square": "https://example.com/alpha-square.png"
                },
                "iconDarkAssets": {
                    "256_square": "https://example.com/alpha-square-dark.png"
                },
                "distributionChannel": null,
                "branding": null,
                "appMetadata": null,
                "labels": null,
                "installUrl": null,
                "isAccessible": false,
                "isEnabled": true,
                "pluginDisplayNames": []
            })
        );
        Ok(())
    }

    #[test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serializes tests that mutate the shared connector directory cache"
    )]
    fn directory_cache_publication_yields_and_persists_fetched_connectors() -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()?;
        runtime.block_on(async {
            let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;
            clear_directory_memory_cache();
            let codex_home = TempDir::new()?;
            let context = cache_context(&codex_home, "publication-worker", false);
            let cache_path = context.cache_path();
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = wait.recv();
            });
            ready.await?;
            let listing = list_all_connectors_with_options(context.clone(), true, |_| async {
                Ok(DirectoryListResponse {
                    apps: vec![app("alpha", "Alpha")],
                    next_token: None,
                })
            });
            tokio::pin!(listing);
            let publication_waits_for_worker =
                tokio::time::timeout(Duration::from_millis(20), &mut listing)
                    .await
                    .is_err();
            let published_before_worker = cache_path.exists();
            drop(release);
            blocker.await?;
            assert!(
                publication_waits_for_worker,
                "disk publication must yield to a blocking worker"
            );
            assert!(
                !published_before_worker,
                "cache must not be written on the runtime thread"
            );
            let connectors = listing.await?;
            let mut expected = directory_app_to_app_info(app("alpha", "Alpha"));
            expected.install_url = Some("https://chatgpt.com/apps/alpha/alpha".to_string());
            assert_eq!(connectors, vec![expected.clone()]);
            clear_directory_memory_cache();
            assert_eq!(cached_directory_connectors(&context), Some(vec![expected]));
            Ok(())
        })
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn list_all_connectors_uses_shared_directory_cache() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = Arc::clone(&calls);
        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "shared", false);

        let first = list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ false,
            move |_path| {
                let call_counter = Arc::clone(&call_counter);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(DirectoryListResponse {
                        apps: vec![app("alpha", "Alpha")],
                        next_token: None,
                    })
                }
            },
        )
        .await?;

        let second = list_all_connectors_with_options(
            cache_context,
            /*force_refetch*/ false,
            move |_path| async move {
                anyhow::bail!("cache should have been used");
            },
        )
        .await?;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn list_all_connectors_merges_and_normalizes_directory_apps() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "merged", true);
        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = Arc::clone(&calls);

        let connectors = list_all_connectors_with_options(
            cache_context,
            /*force_refetch*/ true,
            move |path| {
                let call_counter = Arc::clone(&call_counter);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    if path.starts_with("/connectors/directory/list_workspace") {
                        Ok(DirectoryListResponse {
                            apps: vec![
                                DirectoryApp {
                                    description: Some("Merged description".to_string()),
                                    branding: Some(AppBranding {
                                        category: Some("calendar".to_string()),
                                        developer: None,
                                        website: None,
                                        privacy_policy: None,
                                        terms_of_service: None,
                                        is_discoverable_app: true,
                                    }),
                                    ..app("alpha", "")
                                },
                                DirectoryApp {
                                    visibility: Some("HIDDEN".to_string()),
                                    ..app("hidden", "Hidden")
                                },
                            ],
                            next_token: None,
                        })
                    } else {
                        Ok(DirectoryListResponse {
                            apps: vec![
                                app("alpha", " Alpha "),
                                app("beta/雪?query#fragment%", "Beta"),
                            ],
                            next_token: None,
                        })
                    }
                }
            },
        )
        .await?;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(connectors.len(), 2);
        assert_eq!(connectors[0].id, "alpha");
        assert_eq!(connectors[0].name, "Alpha");
        assert_eq!(
            connectors[0].description.as_deref(),
            Some("Merged description")
        );
        assert_eq!(
            connectors[0].install_url.as_deref(),
            Some("https://chatgpt.com/apps/alpha/alpha")
        );
        assert_eq!(
            connectors[0]
                .branding
                .as_ref()
                .and_then(|branding| branding.category.as_deref()),
            Some("calendar")
        );
        assert_eq!(connectors[1].id, "beta/雪?query#fragment%");
        assert_eq!(connectors[1].name, "Beta");
        assert_eq!(
            connectors[1].install_url.as_deref(),
            Some("https://chatgpt.com/apps/beta/beta%2F%E9%9B%AA%3Fquery%23fragment%25")
        );
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn list_all_connectors_overlaps_workspace_and_directory_requests() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "overlap", true);
        let workspace_started = Arc::new(tokio::sync::Notify::new());

        // The directory page completes only after the workspace request is polled,
        // so a serialized refresh cannot finish; the timeout only bounds that regression.
        let connectors = tokio::time::timeout(
            Duration::from_secs(1),
            list_all_connectors_with_options(
                cache_context,
                /*force_refetch*/ true,
                move |path| {
                    let workspace_started = Arc::clone(&workspace_started);
                    async move {
                        if path.starts_with("/connectors/directory/list_workspace") {
                            workspace_started.notify_one();
                            Ok(DirectoryListResponse {
                                apps: vec![app("workspace", "Workspace")],
                                next_token: None,
                            })
                        } else {
                            workspace_started.notified().await;
                            Ok(DirectoryListResponse {
                                apps: vec![app("directory", "Directory")],
                                next_token: None,
                            })
                        }
                    }
                },
            ),
        )
        .await
        .expect("workspace request should start while the directory request is pending")?;

        assert_eq!(
            connectors
                .into_iter()
                .map(|connector| connector.id)
                .collect::<Vec<_>>(),
            vec!["directory".to_string(), "workspace".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn list_all_connectors_retries_workspace_page_after_transient_failure()
    -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "workspace-retry", true);
        let workspace_calls = Arc::new(AtomicUsize::new(0));
        let first_workspace_calls = Arc::clone(&workspace_calls);

        let first = list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ false,
            move |path| {
                let workspace_calls = Arc::clone(&first_workspace_calls);
                async move {
                    if path.starts_with("/connectors/directory/list_workspace") {
                        workspace_calls.fetch_add(1, Ordering::SeqCst);
                        anyhow::bail!("transient workspace failure");
                    }
                    Ok(DirectoryListResponse {
                        apps: vec![app("global", "Global")],
                        next_token: None,
                    })
                }
            },
        )
        .await;
        assert!(first.is_err());

        let second_workspace_calls = Arc::clone(&workspace_calls);
        let second = list_all_connectors_with_options(
            cache_context,
            /*force_refetch*/ false,
            move |path| {
                let workspace_calls = Arc::clone(&second_workspace_calls);
                async move {
                    if path.starts_with("/connectors/directory/list_workspace") {
                        workspace_calls.fetch_add(1, Ordering::SeqCst);
                        return Ok(DirectoryListResponse {
                            apps: vec![app("workspace", "Workspace")],
                            next_token: None,
                        });
                    }
                    Ok(DirectoryListResponse {
                        apps: vec![app("global", "Global")],
                        next_token: None,
                    })
                }
            },
        )
        .await?;

        assert_eq!(workspace_calls.load(Ordering::SeqCst), 2);
        assert!(second.iter().any(|connector| connector.id == "workspace"));
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn failed_workspace_refresh_preserves_complete_cached_listing() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "workspace-refresh-failure", true);
        list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ false,
            move |path| async move {
                if path.starts_with("/connectors/directory/list_workspace") {
                    return Ok(DirectoryListResponse {
                        apps: vec![app("workspace", "Workspace")],
                        next_token: None,
                    });
                }
                Ok(DirectoryListResponse {
                    apps: vec![app("global", "Global")],
                    next_token: None,
                })
            },
        )
        .await?;

        let refresh = list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ true,
            move |path| async move {
                if path.starts_with("/connectors/directory/list_workspace") {
                    anyhow::bail!("transient workspace failure");
                }
                Ok(DirectoryListResponse {
                    apps: vec![app("new-global", "New Global")],
                    next_token: None,
                })
            },
        )
        .await;
        assert!(refresh.is_err());

        let cached = list_all_connectors_with_options(
            cache_context,
            /*force_refetch*/ false,
            move |_path| async move {
                anyhow::bail!("the previous complete listing should remain cached");
            },
        )
        .await?;

        assert_eq!(
            cached
                .iter()
                .map(|connector| connector.id.as_str())
                .collect::<Vec<_>>(),
            vec!["global", "workspace"]
        );
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn cached_directory_connectors_reads_directory_disk_cache() -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "disk", false);
        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = Arc::clone(&calls);

        let first = list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ false,
            move |_path| {
                let call_counter = Arc::clone(&call_counter);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(DirectoryListResponse {
                        apps: vec![app("alpha", "Alpha")],
                        next_token: None,
                    })
                }
            },
        )
        .await?;

        clear_directory_memory_cache();

        let second = cached_directory_connectors(&cache_context).expect("disk cache should load");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first, second);
        Ok(())
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "test serializes access to the shared connector cache for its full duration"
    )]
    async fn list_all_connectors_refreshes_when_only_directory_disk_cache_exists()
    -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "disk-refresh", false);
        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = Arc::clone(&calls);

        list_all_connectors_with_options(
            cache_context.clone(),
            /*force_refetch*/ false,
            move |_path| {
                let call_counter = Arc::clone(&call_counter);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(DirectoryListResponse {
                        apps: vec![app("alpha", "Alpha")],
                        next_token: None,
                    })
                }
            },
        )
        .await?;

        clear_directory_memory_cache();
        let mut cached_expected = directory_app_to_app_info(app("alpha", "Alpha"));
        cached_expected.install_url = Some(connector_install_url(
            &cached_expected.name,
            &cached_expected.id,
        ));
        assert_eq!(
            cached_directory_connectors(&cache_context),
            Some(vec![cached_expected])
        );
        let refreshed_calls = Arc::clone(&calls);

        let refreshed = list_all_connectors_with_options(
            cache_context,
            /*force_refetch*/ false,
            move |_path| {
                let call_counter = Arc::clone(&refreshed_calls);
                async move {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    Ok(DirectoryListResponse {
                        apps: vec![app("beta", "Beta")],
                        next_token: None,
                    })
                }
            },
        )
        .await?;

        let mut expected = directory_app_to_app_info(app("beta", "Beta"));
        expected.install_url = Some(connector_install_url(&expected.name, &expected.id));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(refreshed, vec![expected]);
        Ok(())
    }

    #[tokio::test]
    async fn cached_directory_connectors_rejects_stale_disk_schema_without_deleting()
    -> anyhow::Result<()> {
        let _cache_guard = CONNECTOR_DIRECTORY_CACHE_TEST_LOCK.lock().await;

        clear_directory_memory_cache();
        let codex_home = TempDir::new()?;
        let cache_context = cache_context(&codex_home, "stale-schema", false);
        let cache_path = cache_context.cache_path();
        std::fs::create_dir_all(cache_path.parent().expect("cache parent"))?;
        std::fs::write(
            &cache_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 0,
                "connectors": [],
            }))?,
        )?;

        assert_eq!(cached_directory_connectors(&cache_context), None);
        assert!(cache_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn list_directory_connectors_omits_tier_for_all_pages() -> anyhow::Result<()> {
        let requested_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let paths = Arc::clone(&requested_paths);

        let apps = list_directory_connectors(&mut move |path| {
            let paths = Arc::clone(&paths);
            async move {
                paths
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(path.clone());
                if path == "/connectors/directory/list?external_logos=true" {
                    Ok(DirectoryListResponse {
                        apps: vec![app("alpha", "Alpha")],
                        next_token: Some("page 2".to_string()),
                    })
                } else {
                    assert_eq!(
                        path,
                        "/connectors/directory/list?token=page%202&external_logos=true"
                    );
                    Ok(DirectoryListResponse {
                        apps: vec![app("beta", "Beta")],
                        next_token: None,
                    })
                }
            }
        })
        .await?;

        assert_eq!(
            apps.iter().map(|app| app.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            requested_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[
                "/connectors/directory/list?external_logos=true".to_string(),
                "/connectors/directory/list?token=page%202&external_logos=true".to_string(),
            ]
        );
        Ok(())
    }
}
