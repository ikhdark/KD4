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

const MAX_CACHED_IDENTITIES: usize = 8;

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

    pub(crate) fn ttl(&self) -> Duration {
        self.cache_ttl
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
        let Some(contents) = self.read_contents(expected_identity).await? else {
            return Ok(None);
        };
        // Older caches can contain model shapes that no longer decode. Reject
        // unusable entries from their metadata before decoding any model.
        let header = ModelsCacheHeader::parse(&contents)?;
        info!(
            cache_path = %self.cache_path.display(),
            cached_version = ?header.client_version,
            fetched_at = ?header.fetched_at,
            "models cache: loaded cache file"
        );
        if header.client_version.as_deref() != Some(expected_version) {
            info!(
                cache_path = %self.cache_path.display(),
                expected_version,
                cached_version = ?header.client_version,
                "models cache: cache version mismatch"
            );
            return Ok(None);
        }
        if header.provider_cache_identity.as_deref() != Some(expected_identity) {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: eligibility mismatch"
            );
            return Ok(None);
        }
        if !header.is_fresh(self.cache_ttl) {
            info!(
                cache_path = %self.cache_path.display(),
                cache_ttl_secs = self.cache_ttl.as_secs(),
                fetched_at = ?header.fetched_at,
                "models cache: cache is stale"
            );
            return Ok(None);
        }
        let cache = decode_cache(&contents)?;
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
        self.read_write_basis(expected_identity).await
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
        let current_basis = match self.read_write_basis(expected_identity).await {
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
        if let Err(err) = self
            .save_internal(cache, expected_identity, _file_lock)
            .await
        {
            error!("failed to write models cache: {err}");
            return false;
        }
        true
    }

    /// Renew the cache TTL once more than half of it has elapsed.
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
        let contents = self
            .read_contents(expected_identity)
            .await?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "cache identity not found"))?;
        let header = ModelsCacheHeader::parse(&contents)?;
        let current_basis = header.write_basis(&contents);
        if &current_basis != expected_basis {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache revision changed before TTL renewal",
            ));
        }
        if header.client_version.as_deref() != Some(expected_version)
            || header.provider_cache_identity.as_deref() != Some(expected_identity)
            || header.etag.as_deref() != Some(expected_etag)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache belongs to a different client, provider/auth scope, or ETag identity",
            ));
        }
        // Model responses revalidate the ETag on every request. Rewriting the whole
        // cache for each would cost a durable write per response without extending
        // its useful life, so renew only once more than half the TTL has elapsed.
        if header.age().is_some_and(|age| age < self.cache_ttl / 2) {
            return Ok(());
        }
        let mut cache = decode_cache(&contents)?;
        cache.fetched_at = Utc::now();
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        if !self.identity_is_current(expected_identity) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache identity changed during TTL renewal",
            ));
        }
        self.save_internal(cache, expected_identity, _file_lock)
            .await
    }

    async fn read_contents(&self, expected_identity: &str) -> io::Result<Option<Vec<u8>>> {
        let contents = match fs::read(&self.cache_path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        // Keep corrupt bytes available to the revision guard. Loads still report
        // corruption rather than treating it as a cache miss.
        let Ok(header) = ModelsCacheHeader::parse(&contents) else {
            return Ok(Some(contents));
        };
        if header.provider_cache_identity.as_deref() == Some(expected_identity) {
            return Ok(Some(contents));
        }
        for previous in header.previous {
            let bytes = previous.get().as_bytes();
            if ModelsCacheHeader::parse(bytes)?
                .provider_cache_identity
                .as_deref()
                == Some(expected_identity)
            {
                return Ok(Some(bytes.to_vec()));
            }
        }
        Ok(None)
    }

    #[cfg(test)]
    async fn load(&self) -> io::Result<Option<ModelsCache>> {
        self.read_contents(&self.current_identity())
            .await?
            .map(|contents| decode_cache(&contents))
            .transpose()
    }

    async fn read_write_basis(&self, expected_identity: &str) -> io::Result<CacheWriteBasis> {
        let Some(contents) = self.read_contents(expected_identity).await? else {
            return Ok(CacheWriteBasis {
                disk_revision: DiskRevision::Missing,
                client_version: None,
                provider_cache_identity: None,
                etag: None,
            });
        };
        match ModelsCacheHeader::parse(&contents) {
            Ok(header) => Ok(header.write_basis(&contents)),
            Err(_) => Ok(CacheWriteBasis {
                disk_revision: DiskRevision::Opaque(contents),
                client_version: None,
                provider_cache_identity: None,
                etag: None,
            }),
        }
    }

    async fn save_internal(
        &self,
        cache: ModelsCache,
        replaced_identity: &str,
        file_lock: AtomicWriteLock,
    ) -> io::Result<()> {
        // Merge under the existing cross-process lock. Revisions are scoped to
        // the selected identity, so another identity's publication is not a
        // conflict and cannot erase its independently fetched catalog.
        let replaced_identity = replaced_identity.to_string();
        let cache_path = self.cache_path.clone();
        tokio::task::spawn_blocking(move || {
            // Ownership of the lock moves into the worker before its first wait.
            let _file_lock = file_lock;
            let existing = match std::fs::read(&cache_path) {
                Ok(contents) => serde_json::from_slice::<ModelsCacheFile>(&contents).ok(),
                Err(err) if err.kind() == ErrorKind::NotFound => None,
                Err(err) => return Err(err),
            };
            let mut current = cache;
            if let Some(file) = existing.as_ref() {
                // Allocate across the whole file so eviction/reinsertion cannot reuse
                // a revision and let an older in-flight writer pass an ABA check.
                current.revision = Some(
                    current
                        .revision
                        .unwrap_or(1)
                        .max(file.current.revision.unwrap_or(0).saturating_add(1)),
                );
            }
            let previous = existing
                .map(|file| {
                    std::iter::once(file.current)
                        .chain(file.previous)
                        .filter(|entry| {
                            entry.provider_cache_identity.is_some()
                                && entry.provider_cache_identity.as_deref()
                                    != Some(replaced_identity.as_str())
                                && entry.provider_cache_identity != current.provider_cache_identity
                        })
                        .take(MAX_CACHED_IDENTITIES - 1)
                        .collect()
                })
                .unwrap_or_default();
            let json = serde_json::to_vec_pretty(&ModelsCacheFile { current, previous })
                .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
            write_bytes_atomically(&cache_path, &json)
        })
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
        let current_basis = self.read_write_basis(&self.current_identity()).await?;
        f(&mut cache.fetched_at);
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        self.save_internal(cache, &self.current_identity(), _file_lock)
            .await
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
        let current_basis = self.read_write_basis(&self.current_identity()).await?;
        f(&mut cache);
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        self.save_internal(cache, &self.current_identity(), _file_lock)
            .await
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

/// Entry metadata decoded without the model list, which dominates the file.
/// It decides whether an entry is usable, renewable, or safely replaceable.
#[derive(Deserialize)]
struct ModelsCacheHeader {
    revision: Option<u64>,
    fetched_at: Option<DateTime<Utc>>,
    etag: Option<String>,
    client_version: Option<String>,
    provider_cache_identity: Option<String>,
    #[serde(default)]
    previous: Vec<Box<serde_json::value::RawValue>>,
}

// Keep the latest entry at the legacy top level for older readers, while newer
// readers can select one of the bounded, most recently published identities.
#[derive(Serialize, Deserialize)]
struct ModelsCacheFile {
    #[serde(flatten)]
    current: ModelsCache,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous: Vec<ModelsCache>,
}

impl ModelsCacheHeader {
    fn parse(contents: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(contents)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))
    }

    fn write_basis(&self, contents: &[u8]) -> CacheWriteBasis {
        CacheWriteBasis {
            disk_revision: self
                .revision
                .map(DiskRevision::Persisted)
                .unwrap_or_else(|| DiskRevision::Legacy(contents.to_vec())),
            client_version: self.client_version.clone(),
            provider_cache_identity: self.provider_cache_identity.clone(),
            etag: self.etag.clone(),
        }
    }

    /// Time since the entry was fetched; `None` when missing or in the future.
    fn age(&self) -> Option<Duration> {
        Utc::now()
            .signed_duration_since(self.fetched_at?)
            .to_std()
            .ok()
    }

    /// Returns `true` when the cache entry has not exceeded the configured TTL.
    fn is_fresh(&self, ttl: Duration) -> bool {
        !ttl.is_zero() && self.age().is_some_and(|age| age <= ttl)
    }
}

fn decode_cache(contents: &[u8]) -> io::Result<ModelsCache> {
    serde_json::from_slice(contents)
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))
}

fn next_revision(revision: &DiskRevision) -> u64 {
    match revision {
        DiskRevision::Persisted(revision) => revision.saturating_add(1),
        DiskRevision::Missing | DiskRevision::Legacy(_) | DiskRevision::Opaque(_) => 1,
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
    async fn independent_identity_publications_merge_without_cross_account_reads() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("models_cache.json");
        let first = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("first"),
        );
        let second =
            ModelsCacheManager::new(path, Duration::from_secs(300), fixed_identity("second"));
        let first_basis = first.write_basis_for_identity("first").await.unwrap();
        let second_basis = second.write_basis_for_identity("second").await.unwrap();
        let mut first_models = crate::bundled_models_response().unwrap().models;
        first_models[0].slug = "first-account-model".into();
        let mut second_models = first_models.clone();
        second_models[0].slug = "second-account-model".into();
        assert!(
            first
                .persist_cache_for_identity_if_unchanged(
                    &first_models,
                    Some("first-etag".into()),
                    "client".into(),
                    "first",
                    &first_basis
                )
                .await
        );
        assert!(
            second
                .persist_cache_for_identity_if_unchanged(
                    &second_models,
                    Some("second-etag".into()),
                    "client".into(),
                    "second",
                    &second_basis
                )
                .await
        );
        for _ in 0..3 {
            assert_eq!(
                first.load_fresh("client").await.unwrap().unwrap().models[0].slug,
                "first-account-model"
            );
            assert_eq!(
                second.load_fresh("client").await.unwrap().unwrap().models[0].slug,
                "second-account-model"
            );
        }
        assert!(
            first
                .load_fresh("different-client")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn identity_retention_is_bounded_and_reinsertion_rejects_stale_writers() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("models_cache.json");
        let first = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("first"),
        );
        first.persist_cache(&[], None, "client".into()).await;
        let stale = first.write_basis_for_identity("first").await.unwrap();
        for index in 0..MAX_CACHED_IDENTITIES {
            let manager = ModelsCacheManager::new(
                path.clone(),
                Duration::from_secs(300),
                fixed_identity(&format!("account-{index}")),
            );
            manager.persist_cache(&[], None, "client".into()).await;
        }
        assert!(first.load_fresh("client").await.unwrap().is_none());
        let file: ModelsCacheFile =
            serde_json::from_slice(&fs::read(&path).await.unwrap()).unwrap();
        assert_eq!(file.previous.len() + 1, MAX_CACHED_IDENTITIES);
        first.persist_cache(&[], None, "client".into()).await;
        assert!(
            !first
                .persist_cache_for_identity_if_unchanged(
                    &[],
                    None,
                    "client".into(),
                    "first",
                    &stale
                )
                .await
        );
        assert!(first.load_fresh("client").await.unwrap().is_some());
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
    async fn current_version_cache_with_invalid_models_is_not_reported_as_a_miss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let contents = serde_json::json!({
            "client_version": "client-one",
            "provider_cache_identity": "provider",
            "fetched_at": Utc::now(),
            "models": [{"slug": "incomplete-model"}],
        });
        std::fs::write(&path, contents.to_string()).expect("write invalid current cache");
        let manager =
            ModelsCacheManager::new(path, Duration::from_secs(300), fixed_identity("provider"));

        let error = manager
            .load_fresh("client-one")
            .await
            .expect_err("current-version model corruption must remain an error");
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
    async fn revision_alone_rejects_stale_writes_and_renewals() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = ModelsCacheManager::new(
            temp.path().join("models_cache.json"),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        cache
            .persist_cache(&[], Some("same-etag".into()), "client".into())
            .await;
        // Age the entry so its renewal is due and changes nothing but the revision.
        cache
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("age cache");
        let stale = cache
            .write_basis_for_identity("provider")
            .await
            .expect("basis");
        cache
            .renew_cache_ttl("client", "same-etag")
            .await
            .expect("renew");
        assert!(
            !cache
                .persist_cache_for_identity_if_unchanged(
                    &[],
                    Some("same-etag".into()),
                    "client".into(),
                    "provider",
                    &stale
                )
                .await
        );
        let error = cache
            .renew_cache_ttl_for_identity_if_unchanged("client", "same-etag", "provider", &stale)
            .await
            .expect_err("reject stale renewal");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        let persisted = cache
            .load_fresh("client")
            .await
            .expect("read")
            .expect("cache");
        assert_eq!(persisted.revision, Some(3));
        assert_eq!(persisted.etag.as_deref(), Some("same-etag"));
    }

    #[tokio::test]
    async fn renewal_rewrites_only_after_half_the_ttl_has_elapsed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let cache = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        cache
            .persist_cache(&[], Some("etag".into()), "client".into())
            .await;
        let persisted = std::fs::read(&path).expect("read persisted cache");

        cache
            .renew_cache_ttl("client", "etag")
            .await
            .expect("renew young entry");
        assert_eq!(
            std::fs::read(&path).expect("read cache"),
            persisted,
            "an entry with most of its TTL left must not be rewritten"
        );

        cache
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::seconds(151);
            })
            .await
            .expect("age cache past half its TTL");
        cache
            .renew_cache_ttl("client", "etag")
            .await
            .expect("renew aged entry");
        let renewed = cache
            .load_fresh("client")
            .await
            .expect("read")
            .expect("renewed cache");
        assert_eq!(renewed.revision, Some(3));
        assert!(Utc::now() - renewed.fetched_at < chrono::Duration::seconds(60));
    }

    #[tokio::test]
    async fn unusable_entries_are_rejected_before_decoding_models() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models_cache.json");
        let cache = ModelsCacheManager::new(
            path.clone(),
            Duration::from_secs(300),
            fixed_identity("provider"),
        );
        for (client_version, identity, fetched_at) in [
            ("other-client", "provider", Utc::now()),
            ("client", "other-provider", Utc::now()),
            (
                "client",
                "provider",
                Utc::now() - chrono::Duration::hours(1),
            ),
        ] {
            let contents = serde_json::json!({
                "revision": 1,
                "client_version": client_version,
                "provider_cache_identity": identity,
                "fetched_at": fetched_at,
                "models": [{"slug": "undecodable-model"}],
            });
            std::fs::write(&path, contents.to_string()).expect("write cache");

            assert!(
                cache
                    .load_fresh("client")
                    .await
                    .expect("an unusable entry is a miss, not a decode error")
                    .is_none()
            );
            assert_eq!(
                cache
                    .write_basis_for_identity("provider")
                    .await
                    .expect("basis")
                    .disk_revision,
                if identity == "provider" {
                    DiskRevision::Persisted(1)
                } else {
                    DiskRevision::Missing
                }
            );
        }
    }

    #[test]
    fn cancelled_write_keeps_file_lock_until_blocking_task_finishes() {
        use std::future::Future;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let path = temp.path().join("models_cache.json");
            let manager = ModelsCacheManager::new(
                path.clone(),
                Duration::from_secs(300),
                fixed_identity("provider"),
            );
            let file_lock = manager.acquire_file_lock().await.expect("file lock");
            let document = ModelsCache {
                revision: Some(1),
                fetched_at: Utc::now(),
                etag: Some("written".into()),
                client_version: Some("client".into()),
                provider_cache_identity: Some("provider".into()),
                models: Vec::new(),
            };
            let (release, wait) = std::sync::mpsc::channel();
            let (started, ready) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).expect("started");
                wait.recv().expect("release");
            });
            ready.await.expect("blocking pool occupied");
            let mut save = Box::pin(manager.save_internal(document, "provider", file_lock));
            std::future::poll_fn(|cx| {
                assert!(save.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(save);
            let contender = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(codex_file_system::atomic_write_lock_path(&path).expect("lock path"))
                .expect("lock file");
            let lock_result = contender.try_lock();
            // Release the pool before asserting so a regression cannot hang runtime shutdown.
            release.send(()).expect("release writer");
            assert!(matches!(
                lock_result,
                Err(std::fs::TryLockError::WouldBlock)
            ));
            blocker.await.expect("blocker");
            tokio::task::spawn_blocking(move || acquire_atomic_write_lock(&path))
                .await
                .expect("lock task")
                .expect("write released lock");
            let written = manager
                .load_fresh("client")
                .await
                .expect("read")
                .expect("written cache");
            assert_eq!(written.etag.as_deref(), Some("written"));
        });
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
