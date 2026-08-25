use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use hmac::Hmac;
use hmac::Mac;
use sha2::Sha256;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use tokio::sync::Mutex;

const KEY_FILE: &str = "validation-history.key";
const KEY_LOCK_FILE: &str = "validation-history.key.lock";
const KEY_VERSION: i64 = 1;
const CACHE_CAPACITY: usize = 256;
const PRUNE_EVERY_WRITE_BATCHES: u64 = 64;
const MAX_PERSISTED_ROWS: i64 = 8_192;
const FINE_GRAINED_EXPIRY_SECONDS: i64 = 30 * 24 * 60 * 60;
const READ_TIMEOUT: Duration = Duration::from_millis(40);
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
static WRITE_ERROR_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);
static WRITE_TIMEOUT_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

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
    entries: HashMap<StoredKey, Option<ValidationHistoryAggregate>>,
    generation: u64,
}

#[derive(Clone, Copy)]
struct ObservationDelta {
    completed: i64,
    below: i64,
    above: i64,
    sum: f64,
    squares: f64,
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
    successful_write_batches: Arc<AtomicU64>,
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
            successful_write_batches: Arc::new(AtomicU64::new(0)),
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
                return Ok(value);
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
        #[cfg(test)]
        run_lookup_before_cache_insert_hook(&key).await;
        self.cache_insert_if_generation(key, value.clone(), observed_generation)
            .await;
        Ok(value)
    }

    pub async fn record(
        &self,
        input: ValidationHistoryKey<'_>,
        observation: ValidationHistoryObservation,
    ) {
        self.record_batch(std::slice::from_ref(&input), observation)
            .await;
    }

    pub async fn record_batch(
        &self,
        inputs: &[ValidationHistoryKey<'_>],
        observation: ValidationHistoryObservation,
    ) {
        let keys = match inputs
            .iter()
            .map(|input| self.stored_key(input.clone()))
            .collect::<anyhow::Result<Vec<_>>>()
        {
            Ok(keys) => keys,
            Err(error) => {
                tracing::warn!(%error, "validation history fingerprint failed");
                return;
            }
        };
        if keys.is_empty() {
            return;
        }
        let delta = match observation {
            ValidationHistoryObservation::Completed { duration_ms } => {
                let duration = duration_ms as f64;
                ObservationDelta {
                    completed: 1,
                    below: 0,
                    above: 0,
                    sum: duration,
                    squares: duration * duration,
                }
            }
            ValidationHistoryObservation::Cancelled {
                elapsed_ms,
                threshold_ms,
            } if elapsed_ms >= threshold_ms => ObservationDelta {
                completed: 0,
                below: 0,
                above: 1,
                sum: 0.0,
                squares: 0.0,
            },
            ValidationHistoryObservation::Cancelled { .. } => ObservationDelta {
                completed: 0,
                below: 1,
                above: 0,
                sum: 0.0,
                squares: 0.0,
            },
        };
        let write = async {
            let mut transaction = self.pool.begin().await?;
            let mut query = batch_upsert_query(&keys, delta);
            query.build().execute(&mut *transaction).await?;
            transaction.commit().await
        };
        match tokio::time::timeout(WRITE_TIMEOUT, write).await {
            Ok(Ok(_)) => {
                let (write_error_recovered, write_timeout_recovered) = clear_write_warnings(
                    &WRITE_ERROR_WARNING_EMITTED,
                    &WRITE_TIMEOUT_WARNING_EMITTED,
                );
                if write_error_recovered {
                    tracing::info!("validation history writes recovered after an earlier error");
                }
                if write_timeout_recovered {
                    tracing::info!("validation history writes recovered after an earlier timeout");
                }
                self.invalidate_cache_batch(&keys).await;
                if self.should_prune_after_write() {
                    self.prune_persisted_rows().await;
                }
            }
            Ok(Err(error)) if !WRITE_ERROR_WARNING_EMITTED.swap(true, Ordering::Relaxed) => {
                tracing::warn!(%error, "validation history write failed")
            }
            Err(_) if !WRITE_TIMEOUT_WARNING_EMITTED.swap(true, Ordering::Relaxed) => {
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
        value: Option<ValidationHistoryAggregate>,
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

    async fn invalidate_cache_batch(&self, keys: &[StoredKey]) {
        if keys.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().await;
        cache.generation = cache.generation.wrapping_add(1);
        for key in keys {
            cache.entries.remove(key);
        }
    }

    fn should_prune_after_write(&self) -> bool {
        let write_batch = self
            .successful_write_batches
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        write_batch == 1 || write_batch.is_multiple_of(PRUNE_EVERY_WRITE_BATCHES)
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

fn batch_upsert_query(keys: &[StoredKey], delta: ObservationDelta) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        r#"
INSERT INTO validation_history_aggregates (
    scope_kind, repository_id, fingerprint_id, operation, ecosystem, breadth,
    model_version, key_version, completed_count, censored_below_count,
    censored_above_count, duration_sum_ms, duration_sum_squares_ms, updated_at
) "#,
    );
    query.push_values(keys, |mut row, key| {
        row.push_bind(key.scope)
            .push_bind(&key.repository_id)
            .push_bind(&key.fingerprint_id)
            .push_bind(key.operation)
            .push_bind(key.ecosystem)
            .push_bind(key.breadth)
            .push_bind(key.model_version)
            .push_bind(key.key_version)
            .push_bind(delta.completed)
            .push_bind(delta.below)
            .push_bind(delta.above)
            .push_bind(delta.sum)
            .push_bind(delta.squares)
            .push("unixepoch()");
    });
    query.push(
        r#"
ON CONFLICT DO UPDATE SET
    completed_count = completed_count + excluded.completed_count,
    censored_below_count = censored_below_count + excluded.censored_below_count,
    censored_above_count = censored_above_count + excluded.censored_above_count,
    duration_sum_ms = duration_sum_ms + excluded.duration_sum_ms,
    duration_sum_squares_ms = duration_sum_squares_ms + excluded.duration_sum_squares_ms,
    updated_at = excluded.updated_at
        "#,
    );
    query
}

fn clear_write_warnings(error_warning: &AtomicBool, timeout_warning: &AtomicBool) -> (bool, bool) {
    (
        error_warning.swap(false, Ordering::Relaxed),
        timeout_warning.swap(false, Ordering::Relaxed),
    )
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
        Ok(bytes) => return key_from_bytes(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let secret = rand::random::<[u8; 32]>();
    let temporary_path =
        codex_home.join(format!(".{KEY_FILE}.{:032x}.tmp", rand::random::<u128>()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);

    let mut file = options.open(&temporary_path).await?;
    use tokio::io::AsyncWriteExt;
    if let Err(error) = async {
        file.write_all(&secret).await?;
        file.sync_all().await
    }
    .await
    {
        let _ = tokio::fs::remove_file(&temporary_path).await;
        return Err(error.into());
    }
    drop(file);

    let install_result = tokio::fs::hard_link(&temporary_path, &path).await;
    let result = match install_result {
        Ok(()) => Ok(secret),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = tokio::fs::read(path).await?;
            key_from_bytes(bytes)
        }
        Err(error) if should_fallback_from_hard_link(error.kind()) => {
            install_key_with_locked_rename(codex_home, &temporary_path, &path, secret).await
        }
        Err(error) => Err(error.into()),
    };
    let _ = tokio::fs::remove_file(&temporary_path).await;
    result
}

fn key_from_bytes(bytes: Vec<u8>) -> anyhow::Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid key length"))
}

fn should_fallback_from_hard_link(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
    )
}

async fn install_key_with_locked_rename(
    codex_home: &Path,
    temporary_path: &Path,
    path: &Path,
    secret: [u8; 32],
) -> anyhow::Result<[u8; 32]> {
    let lock_path = codex_home.join(KEY_LOCK_FILE);
    let temporary_path = temporary_path.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);

        let lock_file = options
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;
        lock_file
            .lock()
            .with_context(|| format!("failed to lock {}", lock_path.display()))?;

        match std::fs::read(&path) {
            Ok(bytes) => return key_from_bytes(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::rename(&temporary_path, &path).with_context(|| {
            format!(
                "failed to atomically install {} as {}",
                temporary_path.display(),
                path.display()
            )
        })?;
        Ok(secret)
    })
    .await
    .context("validation history key install task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn successful_write_clears_each_warning_class_for_future_failures() {
        let error_warning = AtomicBool::new(true);
        let timeout_warning = AtomicBool::new(true);

        assert_eq!(
            clear_write_warnings(&error_warning, &timeout_warning),
            (true, true)
        );
        assert!(!error_warning.load(Ordering::Relaxed));
        assert!(!timeout_warning.load(Ordering::Relaxed));
        assert_eq!(
            clear_write_warnings(&error_warning, &timeout_warning),
            (false, false)
        );
    }

    #[tokio::test]
    async fn concurrent_key_creation_never_exposes_a_partial_key() {
        let codex_home = tempfile::tempdir().expect("temporary codex home");
        let (left, right) = tokio::join!(
            load_or_create_key(codex_home.path()),
            load_or_create_key(codex_home.path())
        );
        let left = left.expect("left key");
        let right = right.expect("right key");
        assert_eq!(left, right);
        assert_eq!(
            tokio::fs::read(codex_home.path().join(KEY_FILE))
                .await
                .expect("installed key"),
            left
        );
    }

    #[test]
    fn hard_link_portability_errors_use_the_locked_rename_fallback() {
        assert!(should_fallback_from_hard_link(
            std::io::ErrorKind::Unsupported
        ));
        assert!(should_fallback_from_hard_link(
            std::io::ErrorKind::PermissionDenied
        ));
        assert!(!should_fallback_from_hard_link(
            std::io::ErrorKind::AlreadyExists
        ));
        assert!(!should_fallback_from_hard_link(std::io::ErrorKind::Other));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn locked_rename_fallback_installs_one_complete_key_concurrently() {
        let codex_home = tempfile::tempdir().expect("temporary codex home");
        let path = codex_home.path().join(KEY_FILE);
        let left_temporary = codex_home.path().join("left-key.tmp");
        let right_temporary = codex_home.path().join("right-key.tmp");
        let left_secret = [1; 32];
        let right_secret = [2; 32];
        tokio::fs::write(&left_temporary, left_secret)
            .await
            .expect("write left temporary key");
        tokio::fs::write(&right_temporary, right_secret)
            .await
            .expect("write right temporary key");

        let (left, right) = tokio::join!(
            install_key_with_locked_rename(codex_home.path(), &left_temporary, &path, left_secret,),
            install_key_with_locked_rename(
                codex_home.path(),
                &right_temporary,
                &path,
                right_secret,
            )
        );
        let left = left.expect("left fallback result");
        let right = right.expect("right fallback result");

        assert_eq!(left, right);
        assert_eq!(tokio::fs::read(&path).await.expect("installed key"), left);
    }

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

    fn missing_history_key() -> ValidationHistoryKey<'static> {
        ValidationHistoryKey {
            fingerprint: b"missing-fingerprint",
            ..history_key()
        }
    }

    fn test_store(pool: sqlx::SqlitePool) -> ValidationHistoryStore {
        ValidationHistoryStore {
            pool: Arc::new(pool),
            secret: Some(Arc::new([7; 32])),
            cache: Arc::new(Mutex::new(ValidationHistoryCache::default())),
            successful_write_batches: Arc::new(AtomicU64::new(0)),
        }
    }

    #[test]
    fn batch_upsert_builds_one_statement_for_all_keys() {
        let store_key = StoredKey {
            scope: 0,
            repository_id: "repository".to_string(),
            fingerprint_id: "fingerprint".to_string(),
            operation: 1,
            ecosystem: 2,
            breadth: 3,
            model_version: 4,
            key_version: KEY_VERSION,
        };
        let mut second_key = store_key.clone();
        second_key.fingerprint_id = "second".to_string();
        let keys = [store_key, second_key];
        let query = batch_upsert_query(
            &keys,
            ObservationDelta {
                completed: 1,
                below: 0,
                above: 0,
                sum: 10.0,
                squares: 100.0,
            },
        );

        let sql = query.sql();
        let sql: &str = sql.as_ref();
        assert_eq!(sql.matches("INSERT INTO").count(), 1);
        assert_eq!(sql.matches("unixepoch()").count(), 2);
    }

    #[tokio::test]
    async fn batch_invalidation_uses_one_generation_change() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        let store = test_store(pool);
        let first = store.stored_key(history_key()).expect("first stored key");
        let mut second = first.clone();
        second.fingerprint_id = "second".to_string();
        {
            let mut cache = store.cache.lock().await;
            cache.entries.insert(first.clone(), None);
            cache.entries.insert(second.clone(), None);
        }

        store
            .invalidate_cache_batch(&[first.clone(), second.clone()])
            .await;

        let cache = store.cache.lock().await;
        assert_eq!(cache.generation, 1);
        assert!(!cache.entries.contains_key(&first));
        assert!(!cache.entries.contains_key(&second));
    }

    #[tokio::test]
    async fn pruning_is_amortized_across_successful_write_batches() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        let store = test_store(pool);

        assert!(store.should_prune_after_write());
        for _ in 2..PRUNE_EVERY_WRITE_BATCHES {
            assert!(!store.should_prune_after_write());
        }
        assert!(store.should_prune_after_write());
    }

    #[tokio::test]
    async fn lookup_caches_a_missing_aggregate() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        create_history_table(&pool).await;
        let store = test_store(pool.clone());

        assert_eq!(
            store.lookup(history_key()).await.expect("first lookup"),
            None
        );
        sqlx::query("DROP TABLE validation_history_aggregates")
            .execute(&pool)
            .await
            .expect("drop history table");
        assert_eq!(
            store.lookup(history_key()).await.expect("cached lookup"),
            None
        );
    }

    async fn create_history_table(pool: &sqlx::SqlitePool) {
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
        .execute(pool)
        .await
        .expect("history table");
    }

    #[tokio::test]
    async fn concurrent_record_prevents_stale_lookup_cache_publication() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        create_history_table(&pool).await;
        let store = test_store(pool);
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

    #[tokio::test]
    async fn concurrent_record_prevents_stale_negative_cache_publication() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        create_history_table(&pool).await;
        let store = test_store(pool);
        let stored_key = store.stored_key(missing_history_key()).expect("stored key");
        let reached = Arc::new(tokio::sync::Barrier::new(2));
        let resume = Arc::new(tokio::sync::Barrier::new(2));
        install_lookup_before_cache_insert_hook(
            stored_key.clone(),
            Arc::clone(&reached),
            Arc::clone(&resume),
        );

        let lookup_store = store.clone();
        let lookup = tokio::spawn(async move { lookup_store.lookup(missing_history_key()).await });
        reached.wait().await;
        store
            .record(
                missing_history_key(),
                ValidationHistoryObservation::Completed { duration_ms: 20 },
            )
            .await;
        resume.wait().await;

        assert_eq!(lookup.await.expect("lookup task").expect("lookup"), None);
        assert!(!store.cache.lock().await.entries.contains_key(&stored_key));
        assert_eq!(
            store
                .lookup(missing_history_key())
                .await
                .expect("fresh lookup")
                .expect("updated aggregate")
                .completed_count,
            1
        );
    }
}
