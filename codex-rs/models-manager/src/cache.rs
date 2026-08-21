use chrono::DateTime;
use chrono::Utc;
use codex_protocol::openai_models::ModelInfo;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tracing::error;
use tracing::info;

use crate::manager::ModelsCacheIdentity;

/// Manages loading and saving of models cache to disk.
pub(crate) struct ModelsCacheManager {
    cache_path: PathBuf,
    cache_ttl: Duration,
    cache_identity: ModelsCacheIdentity,
    io_permit: Semaphore,
}

impl fmt::Debug for ModelsCacheManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelsCacheManager")
            .field("cache_path", &self.cache_path)
            .field("cache_ttl", &self.cache_ttl)
            .field("cache_identity", &"<redacted resolver>")
            .finish()
    }
}

impl ModelsCacheManager {
    /// Create a new cache manager with the given path and TTL.
    pub(crate) fn new(
        cache_path: PathBuf,
        cache_ttl: Duration,
        cache_identity: ModelsCacheIdentity,
    ) -> Self {
        Self {
            cache_path,
            cache_ttl,
            cache_identity,
            io_permit: Semaphore::new(/*permits*/ 1),
        }
    }

    pub(crate) fn current_identity(&self) -> String {
        (self.cache_identity)()
    }

    pub(crate) fn identity_is_current(&self, expected_identity: &str) -> bool {
        self.current_identity() == expected_identity
    }

    async fn acquire_io_permit(&self) -> io::Result<SemaphorePermit<'_>> {
        self.io_permit
            .acquire()
            .await
            .map_err(|_| io::Error::other("models cache I/O gate closed"))
    }

    /// Attempt to load a fresh cache entry. Returns `None` if the cache doesn't exist or is stale.
    #[cfg(test)]
    pub(crate) async fn load_fresh(&self, expected_version: &str) -> Option<ModelsCache> {
        let expected_identity = self.current_identity();
        self.load_fresh_for_identity(expected_version, &expected_identity)
            .await
    }

    /// Attempt to load a fresh entry only while the caller's identity snapshot
    /// remains authoritative.
    pub(crate) async fn load_fresh_for_identity(
        &self,
        expected_version: &str,
        expected_identity: &str,
    ) -> Option<ModelsCache> {
        let _permit = match self.acquire_io_permit().await {
            Ok(permit) => permit,
            Err(err) => {
                error!("failed to acquire models cache I/O gate: {err}");
                return None;
            }
        };
        if !self.identity_is_current(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: skipped load after identity changed"
            );
            return None;
        }
        info!(
                cache_path = %self.cache_path.display(),
                expected_version,
            "models cache: attempting load_fresh"
        );
        let cache = match self.load().await {
            Ok(cache) => cache?,
            Err(err) => {
                error!("failed to load models cache: {err}");
                return None;
            }
        };
        info!(
            cache_path = %self.cache_path.display(),
            cached_version = ?cache.client_version,
            fetched_at = %cache.fetched_at,
            "models cache: loaded cache file"
        );
        if cache.client_version.as_deref() != Some(expected_version) {
            info!(
                cache_path = %self.cache_path.display(),
                expected_version,
                cached_version = ?cache.client_version,
                "models cache: cache version mismatch"
            );
            return None;
        }
        if cache.provider_cache_identity.as_deref() != Some(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: eligibility mismatch"
            );
            return None;
        }
        if !cache.is_fresh(self.cache_ttl) {
            info!(
                cache_path = %self.cache_path.display(),
                cache_ttl_secs = self.cache_ttl.as_secs(),
                fetched_at = %cache.fetched_at,
                "models cache: cache is stale"
            );
            return None;
        }
        info!(
            cache_path = %self.cache_path.display(),
            cache_ttl_secs = self.cache_ttl.as_secs(),
            "models cache: cache hit"
        );
        self.identity_is_current(expected_identity).then_some(cache)
    }

    /// Persist the cache to disk, creating parent directories as needed.
    #[cfg(test)]
    pub(crate) async fn persist_cache(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
    ) {
        let expected_identity = self.current_identity();
        self.persist_cache_for_identity(models, etag, client_version, &expected_identity)
            .await;
    }

    /// Persist only if the request that produced these models still belongs
    /// to the current complete cache identity.
    pub(crate) async fn persist_cache_for_identity(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
        expected_identity: &str,
    ) -> bool {
        let _permit = match self.acquire_io_permit().await {
            Ok(permit) => permit,
            Err(err) => {
                error!("failed to acquire models cache I/O gate: {err}");
                return false;
            }
        };
        let current_identity = self.current_identity();
        if current_identity != expected_identity {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: skipped write after identity changed"
            );
            return false;
        }
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            etag,
            client_version: Some(client_version),
            provider_cache_identity: Some(current_identity),
            models: models.to_vec(),
        };
        if let Err(err) = self.save_internal(&cache).await {
            error!("failed to write models cache: {err}");
            return false;
        }
        true
    }

    /// Renew the cache TTL by updating the fetched_at timestamp to now.
    #[cfg(test)]
    pub(crate) async fn renew_cache_ttl(
        &self,
        expected_version: &str,
        expected_etag: &str,
    ) -> io::Result<()> {
        let expected_identity = self.current_identity();
        self.renew_cache_ttl_for_identity(expected_version, expected_etag, &expected_identity)
            .await
    }

    /// Renew only while the caller's identity snapshot remains authoritative.
    pub(crate) async fn renew_cache_ttl_for_identity(
        &self,
        expected_version: &str,
        expected_etag: &str,
        expected_identity: &str,
    ) -> io::Result<()> {
        let _permit = self.acquire_io_permit().await?;
        if !self.identity_is_current(expected_identity) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache identity changed before TTL renewal",
            ));
        }
        let mut cache = self
            .load()
            .await?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "cache not found"))?;
        if cache.client_version.as_deref() != Some(expected_version)
            || cache.provider_cache_identity.as_deref() != Some(expected_identity)
            || cache.etag.as_deref() != Some(expected_etag)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache belongs to a different client, provider/auth scope, or ETag identity",
            ));
        }
        cache.fetched_at = Utc::now();
        if !self.identity_is_current(expected_identity) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache identity changed during TTL renewal",
            ));
        }
        self.save_internal(&cache).await
    }

    async fn load(&self) -> io::Result<Option<ModelsCache>> {
        match fs::read(&self.cache_path).await {
            Ok(contents) => {
                let cache = serde_json::from_slice(&contents)
                    .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
                Ok(Some(cache))
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn save_internal(&self, cache: &ModelsCache) -> io::Result<()> {
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_vec_pretty(cache)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
        fs::write(&self.cache_path, json).await
    }

    #[cfg(test)]
    /// Set the cache TTL.
    pub(crate) fn set_ttl(&mut self, ttl: Duration) {
        self.cache_ttl = ttl;
    }

    #[cfg(test)]
    /// Manipulate cache file for testing. Allows setting a custom fetched_at timestamp.
    pub(crate) async fn manipulate_cache_for_test<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut DateTime<Utc>),
    {
        let _permit = self.acquire_io_permit().await?;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        f(&mut cache.fetched_at);
        self.save_internal(&cache).await
    }

    #[cfg(test)]
    /// Mutate the full cache contents for testing.
    pub(crate) async fn mutate_cache_for_test<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut ModelsCache),
    {
        let _permit = self.acquire_io_permit().await?;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        f(&mut cache);
        self.save_internal(&cache).await
    }
}

/// Serialized snapshot of models and metadata cached on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelsCache {
    pub(crate) fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_cache_identity: Option<String>,
    pub(crate) models: Vec<ModelInfo>,
}

impl ModelsCache {
    /// Returns `true` when the cache entry has not exceeded the configured TTL.
    fn is_fresh(&self, ttl: Duration) -> bool {
        if ttl.is_zero() {
            return false;
        }
        let Ok(ttl_duration) = chrono::Duration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age <= ttl_duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    fn fixed_identity(value: &str) -> ModelsCacheIdentity {
        let value = value.to_string();
        Arc::new(move || value.clone())
    }

    #[tokio::test]
    async fn cache_is_scoped_to_complete_identity_and_legacy_entries_miss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let first = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider-one"),
        );
        first
            .persist_cache(&[], Some("etag-one".to_string()), "client-one".to_string())
            .await;
        assert!(first.load_fresh("client-one").await.is_some());

        let second = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider-two"),
        );
        assert!(second.load_fresh("client-one").await.is_none());
        assert!(
            second
                .renew_cache_ttl("client-one", "etag-one")
                .await
                .is_err()
        );

        first
            .mutate_cache_for_test(|cache| cache.provider_cache_identity = None)
            .await
            .expect("rewrite as providerless legacy cache");
        assert!(first.load_fresh("client-one").await.is_none());
    }

    #[tokio::test]
    async fn cache_renewal_requires_exact_identity_and_refreshes_stale_ttl() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manager = ModelsCacheManager::new(
            temp.path().join("models_cache.json"),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        manager
            .persist_cache(&[], Some("etag-one".to_string()), "client-one".to_string())
            .await;
        assert!(manager.load_fresh("client-two").await.is_none());
        assert!(
            manager
                .renew_cache_ttl("client-two", "etag-one")
                .await
                .is_err()
        );

        manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("make cache stale");
        assert!(manager.load_fresh("client-one").await.is_none());
        assert!(
            manager
                .renew_cache_ttl("client-one", "different-etag")
                .await
                .is_err()
        );
        manager
            .renew_cache_ttl("client-one", "etag-one")
            .await
            .expect("a matching 304 response should refresh a stale cache entry");
        assert!(manager.load_fresh("client-one").await.is_some());
    }

    #[tokio::test]
    async fn cache_identity_is_resolved_for_each_operation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = Arc::new(StdMutex::new("scope-digest-one".to_string()));
        let identity_for_cache = Arc::clone(&identity);
        let manager = ModelsCacheManager::new(
            temp.path().join("models_cache.json"),
            Duration::from_secs(300),
            Arc::new(move || {
                identity_for_cache
                    .lock()
                    .expect("identity lock should not be poisoned")
                    .clone()
            }),
        );
        manager
            .persist_cache(&[], Some("etag-one".to_string()), "client-one".to_string())
            .await;
        assert!(manager.load_fresh("client-one").await.is_some());

        *identity
            .lock()
            .expect("identity lock should not be poisoned") = "scope-digest-two".to_string();

        assert!(manager.load_fresh("client-one").await.is_none());
        assert!(
            manager
                .renew_cache_ttl("client-one", "etag-one")
                .await
                .is_err()
        );
    }
}
