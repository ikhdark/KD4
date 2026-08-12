use super::*;

const LOG_RETENTION_DAYS: i64 = 10;
const MAX_PENDING_RETENTION_KEYS: usize = 256;
const RETENTION_QUERY_KEY_BATCH: usize = 200;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LogRetentionScope {
    reconcile_all: bool,
    thread_ids: BTreeSet<String>,
    threadless_process_uuids: BTreeSet<String>,
    has_threadless_null_process_uuid: bool,
}

impl LogRetentionScope {
    pub(crate) fn for_reconciliation() -> Self {
        Self {
            reconcile_all: true,
            ..Self::default()
        }
    }

    fn from_entries(entries: &[LogEntry]) -> Self {
        let mut scope = Self::default();
        for entry in entries {
            if let Some(thread_id) = entry.thread_id.as_ref() {
                scope.thread_ids.insert(thread_id.clone());
            } else if let Some(process_uuid) = entry.process_uuid.as_ref() {
                scope.threadless_process_uuids.insert(process_uuid.clone());
            } else {
                scope.has_threadless_null_process_uuid = true;
            }
        }
        scope
    }

    pub(crate) fn merge(&mut self, other: Self) {
        if self.reconcile_all || other.reconcile_all {
            *self = Self::for_reconciliation();
            return;
        }
        self.thread_ids.extend(other.thread_ids);
        self.threadless_process_uuids
            .extend(other.threadless_process_uuids);
        self.has_threadless_null_process_uuid |= other.has_threadless_null_process_uuid;
        if self.thread_ids.len() + self.threadless_process_uuids.len() > MAX_PENDING_RETENTION_KEYS
        {
            *self = Self::for_reconciliation();
        }
    }

    fn is_empty(&self) -> bool {
        !self.reconcile_all
            && self.thread_ids.is_empty()
            && self.threadless_process_uuids.is_empty()
            && !self.has_threadless_null_process_uuid
    }

    fn into_query_batches(self) -> Vec<Self> {
        let mut batches = Vec::new();
        let thread_ids = self.thread_ids.into_iter().collect::<Vec<_>>();
        for ids in thread_ids.chunks(RETENTION_QUERY_KEY_BATCH) {
            batches.push(Self {
                thread_ids: ids.iter().cloned().collect(),
                ..Self::default()
            });
        }
        let process_uuids = self
            .threadless_process_uuids
            .into_iter()
            .collect::<Vec<_>>();
        for ids in process_uuids.chunks(RETENTION_QUERY_KEY_BATCH) {
            batches.push(Self {
                threadless_process_uuids: ids.iter().cloned().collect(),
                ..Self::default()
            });
        }
        if self.has_threadless_null_process_uuid {
            batches.push(Self {
                has_threadless_null_process_uuid: true,
                ..Self::default()
            });
        }
        batches
    }
}

#[cfg(test)]
pub(crate) struct LogRetentionTestControl {
    block_next_deletion: std::sync::atomic::AtomicBool,
    fail_next_deletion: std::sync::atomic::AtomicBool,
    active_deletions: std::sync::atomic::AtomicUsize,
    max_active_deletions: std::sync::atomic::AtomicUsize,
    deletion_entered: tokio::sync::Notify,
    deletion_release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Default for LogRetentionTestControl {
    fn default() -> Self {
        Self {
            block_next_deletion: std::sync::atomic::AtomicBool::default(),
            fail_next_deletion: std::sync::atomic::AtomicBool::default(),
            active_deletions: std::sync::atomic::AtomicUsize::default(),
            max_active_deletions: std::sync::atomic::AtomicUsize::default(),
            deletion_entered: tokio::sync::Notify::new(),
            deletion_release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[cfg(test)]
struct ActiveLogRetentionDeletion<'a>(&'a std::sync::atomic::AtomicUsize);

#[cfg(test)]
impl Drop for ActiveLogRetentionDeletion<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl LogRetentionTestControl {
    pub(crate) fn block_next_deletion(&self) {
        self.block_next_deletion
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn fail_next_deletion(&self) {
        self.fail_next_deletion
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) async fn wait_until_deletion_active(&self) {
        while self
            .active_deletions
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            self.deletion_entered.notified().await;
        }
    }

    pub(crate) fn release_blocked_deletion(&self) {
        self.deletion_release.add_permits(1);
    }

    pub(crate) fn max_active_deletions(&self) -> usize {
        self.max_active_deletions
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn before_deletion(&self) -> anyhow::Result<ActiveLogRetentionDeletion<'_>> {
        let active = self
            .active_deletions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max_active_deletions
            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
        let active = ActiveLogRetentionDeletion(&self.active_deletions);
        self.deletion_entered.notify_waiters();

        if self
            .block_next_deletion
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.deletion_release
                .acquire()
                .await
                .expect("retention test semaphore remains open")
                .forget();
        }
        if self
            .fail_next_deletion
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            anyhow::bail!("injected log retention deletion failure");
        }
        Ok(active)
    }
}

impl StateRuntime {
    pub(crate) fn record_log_retention_event(&self, event: &'static str) {
        crate::telemetry::record_log_retention_event(self.db_telemetry.as_deref(), event);
    }

    #[cfg(test)]
    pub(crate) fn log_retention_test_control(&self) -> Arc<LogRetentionTestControl> {
        Arc::clone(&self.log_retention_test_control)
    }

    pub async fn insert_log(&self, entry: &LogEntry) -> anyhow::Result<()> {
        self.insert_logs(std::slice::from_ref(entry)).await
    }

    /// Insert a batch of log entries into the logs table.
    pub async fn insert_logs(&self, entries: &[LogEntry]) -> anyhow::Result<()> {
        let scope = self.insert_logs_deferred_retention(entries).await?;
        if let Err(_err) = self.prune_log_retention(scope).await {
            self.record_log_retention_event("cleanup_failed");
        }
        Ok(())
    }

    pub(crate) async fn insert_logs_deferred_retention(
        &self,
        entries: &[LogEntry],
    ) -> anyhow::Result<LogRetentionScope> {
        if entries.is_empty() {
            return Ok(LogRetentionScope::default());
        }

        let scope = LogRetentionScope::from_entries(entries);
        let started = Instant::now();
        let connection_result = self.logs_pool.acquire().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "insert",
            "pool_acquire",
            started.elapsed(),
            &connection_result,
        );
        let mut connection = connection_result?;
        let started = Instant::now();
        let transaction_result = connection.begin().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "insert",
            "transaction_begin",
            started.elapsed(),
            &transaction_result,
        );
        let mut tx = transaction_result?;
        let mut builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body, thread_id, process_uuid, module_path, file, line, estimated_bytes) ",
        );
        builder.push_values(entries, |mut row, entry| {
            let feedback_log_body = entry.feedback_log_body.as_ref().or(entry.message.as_ref());
            // Keep about 10 MiB of reader-visible log content per partition.
            // Both `query_logs` and `/feedback` read the persisted
            // `feedback_log_body`, while `LogEntry.message` is only a write-time
            // fallback for callers that still populate the old field.
            let estimated_bytes = feedback_log_body.map_or(0, String::len) as i64
                + entry.level.len() as i64
                + entry.target.len() as i64
                + entry.module_path.as_ref().map_or(0, String::len) as i64
                + entry.file.as_ref().map_or(0, String::len) as i64;
            row.push_bind(entry.ts)
                .push_bind(entry.ts_nanos)
                .push_bind(&entry.level)
                .push_bind(&entry.target)
                .push_bind(feedback_log_body)
                .push_bind(&entry.thread_id)
                .push_bind(&entry.process_uuid)
                .push_bind(&entry.module_path)
                .push_bind(&entry.file)
                .push_bind(entry.line)
                .push_bind(estimated_bytes);
        });
        let started = Instant::now();
        let insert_result = builder
            .build()
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "insert",
            "execute",
            started.elapsed(),
            &insert_result,
        );
        insert_result?;
        let started = Instant::now();
        let commit_result = tx.commit().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "insert",
            "commit",
            started.elapsed(),
            &commit_result,
        );
        commit_result?;
        Ok(scope)
    }

    pub(crate) async fn prune_log_retention(
        &self,
        mut scope: LogRetentionScope,
    ) -> anyhow::Result<()> {
        let reconcile_all = scope.reconcile_all;
        let started = Instant::now();
        let decision_result: anyhow::Result<()> = async {
            if reconcile_all {
                scope.thread_ids = sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT thread_id FROM logs WHERE thread_id IS NOT NULL",
                )
                .fetch_all(self.logs_pool.as_ref())
                .await?
                .into_iter()
                .collect();
                scope.threadless_process_uuids = sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT process_uuid FROM logs WHERE thread_id IS NULL AND process_uuid IS NOT NULL",
                )
                .fetch_all(self.logs_pool.as_ref())
                .await?
                .into_iter()
                .collect();
                scope.has_threadless_null_process_uuid = sqlx::query_scalar::<_, i64>(
                    "SELECT EXISTS(SELECT 1 FROM logs WHERE thread_id IS NULL AND process_uuid IS NULL)",
                )
                .fetch_one(self.logs_pool.as_ref())
                .await?
                    != 0;
                scope.reconcile_all = false;
            }
            Ok(())
        }
        .await;
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "decision",
            started.elapsed(),
            &decision_result,
        );
        decision_result?;
        if scope.is_empty() && !reconcile_all {
            return Ok(());
        }

        let started = Instant::now();
        let connection_result = self.logs_pool.acquire().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "pool_acquire",
            started.elapsed(),
            &connection_result,
        );
        let mut connection = connection_result?;
        let started = Instant::now();
        let transaction_result = connection.begin().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "transaction_begin",
            started.elapsed(),
            &transaction_result,
        );
        let mut tx = transaction_result?;
        let started = Instant::now();
        let deletion_result: anyhow::Result<()> = async {
            #[cfg(test)]
            let _active_deletion = self.log_retention_test_control.before_deletion().await?;
            if let Some(cutoff) = Utc::now()
                .checked_sub_signed(chrono::Duration::days(LOG_RETENTION_DAYS))
                .map(|cutoff| cutoff.timestamp())
            {
                sqlx::query("DELETE FROM logs WHERE ts < ?")
                    .bind(cutoff)
                    .execute(&mut *tx)
                    .await?;
            }
            for batch in scope.into_query_batches() {
                self.prune_logs_for_scope(&batch, &mut tx).await?;
            }
            Ok(())
        }
        .await;
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "deletion",
            started.elapsed(),
            &deletion_result,
        );
        deletion_result?;
        let started = Instant::now();
        let commit_result = tx.commit().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "commit",
            started.elapsed(),
            &commit_result,
        );
        commit_result
    }

    /// Enforce per-partition retained-log-content caps after a successful batch insert.
    ///
    /// We maintain two independent budgets:
    /// - Thread logs: rows with `thread_id IS NOT NULL`, capped per `thread_id`.
    /// - Threadless process logs: rows with `thread_id IS NULL` ("threadless"),
    ///   capped per `process_uuid` (including `process_uuid IS NULL` as its own
    ///   threadless partition).
    ///
    /// "Threadless" means the log row is not associated with any conversation
    /// thread, so retention is keyed by process identity instead.
    ///
    async fn prune_logs_for_scope(
        &self,
        scope: &LogRetentionScope,
        tx: &mut SqliteConnection,
    ) -> anyhow::Result<()> {
        let thread_ids: BTreeSet<&str> = scope.thread_ids.iter().map(String::as_str).collect();
        if !thread_ids.is_empty() {
            // Cheap precheck: only run the heavier window-function prune for
            // threads that are currently above the cap.
            let mut over_limit_threads_query =
                QueryBuilder::<Sqlite>::new("SELECT thread_id FROM logs WHERE thread_id IN (");
            {
                let mut separated = over_limit_threads_query.separated(", ");
                for thread_id in &thread_ids {
                    separated.push_bind(*thread_id);
                }
            }
            over_limit_threads_query.push(") GROUP BY thread_id HAVING SUM(");
            over_limit_threads_query.push("estimated_bytes");
            over_limit_threads_query.push(") > ");
            over_limit_threads_query.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
            over_limit_threads_query.push(" OR COUNT(*) > ");
            over_limit_threads_query.push_bind(LOG_PARTITION_ROW_LIMIT);
            let over_limit_thread_ids: Vec<String> = over_limit_threads_query
                .build()
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|row| row.try_get("thread_id"))
                .collect::<Result<_, _>>()?;
            if !over_limit_thread_ids.is_empty() {
                // Enforce a strict per-thread cap by deleting every row whose
                // newest-first cumulative bytes exceed the partition budget.
                let mut prune_threads = QueryBuilder::<Sqlite>::new(
                    r#"
DELETE FROM logs
WHERE id IN (
    SELECT id
    FROM (
        SELECT
            id,
            SUM(
"#,
                );
                prune_threads.push("estimated_bytes");
                prune_threads.push(
                    r#"
            ) OVER (
                PARTITION BY thread_id
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS cumulative_bytes,
            ROW_NUMBER() OVER (
                PARTITION BY thread_id
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS row_number
        FROM logs
        WHERE thread_id IN (
"#,
                );
                {
                    let mut separated = prune_threads.separated(", ");
                    for thread_id in &over_limit_thread_ids {
                        separated.push_bind(thread_id);
                    }
                }
                prune_threads.push(
                    r#"
        )
    )
    WHERE cumulative_bytes >
"#,
                );
                prune_threads.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
                prune_threads.push(" OR row_number > ");
                prune_threads.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threads.push("\n)");
                prune_threads.build().execute(&mut *tx).await?;
            }
        }

        let threadless_process_uuids: BTreeSet<&str> = scope
            .threadless_process_uuids
            .iter()
            .map(String::as_str)
            .collect();
        let has_threadless_null_process_uuid = scope.has_threadless_null_process_uuid;
        if !threadless_process_uuids.is_empty() {
            // Threadless logs are budgeted separately per process UUID.
            let mut over_limit_processes_query = QueryBuilder::<Sqlite>::new(
                "SELECT process_uuid FROM logs WHERE thread_id IS NULL AND process_uuid IN (",
            );
            {
                let mut separated = over_limit_processes_query.separated(", ");
                for process_uuid in &threadless_process_uuids {
                    separated.push_bind(*process_uuid);
                }
            }
            over_limit_processes_query.push(") GROUP BY process_uuid HAVING SUM(");
            over_limit_processes_query.push("estimated_bytes");
            over_limit_processes_query.push(") > ");
            over_limit_processes_query.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
            over_limit_processes_query.push(" OR COUNT(*) > ");
            over_limit_processes_query.push_bind(LOG_PARTITION_ROW_LIMIT);
            let over_limit_process_uuids: Vec<String> = over_limit_processes_query
                .build()
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|row| row.try_get("process_uuid"))
                .collect::<Result<_, _>>()?;
            if !over_limit_process_uuids.is_empty() {
                // Same strict cap policy as thread pruning, but only for
                // threadless rows in the affected process UUIDs.
                let mut prune_threadless_process_logs = QueryBuilder::<Sqlite>::new(
                    r#"
DELETE FROM logs
WHERE id IN (
    SELECT id
    FROM (
        SELECT
            id,
            SUM(
"#,
                );
                prune_threadless_process_logs.push("estimated_bytes");
                prune_threadless_process_logs.push(
                    r#"
            ) OVER (
                PARTITION BY process_uuid
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS cumulative_bytes,
            ROW_NUMBER() OVER (
                PARTITION BY process_uuid
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS row_number
        FROM logs
        WHERE thread_id IS NULL
          AND process_uuid IN (
"#,
                );
                {
                    let mut separated = prune_threadless_process_logs.separated(", ");
                    for process_uuid in &over_limit_process_uuids {
                        separated.push_bind(process_uuid);
                    }
                }
                prune_threadless_process_logs.push(
                    r#"
          )
    )
    WHERE cumulative_bytes >
"#,
                );
                prune_threadless_process_logs.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
                prune_threadless_process_logs.push(" OR row_number > ");
                prune_threadless_process_logs.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threadless_process_logs.push("\n)");
                prune_threadless_process_logs
                    .build()
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if has_threadless_null_process_uuid {
            // Rows without a process UUID still need a cap; treat NULL as its
            // own threadless partition.
            let mut null_process_usage_query = QueryBuilder::<Sqlite>::new("SELECT SUM(");
            null_process_usage_query.push("estimated_bytes");
            null_process_usage_query.push(
                ") AS total_bytes, COUNT(*) AS row_count FROM logs WHERE thread_id IS NULL AND process_uuid IS NULL",
            );
            let null_process_usage = null_process_usage_query.build().fetch_one(&mut *tx).await?;
            let total_null_process_bytes: Option<i64> =
                null_process_usage.try_get("total_bytes")?;
            let null_process_row_count: i64 = null_process_usage.try_get("row_count")?;

            if total_null_process_bytes.unwrap_or(0) > LOG_PARTITION_SIZE_LIMIT_BYTES
                || null_process_row_count > LOG_PARTITION_ROW_LIMIT
            {
                let mut prune_threadless_null_process_logs = QueryBuilder::<Sqlite>::new(
                    r#"
DELETE FROM logs
WHERE id IN (
    SELECT id
    FROM (
        SELECT
            id,
            SUM(
"#,
                );
                prune_threadless_null_process_logs.push("estimated_bytes");
                prune_threadless_null_process_logs.push(
                    r#"
            ) OVER (
                PARTITION BY process_uuid
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS cumulative_bytes,
            ROW_NUMBER() OVER (
                PARTITION BY process_uuid
                ORDER BY ts DESC, ts_nanos DESC, id DESC
            ) AS row_number
        FROM logs
        WHERE thread_id IS NULL
          AND process_uuid IS NULL
    )
    WHERE cumulative_bytes >
"#,
                );
                prune_threadless_null_process_logs.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
                prune_threadless_null_process_logs.push(" OR row_number > ");
                prune_threadless_null_process_logs.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threadless_null_process_logs.push("\n)");
                prune_threadless_null_process_logs
                    .build()
                    .execute(&mut *tx)
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn delete_logs_before(&self, cutoff_ts: i64) -> anyhow::Result<u64> {
        let result = sqlx::query("DELETE FROM logs WHERE ts < ?")
            .bind(cutoff_ts)
            .execute(self.logs_pool.as_ref())
            .await?;
        Ok(result.rows_affected())
    }

    pub(crate) async fn run_logs_startup_maintenance(&self) -> anyhow::Result<()> {
        let Some(cutoff) =
            Utc::now().checked_sub_signed(chrono::Duration::days(LOG_RETENTION_DAYS))
        else {
            return Ok(());
        };
        self.delete_logs_before(cutoff.timestamp()).await?;
        // Startup cleanup should not wait behind or block foreground work.
        // PASSIVE checkpoints copy whatever is immediately available and skip
        // frames that would require waiting on active readers or writers.
        sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
            .execute(self.logs_pool.as_ref())
            .await?;
        Ok(())
    }

    /// Query logs with optional filters.
    pub async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, ts, ts_nanos, level, target, feedback_log_body AS message, thread_id, process_uuid, file, line FROM logs WHERE 1 = 1",
        );
        push_log_filters(&mut builder, query);
        if query.descending {
            builder.push(" ORDER BY id DESC");
        } else {
            builder.push(" ORDER BY id ASC");
        }
        if let Some(limit) = query.limit {
            builder.push(" LIMIT ").push_bind(limit as i64);
        }

        let rows = builder
            .build_query_as::<LogRow>()
            .fetch_all(self.logs_pool.as_ref())
            .await?;
        Ok(rows)
    }

    /// Query feedback logs for a set of threads, capped to the SQLite retention budget.
    pub async fn query_feedback_logs_for_threads(
        &self,
        thread_ids: &[&str],
    ) -> anyhow::Result<Vec<u8>> {
        if thread_ids.is_empty() {
            return Ok(Vec::new());
        }

        let max_bytes = usize::try_from(LOG_PARTITION_SIZE_LIMIT_BYTES).unwrap_or(usize::MAX);
        // Bound the fetched rows in SQL first so over-retained partitions do not have to load
        // every row into memory, then apply the exact whole-line byte cap after formatting.
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
WITH requested_threads(thread_id) AS (
    VALUES
            "#,
        );
        {
            let mut separated = builder.separated(", ");
            for thread_id in thread_ids {
                separated
                    .push("(")
                    .push_bind_unseparated(*thread_id)
                    .push_unseparated(")");
            }
        }
        builder.push(
            r#"
),
latest_processes AS (
    SELECT (
        SELECT process_uuid
        FROM logs
        WHERE logs.thread_id = requested_threads.thread_id AND process_uuid IS NOT NULL
        ORDER BY ts DESC, ts_nanos DESC, id DESC
        LIMIT 1
    ) AS process_uuid
    FROM requested_threads
),
feedback_logs AS (
    SELECT ts, ts_nanos, level, feedback_log_body, estimated_bytes, id
    FROM logs
    WHERE feedback_log_body IS NOT NULL AND (
        thread_id IN (SELECT thread_id FROM requested_threads)
        OR (
            thread_id IS NULL
            AND process_uuid IN (
                SELECT process_uuid
                FROM latest_processes
                WHERE process_uuid IS NOT NULL
            )
        )
    )
),
bounded_feedback_logs AS (
    SELECT
        ts,
        ts_nanos,
        level,
        feedback_log_body,
        id,
        SUM(estimated_bytes) OVER (
            ORDER BY ts DESC, ts_nanos DESC, id DESC
        ) AS cumulative_estimated_bytes
    FROM feedback_logs
)
SELECT ts, ts_nanos, level, feedback_log_body
FROM bounded_feedback_logs
WHERE cumulative_estimated_bytes <=
"#,
        );
        builder.push_bind(LOG_PARTITION_SIZE_LIMIT_BYTES);
        builder.push(" ORDER BY ts DESC, ts_nanos DESC, id DESC");
        let rows = builder
            .build_query_as::<FeedbackLogRow>()
            .fetch_all(self.logs_pool.as_ref())
            .await?;

        let mut lines = Vec::new();
        let mut total_bytes = 0usize;
        for row in rows {
            let line =
                format_feedback_log_line(row.ts, row.ts_nanos, &row.level, &row.feedback_log_body);
            if total_bytes.saturating_add(line.len()) > max_bytes {
                break;
            }
            total_bytes += line.len();
            lines.push(line);
        }

        let mut ordered_bytes = Vec::with_capacity(total_bytes);
        for line in lines.into_iter().rev() {
            ordered_bytes.extend_from_slice(line.as_bytes());
        }

        Ok(ordered_bytes)
    }

    /// Query per-thread feedback logs, capped to the per-thread SQLite retention budget.
    pub async fn query_feedback_logs(&self, thread_id: &str) -> anyhow::Result<Vec<u8>> {
        self.query_feedback_logs_for_threads(&[thread_id]).await
    }

    /// Return the max log id matching optional filters.
    pub async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT MAX(id) AS max_id FROM logs WHERE 1 = 1");
        push_log_filters(&mut builder, query);
        let row = builder.build().fetch_one(self.logs_pool.as_ref()).await?;
        let max_id: Option<i64> = row.try_get("max_id")?;
        Ok(max_id.unwrap_or(0))
    }
}

#[derive(sqlx::FromRow)]
struct FeedbackLogRow {
    ts: i64,
    ts_nanos: i64,
    level: String,
    feedback_log_body: String,
}

fn format_feedback_log_line(
    ts: i64,
    ts_nanos: i64,
    level: &str,
    feedback_log_body: &str,
) -> String {
    let nanos = u32::try_from(ts_nanos).unwrap_or(0);
    let timestamp = match DateTime::<Utc>::from_timestamp(ts, nanos) {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        None => format!("{ts}.{ts_nanos:09}Z"),
    };
    let mut line = format!("{timestamp} {level:>5} {feedback_log_body}");
    if !line.ends_with('\n') {
        line.push('\n');
    }
    line
}

fn push_log_filters(builder: &mut QueryBuilder<Sqlite>, query: &LogQuery) {
    if !query.levels_upper.is_empty() {
        builder.push(" AND UPPER(level) IN (");
        {
            let mut separated = builder.separated(", ");
            for level_upper in &query.levels_upper {
                separated.push_bind(level_upper.as_str());
            }
        }
        builder.push(")");
    }
    if let Some(from_ts) = query.from_ts {
        builder.push(" AND ts >= ").push_bind(from_ts);
    }
    if let Some(to_ts) = query.to_ts {
        builder.push(" AND ts <= ").push_bind(to_ts);
    }
    push_like_filters(builder, "module_path", &query.module_like);
    push_like_filters(builder, "file", &query.file_like);
    let has_thread_filter = !query.thread_ids.is_empty() || query.include_threadless;
    if has_thread_filter {
        builder.push(" AND (");
        let mut needs_or = false;
        for thread_id in &query.thread_ids {
            if needs_or {
                builder.push(" OR ");
            }
            builder.push("thread_id = ").push_bind(thread_id.as_str());
            needs_or = true;
        }
        if query.include_threadless {
            if needs_or {
                builder.push(" OR ");
            }
            builder.push("thread_id IS NULL");
        }
        builder.push(")");
    }
    if let Some(after_id) = query.after_id {
        builder.push(" AND id > ").push_bind(after_id);
    }
    if let Some(search) = query.search.as_ref() {
        builder.push(" AND INSTR(COALESCE(feedback_log_body, ''), ");
        builder.push_bind(search.as_str());
        builder.push(") > 0");
    }
}

fn push_like_filters(builder: &mut QueryBuilder<Sqlite>, column: &str, filters: &[String]) {
    if filters.is_empty() {
        return;
    }
    builder.push(" AND (");
    for (idx, filter) in filters.iter().enumerate() {
        if idx > 0 {
            builder.push(" OR ");
        }
        builder
            .push(column)
            .push(" LIKE '%' || ")
            .push_bind(filter.as_str())
            .push(" || '%'");
    }
    builder.push(")");
}

#[cfg(test)]
mod tests {
    use super::StateRuntime;
    use super::format_feedback_log_line;
    use super::test_support::unique_temp_dir;
    use crate::DB_LOG_PHASE_DURATION_METRIC;
    use crate::DbTelemetry;
    use crate::LogEntry;
    use crate::LogQuery;
    use crate::logs_db_path;
    use crate::migrations::LOGS_MIGRATOR;
    use chrono::Utc;
    use pretty_assertions::assert_eq;
    use sqlx::SqlitePool;
    use sqlx::migrate::Migrator;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::borrow::Cow;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct CapturingTelemetry {
        phases: Mutex<Vec<(String, String)>>,
    }

    impl CapturingTelemetry {
        fn clear(&self) {
            self.phases.lock().expect("telemetry mutex").clear();
        }

        fn phases(&self) -> Vec<(String, String)> {
            self.phases.lock().expect("telemetry mutex").clone()
        }
    }

    impl DbTelemetry for CapturingTelemetry {
        fn counter(&self, _name: &str, _inc: i64, _tags: &[(&str, &str)]) {}

        fn record_duration(&self, name: &str, _duration: Duration, tags: &[(&str, &str)]) {
            if name != DB_LOG_PHASE_DURATION_METRIC {
                return;
            }
            let operation = tags
                .iter()
                .find_map(|(key, value)| (*key == "operation").then(|| (*value).to_string()))
                .expect("operation tag");
            let phase = tags
                .iter()
                .find_map(|(key, value)| (*key == "phase").then(|| (*value).to_string()))
                .expect("phase tag");
            self.phases
                .lock()
                .expect("telemetry mutex")
                .push((operation, phase));
        }
    }

    fn test_log(message: &str, thread_id: &str) -> LogEntry {
        LogEntry {
            ts: Utc::now().timestamp(),
            ts_nanos: 0,
            level: "INFO".to_string(),
            target: "state-retention-test".to_string(),
            message: Some(message.to_string()),
            feedback_log_body: Some(message.to_string()),
            thread_id: Some(thread_id.to_string()),
            process_uuid: Some("retention-test-process".to_string()),
            module_path: None,
            file: None,
            line: None,
        }
    }

    async fn open_db_pool(path: &Path) -> SqlitePool {
        SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false),
        )
        .await
        .expect("open sqlite pool")
    }

    async fn log_row_count(path: &Path) -> i64 {
        let pool = open_db_pool(path).await;
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM logs")
            .fetch_one(&pool)
            .await
            .expect("count log rows");
        pool.close().await;
        count
    }

    #[tokio::test]
    async fn deferred_retention_does_not_block_insert_and_failure_preserves_commit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let control = runtime.log_retention_test_control();

        let blocked_scope = runtime
            .insert_logs_deferred_retention(&[test_log("before blocked prune", "thread-a")])
            .await
            .expect("commit first insertion");
        control.block_next_deletion();
        let cleanup_runtime = Arc::clone(&runtime);
        let cleanup =
            tokio::spawn(async move { cleanup_runtime.prune_log_retention(blocked_scope).await });
        control.wait_until_deletion_active().await;

        tokio::time::timeout(
            Duration::from_secs(1),
            runtime.insert_logs_deferred_retention(&[test_log(
                "insert while prune blocked",
                "thread-b",
            )]),
        )
        .await
        .expect("insertion must not wait for retention deletion")
        .expect("commit concurrent insertion");
        control.release_blocked_deletion();
        cleanup
            .await
            .expect("join blocked cleanup")
            .expect("complete blocked cleanup");

        let failed_scope = runtime
            .insert_logs_deferred_retention(&[test_log(&"x".repeat(11 * 1024 * 1024), "thread-c")])
            .await
            .expect("commit insertion before failed pruning");
        let retry_scope = failed_scope.clone();
        control.fail_next_deletion();
        assert!(runtime.prune_log_retention(failed_scope).await.is_err());
        assert_eq!(log_row_count(&logs_db_path(&codex_home)).await, 3);
        runtime
            .prune_log_retention(retry_scope)
            .await
            .expect("retry retention after injected failure");
        assert_eq!(log_row_count(&logs_db_path(&codex_home)).await, 2);
    }

    #[tokio::test]
    async fn log_insert_and_retention_timings_are_independently_attributable() {
        let codex_home = unique_temp_dir();
        let telemetry = Arc::new(CapturingTelemetry::default());
        let runtime = StateRuntime::init_with_telemetry_for_tests(
            codex_home,
            "test-provider".to_string(),
            telemetry.clone(),
        )
        .await
        .expect("initialize runtime");
        telemetry.clear();

        let scope = runtime
            .insert_logs_deferred_retention(&[test_log("timed", "timed-thread")])
            .await
            .expect("insert timed log");
        assert_eq!(
            telemetry.phases(),
            vec![
                ("insert".to_string(), "pool_acquire".to_string()),
                ("insert".to_string(), "transaction_begin".to_string()),
                ("insert".to_string(), "execute".to_string()),
                ("insert".to_string(), "commit".to_string()),
            ],
            "retention operations must not appear on the insert critical path",
        );

        runtime
            .prune_log_retention(scope)
            .await
            .expect("run timed retention");
        assert_eq!(
            telemetry.phases(),
            vec![
                ("insert".to_string(), "pool_acquire".to_string()),
                ("insert".to_string(), "transaction_begin".to_string()),
                ("insert".to_string(), "execute".to_string()),
                ("insert".to_string(), "commit".to_string()),
                ("retention".to_string(), "decision".to_string()),
                ("retention".to_string(), "pool_acquire".to_string()),
                ("retention".to_string(), "transaction_begin".to_string()),
                ("retention".to_string(), "deletion".to_string()),
                ("retention".to_string(), "commit".to_string()),
            ],
        );
    }

    #[tokio::test]
    async fn insert_logs_use_dedicated_log_database() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[LogEntry {
                ts: 1,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some("dedicated-log-db".to_string()),
                feedback_log_body: Some("dedicated-log-db".to_string()),
                thread_id: Some("thread-1".to_string()),
                process_uuid: Some("proc-1".to_string()),
                module_path: Some("mod".to_string()),
                file: Some("main.rs".to_string()),
                line: Some(7),
            }])
            .await
            .expect("insert test logs");

        let logs_count = log_row_count(logs_db_path(codex_home.as_path()).as_path()).await;

        assert_eq!(logs_count, 1);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn init_migrates_message_only_logs_db_to_feedback_log_body_schema() {
        let codex_home = unique_temp_dir();
        tokio::fs::create_dir_all(&codex_home)
            .await
            .expect("create codex home");
        let logs_path = logs_db_path(codex_home.as_path());
        let old_logs_migrator = Migrator {
            migrations: Cow::Owned(vec![LOGS_MIGRATOR.migrations[0].clone()]),
            ignore_missing: false,
            locking: true,
            no_tx: false,
            table_name: LOGS_MIGRATOR.table_name.clone(),
            create_schemas: LOGS_MIGRATOR.create_schemas.clone(),
        };
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&logs_path)
                .create_if_missing(true),
        )
        .await
        .expect("open old logs db");
        old_logs_migrator
            .run(&pool)
            .await
            .expect("apply old logs schema");
        sqlx::query(
            "INSERT INTO logs (ts, ts_nanos, level, target, message, module_path, file, line, thread_id, process_uuid, estimated_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Utc::now().timestamp())
        .bind(0_i64)
        .bind("INFO")
        .bind("cli")
        .bind("legacy-body")
        .bind("mod")
        .bind("main.rs")
        .bind(7_i64)
        .bind("thread-1")
        .bind("proc-1")
        .bind(16_i64)
        .execute(&pool)
        .await
        .expect("insert legacy log row");
        pool.close().await;
        drop(pool);

        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let rows = runtime
            .query_logs(&LogQuery::default())
            .await
            .expect("query migrated logs");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message.as_deref(), Some("legacy-body"));

        let migrated_pool = open_db_pool(logs_path.as_path()).await;
        let columns = sqlx::query_scalar::<_, String>("SELECT name FROM pragma_table_info('logs')")
            .fetch_all(&migrated_pool)
            .await
            .expect("load migrated columns");
        assert_eq!(
            columns,
            vec![
                "id".to_string(),
                "ts".to_string(),
                "ts_nanos".to_string(),
                "level".to_string(),
                "target".to_string(),
                "feedback_log_body".to_string(),
                "module_path".to_string(),
                "file".to_string(),
                "line".to_string(),
                "thread_id".to_string(),
                "process_uuid".to_string(),
                "estimated_bytes".to_string(),
            ]
        );
        let indexes = sqlx::query_scalar::<_, String>(
            "SELECT name FROM pragma_index_list('logs') ORDER BY name",
        )
        .fetch_all(&migrated_pool)
        .await
        .expect("load migrated indexes");
        assert_eq!(
            indexes,
            vec![
                "idx_logs_feedback_process_threadless_ts".to_string(),
                "idx_logs_feedback_thread_ts".to_string(),
                "idx_logs_process_uuid_threadless_ts".to_string(),
                "idx_logs_thread_id".to_string(),
                "idx_logs_thread_id_ts".to_string(),
                "idx_logs_ts".to_string(),
            ]
        );
        migrated_pool.close().await;

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn init_configures_logs_db_with_incremental_auto_vacuum() {
        let codex_home = unique_temp_dir();
        let _runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let pool = open_db_pool(logs_db_path(codex_home.as_path()).as_path()).await;
        let auto_vacuum = sqlx::query_scalar::<_, i64>("PRAGMA auto_vacuum")
            .fetch_one(&pool)
            .await
            .expect("read auto_vacuum pragma");
        assert_eq!(auto_vacuum, 2);
        pool.close().await;

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[test]
    fn format_feedback_log_line_matches_feedback_formatter_shape() {
        assert_eq!(
            format_feedback_log_line(
                /*ts*/ 1,
                /*ts_nanos*/ 123_456_000,
                "INFO",
                "alpha"
            ),
            "1970-01-01T00:00:01.123456Z  INFO alpha\n"
        );
    }

    #[test]
    fn format_feedback_log_line_preserves_existing_trailing_newline() {
        assert_eq!(
            format_feedback_log_line(
                /*ts*/ 1,
                /*ts_nanos*/ 123_456_000,
                "INFO",
                "alpha\n"
            ),
            "1970-01-01T00:00:01.123456Z  INFO alpha\n"
        );
    }

    #[tokio::test]
    async fn query_logs_with_search_matches_rendered_body_substring() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1_700_000_001,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("alpha".to_string()),
                    feedback_log_body: Some("foo=1 alpha".to_string()),
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(42),
                    module_path: None,
                },
                LogEntry {
                    ts: 1_700_000_002,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("alphabet".to_string()),
                    feedback_log_body: Some("foo=2 alphabet".to_string()),
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(43),
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                search: Some("foo=2".to_string()),
                ..Default::default()
            })
            .await
            .expect("query matching logs");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message.as_deref(), Some("foo=2 alphabet"));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_logs_filters_level_set_without_rewriting_stored_level() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "TRACE".to_string(),
                    target: "cli".to_string(),
                    message: Some("trace-row".to_string()),
                    feedback_log_body: Some("trace-row".to_string()),
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(1),
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("info-row".to_string()),
                    feedback_log_body: Some("info-row".to_string()),
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(2),
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "warn".to_string(),
                    target: "cli".to_string(),
                    message: Some("warn-row".to_string()),
                    feedback_log_body: Some("warn-row".to_string()),
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(3),
                    module_path: None,
                },
                LogEntry {
                    ts: 4,
                    ts_nanos: 0,
                    level: "ERROR".to_string(),
                    target: "cli".to_string(),
                    message: Some("error-row".to_string()),
                    feedback_log_body: Some("error-row".to_string()),
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(4),
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                levels_upper: vec!["WARN".to_string(), "ERROR".to_string()],
                ..Default::default()
            })
            .await
            .expect("query matching logs");
        let actual = rows
            .iter()
            .map(|row| (row.level.as_str(), row.message.as_deref()))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![("warn", Some("warn-row")), ("ERROR", Some("error-row"))]
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_old_rows_when_thread_exceeds_size_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let six_mebibytes = "a".repeat(6 * 1024 * 1024);
        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("small".to_string()),
                    feedback_log_body: Some(six_mebibytes.clone()),
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(1),
                    module_path: Some("mod".to_string()),
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("small".to_string()),
                    feedback_log_body: Some(six_mebibytes.clone()),
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(2),
                    module_path: Some("mod".to_string()),
                },
            ])
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                thread_ids: vec!["thread-1".to_string()],
                ..Default::default()
            })
            .await
            .expect("query thread logs");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 2);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_single_thread_row_when_it_exceeds_size_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let eleven_mebibytes = "d".repeat(11 * 1024 * 1024);
        runtime
            .insert_logs(&[LogEntry {
                ts: 1,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some("small".to_string()),
                feedback_log_body: Some(eleven_mebibytes),
                thread_id: Some("thread-oversized".to_string()),
                process_uuid: Some("proc-1".to_string()),
                file: Some("main.rs".to_string()),
                line: Some(1),
                module_path: Some("mod".to_string()),
            }])
            .await
            .expect("insert test log");

        let rows = runtime
            .query_logs(&LogQuery {
                thread_ids: vec!["thread-oversized".to_string()],
                ..Default::default()
            })
            .await
            .expect("query thread logs");

        assert!(rows.is_empty());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_threadless_rows_per_process_uuid_only() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let six_mebibytes = "b".repeat(6 * 1024 * 1024);
        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(six_mebibytes.clone()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(1),
                    module_path: Some("mod".to_string()),
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(six_mebibytes.clone()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(2),
                    module_path: Some("mod".to_string()),
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(six_mebibytes),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(3),
                    module_path: Some("mod".to_string()),
                },
            ])
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                thread_ids: vec!["thread-1".to_string()],
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query thread and threadless logs");

        let mut timestamps: Vec<i64> = rows.into_iter().map(|row| row.ts).collect();
        timestamps.sort_unstable();
        assert_eq!(timestamps, vec![2, 3]);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_single_threadless_process_row_when_it_exceeds_size_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let eleven_mebibytes = "e".repeat(11 * 1024 * 1024);
        runtime
            .insert_logs(&[LogEntry {
                ts: 1,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some("small".to_string()),
                feedback_log_body: Some(eleven_mebibytes),
                thread_id: None,
                process_uuid: Some("proc-oversized".to_string()),
                file: Some("main.rs".to_string()),
                line: Some(1),
                module_path: Some("mod".to_string()),
            }])
            .await
            .expect("insert test log");

        let rows = runtime
            .query_logs(&LogQuery {
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query threadless logs");

        assert!(rows.is_empty());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_threadless_rows_with_null_process_uuid() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let six_mebibytes = "c".repeat(6 * 1024 * 1024);
        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(six_mebibytes.clone()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(1),
                    module_path: Some("mod".to_string()),
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(six_mebibytes),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: None,
                    file: Some("main.rs".to_string()),
                    line: Some(2),
                    module_path: Some("mod".to_string()),
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("small".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: Some("main.rs".to_string()),
                    line: Some(3),
                    module_path: Some("mod".to_string()),
                },
            ])
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query threadless logs");

        let mut timestamps: Vec<i64> = rows.into_iter().map(|row| row.ts).collect();
        timestamps.sort_unstable();
        assert_eq!(timestamps, vec![2, 3]);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_single_threadless_null_process_row_when_it_exceeds_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let eleven_mebibytes = "f".repeat(11 * 1024 * 1024);
        runtime
            .insert_logs(&[LogEntry {
                ts: 1,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some("small".to_string()),
                feedback_log_body: Some(eleven_mebibytes),
                thread_id: None,
                process_uuid: None,
                file: Some("main.rs".to_string()),
                line: Some(1),
                module_path: Some("mod".to_string()),
            }])
            .await
            .expect("insert test log");

        let rows = runtime
            .query_logs(&LogQuery {
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query threadless logs");

        assert!(rows.is_empty());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_old_rows_when_thread_exceeds_row_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let entries: Vec<LogEntry> = (1..=1_001)
            .map(|ts| LogEntry {
                ts,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some(format!("thread-row-{ts}")),
                feedback_log_body: None,
                thread_id: Some("thread-row-limit".to_string()),
                process_uuid: Some("proc-1".to_string()),
                file: Some("main.rs".to_string()),
                line: Some(ts),
                module_path: Some("mod".to_string()),
            })
            .collect();
        runtime
            .insert_logs(&entries)
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                thread_ids: vec!["thread-row-limit".to_string()],
                ..Default::default()
            })
            .await
            .expect("query thread logs");

        let timestamps: Vec<i64> = rows.into_iter().map(|row| row.ts).collect();
        assert_eq!(timestamps.len(), 1_000);
        assert_eq!(timestamps.first().copied(), Some(2));
        assert_eq!(timestamps.last().copied(), Some(1_001));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_old_threadless_rows_when_process_exceeds_row_limit() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let entries: Vec<LogEntry> = (1..=1_001)
            .map(|ts| LogEntry {
                ts,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some(format!("process-row-{ts}")),
                feedback_log_body: None,
                thread_id: None,
                process_uuid: Some("proc-row-limit".to_string()),
                file: Some("main.rs".to_string()),
                line: Some(ts),
                module_path: Some("mod".to_string()),
            })
            .collect();
        runtime
            .insert_logs(&entries)
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query threadless logs");

        let timestamps: Vec<i64> = rows
            .into_iter()
            .filter(|row| row.process_uuid.as_deref() == Some("proc-row-limit"))
            .map(|row| row.ts)
            .collect();
        assert_eq!(timestamps.len(), 1_000);
        assert_eq!(timestamps.first().copied(), Some(2));
        assert_eq!(timestamps.last().copied(), Some(1_001));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn insert_logs_prunes_old_threadless_null_process_rows_when_row_limit_exceeded() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let entries: Vec<LogEntry> = (1..=1_001)
            .map(|ts| LogEntry {
                ts,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "cli".to_string(),
                message: Some(format!("null-process-row-{ts}")),
                feedback_log_body: None,
                thread_id: None,
                process_uuid: None,
                file: Some("main.rs".to_string()),
                line: Some(ts),
                module_path: Some("mod".to_string()),
            })
            .collect();
        runtime
            .insert_logs(&entries)
            .await
            .expect("insert test logs");

        let rows = runtime
            .query_logs(&LogQuery {
                include_threadless: true,
                ..Default::default()
            })
            .await
            .expect("query threadless logs");

        let timestamps: Vec<i64> = rows
            .into_iter()
            .filter(|row| row.process_uuid.is_none())
            .map(|row| row.ts)
            .collect();
        assert_eq!(timestamps.len(), 1_000);
        assert_eq!(timestamps.first().copied(), Some(2));
        assert_eq!(timestamps.last().copied(), Some(1_001));

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_returns_newest_lines_within_limit_in_order() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("alpha".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("bravo".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("charlie".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs("thread-1")
            .await
            .expect("query feedback logs");

        assert_eq!(
            String::from_utf8(bytes).expect("valid utf-8"),
            [
                format_feedback_log_line(/*ts*/ 1, /*ts_nanos*/ 0, "INFO", "alpha"),
                format_feedback_log_line(/*ts*/ 2, /*ts_nanos*/ 0, "INFO", "bravo"),
                format_feedback_log_line(/*ts*/ 3, /*ts_nanos*/ 0, "INFO", "charlie"),
            ]
            .concat()
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_excludes_oversized_newest_row() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let eleven_mebibytes = "z".repeat(11 * 1024 * 1024);

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("small".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-oversized".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(eleven_mebibytes),
                    feedback_log_body: None,
                    thread_id: Some("thread-oversized".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs("thread-oversized")
            .await
            .expect("query feedback logs");

        assert_eq!(bytes, Vec::<u8>::new());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_includes_threadless_rows_from_same_process() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("threadless-before".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("thread-scoped".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("threadless-after".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 4,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("other-process-threadless".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-2".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs("thread-1")
            .await
            .expect("query feedback logs");

        assert_eq!(
            String::from_utf8(bytes).expect("valid utf-8"),
            [
                format_feedback_log_line(
                    /*ts*/ 1,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "threadless-before"
                ),
                format_feedback_log_line(
                    /*ts*/ 2,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "thread-scoped"
                ),
                format_feedback_log_line(
                    /*ts*/ 3,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "threadless-after"
                ),
            ]
            .concat()
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_excludes_threadless_rows_from_prior_processes() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("old-process-threadless".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-old".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("old-process-thread".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-old".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("new-process-thread".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-new".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 4,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("new-process-threadless".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-new".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs("thread-1")
            .await
            .expect("query feedback logs");

        assert_eq!(
            String::from_utf8(bytes).expect("valid utf-8"),
            [
                format_feedback_log_line(
                    /*ts*/ 2,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "old-process-thread"
                ),
                format_feedback_log_line(
                    /*ts*/ 3,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "new-process-thread"
                ),
                format_feedback_log_line(
                    /*ts*/ 4,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "new-process-threadless"
                ),
            ]
            .concat()
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_keeps_newest_suffix_across_thread_and_threadless_logs() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");
        let thread_marker = "thread-scoped-oldest";
        let threadless_older_marker = "threadless-older";
        let threadless_newer_marker = "threadless-newer";
        let five_mebibytes = format!("{threadless_older_marker} {}", "a".repeat(5 * 1024 * 1024));
        let four_and_half_mebibytes = format!(
            "{threadless_newer_marker} {}",
            "b".repeat((9 * 1024 * 1024) / 2)
        );
        let one_mebibyte = format!("{thread_marker} {}", "c".repeat(1024 * 1024));

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(one_mebibyte.clone()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(five_mebibytes),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some(four_and_half_mebibytes),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs("thread-1")
            .await
            .expect("query feedback logs");
        let logs = String::from_utf8(bytes).expect("valid utf-8");

        assert!(!logs.contains(thread_marker));
        assert!(logs.contains(threadless_older_marker));
        assert!(logs.contains(threadless_newer_marker));
        assert_eq!(logs.matches('\n').count(), 2);

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_for_threads_merges_requested_threads_and_threadless_rows() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        runtime
            .insert_logs(&[
                LogEntry {
                    ts: 1,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("thread-1".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-1".to_string()),
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 2,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("thread-2".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-2".to_string()),
                    process_uuid: Some("proc-2".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 3,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("threadless-proc-1".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-1".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 4,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("threadless-proc-2".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-2".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 5,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("thread-3".to_string()),
                    feedback_log_body: None,
                    thread_id: Some("thread-3".to_string()),
                    process_uuid: Some("proc-3".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
                LogEntry {
                    ts: 6,
                    ts_nanos: 0,
                    level: "INFO".to_string(),
                    target: "cli".to_string(),
                    message: Some("threadless-proc-3".to_string()),
                    feedback_log_body: None,
                    thread_id: None,
                    process_uuid: Some("proc-3".to_string()),
                    file: None,
                    line: None,
                    module_path: None,
                },
            ])
            .await
            .expect("insert test logs");

        let bytes = runtime
            .query_feedback_logs_for_threads(&["thread-1", "thread-2"])
            .await
            .expect("query feedback logs");

        assert_eq!(
            String::from_utf8(bytes).expect("valid utf-8"),
            [
                format_feedback_log_line(/*ts*/ 1, /*ts_nanos*/ 0, "INFO", "thread-1"),
                format_feedback_log_line(/*ts*/ 2, /*ts_nanos*/ 0, "INFO", "thread-2"),
                format_feedback_log_line(
                    /*ts*/ 3,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "threadless-proc-1"
                ),
                format_feedback_log_line(
                    /*ts*/ 4,
                    /*ts_nanos*/ 0,
                    "INFO",
                    "threadless-proc-2"
                ),
            ]
            .concat()
        );

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }

    #[tokio::test]
    async fn query_feedback_logs_for_threads_returns_empty_for_empty_thread_list() {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home.clone(), "test-provider".to_string())
            .await
            .expect("initialize runtime");

        let bytes = runtime
            .query_feedback_logs_for_threads(&[])
            .await
            .expect("query feedback logs");

        assert_eq!(bytes, Vec::<u8>::new());

        let _ = tokio::fs::remove_dir_all(codex_home).await;
    }
}
