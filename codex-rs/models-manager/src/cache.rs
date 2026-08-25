use chrono::DateTime;
use chrono::Utc;
use codex_file_system::AtomicWriteLock;
use codex_file_system::acquire_atomic_write_lock;
use codex_file_system::write_bytes_atomically;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CacheWriteBasis {
    disk_revision: DiskRevision,
    client_version: Option<String>,
    provider_cache_identity: Option<String>,
    etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DiskRevision {
    Missing,
    Persisted(u64),
    Legacy(Vec<u8>),
    Opaque(Vec<u8>),
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

    async fn acquire_file_lock(&self) -> io::Result<AtomicWriteLock> {
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let cache_path = self.cache_path.clone();
        tokio::task::spawn_blocking(move || acquire_atomic_write_lock(&cache_path))
            .await
            .map_err(|err| io::Error::other(format!("models cache lock task failed: {err}")))?
    }

    /// Attempt to load a fresh cache entry. Returns `None` if the cache doesn't exist or is stale.
    #[cfg(test)]
    pub(crate) async fn load_fresh(
        &self,
        expected_version: &str,
    ) -> io::Result<Option<ModelsCache>> {
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
    ) -> io::Result<Option<ModelsCache>> {
        let _permit = self.acquire_io_permit().await?;
        let _file_lock = self.acquire_file_lock().await?;
        if !self.identity_is_current(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: skipped load after identity changed"
            );
            return Ok(None);
        }
        info!(
                cache_path = %self.cache_path.display(),
                expected_version,
            "models cache: attempting load_fresh"
        );
        let Some(cache) = self.load().await? else {
            return Ok(None);
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
            return Ok(None);
        }
        if cache.provider_cache_identity.as_deref() != Some(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: eligibility mismatch"
            );
            return Ok(None);
        }
        if !cache.is_fresh(self.cache_ttl) {
            info!(
                cache_path = %self.cache_path.display(),
                cache_ttl_secs = self.cache_ttl.as_secs(),
                fetched_at = %cache.fetched_at,
                "models cache: cache is stale"
            );
            return Ok(None);
        }
        info!(
            cache_path = %self.cache_path.display(),
            cache_ttl_secs = self.cache_ttl.as_secs(),
            "models cache: cache hit"
        );
        Ok(self.identity_is_current(expected_identity).then_some(cache))
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
    #[cfg(test)]
    pub(crate) async fn persist_cache_for_identity(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
        expected_identity: &str,
    ) -> bool {
        let basis = match self.write_basis_for_identity(expected_identity).await {
            Ok(basis) => basis,
            Err(err) => {
                error!("failed to capture models cache write basis: {err}");
                return false;
            }
        };
        self.persist_cache_for_identity_if_unchanged(
            models,
            etag,
            client_version,
            expected_identity,
            &basis,
        )
        .await
    }

    pub(crate) async fn write_basis_for_identity(
        &self,
        expected_identity: &str,
    ) -> io::Result<CacheWriteBasis> {
        let _permit = match self.acquire_io_permit().await {
            Ok(permit) => permit,
            Err(err) => {
                return Err(io::Error::other(format!(
                    "failed to acquire models cache I/O gate: {err}"
                )));
            }
        };
        let _file_lock = self.acquire_file_lock().await?;
        if !self.identity_is_current(expected_identity) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache identity changed before write basis capture",
            ));
        }
        self.read_write_basis().await
    }

    pub(crate) async fn persist_cache_for_identity_if_unchanged(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
        expected_identity: &str,
        expected_basis: &CacheWriteBasis,
    ) -> bool {
        let _permit = match self.acquire_io_permit().await {
            Ok(permit) => permit,
            Err(err) => {
                error!("failed to acquire models cache I/O gate: {err}");
                return false;
            }
        };
        let _file_lock = match self.acquire_file_lock().await {
            Ok(lock) => lock,
            Err(err) => {
                error!("failed to acquire models cache file lock: {err}");
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
        let current_basis = match self.read_write_basis().await {
            Ok(basis) => basis,
            Err(err) => {
                error!("failed to re-read models cache before write: {err}");
                return false;
            }
        };
        if &current_basis != expected_basis {
            info!(
                cache_path = %self.cache_path.display(),
                "models cache: skipped stale cross-process write"
            );
            return false;
        }
        let cache = ModelsCache {
            revision: Some(next_revision(&current_basis.disk_revision)),
            fetched_at: Utc::now(),
            etag,
            client_version: Some(client_version),
            provider_cache_identity: Some(current_identity),
            models: models.to_vec(),
        };
        if !self.identity_is_current(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: skipped write after identity changed under file lock"
            );
            return false;
        }
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
    #[cfg(test)]
    pub(crate) async fn renew_cache_ttl_for_identity(
        &self,
        expected_version: &str,
        expected_etag: &str,
        expected_identity: &str,
    ) -> io::Result<()> {
        let basis = self.write_basis_for_identity(expected_identity).await?;
        self.renew_cache_ttl_for_identity_if_unchanged(
            expected_version,
            expected_etag,
            expected_identity,
            &basis,
        )
        .await
    }

    pub(crate) async fn renew_cache_ttl_for_identity_if_unchanged(
        &self,
        expected_version: &str,
        expected_etag: &str,
        expected_identity: &str,
        expected_basis: &CacheWriteBasis,
    ) -> io::Result<()> {
        let _permit = self.acquire_io_permit().await?;
        let _file_lock = self.acquire_file_lock().await?;
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
        let current_basis = self.read_write_basis().await?;
        if &current_basis != expected_basis {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache revision changed before TTL renewal",
            ));
        }
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
        cache.revision = Some(next_revision(&current_basis.disk_revision));
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

    async fn read_write_basis(&self) -> io::Result<CacheWriteBasis> {
        let contents = match fs::read(&self.cache_path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(CacheWriteBasis {
                    disk_revision: DiskRevision::Missing,
                    client_version: None,
                    provider_cache_identity: None,
                    etag: None,
                });
            }
            Err(err) => return Err(err),
        };
        match serde_json::from_slice::<ModelsCache>(&contents) {
            Ok(cache) => Ok(CacheWriteBasis {
                disk_revision: cache
                    .revision
                    .map(DiskRevision::Persisted)
                    .unwrap_or_else(|| DiskRevision::Legacy(contents)),
                client_version: cache.client_version,
                provider_cache_identity: cache.provider_cache_identity,
                etag: cache.etag,
            }),
            Err(_) => Ok(CacheWriteBasis {
                disk_revision: DiskRevision::Opaque(contents),
                client_version: None,
                provider_cache_identity: None,
                etag: None,
            }),
        }
    }

    async fn save_internal(&self, cache: &ModelsCache) -> io::Result<()> {
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_vec_pretty(cache)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
        let cache_path = self.cache_path.clone();
        tokio::task::spawn_blocking(move || write_bytes_atomically(&cache_path, &json))
            .await
            .map_err(|err| io::Error::other(format!("models cache write task failed: {err}")))?
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
        let _file_lock = self.acquire_file_lock().await?;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        let current_basis = self.read_write_basis().await?;
        f(&mut cache.fetched_at);
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        self.save_internal(&cache).await
    }

    #[cfg(test)]
    /// Mutate the full cache contents for testing.
    pub(crate) async fn mutate_cache_for_test<F>(&self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut ModelsCache),
    {
        let _permit = self.acquire_io_permit().await?;
        let _file_lock = self.acquire_file_lock().await?;
        let mut cache = match self.load().await? {
            Some(cache) => cache,
            None => return Err(io::Error::new(ErrorKind::NotFound, "cache not found")),
        };
        let current_basis = self.read_write_basis().await?;
        f(&mut cache);
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        self.save_internal(&cache).await
    }
}

/// Serialized snapshot of models and metadata cached on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelsCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<u64>,
    pub(crate) fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider_cache_identity: Option<String>,
    pub(crate) models: Vec<ModelInfo>,
}

fn next_revision(revision: &DiskRevision) -> u64 {
    match revision {
        DiskRevision::Persisted(revision) => revision.saturating_add(1),
        DiskRevision::Missing | DiskRevision::Legacy(_) | DiskRevision::Opaque(_) => 1,
    }
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
        age >= chrono::Duration::zero() && age <= ttl_duration
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
        assert!(first.load_fresh("client-one").await.unwrap().is_some());

        let second = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider-two"),
        );
        assert!(second.load_fresh("client-one").await.unwrap().is_none());
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
        assert!(first.load_fresh("client-one").await.unwrap().is_none());
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
        assert!(manager.load_fresh("client-two").await.unwrap().is_none());
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
        assert!(manager.load_fresh("client-one").await.unwrap().is_none());
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
        assert!(manager.load_fresh("client-one").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn future_timestamp_is_not_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manager = ModelsCacheManager::new(
            temp.path().join("models_cache.json"),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        manager
            .persist_cache(&[], Some("etag-one".to_string()), "client-one".to_string())
            .await;
        manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() + chrono::Duration::hours(1);
            })
            .await
            .expect("move cache timestamp into the future");

        assert!(manager.load_fresh("client-one").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn corrupt_cache_is_not_reported_as_a_miss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        std::fs::write(&path, b"{not-json").expect("write corrupt cache");
        let manager =
            ModelsCacheManager::new(path, Duration::from_secs(300), fixed_identity("provider"));

        let error = manager
            .load_fresh("client-one")
            .await
            .expect_err("corrupt cache must be distinguishable from a miss");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
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
        assert!(manager.load_fresh("client-one").await.unwrap().is_some());

        *identity
            .lock()
            .expect("identity lock should not be poisoned") = "scope-digest-two".to_string();

        assert!(manager.load_fresh("client-one").await.unwrap().is_none());
        assert!(
            manager
                .renew_cache_ttl("client-one", "etag-one")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stale_manager_cannot_overwrite_newer_cross_process_revision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let older = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        let newer =
            ModelsCacheManager::new(path, Duration::from_secs(300), fixed_identity("provider"));
        let older_basis = older
            .write_basis_for_identity("provider")
            .await
            .expect("capture older fetch basis");
        let newer_basis = newer
            .write_basis_for_identity("provider")
            .await
            .expect("capture newer fetch basis");

        assert!(
            newer
                .persist_cache_for_identity_if_unchanged(
                    &[],
                    Some("newer-etag".to_string()),
                    "client".to_string(),
                    "provider",
                    &newer_basis,
                )
                .await
        );
        assert!(
            !older
                .persist_cache_for_identity_if_unchanged(
                    &[],
                    Some("older-etag".to_string()),
                    "client".to_string(),
                    "provider",
                    &older_basis,
                )
                .await
        );

        let persisted = newer
            .load_fresh("client")
            .await
            .expect("cache read")
            .expect("newer cache remains readable");
        assert_eq!(persisted.etag.as_deref(), Some("newer-etag"));
        assert_eq!(persisted.revision, Some(1));
    }

    #[tokio::test]
    async fn stale_ttl_renewal_cannot_rewrite_newer_cross_process_revision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let stale = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        let newer =
            ModelsCacheManager::new(path, Duration::from_secs(300), fixed_identity("provider"));
        stale
            .persist_cache(&[], Some("old-etag".to_string()), "client".to_string())
            .await;
        let stale_basis = stale
            .write_basis_for_identity("provider")
            .await
            .expect("capture stale TTL basis");
        let newer_basis = newer
            .write_basis_for_identity("provider")
            .await
            .expect("capture newer write basis");

        assert!(
            newer
                .persist_cache_for_identity_if_unchanged(
                    &[],
                    Some("new-etag".to_string()),
                    "client".to_string(),
                    "provider",
                    &newer_basis,
                )
                .await
        );
        assert!(
            stale
                .renew_cache_ttl_for_identity_if_unchanged(
                    "client",
                    "old-etag",
                    "provider",
                    &stale_basis,
                )
                .await
                .is_err()
        );

        let persisted = newer
            .load_fresh("client")
            .await
            .expect("cache read")
            .expect("newer cache remains readable");
        assert_eq!(persisted.etag.as_deref(), Some("new-etag"));
        assert_eq!(persisted.revision, Some(2));
    }
}
