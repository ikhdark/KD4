use super::*;

const LOG_RETENTION_DAYS: i64 = 10;
const MAX_PENDING_RETENTION_KEYS: usize = 256;
const RETENTION_QUERY_KEY_BATCH: usize = 200;
const RETENTION_AGE_ROW_BATCH: i64 = 1000;
// Bounded vacuum steps keep each write transaction short for concurrent log writers.
const INCREMENTAL_VACUUM_STEP: &str = "PRAGMA incremental_vacuum(2048)";

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






impl StateRuntime {
    pub(crate) fn record_log_retention_event(&self, event: &'static str) {
        crate::telemetry::record_log_retention_event(self.db_telemetry.as_deref(), event);
    }



    pub(crate) async fn insert_logs_deferred_retention(
        &self,
        entries: &[LogEntry],
    ) -> anyhow::Result<LogRetentionScope> {
        if entries.is_empty() {
            return Ok(LogRetentionScope::default());
        }

        let scope = LogRetentionScope::from_entries(entries);
        let observation_started = tracing::enabled!(target: "codex_state::log_batch_observation", tracing::Level::TRACE)
            .then(Instant::now);
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
        // Reserve the log writer before consulting the durable deletion fence.
        // Deletion releases its state transaction before purging logs, so a
        // concurrent batch is either filtered here or removed by that purge.
        let transaction_result = sqlx::Connection::begin_with(&mut *connection, "BEGIN IMMEDIATE")
            .await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "insert",
            "transaction_begin",
            started.elapsed(),
            &transaction_result,
        );
        let mut tx = transaction_result?;
        let deleted = if scope.thread_ids.is_empty() {
            BTreeSet::new()
        } else {
            let thread_ids = scope.thread_ids.iter().collect::<Vec<_>>();
            sqlx::query_scalar::<_, String>(
                "SELECT thread_id FROM deleted_thread_owners WHERE thread_id IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(&thread_ids)?)
            .fetch_all(self.pool.as_ref())
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>()
        };
        let admitted = entries.iter().filter(|entry| {
            entry.thread_id.as_ref().is_none_or(|id| !deleted.contains(id))
        }).collect::<Vec<_>>();
        // Eleven binds per row; cap each statement below SQLite's 32,766 limit.
        for entries in admitted.chunks(2_978) {
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
        }
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
        if let Some(started) = observation_started {
            // Content-free, opt-in settlement evidence. This target is excluded
            // from the SQLite tracing exporter so observing a writer cannot
            // recursively enqueue more writer work.
            let mut counts = std::collections::BTreeMap::<&str, usize>::new();
            for entry in entries {
                if let Some(thread_id) = entry.thread_id.as_deref() {
                    *counts.entry(thread_id).or_default() += 1;
                }
            }
            for (thread_id, input_entries) in counts {
                let fenced_entries = if deleted.contains(thread_id) { input_entries } else { 0 };
                tracing::trace!(
                    target: "codex_state::log_batch_observation",
                    thread_id,
                    input_entries,
                    admitted_entries = input_entries - fenced_entries,
                    fenced_entries,
                    batch_entries = entries.len(),
                    batch_elapsed_us = started.elapsed().as_micros() as u64,
                    "log_batch_settled"
                );
            }
        }
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

        if let Some(cutoff) = Utc::now()
            .checked_sub_signed(chrono::Duration::days(LOG_RETENTION_DAYS))
            .map(|cutoff| cutoff.timestamp())
        {
            // Fence this pass against concurrent inserts with old timestamps. Each
            // bounded deletion commits before the next one takes writer ownership.
            let last_id: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM logs")
                .fetch_one(self.logs_pool.as_ref())
                .await?;
            loop {
                let removed = self
                    .prune_log_retention_batch(None, Some((cutoff, last_id)))
                    .await?;
                if removed < RETENTION_AGE_ROW_BATCH as u64 {
                    break;
                }
            }
        }
        for batch in scope.into_query_batches() {
            self.prune_log_retention_batch(Some(&batch), None).await?;
        }
        if reconcile_all {
            self.reclaim_free_log_pages().await;
        }
        Ok(())
    }

    async fn prune_log_retention_batch(
        &self,
        scope: Option<&LogRetentionScope>,
        age_bound: Option<(i64, i64)>,
    ) -> anyhow::Result<u64> {
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
        let deletion_result: anyhow::Result<u64> = async {
            let mut removed = 0;
            if let Some((cutoff, last_id)) = age_bound {
                removed = sqlx::query(
                    "DELETE FROM logs WHERE id IN (SELECT id FROM logs WHERE ts < ? AND id <= ? ORDER BY ts, ts_nanos, id LIMIT ?)",
                )
                .bind(cutoff)
                .bind(last_id)
                .bind(RETENTION_AGE_ROW_BATCH)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            }
            if let Some(scope) = scope {
                self.prune_logs_for_scope(scope, &mut tx).await?;
            }
            Ok(removed)
        }
        .await;
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "deletion",
            started.elapsed(),
            &deletion_result,
        );
        let removed = deletion_result?;
        let started = Instant::now();
        let commit_result = tx.commit().await.map_err(anyhow::Error::from);
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "commit",
            started.elapsed(),
            &commit_result,
        );
        commit_result?;
        // Return the pool slot before a following batch or reclamation acquires one.
        drop(connection);
        Ok(removed)
    }

    /// Return free pages to the filesystem on reconciliation passes. Incremental auto-vacuum
    /// only shrinks the file when asked, so without this a one-time spike (a schema copy or a
    /// retention backlog) stays on disk for the life of the database.
    async fn reclaim_free_log_pages(&self) {
        let started = Instant::now();
        let result = async {
            let mut free_pages = log_freelist_count(self.logs_pool.as_ref()).await?;
            while free_pages > 0 {
                sqlx::query(INCREMENTAL_VACUUM_STEP)
                    .execute(self.logs_pool.as_ref())
                    .await?;
                let remaining = log_freelist_count(self.logs_pool.as_ref()).await?;
                if remaining >= free_pages {
                    // Databases created without incremental auto-vacuum cannot shrink this way.
                    break;
                }
                free_pages = remaining;
            }
            anyhow::Ok(())
        }
        .await;
        crate::telemetry::record_log_phase(
            self.db_telemetry.as_deref(),
            "retention",
            "vacuum",
            started.elapsed(),
            &result,
        );
        if let Err(err) = result {
            warn!("failed to reclaim free log database pages: {err}");
        }
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
        // Phase diagnostics are exempt from the noisy row-count budget, but
        // still obey the existing byte and age limits.
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
            over_limit_threads_query
                .push(" OR SUM(target != 'codex_core::tools::execution_phase') > ");
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
            target,
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
            SUM(target != 'codex_core::tools::execution_phase') OVER (
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
                prune_threads
                    .push(" OR (target != 'codex_core::tools::execution_phase' AND row_number > ");
                prune_threads.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threads.push(")\n)");
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
            over_limit_processes_query
                .push(" OR SUM(target != 'codex_core::tools::execution_phase') > ");
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
            target,
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
            SUM(target != 'codex_core::tools::execution_phase') OVER (
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
                prune_threadless_process_logs
                    .push(" OR (target != 'codex_core::tools::execution_phase' AND row_number > ");
                prune_threadless_process_logs.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threadless_process_logs.push(")\n)");
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
                ") AS total_bytes, COUNT(CASE WHEN target != 'codex_core::tools::execution_phase' THEN 1 END) AS row_count FROM logs WHERE thread_id IS NULL AND process_uuid IS NULL",
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
            target,
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
            SUM(target != 'codex_core::tools::execution_phase') OVER (
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
                prune_threadless_null_process_logs
                    .push(" OR (target != 'codex_core::tools::execution_phase' AND row_number > ");
                prune_threadless_null_process_logs.push_bind(LOG_PARTITION_ROW_LIMIT);
                prune_threadless_null_process_logs.push(")\n)");
                prune_threadless_null_process_logs
                    .build()
                    .execute(&mut *tx)
                    .await?;
            }
        }
        Ok(())
    }

    /// Query logs with optional filters.
    pub async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        query_logs(self.logs_pool.as_ref(), query).await
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

    /// Return the max log id matching optional filters.
    pub async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        max_log_id(self.logs_pool.as_ref(), query).await
    }
}

/// Read-only access to an existing logs database, without initializing other stores.
pub struct LogReader {
    pool: SqlitePool,
}

impl LogReader {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    pub async fn query_logs(&self, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
        query_logs(&self.pool, query).await
    }

    pub async fn max_log_id(&self, query: &LogQuery) -> anyhow::Result<i64> {
        max_log_id(&self.pool, query).await
    }
}

async fn query_logs(pool: &SqlitePool, query: &LogQuery) -> anyhow::Result<Vec<LogRow>> {
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

    let rows = builder.build_query_as::<LogRow>().fetch_all(pool).await?;
    Ok(rows)
}

async fn log_freelist_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar::<_, i64>("PRAGMA freelist_count")
        .fetch_one(pool)
        .await?)
}

async fn max_log_id(pool: &SqlitePool, query: &LogQuery) -> anyhow::Result<i64> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT MAX(id) AS max_id FROM logs WHERE 1 = 1");
    push_log_filters(&mut builder, query);
    let row = builder.build().fetch_one(pool).await?;
    let max_id: Option<i64> = row.try_get("max_id")?;
    Ok(max_id.unwrap_or(0))
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
        if !query.thread_ids.is_empty() {
            // IN is flat: an OR per thread reaches SQLite's expression-depth
            // limit well before its supported bind-parameter limit.
            builder.push("thread_id IN (SELECT value FROM json_each(");
            builder.push_bind(sqlx::types::Json(query.thread_ids.clone()));
            builder.push("))");
        }
        if query.include_threadless {
            if !query.thread_ids.is_empty() {
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
            .push_bind(
                filter
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_"),
            )
            .push(" || '%' ESCAPE '\\'");
    }
    builder.push(")");
}

