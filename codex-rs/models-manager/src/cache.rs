use chrono::DateTime;
use chrono::Utc;
use codex_file_system::AtomicWriteLock;
use codex_file_system::try_acquire_atomic_write_lock;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheWriteOutcome {
    Written,
    RevisionConflict,
    IdentityChanged,
    IoFailure,
}

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
        let cache_path = self.cache_path.clone();
        // Persistence is optional: never leave a blocking worker waiting for
        // another process to release this cache's transaction lock.
        tokio::task::spawn_blocking(move || {
            try_acquire_atomic_write_lock(&cache_path)?.ok_or_else(|| {
                io::Error::new(ErrorKind::WouldBlock, "models cache transaction is busy")
            })
        })
        .await
        .map_err(|err| io::Error::other(format!("models cache lock task failed: {err}")))?
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
        let Some(snapshot) = self.read_contents().await? else {
            return Ok(None);
        };
        let Some(entry) = select_cache_entry(&snapshot, expected_identity)? else {
            return Ok(None);
        };
        // Older caches can contain model shapes that no longer decode. Reject
        // unusable entries from their metadata before decoding any model.
        let header = entry.header?;
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
        let cache = decode_cache(entry.contents)?;
        info!(
            cache_path = %self.cache_path.display(),
            cache_ttl_secs = self.cache_ttl.as_secs(),
            "models cache: cache hit"
        );
        Ok(self.identity_is_current(expected_identity).then_some(cache))
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


    pub(crate) async fn persist_cache_for_identity_outcome(
        &self,
        models: &[ModelInfo],
        etag: Option<String>,
        client_version: String,
        expected_identity: &str,
        expected_basis: &CacheWriteBasis,
    ) -> CacheWriteOutcome {
        let _permit = match self.acquire_io_permit().await {
            Ok(permit) => permit,
            Err(err) => {
                error!("failed to acquire models cache I/O gate: {err}");
                return CacheWriteOutcome::IoFailure;
            }
        };
        let _file_lock = match self.acquire_file_lock().await {
            Ok(lock) => lock,
            Err(err) => {
                error!("failed to acquire models cache file lock: {err}");
                return CacheWriteOutcome::IoFailure;
            }
        };
        let current_identity = self.current_identity();
        if current_identity != expected_identity {
            info!(
                cache_path = %self.cache_path.display(),
                mismatch_category = "provider_cache_identity",
                "models cache: skipped write after identity changed"
            );
            return CacheWriteOutcome::IdentityChanged;
        }
        let contents = match self.read_contents().await {
            Ok(contents) => contents,
            Err(err) => {
                error!("failed to read models cache before write: {err}");
                return CacheWriteOutcome::IoFailure;
            }
        };
        let current_basis = match write_basis_from_snapshot(contents.as_deref(), expected_identity) {
            Ok(basis) => basis,
            Err(err) => {
                error!("failed to re-read models cache before write: {err}");
                return CacheWriteOutcome::IoFailure;
            }
        };
        if &current_basis != expected_basis {
            info!(
                cache_path = %self.cache_path.display(),
                "models cache: skipped stale cross-process write"
            );
            return CacheWriteOutcome::RevisionConflict;
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
            return CacheWriteOutcome::IdentityChanged;
        }
        if let Err(err) = self
            .save_internal(cache, expected_identity, contents, _file_lock)
            .await
        {
            error!("failed to write models cache: {err}");
            return CacheWriteOutcome::IoFailure;
        }
        CacheWriteOutcome::Written
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
        let snapshot = self
            .read_contents()
            .await?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "cache identity not found"))?;
        let entry = select_cache_entry(&snapshot, expected_identity)?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "cache identity not found"))?;
        let header = entry.header?;
        let current_basis = header.write_basis(entry.contents);
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
        let mut cache = decode_cache(entry.contents)?;
        cache.fetched_at = Utc::now();
        cache.revision = Some(next_revision(&current_basis.disk_revision));
        if !self.identity_is_current(expected_identity) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "cache identity changed during TTL renewal",
            ));
        }
        self.save_internal(cache, expected_identity, Some(snapshot), _file_lock)
            .await
    }

    async fn read_contents(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(&self.cache_path).await {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }


    async fn read_write_basis(&self, expected_identity: &str) -> io::Result<CacheWriteBasis> {
        let contents = self.read_contents().await?;
        write_basis_from_snapshot(contents.as_deref(), expected_identity)
    }

    async fn save_internal(
        &self,
        cache: ModelsCache,
        replaced_identity: &str,
        contents: Option<Vec<u8>>,
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
            // Reuse the exact snapshot checked under this same lock. Moving it
            // with the lock keeps the writer protected even if its waiter is cancelled.
            let existing = contents
                .as_deref()
                .and_then(|contents| serde_json::from_slice::<serde_json::Value>(contents).ok());
            // Decode siblings independently, just as the read path does. A
            // readable revision still participates in anti-ABA allocation even
            // when that entry's model schema is incompatible.
            let mut entries = Vec::new();
            if let Some(mut current) = existing {
                let previous = current
                    .as_object_mut()
                    .and_then(|object| object.remove("previous"));
                entries.push(current);
                if let Some(serde_json::Value::Array(previous)) = previous {
                    entries.extend(previous);
                }
            }
            let last_revision = entries
                .iter()
                .filter_map(|entry| entry.get("revision")?.as_u64())
                .max()
                .unwrap_or(0);
            let mut current = cache;
            current.revision = Some(
                current
                    .revision
                    .unwrap_or(1)
                    .max(last_revision.saturating_add(1)),
            );
            let previous = entries
                .into_iter()
                .filter(|entry| {
                    let identity = entry
                        .get("provider_cache_identity")
                        .and_then(|value| value.as_str());
                    identity.is_some()
                        && identity != Some(replaced_identity.as_str())
                        && identity != current.provider_cache_identity.as_deref()
                })
                .filter_map(|entry| serde_json::from_value::<ModelsCache>(entry).ok())
                .take(MAX_CACHED_IDENTITIES - 1)
                .collect();
            let json = serde_json::to_vec(&ModelsCacheFile { current, previous })
                .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
            write_bytes_atomically(&cache_path, &json)
        })
        .await
        .map_err(|err| io::Error::other(format!("models cache write task failed: {err}")))?
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
struct ModelsCacheHeader<'a> {
    revision: Option<u64>,
    fetched_at: Option<DateTime<Utc>>,
    etag: Option<String>,
    client_version: Option<String>,
    provider_cache_identity: Option<String>,
    #[serde(borrow, default)]
    previous: Vec<&'a serde_json::value::RawValue>,
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

impl<'a> ModelsCacheHeader<'a> {
    fn parse(contents: &'a [u8]) -> io::Result<Self> {
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

struct CacheEntry<'a> {
    contents: &'a [u8],
    header: io::Result<ModelsCacheHeader<'a>>,
}

/// Select borrowed bytes and metadata from a single owned file snapshot.
fn select_cache_entry<'a>(
    contents: &'a [u8],
    expected_identity: &str,
) -> io::Result<Option<CacheEntry<'a>>> {
    let header = match ModelsCacheHeader::parse(contents) {
        Ok(header) => header,
        Err(error) => {
            // Opaque bytes still participate in stale-writer rejection. Loads
            // propagate the original corruption instead of reporting a cache miss.
            return Ok(Some(CacheEntry {
                contents,
                header: Err(error),
            }));
        }
    };
    if header.provider_cache_identity.as_deref() == Some(expected_identity) {
        return Ok(Some(CacheEntry {
            contents,
            header: Ok(header),
        }));
    }
    for previous in header.previous {
        let contents = previous.get().as_bytes();
        let header = ModelsCacheHeader::parse(contents)?;
        if header.provider_cache_identity.as_deref() == Some(expected_identity) {
            return Ok(Some(CacheEntry {
                contents,
                header: Ok(header),
            }));
        }
    }
    Ok(None)
}

fn write_basis_from_snapshot(
    contents: Option<&[u8]>,
    expected_identity: &str,
) -> io::Result<CacheWriteBasis> {
    if let Some(contents) = contents
        && let Some(entry) = select_cache_entry(contents, expected_identity)?
    {
        return Ok(match entry.header {
            Ok(header) => header.write_basis(entry.contents),
            Err(_) => CacheWriteBasis {
                disk_revision: DiskRevision::Opaque(entry.contents.to_vec()),
                client_version: None,
                provider_cache_identity: None,
                etag: None,
            },
        });
    }
    Ok(CacheWriteBasis {
        disk_revision: DiskRevision::Missing,
        client_version: None,
        provider_cache_identity: None,
        etag: None,
    })
}

fn decode_cache(contents: &[u8]) -> io::Result<ModelsCache> {
    let cache: ModelsCache = serde_json::from_slice(contents)
        .map_err(|err| io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
    for model in &cache.models {
        model
            .validate_capabilities()
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
    }
    Ok(cache)
}

fn next_revision(revision: &DiskRevision) -> u64 {
    match revision {
        DiskRevision::Persisted(revision) => revision.saturating_add(1),
        DiskRevision::Missing | DiskRevision::Legacy(_) | DiskRevision::Opaque(_) => 1,
    }
}

