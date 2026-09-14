use super::RemotePluginDirectoryItem;
use super::RemotePluginServiceConfig;
use codex_login::CodexAuth;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

const REMOTE_PLUGIN_CATALOG_DISK_CACHE_SCHEMA_VERSION: u8 = 1;
const REMOTE_PLUGIN_CATALOG_DISK_CACHE_DIR: &str = "cache/remote_plugin_catalog";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct RemotePluginCatalogCacheKey {
    chatgpt_base_url: String,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

impl RemotePluginCatalogCacheKey {
    fn global(config: &RemotePluginServiceConfig, auth: &CodexAuth) -> Self {
        Self {
            chatgpt_base_url: config.chatgpt_base_url.clone(),
            account_id: auth.get_account_id(),
            chatgpt_user_id: auth.get_chatgpt_user_id(),
            is_workspace_account: auth.is_workspace_account(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemotePluginCatalogDiskCache {
    schema_version: u8,
    plugins: Vec<RemotePluginDirectoryItem>,
}

pub(crate) async fn load_cached_global_directory_plugins_async(
    codex_home: &Path,
    config: &RemotePluginServiceConfig,
    auth: &CodexAuth,
) -> Option<Vec<RemotePluginDirectoryItem>> {
    let cache_path = cache_path(
        codex_home,
        &RemotePluginCatalogCacheKey::global(config, auth),
    );
    match tokio::task::spawn_blocking(move || {
        load_cached_global_directory_plugins_at_path(cache_path)
    })
    .await
    {
        Ok(plugins) => plugins,
        Err(err) => {
            warn!("failed to join remote plugin catalog cache read task: {err}");
            None
        }
    }
}

fn load_cached_global_directory_plugins_at_path(
    cache_path: PathBuf,
) -> Option<Vec<RemotePluginDirectoryItem>> {
    let bytes = match std::fs::read(&cache_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!(
                cache_path = %cache_path.display(),
                "failed to read remote plugin catalog disk cache: {err}"
            );
            return None;
        }
    };
    let cache: RemotePluginCatalogDiskCache = match serde_json::from_slice(&bytes) {
        Ok(cache) => cache,
        Err(err) => {
            warn!(
                cache_path = %cache_path.display(),
                "failed to parse remote plugin catalog disk cache: {err}"
            );
            return None;
        }
    };
    if cache.schema_version != REMOTE_PLUGIN_CATALOG_DISK_CACHE_SCHEMA_VERSION {
        return None;
    }

    Some(cache.plugins)
}

pub(crate) async fn write_cached_global_directory_plugins(
    codex_home: &Path,
    config: &RemotePluginServiceConfig,
    auth: &CodexAuth,
    plugins: Vec<RemotePluginDirectoryItem>,
) {
    let cache_path = cache_path(
        codex_home,
        &RemotePluginCatalogCacheKey::global(config, auth),
    );
    match tokio::task::spawn_blocking(move || {
        let contents = serde_json::to_string_pretty(&RemotePluginCatalogDiskCache {
            schema_version: REMOTE_PLUGIN_CATALOG_DISK_CACHE_SCHEMA_VERSION,
            plugins,
        })
        .map_err(std::io::Error::other)?;
        codex_file_system::write_atomically(&cache_path, &contents)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!("failed to write remote plugin catalog disk cache: {err}"),
        Err(err) => warn!("failed to join remote plugin catalog cache write task: {err}"),
    }
}

fn cache_path(codex_home: &Path, cache_key: &RemotePluginCatalogCacheKey) -> PathBuf {
    let cache_key_json = serde_json::to_vec(cache_key).unwrap_or_default();
    let mut cache_key_hash = 0xcbf29ce484222325_u64;
    for byte in cache_key_json {
        cache_key_hash ^= u64::from(byte);
        cache_key_hash = cache_key_hash.wrapping_mul(0x100000001b3);
    }
    codex_home
        .join(REMOTE_PLUGIN_CATALOG_DISK_CACHE_DIR)
        .join(format!("{cache_key_hash:016x}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_catalog_cache_is_a_miss_without_deleting_the_path() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("catalog.json");
        for contents in ["{", r#"{"schema_version":0,"plugins":[]}"#] {
            std::fs::write(&path, contents).unwrap();
            assert!(load_cached_global_directory_plugins_at_path(path.clone()).is_none());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
    }

    #[tokio::test]
    async fn catalog_cache_publication_replaces_invalid_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let config = RemotePluginServiceConfig::new(
            "https://example.com/backend-api".to_string(),
            codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ),
        );
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        let path = cache_path(
            home.path(),
            &RemotePluginCatalogCacheKey::global(&config, &auth),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{").unwrap();
        write_cached_global_directory_plugins(home.path(), &config, &auth, Vec::new()).await;
        assert_eq!(
            load_cached_global_directory_plugins_async(home.path(), &config, &auth).await,
            Some(Vec::new())
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()["schema_version"],
            1
        );
    }
}
