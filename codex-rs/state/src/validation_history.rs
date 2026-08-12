use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;
use sqlx::Row;
use tokio::sync::Mutex;

const KEY_FILE: &str = "validation-history.key";
const KEY_VERSION: i64 = 1;
const CACHE_CAPACITY: usize = 256;
const MAX_PERSISTED_ROWS: i64 = 8_192;
const FINE_GRAINED_EXPIRY_SECONDS: i64 = 30 * 24 * 60 * 60;
const READ_TIMEOUT: Duration = Duration::from_millis(40);
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
static WRITE_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(i64)]
pub enum ValidationHistoryScope {
    RepositoryFingerprint = 0,
    RepositoryFamily = 1,
    GlobalFamily = 2,
}

#[derive(Clone, Debug)]
pub struct ValidationHistoryKey<'a> {
    pub scope: ValidationHistoryScope,
    pub repository: Option<&'a [u8]>,
    pub fingerprint: &'a [u8],
    pub operation: i64,
    pub ecosystem: i64,
    pub breadth: i64,
    pub model_version: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ValidationHistoryAggregate {
    pub completed_count: u64,
    pub censored_below_count: u64,
    pub censored_above_count: u64,
    pub duration_sum_ms: f64,
    pub duration_sum_squares_ms: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum ValidationHistoryObservation {
    Completed { duration_ms: u64 },
    Cancelled { elapsed_ms: u64, threshold_ms: u64 },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StoredKey {
    scope: i64,
    repository_id: String,
    fingerprint_id: String,
    operation: i64,
    ecosystem: i64,
    breadth: i64,
    model_version: i64,
    key_version: i64,
}

#[derive(Default)]
struct ValidationHistoryCache {
    entries: HashMap<StoredKey, ValidationHistoryAggregate>,
    generation: u64,
}

#[cfg(test)]
#[derive(Clone)]
struct LookupBeforeCacheInsertHook {
    reached: Arc<tokio::sync::Barrier>,
    resume: Arc<tokio::sync::Barrier>,
}

#[cfg(test)]
static LOOKUP_BEFORE_CACHE_INSERT_HOOKS: OnceLock<
    std::sync::Mutex<HashMap<StoredKey, LookupBeforeCacheInsertHook>>,
> = OnceLock::new();

#[derive(Clone)]
pub struct ValidationHistoryStore {
    pool: Arc<sqlx::SqlitePool>,
    secret: Option<Arc<[u8; 32]>>,
    cache: Arc<Mutex<ValidationHistoryCache>>,
}

impl ValidationHistoryStore {
    pub(crate) async fn new(pool: Arc<sqlx::SqlitePool>, codex_home: &Path) -> Self {
        let secret = match load_or_create_key(codex_home).await {
            Ok(secret) => Some(Arc::new(secret)),
            Err(error) => {
                tracing::warn!(%error, "validation history key is unavailable; predictions will fail open");
                None
            }
        };
        Self {
            pool,
            secret,
            cache: Arc::new(Mutex::new(ValidationHistoryCache::default())),
        }
    }

    pub async fn lookup(
        &self,
        input: ValidationHistoryKey<'_>,
    ) -> anyhow::Result<Option<ValidationHistoryAggregate>> {
        let key = self.stored_key(input)?;
        let observed_generation = {
            let cache = self.cache.lock().await;
            if let Some(value) = cache.entries.get(&key).cloned() {
                return Ok(Some(value));
            }
            cache.generation
        };
        let query = sqlx::query(
            r#"
SELECT completed_count, censored_below_count, censored_above_count,
       duration_sum_ms, duration_sum_squares_ms
FROM validation_history_aggregates
WHERE scope_kind = ? AND repository_id = ? AND fingerprint_id = ?
  AND operation = ? AND ecosystem = ? AND breadth = ?
  AND model_version = ? AND key_version = ?
            "#,
        )
        .bind(key.scope)
        .bind(&key.repository_id)
        .bind(&key.fingerprint_id)
        .bind(key.operation)
        .bind(key.ecosystem)
        .bind(key.breadth)
        .bind(key.model_version)
        .bind(key.key_version)
        .fetch_optional(self.pool.as_ref());
        let row = tokio::time::timeout(READ_TIMEOUT, query)
            .await
            .context("validation history read timed out")??;
        let value = row.map(|row| ValidationHistoryAggregate {
            completed_count: row.get::<i64, _>(0).max(0) as u64,
            censored_below_count: row.get::<i64, _>(1).max(0) as u64,
            censored_above_count: row.get::<i64, _>(2).max(0) as u64,
            duration_sum_ms: row.get(3),
            duration_sum_squares_ms: row.get(4),
        });
        if let Some(value) = value.clone() {
            #[cfg(test)]
            run_lookup_before_cache_insert_hook(&key).await;
            self.cache_insert_if_generation(key, value, observed_generation)
                .await;
        }
        Ok(value)
    }

    pub async fn record(
        &self,
        input: ValidationHistoryKey<'_>,
        observation: ValidationHistoryObservation,
    ) {
        let key = match self.stored_key(input) {
            Ok(key) => key,
            Err(error) => {
                tracing::warn!(%error, "validation history fingerprint failed");
                return;
            }
        };
        let (completed, below, above, sum, squares) = match observation {
            ValidationHistoryObservation::Completed { duration_ms } => {
                let duration = duration_ms as f64;
                (1_i64, 0_i64, 0_i64, duration, duration * duration)
            }
            ValidationHistoryObservation::Cancelled {
                elapsed_ms,
                threshold_ms,
            } if elapsed_ms >= threshold_ms => (0, 0, 1, 0.0, 0.0),
            ValidationHistoryObservation::Cancelled { .. } => (0, 1, 0, 0.0, 0.0),
        };
        let query = sqlx::query(
            r#"
INSERT INTO validation_history_aggregates (
    scope_kind, repository_id, fingerprint_id, operation, ecosystem, breadth,
    model_version, key_version, completed_count, censored_below_count,
    censored_above_count, duration_sum_ms, duration_sum_squares_ms, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, unixepoch())
ON CONFLICT DO UPDATE SET
    completed_count = completed_count + excluded.completed_count,
    censored_below_count = censored_below_count + excluded.censored_below_count,
    censored_above_count = censored_above_count + excluded.censored_above_count,
    duration_sum_ms = duration_sum_ms + excluded.duration_sum_ms,
    duration_sum_squares_ms = duration_sum_squares_ms + excluded.duration_sum_squares_ms,
    updated_at = excluded.updated_at
            "#,
        )
        .bind(key.scope)
        .bind(&key.repository_id)
        .bind(&key.fingerprint_id)
        .bind(key.operation)
        .bind(key.ecosystem)
        .bind(key.breadth)
        .bind(key.model_version)
        .bind(key.key_version)
        .bind(completed)
        .bind(below)
        .bind(above)
        .bind(sum)
        .bind(squares)
        .execute(self.pool.as_ref());
        match tokio::time::timeout(WRITE_TIMEOUT, query).await {
            Ok(Ok(_)) => {
                self.invalidate_cache(&key).await;
                self.prune_persisted_rows().await;
            }
            Ok(Err(error)) if !WRITE_WARNING_EMITTED.swap(true, Ordering::Relaxed) => {
                tracing::warn!(%error, "validation history write failed")
            }
            Err(_) if !WRITE_WARNING_EMITTED.swap(true, Ordering::Relaxed) => {
                tracing::warn!("validation history write timed out")
            }
            Ok(Err(_)) | Err(_) => {}
        }
    }

    fn stored_key(&self, input: ValidationHistoryKey<'_>) -> anyhow::Result<StoredKey> {
        let secret = self
            .secret
            .as_deref()
            .context("validation history key is unavailable")?;
        Ok(StoredKey {
            scope: input.scope as i64,
            repository_id: input
                .repository
                .map(|value| keyed_identifier(secret, b"repository", value))
                .transpose()?
                .unwrap_or_default(),
            fingerprint_id: keyed_identifier(secret, b"fingerprint", input.fingerprint)?,
            operation: input.operation,
            ecosystem: input.ecosystem,
            breadth: input.breadth,
            model_version: input.model_version,
            key_version: KEY_VERSION,
        })
    }

    async fn cache_insert_if_generation(
        &self,
        key: StoredKey,
        value: ValidationHistoryAggregate,
        observed_generation: u64,
    ) {
        let mut cache = self.cache.lock().await;
        if cache.generation != observed_generation {
            return;
        }
        if cache.entries.len() >= CACHE_CAPACITY
            && let Some(victim) = cache.entries.keys().next().cloned()
        {
            cache.entries.remove(&victim);
        }
        cache.entries.insert(key, value);
    }

    async fn invalidate_cache(&self, key: &StoredKey) {
        let mut cache = self.cache.lock().await;
        cache.generation = cache.generation.wrapping_add(1);
        cache.entries.remove(key);
    }

    async fn prune_persisted_rows(&self) {
        let query = sqlx::query(
            r#"
DELETE FROM validation_history_aggregates
WHERE (scope_kind = ? AND updated_at < unixepoch() - ?)
   OR rowid IN (
       SELECT rowid
       FROM validation_history_aggregates
       ORDER BY updated_at DESC
       LIMIT -1 OFFSET ?
   )
            "#,
        )
        .bind(ValidationHistoryScope::RepositoryFingerprint as i64)
        .bind(FINE_GRAINED_EXPIRY_SECONDS)
        .bind(MAX_PERSISTED_ROWS)
        .execute(self.pool.as_ref());
        match tokio::time::timeout(WRITE_TIMEOUT, query).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::debug!(%error, "validation history pruning failed"),
            Err(_) => tracing::debug!("validation history pruning timed out"),
        }
    }
}

#[cfg(test)]
fn install_lookup_before_cache_insert_hook(
    key: StoredKey,
    reached: Arc<tokio::sync::Barrier>,
    resume: Arc<tokio::sync::Barrier>,
) {
    LOOKUP_BEFORE_CACHE_INSERT_HOOKS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, LookupBeforeCacheInsertHook { reached, resume });
}

#[cfg(test)]
async fn run_lookup_before_cache_insert_hook(key: &StoredKey) {
    let hook = LOOKUP_BEFORE_CACHE_INSERT_HOOKS.get().and_then(|hooks| {
        hooks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key)
    });
    if let Some(hook) = hook {
        hook.reached.wait().await;
        hook.resume.wait().await;
    }
}

fn keyed_identifier(secret: &[u8; 32], domain: &[u8], value: &[u8]) -> anyhow::Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).context("invalid HMAC key")?;
    mac.update(domain);
    mac.update(&[0]);
    mac.update(value);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

async fn load_or_create_key(codex_home: &Path) -> anyhow::Result<[u8; 32]> {
    let path = codex_home.join(KEY_FILE);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            return bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length"));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let secret = rand::random::<[u8; 32]>();
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path).await {
        Ok(mut file) => {
            use tokio::io::AsyncWriteExt;
            file.write_all(&secret).await?;
            file.sync_all().await?;
            Ok(secret)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = tokio::fs::read(path).await?;
            bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid key length"))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn history_key() -> ValidationHistoryKey<'static> {
        ValidationHistoryKey {
            scope: ValidationHistoryScope::RepositoryFingerprint,
            repository: Some(b"repository"),
            fingerprint: b"fingerprint",
            operation: 1,
            ecosystem: 2,
            breadth: 3,
            model_version: 4,
        }
    }

    #[tokio::test]
    async fn concurrent_record_prevents_stale_lookup_cache_publication() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        sqlx::query(
            r#"
CREATE TABLE validation_history_aggregates (
    scope_kind INTEGER NOT NULL,
    repository_id TEXT NOT NULL,
    fingerprint_id TEXT NOT NULL,
    operation INTEGER NOT NULL,
    ecosystem INTEGER NOT NULL,
    breadth INTEGER NOT NULL,
    model_version INTEGER NOT NULL,
    key_version INTEGER NOT NULL,
    completed_count INTEGER NOT NULL,
    censored_below_count INTEGER NOT NULL,
    censored_above_count INTEGER NOT NULL,
    duration_sum_ms REAL NOT NULL,
    duration_sum_squares_ms REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (
        scope_kind, repository_id, fingerprint_id, operation,
        ecosystem, breadth, model_version, key_version
    )
)
            "#,
        )
        .execute(&pool)
        .await
        .expect("history table");
        let store = ValidationHistoryStore {
            pool: Arc::new(pool),
            secret: Some(Arc::new([7; 32])),
            cache: Arc::new(Mutex::new(ValidationHistoryCache::default())),
        };
        store
            .record(
                history_key(),
                ValidationHistoryObservation::Completed { duration_ms: 10 },
            )
            .await;
        let stored_key = store.stored_key(history_key()).expect("stored key");
        let reached = Arc::new(tokio::sync::Barrier::new(2));
        let resume = Arc::new(tokio::sync::Barrier::new(2));
        install_lookup_before_cache_insert_hook(
            stored_key.clone(),
            Arc::clone(&reached),
            Arc::clone(&resume),
        );

        let lookup_store = store.clone();
        let lookup = tokio::spawn(async move { lookup_store.lookup(history_key()).await });
        reached.wait().await;
        store
            .record(
                history_key(),
                ValidationHistoryObservation::Completed { duration_ms: 20 },
            )
            .await;
        resume.wait().await;

        let stale = lookup
            .await
            .expect("lookup task")
            .expect("lookup")
            .expect("seeded aggregate");
        assert_eq!(stale.completed_count, 1);
        assert!(!store.cache.lock().await.entries.contains_key(&stored_key));

        let fresh = store
            .lookup(history_key())
            .await
            .expect("fresh lookup")
            .expect("updated aggregate");
        assert_eq!(fresh.completed_count, 2);
    }
}
