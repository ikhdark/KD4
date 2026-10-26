use crate::AgentJob;
use crate::AgentJobCreateParams;
use crate::AgentJobItem;
use crate::AgentJobItemCreateParams;
use crate::AgentJobItemStatus;
use crate::AgentJobProgress;
use crate::AgentJobStatus;
use crate::LOGS_DB_FILENAME;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::STATE_DB_FILENAME;
use crate::SortKey;
use crate::ThreadMetadata;
use crate::ThreadMetadataBuilder;
use crate::ThreadsPage;
use crate::migrations::migrate_tolerating_concurrent_initializers;
use crate::migrations::repair_legacy_recency_migration_version;
use crate::migrations::repair_legacy_validation_index_migration_order;
use crate::migrations::runtime_logs_migrator;
use crate::migrations::runtime_migrator_for_pool;
use crate::migrations::runtime_state_migrator;
use crate::model::AgentJobRow;
use crate::model::ThreadRow;
use crate::model::anchor_from_item;
use crate::model::datetime_to_epoch_millis;
use crate::model::datetime_to_epoch_seconds;
use crate::model::epoch_millis_to_datetime;
use crate::telemetry::DbKind;
use crate::telemetry::DbTelemetry;
use crate::telemetry::DbTelemetryHandle;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use log::LevelFilter;
use serde_json::Value;
use sqlx::Acquire;
use sqlx::ConnectOptions;
use sqlx::QueryBuilder;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqliteAutoVacuum;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use std::collections::BTreeSet;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;
use std::time::Instant;
use tracing::warn;
use uuid::Uuid;

mod agent_jobs;
mod backfill;
mod external_agent_config_imports;
pub use external_agent_config_imports::{ExternalAgentConfigImportFailureRecord, ExternalAgentConfigImportHistoryRecord, ExternalAgentConfigImportSuccessRecord};

mod logs;
pub use logs::LogReader;
pub(crate) use logs::LogRetentionScope;
mod projects;
mod recovery;
mod remote_control;
mod thread_queues;
mod threads;

async fn file_modified_time_utc(path: &Path) -> Option<DateTime<Utc>> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let updated_at: DateTime<Utc> = modified.into();
    Some(updated_at)
}

pub use recovery::RuntimeDbBackup;
pub use recovery::backup_runtime_db_for_fresh_start;
pub use recovery::is_sqlite_corruption_error;
pub use recovery::runtime_db_path_for_corruption_error;
pub use recovery::sqlite_error_detail_is_corruption;
pub use recovery::sqlite_error_detail_is_lock;
pub use remote_control::RemoteControlEnrollmentRecord;
pub use threads::ThreadFilterOptions;
pub use threads::ThreadSpawnEdgeWriteError;

// "Partition" is the retained-log-content bucket we cap at 10 MiB:
// - one bucket per non-null thread_id
// - one bucket per threadless (thread_id IS NULL) non-null process_uuid
// - one bucket for threadless rows with process_uuid IS NULL
// This budget tracks each row's persisted rendered log body plus non-body
// metadata, rather than the exact sum of all persisted SQLite column bytes.
const LOG_PARTITION_SIZE_LIMIT_BYTES: i64 = 10 * 1024 * 1024;
const LOG_PARTITION_ROW_LIMIT: i64 = 1_000;

#[derive(Clone, Copy)]
struct RuntimeDbSpec {
    label: &'static str,
    filename: &'static str,
    kind: DbKind,
    open_phase: &'static str,
    migrate_phase: &'static str,
}

impl RuntimeDbSpec {
    fn path(self, codex_home: &Path) -> PathBuf {
        codex_home.join(self.filename)
    }
}

const STATE_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "state DB",
    filename: STATE_DB_FILENAME,
    kind: DbKind::State,
    open_phase: "open_state",
    migrate_phase: "migrate_state",
};

const LOGS_DB: RuntimeDbSpec = RuntimeDbSpec {
    label: "log DB",
    filename: LOGS_DB_FILENAME,
    kind: DbKind::Logs,
    open_phase: "open_logs",
    migrate_phase: "migrate_logs",
};

const RUNTIME_DBS: [RuntimeDbSpec; 2] = [STATE_DB, LOGS_DB];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDbPath {
    pub label: &'static str,
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct StateRuntime {
    codex_home: PathBuf,
    default_provider: String,
    pool: Arc<sqlx::SqlitePool>,
    logs_pool: Arc<sqlx::SqlitePool>,
    agent_job_runner_instance_id: String,
    // Capability for owning jobs; the actual lease is enrolled on first use.
    agent_job_runner_registered: bool,
    agent_job_runner_enrollment: Arc<tokio::sync::Mutex<bool>>,
    agent_job_runner_heartbeat_shutdown: tokio::sync::watch::Sender<bool>,
    agent_job_updates: tokio::sync::watch::Sender<u64>,
    db_telemetry: Option<DbTelemetryHandle>,
    thread_updated_at_millis: Arc<AtomicI64>,
    thread_recency_at_millis: Arc<AtomicI64>,
}

impl StateRuntime {
    /// Initialize the state runtime using the provided Codex home and default provider.
    ///
    /// This opens (and migrates) the SQLite databases under `codex_home`,
    /// keeping logs in a dedicated file to reduce lock contention with the
    /// rest of the state store.
    pub async fn init(codex_home: PathBuf, default_provider: String) -> anyhow::Result<Arc<Self>> {
        Self::init_serialized(
            codex_home,
            default_provider,
            /*telemetry_override*/ None,
            /*register_job_runner*/ true,
        )
        .await
    }

    /// Open state for non-owning access without starting a job-runner lease.
    /// Database migrations remain enabled; this is not a SQLite read-only pool.
    pub async fn init_for_read(codex_home: PathBuf, default_provider: String) -> anyhow::Result<Arc<Self>> {
        Self::init_serialized(codex_home, default_provider, None, false).await
    }


    async fn init_serialized(
        codex_home: PathBuf,
        default_provider: String,
        telemetry_override: Option<DbTelemetryHandle>,
        register_job_runner: bool,
    ) -> anyhow::Result<Arc<Self>> {
        // The owned task retains exclusion through pool cleanup even if its
        // caller stops waiting during a migration.
        tokio::spawn(async move {
            let _owners = recovery::lock_runtime_initialization(&codex_home).await?;
            Self::init_inner(codex_home, default_provider, telemetry_override, register_job_runner).await
        })
        .await?
    }

    /// Recheck damage after admission, quarantine only damaged databases, and
    /// retain ownership until replacement initialization has completed.
    pub async fn recover_for_fresh_start(
        database_path: PathBuf,
        default_provider: String,
    ) -> anyhow::Result<Vec<RuntimeDbBackup>> {
        tokio::spawn(async move {
            let home = database_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("database has no parent"))?
                .to_path_buf();
            let _owners = recovery::lock_runtime_initialization(&home).await?;
            let mut backups = Vec::new();
            let mut attempted = BTreeSet::new();
            loop {
                let error =
                    match Self::init_inner(home.clone(), default_provider.clone(), None, true).await {
                        Ok(runtime) => {
                            runtime.close().await;
                            return Ok(backups);
                        }
                        Err(error) => error,
                    };
                let blocking_home = tokio::fs::metadata(&home)
                    .await
                    .is_ok_and(|metadata| metadata.is_file());
                let path = match runtime_db_path_for_corruption_error(&error) {
                    Some(path) => path,
                    None if blocking_home => database_path.clone(),
                    None => return Err(error),
                };
                if !attempted.insert(path.clone()) {
                    return Err(error);
                }
                backups.extend(recovery::backup_runtime_db_owned(&path).await?);
            }
        })
        .await?
    }

    async fn init_inner(
        codex_home: PathBuf,
        default_provider: String,
        telemetry_override: Option<DbTelemetryHandle>,
        register_job_runner: bool,
    ) -> anyhow::Result<Arc<Self>> {
        tokio::fs::create_dir_all(&codex_home).await?;

        let state_migrator = runtime_state_migrator();
        let logs_migrator = runtime_logs_migrator();
        let state_path = STATE_DB.path(codex_home.as_path());
        let logs_path = LOGS_DB.path(codex_home.as_path());
        let telemetry = telemetry_override.as_deref();
        // These migrations touch only their own database. Await every opener
        // so failure cleanup also closes pools that succeeded concurrently.
        let results = tokio::join!(
            open_state_sqlite(&state_path, &state_migrator, telemetry),
            open_logs_sqlite(&logs_path, &logs_migrator, telemetry),
        );
        let results = [results.0, results.1];
        if results.iter().any(Result::is_err) {
            for pool in results.iter().filter_map(|result| result.as_ref().ok()) {
                pool.close().await;
            }
        }
        let [pool, logs_pool] = results;
        let (pool, logs_pool) = (Arc::new(pool?), Arc::new(logs_pool?));
        let started = Instant::now();
        let backfill_state_result = ensure_backfill_state_row_in_pool(pool.as_ref())
            .await
            .map_err(|source| {
                anyhow::Error::new(recovery::RuntimeDbInitError::new(
                    STATE_DB.label,
                    "ensure backfill state",
                    &state_path,
                    source,
                ))
            });
        crate::telemetry::record_init_result(
            telemetry,
            DbKind::State,
            "ensure_backfill_state",
            started.elapsed(),
            &backfill_state_result,
        );
        if let Err(err) = backfill_state_result {
            close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()]).await;
            return Err(err);
        }
        let started = Instant::now();
        let thread_timestamp_millis_result: anyhow::Result<(Option<i64>, Option<i64>)> =
            sqlx::query_as(
                r#"
SELECT
    (SELECT MAX(updated_at_ms) FROM threads),
    (SELECT MAX(recency_at_ms) FROM threads)
                "#,
            )
            .fetch_one(pool.as_ref())
            .await
            .map_err(|source| {
                anyhow::Error::new(recovery::RuntimeDbInitError::new(
                    STATE_DB.label,
                    "initialize thread timestamps",
                    &state_path,
                    source.into(),
                ))
            });
        crate::telemetry::record_init_result(
            telemetry,
            DbKind::State,
            "post_init_query",
            started.elapsed(),
            &thread_timestamp_millis_result,
        );
        let (thread_updated_at_millis, thread_recency_at_millis) =
            match thread_timestamp_millis_result {
                Ok(value) => value,
                Err(err) => {
                    close_sqlite_pools(&[pool.as_ref(), logs_pool.as_ref()])
                        .await;
                    return Err(err);
                }
            };
        let thread_updated_at_millis = thread_updated_at_millis.unwrap_or(0);
        let thread_recency_at_millis = thread_recency_at_millis.unwrap_or(0);
        let agent_job_runner_instance_id = Uuid::new_v4().to_string();
        let (agent_job_runner_heartbeat_shutdown, _) =
            tokio::sync::watch::channel(false);
        let runtime = Arc::new(Self {
            pool,
            logs_pool,
            agent_job_runner_instance_id,
            agent_job_runner_registered: register_job_runner,
            agent_job_runner_enrollment: Arc::new(tokio::sync::Mutex::new(false)),
            agent_job_runner_heartbeat_shutdown,
            agent_job_updates: tokio::sync::watch::channel(0).0,
            db_telemetry: telemetry_override,
            codex_home,
            default_provider,
            thread_updated_at_millis: Arc::new(AtomicI64::new(thread_updated_at_millis)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(thread_recency_at_millis)),
        });
        Ok(runtime)
    }

    /// Return the configured Codex home directory for this runtime.
    pub fn codex_home(&self) -> &Path {
        self.codex_home.as_path()
    }

    pub fn is_closed(&self) -> bool {
        self.pool.is_closed()
    }

    /// Close all SQLite pools and wait for outstanding pool workers to exit.
    pub async fn close(&self) {
        // Registration and close share custody: a cancelled observer cannot
        // enroll a heartbeat after shutdown or leave enrollment half-settled.
        let mut enrolled = self.agent_job_runner_enrollment.lock().await;
        self.agent_job_runner_heartbeat_shutdown.send_replace(true);
        if *enrolled && let Err(error) = agent_jobs::unregister_agent_job_runner_instance(
            self.pool.as_ref(),
            self.agent_job_runner_instance_id.as_str(),
        )
        .await
        {
            warn!(%error, "failed to unregister agent job runner instance");
        }
        *enrolled = false;
        drop(enrolled);
        self.logs_pool.close().await;
        self.pool.close().await;
    }
}

async fn close_sqlite_pools(pools: &[&SqlitePool]) {
    for pool in pools {
        pool.close().await;
    }
}

fn base_sqlite_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .log_statements(LevelFilter::Off)
}

async fn open_state_sqlite(
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    // New state DBs should use incremental auto-vacuum, but retrofitting an
    // existing DB requires a full VACUUM. Do not attempt that during process
    // startup: it is maintenance work that can contend with foreground writers.
    open_sqlite(path, migrator, STATE_DB, telemetry_override).await
}

async fn open_logs_sqlite(
    path: &Path,
    migrator: &Migrator,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    open_sqlite(path, migrator, LOGS_DB, telemetry_override).await
}

/// Connect a SQLite pool using the shared bounded retry for startup PRAGMA contention.
pub async fn connect_sqlite_pool_with_retry(
    options: SqliteConnectOptions,
    max_connections: u32,
    started: Instant,
) -> anyhow::Result<SqlitePool> {
    // Initial WAL/auto-vacuum pragmas can report SQLITE_BUSY before SQLite's
    // busy handler takes effect when another process creates the same database.
    loop {
        let result = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options.clone())
            .await;
        match result {
            Err(ref err)
                if sqlite_error_detail_is_lock(&err.to_string())
                    && started.elapsed() < std::time::Duration::from_secs(5) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            result => break result.map_err(anyhow::Error::from),
        }
    }
}

async fn open_sqlite(
    path: &Path,
    migrator: &Migrator,
    spec: RuntimeDbSpec,
    telemetry_override: Option<&dyn DbTelemetry>,
) -> anyhow::Result<SqlitePool> {
    let options = base_sqlite_options(path).auto_vacuum(SqliteAutoVacuum::Incremental);
    let started = Instant::now();
    let pool_result = connect_sqlite_pool_with_retry(options, 5, started).await;
    crate::telemetry::record_init_result(
        telemetry_override,
        spec.kind,
        spec.open_phase,
        started.elapsed(),
        &pool_result,
    );
    let pool = pool_result
        .map_err(|source| recovery::RuntimeDbInitError::new(spec.label, "open", path, source))?;
    let started = Instant::now();
    let pool_ref = &pool;
    let migrate_result = migrate_tolerating_concurrent_initializers(pool_ref, move || async move {
        if matches!(spec.kind, DbKind::State) {
            repair_legacy_validation_index_migration_order(pool_ref, migrator).await?;
        }
        let compatible = runtime_migrator_for_pool(pool_ref, migrator).await?;
        if matches!(spec.kind, DbKind::State) {
            repair_legacy_recency_migration_version(pool_ref, &compatible).await?;
        }
        // A legacy repair can move a checksum to a different version. Rebuild
        // from the embedded source, not the pre-repair per-version overrides.
        let migrator = runtime_migrator_for_pool(pool_ref, migrator).await?;
        if matches!(spec.kind, DbKind::State) {
            crate::migrations::run_state_migrations(pool_ref, &migrator).await?;
        } else {
            migrator.run(pool_ref).await.map_err(anyhow::Error::from)?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;
    crate::telemetry::record_init_result(
        telemetry_override,
        spec.kind,
        spec.migrate_phase,
        started.elapsed(),
        &migrate_result,
    );
    if let Err(source) = migrate_result {
        pool.close().await;
        return Err(recovery::RuntimeDbInitError::new(spec.label, "migrate", path, source).into());
    }
    Ok(pool)
}

pub(super) async fn ensure_backfill_state_row_in_pool(
    pool: &sqlx::SqlitePool,
) -> anyhow::Result<()> {
    // Eagerly check if the operation would have no effect to avoid blocking waiting for a SQLite
    // writer for no reason in the hot startup path.
    if sqlx::query_scalar::<_, i64>("SELECT 1 FROM backfill_state WHERE id = 1")
        .fetch_optional(pool)
        .await?
        .is_some()
    {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO backfill_state (id, status, last_watermark, last_success_at, updated_at)
VALUES (?, ?, NULL, NULL, ?)
ON CONFLICT(id) DO NOTHING
            "#,
    )
    .bind(1_i64)
    .bind(crate::BackfillStatus::Pending.as_str())
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

pub fn state_db_filename() -> String {
    STATE_DB.filename.to_string()
}

pub fn state_db_path(codex_home: &Path) -> PathBuf {
    STATE_DB.path(codex_home)
}

pub fn logs_db_filename() -> String {
    LOGS_DB.filename.to_string()
}

pub fn logs_db_path(codex_home: &Path) -> PathBuf {
    LOGS_DB.path(codex_home)
}

pub fn runtime_db_paths(codex_home: &Path) -> Vec<RuntimeDbPath> {
    RUNTIME_DBS
        .iter()
        .map(|spec| RuntimeDbPath {
            label: spec.label,
            path: spec.path(codex_home),
        })
        .collect()
}

/// Run SQLite's built-in integrity check against an existing database file.
pub async fn sqlite_integrity_check(path: &Path) -> anyhow::Result<Vec<String>> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .log_statements(LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let rows = sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
        .fetch_all(&pool)
        .await?;
    pool.close().await;
    Ok(rows)
}


