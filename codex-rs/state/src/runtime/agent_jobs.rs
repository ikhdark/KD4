use super::*;
use crate::model::AgentJobItemRow;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

const AGENT_JOB_RUNNER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const AGENT_JOB_RUNNER_LEASE_TIMEOUT_SECONDS: i64 = 30;

// Shared by ordinary settlement and restart recovery. Non-null bytes alone are
// not a report. A retained current-attempt binding may not name another worker.
const VALID_RETAINED_REPORT: &str = "status = 'running' AND attempt_count > 0 AND reported_at IS NOT NULL AND CASE WHEN json_valid(result_json) THEN json_type(result_json) = 'object' ELSE 0 END AND NOT EXISTS (SELECT 1 FROM agent_jobs WHERE id = agent_job_items.job_id AND status = 'cancelled') AND NOT EXISTS (SELECT 1 FROM agent_job_attempts a WHERE a.job_id = agent_job_items.job_id AND a.item_id = agent_job_items.item_id AND a.attempt_count = agent_job_items.attempt_count AND agent_job_items.assigned_thread_id IS NOT NULL AND a.thread_id <> agent_job_items.assigned_thread_id)";

pub(super) async fn register_agent_job_runner_instance(
    pool: &SqlitePool,
    runner_instance_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
INSERT INTO agent_job_runner_instances (runner_instance_id, heartbeat_at)
VALUES (?, ?)
ON CONFLICT(runner_instance_id) DO UPDATE SET heartbeat_at = excluded.heartbeat_at
        "#,
    )
    .bind(runner_instance_id)
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

pub(super) fn spawn_agent_job_runner_heartbeat(
    pool: Arc<SqlitePool>,
    runner_instance_id: String,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AGENT_JOB_RUNNER_HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = sqlx::query(
                        "UPDATE agent_job_runner_instances SET heartbeat_at = ? WHERE runner_instance_id = ?",
                    )
                    .bind(Utc::now().timestamp())
                    .bind(runner_instance_id.as_str())
                    .execute(pool.as_ref())
                    .await
                    {
                        tracing::warn!(%error, %runner_instance_id, "failed to heartbeat agent job runner instance");
                    }
                }
            }
        }
    });
}

pub(super) async fn unregister_agent_job_runner_instance(
    pool: &SqlitePool,
    runner_instance_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM agent_job_runner_instances WHERE runner_instance_id = ?")
        .bind(runner_instance_id)
        .execute(pool)
        .await?;
    Ok(())
}

impl StateRuntime {
    pub const AGENT_JOB_RESTART_ERROR: &'static str =
        "agent job runner was interrupted by a process restart";

    pub fn agent_job_runner_instance_id(&self) -> &str {
        self.agent_job_runner_instance_id.as_str()
    }

    pub async fn agent_job_runner_instance_is_live(&self, owner: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM agent_job_runner_instances WHERE runner_instance_id = ? AND heartbeat_at >= ?)")
            .bind(owner).bind(Utc::now().timestamp().saturating_sub(AGENT_JOB_RUNNER_LEASE_TIMEOUT_SECONDS))
            .fetch_one(self.pool.as_ref()).await? != 0)
    }

    pub async fn create_agent_job(
        &self,
        params: &AgentJobCreateParams,
        items: &[AgentJobItemCreateParams],
    ) -> anyhow::Result<AgentJob> {
        let now = Utc::now().timestamp();
        let input_headers_json = serde_json::to_string(&params.input_headers)?;
        let output_schema_json = params
            .output_schema_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let max_runtime_seconds = params
            .max_runtime_seconds
            .map(i64::try_from)
            .transpose()
            .map_err(|_| anyhow::anyhow!("invalid max_runtime_seconds value"))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
INSERT INTO agent_jobs (
    id,
    name,
    status,
    instruction,
    auto_export,
    max_runtime_seconds,
    output_schema_json,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    runner_instance_id,
    created_at,
    updated_at,
    started_at,
    completed_at,
    last_error
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)
            "#,
        )
        .bind(params.id.as_str())
        .bind(params.name.as_str())
        .bind(AgentJobStatus::Pending.as_str())
        .bind(params.instruction.as_str())
        .bind(i64::from(params.auto_export))
        .bind(max_runtime_seconds)
        .bind(output_schema_json)
        .bind(input_headers_json)
        .bind(params.input_csv_path.as_str())
        .bind(params.output_csv_path.as_str())
        .bind(self.agent_job_runner_instance_id())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        // Eight bound columns per item, below SQLite's supported variable limit.
        for chunk in items.chunks(4_000) {
            let rows = chunk
                .iter()
                .map(|item| serde_json::to_string(&item.row_json))
                .collect::<Result<Vec<_>, _>>()?;
            let mut builder = QueryBuilder::<Sqlite>::new(
                "INSERT INTO agent_job_items (job_id, item_id, row_index, source_id, row_json, status, created_at, updated_at) ",
            );
            builder.push_values(chunk.iter().zip(&rows), |mut row, (item, row_json)| {
                row.push_bind(params.id.as_str())
                    .push_bind(item.item_id.as_str())
                    .push_bind(item.row_index)
                    .push_bind(item.source_id.as_deref())
                    .push_bind(row_json)
                    .push_bind(AgentJobItemStatus::Pending.as_str())
                    .push_bind(now)
                    .push_bind(now);
            });
            builder.build().execute(&mut *tx).await?;
        }

        tx.commit().await?;
        self.notify_agent_job_change();

        let job_id = params.id.as_str();
        self.get_agent_job(job_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("failed to load created agent job {job_id}"))
    }

    pub async fn reconcile_orphaned_agent_jobs_after_restart(
        &self,
    ) -> anyhow::Result<Vec<AgentJob>> {
        let now = Utc::now().timestamp();
        let stale_before = now.saturating_sub(AGENT_JOB_RUNNER_LEASE_TIMEOUT_SECONDS);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            r#"
SELECT jobs.id, jobs.runner_instance_id
FROM agent_jobs AS jobs
LEFT JOIN agent_job_runner_instances AS runners
    ON runners.runner_instance_id = jobs.runner_instance_id
WHERE
    (jobs.status IN (?, ?) OR EXISTS (
        SELECT 1 FROM agent_job_items AS items
        WHERE items.job_id = jobs.id AND items.status = 'running'
    ))
    AND jobs.runner_instance_id IS NOT NULL
    AND jobs.runner_instance_id <> ?
    AND (runners.runner_instance_id IS NULL OR runners.heartbeat_at < ?)
ORDER BY jobs.created_at ASC, jobs.id ASC
            "#,
        )
        .bind(AgentJobStatus::Running.as_str())
        .bind(AgentJobStatus::Pending.as_str())
        .bind(self.agent_job_runner_instance_id())
        .bind(stale_before)
        .fetch_all(&mut *tx)
        .await?;
        let candidates = rows
            .into_iter()
            .map(|row| -> Result<(String, String), sqlx::Error> {
                Ok((
                    row.try_get::<String, _>("id")?,
                    row.try_get::<String, _>("runner_instance_id")?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut job_ids = Vec::with_capacity(candidates.len());
        for (job_id, owner_runner_instance_id) in candidates {
            let updated = sqlx::query(
                r#"
UPDATE agent_jobs
SET status = CASE WHEN status IN ('pending', 'running') THEN ? ELSE status END,
    updated_at = ?, completed_at = COALESCE(completed_at, ?),
    last_error = CASE WHEN status IN ('pending', 'running') THEN ? ELSE last_error END
WHERE
    id = ?
    AND (status IN (?, ?) OR EXISTS (
        SELECT 1 FROM agent_job_items WHERE job_id = agent_jobs.id AND status = 'running'
    ))
    AND runner_instance_id = ?
    AND runner_instance_id <> ?
    AND NOT EXISTS (
        SELECT 1
        FROM agent_job_runner_instances AS runners
        WHERE runners.runner_instance_id = ? AND runners.heartbeat_at >= ?
    )
                "#,
            )
            .bind(AgentJobStatus::Failed.as_str())
            .bind(now)
            .bind(now)
            .bind(Self::AGENT_JOB_RESTART_ERROR)
            .bind(job_id.as_str())
            .bind(AgentJobStatus::Running.as_str())
            .bind(AgentJobStatus::Pending.as_str())
            .bind(owner_runner_instance_id.as_str())
            .bind(self.agent_job_runner_instance_id())
            .bind(owner_runner_instance_id.as_str())
            .bind(stale_before)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                continue;
            }
            QueryBuilder::<Sqlite>::new(format!(
                r#"
UPDATE agent_job_items
SET
    status = CASE WHEN ({VALID_RETAINED_REPORT}) AND NOT EXISTS (SELECT 1 FROM agent_jobs WHERE id = agent_job_items.job_id AND status = 'cancelled') THEN 'completed' ELSE 'failed' END,
    assigned_thread_id = NULL,
    updated_at = ?,
    completed_at = ?,
    last_error = CASE WHEN ({VALID_RETAINED_REPORT}) AND NOT EXISTS (SELECT 1 FROM agent_jobs WHERE id = agent_job_items.job_id AND status = 'cancelled') THEN NULL ELSE ? END
WHERE job_id = ? AND (status = ? OR (status = ? AND EXISTS (
    SELECT 1 FROM agent_jobs WHERE id = agent_job_items.job_id AND status = 'failed'
    AND last_error = ?
)))
                "#,
            ))
            .build()
            .bind(now)
            .bind(now)
            .bind(Self::AGENT_JOB_RESTART_ERROR)
            .bind(job_id.as_str())
            .bind(AgentJobItemStatus::Running.as_str())
            .bind(AgentJobItemStatus::Pending.as_str())
            .bind(Self::AGENT_JOB_RESTART_ERROR)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO agent_job_recovery_exports (job_id) VALUES (?) ON CONFLICT(job_id) DO NOTHING")
                .bind(&job_id).execute(&mut *tx).await?;
            job_ids.push(job_id);
        }
        tx.commit().await?;
        self.notify_agent_job_change();

        let mut jobs = Vec::with_capacity(job_ids.len());
        for job_id in job_ids {
            let job = self
                .get_agent_job(job_id.as_str())
                .await?
                .ok_or_else(|| anyhow::anyhow!("reconciled agent job {job_id} was not found"))?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    /// Claim a bounded batch. The token fences acknowledgment after cancellation,
    /// restart or a lease takeover; failed exports are retried by the existing timer.
    pub async fn claim_agent_job_recovery_exports(&self) -> anyhow::Result<Vec<(String, String)>> {
        let now = Utc::now().timestamp();
        let token = uuid::Uuid::now_v7().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let ids = sqlx::query_scalar::<_, String>("SELECT job_id FROM agent_job_recovery_exports AS exports WHERE retry_at <= ? AND revision > published_revision AND EXISTS (SELECT 1 FROM agent_jobs WHERE id = exports.job_id AND status IN ('completed', 'failed', 'cancelled')) ORDER BY retry_at, job_id LIMIT 32")
            .bind(now).fetch_all(&mut *tx).await?;
        for id in &ids {
            sqlx::query("UPDATE agent_job_recovery_exports SET claim_token = ?, claim_revision = revision, retry_at = ? WHERE job_id = ?")
                .bind(&token).bind(now + 300).bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(ids.into_iter().map(|id| (id, token.clone())).collect())
    }

    pub async fn finish_agent_job_recovery_export(
        &self,
        job_id: &str,
        token: &str,
        succeeded: bool,
    ) -> anyhow::Result<()> {
        if succeeded {
            sqlx::query(
                "UPDATE agent_job_recovery_exports SET published_revision = MAX(published_revision, claim_revision), claim_token = NULL, claim_revision = NULL, retry_at = 0 WHERE job_id = ? AND claim_token = ?",
            )
            .bind(job_id)
            .bind(token)
            .execute(self.pool.as_ref())
            .await?;
        } else {
            sqlx::query("UPDATE agent_job_recovery_exports SET claim_token = NULL, retry_at = ? WHERE job_id = ? AND claim_token = ?")
                .bind(Utc::now().timestamp() + 30).bind(job_id).bind(token).execute(self.pool.as_ref()).await?;
        }
        Ok(())
    }

    /// Call only while holding the destination publication lock. A read
    /// transaction pairs the exact revision with the rows it describes.
    pub async fn agent_job_export_snapshot(
        &self,
        job_id: &str,
    ) -> anyhow::Result<(i64, Vec<AgentJobItem>)> {
        let mut tx = self.pool.begin().await?;
        let revision: i64 =
            sqlx::query_scalar("SELECT revision FROM agent_job_recovery_exports WHERE job_id = ?")
                .bind(job_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(0);
        let rows = sqlx::query_as::<_, AgentJobItemRow>(
            "SELECT * FROM agent_job_items WHERE job_id = ? ORDER BY row_index",
        )
        .bind(job_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((
            revision,
            rows.into_iter()
                .map(AgentJobItem::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }

    pub async fn acknowledge_agent_job_export(
        &self,
        job_id: &str,
        revision: i64,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE agent_job_recovery_exports SET published_revision = MAX(published_revision, ?) WHERE job_id = ? AND revision >= ?")
            .bind(revision).bind(job_id).bind(revision).execute(self.pool.as_ref()).await?;
        Ok(())
    }

    pub async fn agent_job_export_is_published(&self, job_id: &str) -> anyhow::Result<bool> {
        let receipt: Option<(String, String)> = sqlx::query_as("SELECT p.destination, p.sha256 FROM agent_job_publications p JOIN agent_job_recovery_exports e ON e.job_id = p.job_id WHERE p.job_id = ? AND p.revision = e.revision AND e.revision = e.published_revision AND p.sha256 IS NOT NULL")
            .bind(job_id).fetch_optional(self.pool.as_ref()).await?;
        let Some((path, digest)) = receipt else { return Ok(false); };
        use sha2::Digest;
        use tokio::io::AsyncReadExt;
        let mut file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut hash = sha2::Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 { break; }
            hash.update(&buffer[..read]);
        }
        let actual_digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(actual_digest == digest)
    }

    /// Call while owning the filesystem destination lock; database authority
    /// additionally prevents another job in this ledger from claiming it.
    pub async fn reserve_agent_job_destination(&self, job_id: &str, destination: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("INSERT INTO agent_job_publications (destination, job_id) VALUES (?, ?) ON CONFLICT(destination) DO NOTHING")
            .bind(destination).bind(job_id).execute(&mut *tx).await?;
        let owner: String = sqlx::query_scalar("SELECT job_id FROM agent_job_publications WHERE destination = ?")
            .bind(destination).fetch_one(&mut *tx).await?;
        anyhow::ensure!(owner == job_id, "csv_destination_conflict: destination belongs to another job; choose a distinct output path");
        tx.commit().await?;
        Ok(())
    }

    pub async fn acknowledge_agent_job_artifact(&self, job_id: &str, revision: i64, destination: &str, digest: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query("UPDATE agent_job_publications SET revision = ?, sha256 = ? WHERE destination = ? AND job_id = ?")
            .bind(revision).bind(digest).bind(destination).bind(job_id).execute(&mut *tx).await?;
        anyhow::ensure!(updated.rows_affected() == 1, "csv_destination_conflict: publication owner changed");
        sqlx::query("UPDATE agent_job_recovery_exports SET published_revision = MAX(published_revision, ?) WHERE job_id = ? AND revision >= ?")
            .bind(revision).bind(job_id).bind(revision).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn agent_job_artifact_digest(&self, job_id: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, Option<String>>("SELECT p.sha256 FROM agent_job_publications p JOIN agent_job_recovery_exports e ON e.job_id = p.job_id WHERE p.job_id = ? AND p.revision = e.revision AND e.published_revision = e.revision")
            .bind(job_id).fetch_optional(self.pool.as_ref()).await?.flatten())
    }

    pub async fn agent_job_is_owned_running(&self, job_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM agent_jobs WHERE id = ? AND status = 'running' AND runner_instance_id = ?)")
            .bind(job_id).bind(self.agent_job_runner_instance_id()).fetch_one(self.pool.as_ref()).await? != 0)
    }

    pub async fn agent_job_is_owned(&self, job_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT EXISTS(SELECT 1 FROM agent_jobs WHERE id = ? AND runner_instance_id = ?)",
        )
        .bind(job_id)
        .bind(self.agent_job_runner_instance_id())
        .fetch_one(self.pool.as_ref())
        .await?
            != 0)
    }

    pub async fn agent_job_has_unfinished_items(&self, job_id: &str) -> anyhow::Result<bool> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM agent_job_items WHERE job_id = ? AND status IN ('pending', 'running'))")
            .bind(job_id).fetch_one(self.pool.as_ref()).await? != 0)
    }

    /// Worker-event authority is narrower than job-wide administrative failure.
    /// `None` fences a malformed legacy row with no assigned worker.
    pub async fn settle_agent_job_item_for_thread(
        &self,
        job_id: &str,
        item_id: &str,
        thread_id: Option<&str>,
        reason: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = QueryBuilder::<Sqlite>::new(format!("UPDATE agent_job_items SET status = CASE WHEN ({VALID_RETAINED_REPORT}) AND NOT EXISTS (SELECT 1 FROM agent_jobs WHERE id = agent_job_items.job_id AND status = 'cancelled') THEN 'completed' ELSE 'failed' END, completed_at = ?, updated_at = ?, last_error = CASE WHEN ({VALID_RETAINED_REPORT}) THEN NULL ELSE ? END, assigned_thread_id = NULL WHERE job_id = ? AND item_id = ? AND status = 'running' AND assigned_thread_id IS ?"))
            .build()
            .bind(now).bind(now).bind(reason).bind(job_id).bind(item_id).bind(thread_id).execute(self.pool.as_ref()).await?;
        if result.rows_affected() > 0 {
            self.notify_agent_job_change();
        }
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_agent_job_attempts(
        &self,
        job_id: &str,
        item_id: &str,
    ) -> anyhow::Result<Vec<(i64, String)>> {
        Ok(sqlx::query_as("SELECT attempt_count, thread_id FROM agent_job_attempts WHERE job_id = ? AND item_id = ? ORDER BY attempt_count")
            .bind(job_id).bind(item_id).fetch_all(self.pool.as_ref()).await?)
    }

    pub async fn get_agent_job(&self, job_id: &str) -> anyhow::Result<Option<AgentJob>> {
        let row = sqlx::query_as::<_, AgentJobRow>(
            r#"
SELECT
    id,
    name,
    status,
    instruction,
    auto_export,
    max_runtime_seconds,
    output_schema_json,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    created_at,
    updated_at,
    started_at,
    completed_at,
    last_error
FROM agent_jobs
WHERE id = ?
            "#,
        )
        .bind(job_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(AgentJob::try_from).transpose()
    }

    pub async fn list_agent_job_items(
        &self,
        job_id: &str,
        status: Option<AgentJobItemStatus>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<AgentJobItem>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
SELECT
    job_id,
    item_id,
    row_index,
    source_id,
    row_json,
    status,
    assigned_thread_id,
    attempt_count,
    result_json,
    last_error,
    created_at,
    updated_at,
    completed_at,
    reported_at
FROM agent_job_items
WHERE job_id = 
            "#,
        );
        builder.push_bind(job_id);
        if let Some(status) = status {
            builder.push(" AND status = ");
            builder.push_bind(status.as_str());
        }
        builder.push(" ORDER BY row_index ASC");
        if let Some(limit) = limit {
            builder.push(" LIMIT ");
            builder.push_bind(limit as i64);
        }
        let rows: Vec<AgentJobItemRow> = builder
            .build_query_as::<AgentJobItemRow>()
            .fetch_all(self.pool.as_ref())
            .await?;
        rows.into_iter().map(AgentJobItem::try_from).collect()
    }

    pub async fn get_agent_job_item(
        &self,
        job_id: &str,
        item_id: &str,
    ) -> anyhow::Result<Option<AgentJobItem>> {
        let row: Option<AgentJobItemRow> = sqlx::query_as::<_, AgentJobItemRow>(
            r#"
SELECT
    job_id,
    item_id,
    row_index,
    source_id,
    row_json,
    status,
    assigned_thread_id,
    attempt_count,
    result_json,
    last_error,
    created_at,
    updated_at,
    completed_at,
    reported_at
FROM agent_job_items
WHERE job_id = ? AND item_id = ?
            "#,
        )
        .bind(job_id)
        .bind(item_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(AgentJobItem::try_from).transpose()
    }

    pub async fn mark_agent_job_running(&self, job_id: &str) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
UPDATE agent_jobs
SET
    status = ?,
    updated_at = ?,
    started_at = COALESCE(started_at, ?),
    completed_at = NULL,
    last_error = NULL
WHERE id = ? AND status = ?
            "#,
        )
        .bind(AgentJobStatus::Running.as_str())
        .bind(now)
        .bind(now)
        .bind(job_id)
        .bind(AgentJobStatus::Pending.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(())
    }

    pub async fn mark_agent_job_completed(&self, job_id: &str) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
UPDATE agent_jobs
SET status = ?, updated_at = ?, completed_at = ?, last_error = NULL
WHERE id = ? AND status = ?
            "#,
        )
        .bind(AgentJobStatus::Completed.as_str())
        .bind(now)
        .bind(now)
        .bind(job_id)
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(())
    }

    pub async fn mark_agent_job_failed(
        &self,
        job_id: &str,
        error_message: &str,
    ) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
UPDATE agent_jobs
SET status = ?, updated_at = ?, completed_at = ?, last_error = ?
WHERE id = ? AND status IN (?, ?)
            "#,
        )
        .bind(AgentJobStatus::Failed.as_str())
        .bind(now)
        .bind(now)
        .bind(error_message)
        .bind(job_id)
        .bind(AgentJobStatus::Pending.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(())
    }

    pub async fn mark_agent_job_cancelled(
        &self,
        job_id: &str,
        reason: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_jobs
SET status = ?, updated_at = ?, completed_at = ?, last_error = ?
WHERE id = ? AND status IN (?, ?)
            "#,
        )
        .bind(AgentJobStatus::Cancelled.as_str())
        .bind(now)
        .bind(now)
        .bind(reason)
        .bind(job_id)
        .bind(AgentJobStatus::Pending.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn is_agent_job_cancelled(&self, job_id: &str) -> anyhow::Result<bool> {
        let row = sqlx::query(
            r#"
SELECT status
FROM agent_jobs
WHERE id = ?
            "#,
        )
        .bind(job_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let status: String = row.try_get("status")?;
        Ok(AgentJobStatus::parse(status.as_str())? == AgentJobStatus::Cancelled)
    }

    pub async fn mark_agent_job_item_running(
        &self,
        job_id: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    assigned_thread_id = NULL,
    attempt_count = attempt_count + 1,
    updated_at = ?,
    last_error = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
    AND EXISTS (
        SELECT 1
        FROM agent_jobs
        WHERE agent_jobs.id = agent_job_items.job_id AND agent_jobs.status = ?
    )
            "#,
        )
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_agent_job_item_running_with_thread(
        &self,
        job_id: &str,
        item_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    assigned_thread_id = ?,
    attempt_count = attempt_count + 1,
    updated_at = ?,
    last_error = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
    AND EXISTS (
        SELECT 1
        FROM agent_jobs
        WHERE agent_jobs.id = agent_job_items.job_id AND agent_jobs.status = ?
    )
            "#,
        )
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(thread_id)
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_agent_job_item_pending(
        &self,
        job_id: &str,
        item_id: &str,
        error_message: Option<&str>,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    assigned_thread_id = NULL,
    updated_at = ?,
    last_error = ?
WHERE job_id = ? AND item_id = ? AND status = ?
            "#,
        )
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(now)
        .bind(error_message)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_agent_job_item_spawn_failed(
        &self,
        job_id: &str,
        item_id: &str,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    completed_at = ?,
    updated_at = ?,
    last_error = ?,
    assigned_thread_id = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status IN (?, ?)
    AND EXISTS (
        SELECT 1
        FROM agent_jobs
        WHERE agent_jobs.id = agent_job_items.job_id AND agent_jobs.status = ?
    )
            "#,
        )
        .bind(AgentJobItemStatus::Failed.as_str())
        .bind(now)
        .bind(now)
        .bind(error_message)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(AgentJobStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_agent_job_item_thread(
        &self,
        job_id: &str,
        item_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET assigned_thread_id = ?, updated_at = ?
WHERE job_id = ? AND item_id = ? AND status = ?
            "#,
        )
        .bind(thread_id)
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn report_agent_job_item_result(
        &self,
        job_id: &str,
        item_id: &str,
        reporting_thread_id: &str,
        result_json: &Value,
    ) -> anyhow::Result<bool> {
        self.report_agent_job_item_result_and_maybe_cancel(
            job_id,
            item_id,
            reporting_thread_id,
            result_json,
            /*cancel_job*/ false,
            /*cancellation_reason*/ "",
        )
        .await
    }

    pub async fn report_agent_job_item_result_and_maybe_cancel(
        &self,
        job_id: &str,
        item_id: &str,
        reporting_thread_id: &str,
        result_json: &Value,
        cancel_job: bool,
        cancellation_reason: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.report_agent_job_item_result_with_receipt(job_id, item_id, reporting_thread_id,
            result_json, cancel_job, cancellation_reason).await?.is_some())
    }

    pub async fn report_agent_job_item_result_with_receipt(
        &self,
        job_id: &str,
        item_id: &str,
        reporting_thread_id: &str,
        result_json: &Value,
        cancel_job: bool,
        cancellation_reason: &str,
    ) -> anyhow::Result<Option<&'static str>> {
        anyhow::ensure!(result_json.is_object(), "result must be a JSON object");
        let now = Utc::now().timestamp();
        let serialized = serde_json::to_string(result_json)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let replay: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_job_report_receipts r JOIN agent_job_items i USING (job_id, item_id) WHERE r.job_id = ? AND r.item_id = ? AND r.thread_id = ? AND r.result_json = ? AND r.cancel_job = ? AND r.cancellation_reason = ? AND i.attempt_count = r.attempt_count AND i.status = 'completed' AND i.result_json = r.result_json AND i.reported_at = r.reported_at)")
            .bind(job_id).bind(item_id).bind(reporting_thread_id).bind(&serialized)
            .bind(i64::from(cancel_job)).bind(cancellation_reason).fetch_one(&mut *tx).await?;
        if replay != 0 {
            tx.commit().await?;
            return Ok(Some("accepted_replay"));
        }
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    result_json = ?,
    reported_at = ?,
    completed_at = ?,
    updated_at = ?,
    last_error = NULL,
    assigned_thread_id = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
    AND assigned_thread_id = ?
    AND EXISTS (
        SELECT 1
        FROM agent_jobs
        WHERE agent_jobs.id = agent_job_items.job_id AND agent_jobs.status = ?
    )
            "#,
        )
        .bind(AgentJobItemStatus::Completed.as_str())
        .bind(&serialized)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(reporting_thread_id)
        .bind(AgentJobStatus::Running.as_str())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(None);
        }

        sqlx::query("INSERT INTO agent_job_report_receipts (job_id, item_id, attempt_count, thread_id, result_json, cancel_job, cancellation_reason, reported_at) SELECT job_id, item_id, attempt_count, ?, ?, ?, ?, ? FROM agent_job_items WHERE job_id = ? AND item_id = ?")
            .bind(reporting_thread_id).bind(&serialized).bind(i64::from(cancel_job))
            .bind(cancellation_reason).bind(now).bind(job_id).bind(item_id).execute(&mut *tx).await?;

        if cancel_job {
            let cancellation = sqlx::query(
                r#"
UPDATE agent_jobs
SET status = ?, updated_at = ?, completed_at = ?, last_error = ?
WHERE id = ? AND status = ?
                "#,
            )
            .bind(AgentJobStatus::Cancelled.as_str())
            .bind(now)
            .bind(now)
            .bind(cancellation_reason)
            .bind(job_id)
            .bind(AgentJobStatus::Running.as_str())
            .execute(&mut *tx)
            .await?;
            if cancellation.rows_affected() == 0 {
                tx.rollback().await?;
                return Ok(None);
            }
        }

        tx.commit().await?;
        self.notify_agent_job_change();
        Ok(Some("accepted_new"))
    }

    pub async fn mark_agent_job_item_completed(
        &self,
        job_id: &str,
        item_id: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = QueryBuilder::<Sqlite>::new(
            format!(r#"
UPDATE agent_job_items
SET
    status = ?,
    completed_at = ?,
    updated_at = ?,
    assigned_thread_id = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
    AND ({VALID_RETAINED_REPORT})
            "#),
        )
        .build()
        .bind(AgentJobItemStatus::Completed.as_str())
        .bind(now)
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub async fn mark_agent_job_item_failed(
        &self,
        job_id: &str,
        item_id: &str,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE agent_job_items
SET
    status = ?,
    completed_at = ?,
    updated_at = ?,
    last_error = ?,
    assigned_thread_id = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
            "#,
        )
        .bind(AgentJobItemStatus::Failed.as_str())
        .bind(now)
        .bind(now)
        .bind(error_message)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
        self.notify_agent_job_change();
        Ok(result.rows_affected() > 0)
    }

    pub fn subscribe_agent_job_changes(&self) -> watch::Receiver<u64> {
        self.agent_job_updates.subscribe()
    }

    fn notify_agent_job_change(&self) {
        self.agent_job_updates
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub async fn get_agent_job_progress(&self, job_id: &str) -> anyhow::Result<AgentJobProgress> {
        // Counts used by final delivery must not certify impossible completions.
        self.list_agent_job_items(job_id, Some(AgentJobItemStatus::Completed), None).await?;
        let row = sqlx::query(
            r#"
SELECT
    COUNT(*) AS total_items,
    SUM(CASE WHEN status = ? THEN 1 ELSE 0 END) AS pending_items,
    SUM(CASE WHEN status = ? THEN 1 ELSE 0 END) AS running_items,
    SUM(CASE WHEN status = ? THEN 1 ELSE 0 END) AS completed_items,
    SUM(CASE WHEN status = ? THEN 1 ELSE 0 END) AS failed_items
FROM agent_job_items
WHERE job_id = ?
            "#,
        )
        .bind(AgentJobItemStatus::Pending.as_str())
        .bind(AgentJobItemStatus::Running.as_str())
        .bind(AgentJobItemStatus::Completed.as_str())
        .bind(AgentJobItemStatus::Failed.as_str())
        .bind(job_id)
        .fetch_one(self.pool.as_ref())
        .await?;

        let total_items: i64 = row.try_get("total_items")?;
        let pending_items: Option<i64> = row.try_get("pending_items")?;
        let running_items: Option<i64> = row.try_get("running_items")?;
        let completed_items: Option<i64> = row.try_get("completed_items")?;
        let failed_items: Option<i64> = row.try_get("failed_items")?;
        Ok(AgentJobProgress {
            total_items: usize::try_from(total_items).unwrap_or_default(),
            pending_items: usize::try_from(pending_items.unwrap_or_default()).unwrap_or_default(),
            running_items: usize::try_from(running_items.unwrap_or_default()).unwrap_or_default(),
            completed_items: usize::try_from(completed_items.unwrap_or_default())
                .unwrap_or_default(),
            failed_items: usize::try_from(failed_items.unwrap_or_default()).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::unique_temp_dir;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[tokio::test]
    async fn audit142_invalid_completed_rows_block_counts_and_export() -> anyhow::Result<()> {
        for corruption in ["result_json = NULL", "reported_at = NULL", "attempt_count = 0", "result_json = '[]'", "result_json = '{'"] {
            let runtime = StateRuntime::init(unique_temp_dir(), "test-provider".into()).await?;
            let (job, item, thread) = create_running_single_item_job(&runtime).await?;
            assert!(runtime.report_agent_job_item_result(&job, &item, &thread, &json!({"ok":true})).await?);
            assert_eq!(runtime.get_agent_job_progress(&job).await?.completed_items, 1);
            QueryBuilder::<Sqlite>::new("UPDATE agent_job_items SET ")
                .push(corruption)
                .push(" WHERE job_id = ? AND item_id = ?")
                .build()
                .bind(&job).bind(&item).execute(runtime.pool.as_ref()).await?;
            assert!(runtime.get_agent_job_progress(&job).await.is_err(), "{corruption}");
            assert!(runtime.agent_job_export_snapshot(&job).await.is_err(), "{corruption}");
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_job_items WHERE job_id = ?")
                .bind(&job).fetch_one(runtime.pool.as_ref()).await?;
            assert_eq!(count, 1, "invalid evidence must not be silently omitted");
            runtime.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn audit142_exact_report_replay_returns_original_acceptance_without_effects() -> anyhow::Result<()> {
        let runtime = StateRuntime::init(unique_temp_dir(), "test-provider".into()).await?;
        let (job, item, thread) = create_running_single_item_job(&runtime).await?;
        let result = json!({"ok":true});
        assert_eq!(runtime.report_agent_job_item_result_with_receipt(&job, &item, &thread, &result, false, "").await?, Some("accepted_new"));
        let before = runtime.get_agent_job_item(&job, &item).await?;
        let revision = runtime.agent_job_export_snapshot(&job).await?.0;
        assert_eq!(runtime.report_agent_job_item_result_with_receipt(&job, &item, &thread, &result, false, "").await?, Some("accepted_replay"));
        for (worker, payload, stop) in [("other", result.clone(), false), (thread.as_str(), json!({"ok":false}), false), (thread.as_str(), result.clone(), true)] {
            assert_eq!(runtime.report_agent_job_item_result_with_receipt(&job, &item, worker, &payload, stop, "").await?, None);
        }
        assert_eq!(runtime.get_agent_job_item(&job, &item).await?, before);
        assert_eq!(runtime.agent_job_export_snapshot(&job).await?.0, revision);
        runtime.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn audit142_restart_and_ordinary_settlement_agree_on_legacy_report() -> anyhow::Result<()> {
        for reported in [true, false] {
            for restart in [true, false] {
                let home = unique_temp_dir();
                let owner = StateRuntime::init(home.clone(), "test-provider".into()).await?;
                let (job, item, thread) = create_running_single_item_job(&owner).await?;
                if reported {
                    sqlx::query("UPDATE agent_job_items SET result_json = '{\"legacy\":true}', reported_at = updated_at WHERE job_id = ?")
                        .bind(&job).execute(owner.pool.as_ref()).await?;
                }
                let runtime = if restart {
                    owner.close().await;
                    let observer = StateRuntime::init(home, "test-provider".into()).await?;
                    observer.reconcile_orphaned_agent_jobs_after_restart().await?;
                    observer
                } else {
                    owner.settle_agent_job_item_for_thread(&job, &item, Some(&thread), "orphan").await?;
                    owner
                };
                let item = runtime.get_agent_job_item(&job, &item).await?.unwrap();
                assert_eq!(item.status, if reported { AgentJobItemStatus::Completed } else { AgentJobItemStatus::Failed });
                if reported {
                    assert_eq!(item.result_json, Some(json!({"legacy":true})));
                    assert!(item.last_error.is_none());
                }
                runtime.close().await;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn audit142_publication_is_bound_to_one_destination_owner_and_exact_bytes() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test-provider".into()).await?;
        let (job, _, _) = create_running_single_item_job(&runtime).await?;
        runtime.create_agent_job(&AgentJobCreateParams {
            id: "other".into(), name: "other".into(), instruction: "work".into(), auto_export: true,
            max_runtime_seconds: None, output_schema_json: None, input_headers: Vec::new(),
            input_csv_path: "in".into(), output_csv_path: "out".into(),
        }, &[]).await?;
        let path = home.path().join("shared.csv");
        let destination = path.to_string_lossy().into_owned();
        runtime.reserve_agent_job_destination(&job, &destination).await?;
        let revision = runtime.agent_job_export_snapshot(&job).await?.0;
        std::fs::write(&path, b"A")?;
        // SHA-256 of the literal bytes b"A", independently computed for this fixture.
        let digest = "559aead08264d5795d3909718cdd05abd49572e84fe55590eef31a88a08fdffd";
        runtime.acknowledge_agent_job_artifact(&job, revision, &destination, &digest).await?;
        assert!(runtime.agent_job_export_is_published(&job).await?);
        assert!(runtime.reserve_agent_job_destination("other", &destination).await.is_err());
        assert_eq!(std::fs::read(&path)?, b"A");
        std::fs::write(&path, b"external change")?;
        assert!(!runtime.agent_job_export_is_published(&job).await?);
        std::fs::remove_file(&path)?;
        assert!(!runtime.agent_job_export_is_published(&job).await?);
        runtime.reserve_agent_job_destination("other", &home.path().join("other.csv").to_string_lossy()).await?;
        runtime.close().await;
        Ok(())
    }

    async fn create_running_single_item_job(
        runtime: &StateRuntime,
    ) -> anyhow::Result<(String, String, String)> {
        let job_id = "job-1".to_string();
        let item_id = "item-1".to_string();
        let thread_id = ThreadId::new().to_string();
        runtime
            .create_agent_job(
                &AgentJobCreateParams {
                    id: job_id.clone(),
                    name: "test-job".to_string(),
                    instruction: "Return a result".to_string(),
                    auto_export: true,
                    max_runtime_seconds: None,
                    output_schema_json: None,
                    input_headers: vec!["path".to_string()],
                    input_csv_path: "/tmp/in.csv".to_string(),
                    output_csv_path: "/tmp/out.csv".to_string(),
                },
                &[AgentJobItemCreateParams {
                    item_id: item_id.clone(),
                    row_index: 0,
                    source_id: None,
                    row_json: json!({"path":"file-1"}),
                }],
            )
            .await?;
        runtime.mark_agent_job_running(job_id.as_str()).await?;
        let marked_running = runtime
            .mark_agent_job_item_running_with_thread(
                job_id.as_str(),
                item_id.as_str(),
                thread_id.as_str(),
            )
            .await?;
        assert!(marked_running);
        Ok((job_id, item_id, thread_id))
    }

    #[tokio::test]
    async fn deletion_replacement_fences_old_settlement_and_retains_attempts() -> anyhow::Result<()>
    {
        let runtime = StateRuntime::init(unique_temp_dir(), "test-provider".into()).await?;
        let (job, item, old) = create_running_single_item_job(&runtime).await?;
        assert!(runtime.agent_job_has_unfinished_items(&job).await?);
        runtime.delete_thread(ThreadId::from_string(&old)?).await?;
        assert!(runtime.agent_job_has_unfinished_items(&job).await?);
        assert_eq!(
            runtime
                .get_agent_job_item(&job, &item)
                .await?
                .unwrap()
                .status,
            AgentJobItemStatus::Pending
        );
        let replacement = ThreadId::new().to_string();
        assert!(
            runtime
                .mark_agent_job_item_running_with_thread(&job, &item, &replacement)
                .await?
        );
        let before = runtime.get_agent_job_item(&job, &item).await?;
        assert!(
            !runtime
                .settle_agent_job_item_for_thread(&job, &item, Some(&old), "stale timeout")
                .await?
        );
        assert_eq!(runtime.get_agent_job_item(&job, &item).await?, before);
        assert!(
            !runtime
                .report_agent_job_item_result(&job, &item, &old, &json!({"stale":true}))
                .await?
        );
        assert!(
            runtime
                .report_agent_job_item_result(&job, &item, &replacement, &json!({"kept":true}))
                .await?
        );
        assert!(
            !runtime
                .settle_agent_job_item_for_thread(&job, &item, Some(&replacement), "late cleanup")
                .await?
        );
        assert_eq!(
            runtime.list_agent_job_attempts(&job, &item).await?,
            vec![(1, old), (2, replacement)]
        );
        assert!(!runtime.agent_job_has_unfinished_items(&job).await?);
        Ok(())
    }

    #[tokio::test]
    async fn restart_settles_terminal_parent_workers_without_rewriting_outcomes()
    -> anyhow::Result<()> {
        for cancelled in [true, false] {
            let home = unique_temp_dir();
            let owner = StateRuntime::init(home.clone(), "test-provider".into()).await?;
            let (job, item, _) = create_running_single_item_job(&owner).await?;
            for (id, index) in [("never-started", 1_i64), ("accepted", 2)] {
                sqlx::query("INSERT INTO agent_job_items (job_id,item_id,row_index,row_json,status,created_at,updated_at) VALUES (?,?,?,'{}','pending',0,0)")
                    .bind(&job).bind(id).bind(index).execute(owner.pool.as_ref()).await?;
            }
            assert!(
                owner
                    .mark_agent_job_item_running_with_thread(&job, "accepted", "accepted-worker")
                    .await?
            );
            assert!(
                owner
                    .report_agent_job_item_result(
                        &job,
                        "accepted",
                        "accepted-worker",
                        &json!({"kept":7})
                    )
                    .await?
            );
            let accepted = owner.get_agent_job_item(&job, "accepted").await?;
            if cancelled {
                owner
                    .mark_agent_job_cancelled(&job, "original reason")
                    .await?;
            } else {
                owner.mark_agent_job_failed(&job, "original reason").await?;
            }
            let observer = StateRuntime::init(home, "test-provider".into()).await?;
            assert!(
                observer
                    .reconcile_orphaned_agent_jobs_after_restart()
                    .await?
                    .is_empty()
            );
            owner.close().await;
            assert_eq!(
                observer
                    .reconcile_orphaned_agent_jobs_after_restart()
                    .await?
                    .len(),
                1
            );
            let parent = observer.get_agent_job(&job).await?.unwrap();
            assert_eq!(
                parent.status,
                if cancelled {
                    AgentJobStatus::Cancelled
                } else {
                    AgentJobStatus::Failed
                }
            );
            assert_eq!(parent.last_error.as_deref(), Some("original reason"));
            assert_eq!(
                observer.get_agent_job_item(&job, "accepted").await?,
                accepted
            );
            assert_eq!(
                observer
                    .get_agent_job_item(&job, "never-started")
                    .await?
                    .unwrap()
                    .status,
                AgentJobItemStatus::Pending
            );
            assert_eq!(
                observer
                    .get_agent_job_item(&job, &item)
                    .await?
                    .unwrap()
                    .status,
                AgentJobItemStatus::Failed
            );
            assert!(
                observer
                    .reconcile_orphaned_agent_jobs_after_restart()
                    .await?
                    .is_empty()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn publication_acknowledges_only_the_rendered_revision() -> anyhow::Result<()> {
        let runtime = StateRuntime::init(unique_temp_dir(), "test-provider".into()).await?;
        let (job, item, thread) = create_running_single_item_job(&runtime).await?;
        assert!(runtime.agent_job_is_owned_running(&job).await?);
        runtime.mark_agent_job_failed(&job, "owner stopped").await?;
        assert!(!runtime.agent_job_is_owned_running(&job).await?);
        let claims = runtime.claim_agent_job_recovery_exports().await?;
        assert_eq!(claims.len(), 1);
        let (revision, _) = runtime.agent_job_export_snapshot(&job).await?;
        assert!(
            runtime
                .settle_agent_job_item_for_thread(&job, &item, Some(&thread), "stopped")
                .await?
        );
        runtime.acknowledge_agent_job_export(&job, revision).await?;
        runtime
            .finish_agent_job_recovery_export(&job, &claims[0].1, true)
            .await?;
        let next = runtime.claim_agent_job_recovery_exports().await?;
        assert_eq!(
            next.len(),
            1,
            "a ledger update during render remains pending"
        );
        let (new_revision, rows) = runtime.agent_job_export_snapshot(&job).await?;
        assert!(new_revision > revision);
        assert_eq!(rows[0].status, AgentJobItemStatus::Failed);
        runtime
            .finish_agent_job_recovery_export(&job, &claims[0].1, true)
            .await?;
        let published: i64 = sqlx::query_scalar(
            "SELECT published_revision FROM agent_job_recovery_exports WHERE job_id = ?",
        )
        .bind(&job)
        .fetch_one(runtime.pool.as_ref())
        .await?;
        assert_eq!(
            published, revision,
            "a stale claim cannot acknowledge a newer snapshot"
        );
        runtime
            .acknowledge_agent_job_export(&job, new_revision)
            .await?;
        runtime
            .finish_agent_job_recovery_export(&job, &next[0].1, true)
            .await?;
        assert!(runtime.claim_agent_job_recovery_exports().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn large_agent_job_batches_preserve_rows_and_rollback_late_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = StateRuntime::init(temp.path().to_path_buf(), "test-provider".to_string())
            .await
            .expect("runtime");
        let mut params = AgentJobCreateParams {
            id: "large-job".to_string(),
            name: "batch".to_string(),
            instruction: "work".to_string(),
            auto_export: false,
            max_runtime_seconds: None,
            output_schema_json: None,
            input_headers: vec!["value".to_string()],
            input_csv_path: "in.csv".to_string(),
            output_csv_path: "out.csv".to_string(),
        };
        let mut items = (0..4_100)
            .map(|i| AgentJobItemCreateParams {
                item_id: format!("item-{i}"),
                row_index: i,
                source_id: Some(format!("source-{i}")),
                row_json: json!({"value":i}),
            })
            .collect::<Vec<_>>();
        runtime
            .create_agent_job(&params, &items)
            .await
            .expect("large job");
        let progress = runtime
            .get_agent_job_progress(&params.id)
            .await
            .expect("progress");
        assert_eq!(
            (progress.total_items, progress.pending_items),
            (4_100, 4_100)
        );
        let last: (i64, String, String, i64) = sqlx::query_as("SELECT row_index, source_id, row_json, attempt_count FROM agent_job_items WHERE job_id = ? AND item_id = 'item-4099'")
            .bind(&params.id).fetch_one(runtime.pool.as_ref()).await.expect("last row");
        assert_eq!(
            last,
            (
                4_099,
                "source-4099".to_string(),
                json!({"value":4099}).to_string(),
                0
            )
        );
        params.id = "rolled-back-job".to_string();
        items[4_099].item_id = items[0].item_id.clone();
        assert!(runtime.create_agent_job(&params, &items).await.is_err());
        assert!(
            runtime
                .get_agent_job(&params.id)
                .await
                .expect("job lookup")
                .is_none()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_job_items WHERE job_id = ?")
                .bind(&params.id)
                .fetch_one(runtime.pool.as_ref())
                .await
                .expect("rolled back items"),
            0
        );
        runtime.close().await;
    }

    #[tokio::test]
    async fn ma014_pending_restart_and_ma015_export_retry_are_durable() -> anyhow::Result<()> {
        let home = unique_temp_dir();
        let owner = StateRuntime::init(home.clone(), "test-provider".into()).await?;
        owner
            .create_agent_job(
                &AgentJobCreateParams {
                    id: "pending-recovery".into(),
                    name: "pending".into(),
                    instruction: "report".into(),
                    auto_export: true,
                    max_runtime_seconds: None,
                    output_schema_json: None,
                    input_headers: vec!["path".into()],
                    input_csv_path: "in.csv".into(),
                    output_csv_path: "out.csv".into(),
                },
                &[AgentJobItemCreateParams {
                    item_id: "item".into(),
                    row_index: 0,
                    source_id: None,
                    row_json: json!({"path":"src/lib.rs"}),
                }],
            )
            .await?;
        let observer = StateRuntime::init(home, "test-provider".into()).await?;
        assert!(
            observer
                .reconcile_orphaned_agent_jobs_after_restart()
                .await?
                .is_empty(),
            "live Pending owner is protected"
        );
        owner.close().await;
        let jobs = observer
            .reconcile_orphaned_agent_jobs_after_restart()
            .await?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, AgentJobStatus::Failed);
        let item = observer
            .get_agent_job_item("pending-recovery", "item")
            .await?
            .unwrap();
        assert_eq!(item.status, AgentJobItemStatus::Failed);
        assert_eq!(item.assigned_thread_id, None);
        let first = observer.claim_agent_job_recovery_exports().await?;
        assert_eq!(first.len(), 1);
        assert!(
            observer
                .claim_agent_job_recovery_exports()
                .await?
                .is_empty(),
            "active export claim is exclusive"
        );
        observer
            .finish_agent_job_recovery_export(&first[0].0, &first[0].1, false)
            .await?;
        assert!(
            observer
                .claim_agent_job_recovery_exports()
                .await?
                .is_empty(),
            "failed export is backoff-bound"
        );
        // Advance only the durable retry clock, not any worker/job state.
        sqlx::query("UPDATE agent_job_recovery_exports SET retry_at = 0")
            .execute(observer.pool.as_ref())
            .await?;
        let second = observer.claim_agent_job_recovery_exports().await?;
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].1, second[0].1);
        observer
            .finish_agent_job_recovery_export(&first[0].0, &first[0].1, true)
            .await?;
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_job_recovery_exports")
            .fetch_one(observer.pool.as_ref())
            .await?;
        assert_eq!(
            retained, 1,
            "a stale exporter cannot acknowledge the successor claim"
        );
        observer
            .finish_agent_job_recovery_export(&second[0].0, &second[0].1, true)
            .await?;
        assert!(
            observer
                .claim_agent_job_recovery_exports()
                .await?
                .is_empty()
        );
        assert_eq!(
            observer
                .get_agent_job("pending-recovery")
                .await?
                .unwrap()
                .status,
            AgentJobStatus::Failed
        );
        Ok(())
    }

    #[tokio::test]
    async fn durability_regression_restart_reconciles_only_orphaned_agent_jobs()
    -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let owner = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
        let (job_id, item_id, _thread_id) = create_running_single_item_job(owner.as_ref()).await?;
        let observer = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
        assert_ne!(
            owner.agent_job_runner_instance_id(),
            observer.agent_job_runner_instance_id()
        );

        let live_foreign_jobs = observer
            .reconcile_orphaned_agent_jobs_after_restart()
            .await?;
        assert!(live_foreign_jobs.is_empty());
        assert_eq!(
            observer
                .get_agent_job(job_id.as_str())
                .await?
                .expect("job should exist")
                .status,
            AgentJobStatus::Running
        );

        owner.close().await;
        let reconciled = observer
            .reconcile_orphaned_agent_jobs_after_restart()
            .await?;
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].id, job_id);
        assert_eq!(reconciled[0].status, AgentJobStatus::Failed);
        assert_eq!(
            reconciled[0].last_error.as_deref(),
            Some(StateRuntime::AGENT_JOB_RESTART_ERROR)
        );

        let item = observer
            .get_agent_job_item(job_id.as_str(), item_id.as_str())
            .await?
            .expect("job item should exist");
        assert_eq!(item.status, AgentJobItemStatus::Failed);
        assert_eq!(item.assigned_thread_id, None);
        assert_eq!(
            item.last_error.as_deref(),
            Some(StateRuntime::AGENT_JOB_RESTART_ERROR)
        );
        assert!(item.completed_at.is_some());
        assert!(
            observer
                .reconcile_orphaned_agent_jobs_after_restart()
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn restart_reconciliation_preserves_legacy_null_owner_jobs() -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let owner = StateRuntime::init(codex_home.clone(), "test-provider".to_string()).await?;
        let (job_id, _item_id, _thread_id) = create_running_single_item_job(owner.as_ref()).await?;
        sqlx::query("UPDATE agent_jobs SET runner_instance_id = NULL WHERE id = ?")
            .bind(job_id.as_str())
            .execute(owner.pool.as_ref())
            .await?;
        owner.close().await;

        let observer = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
        assert!(
            observer
                .reconcile_orphaned_agent_jobs_after_restart()
                .await?
                .is_empty()
        );
        assert_eq!(
            observer
                .get_agent_job(job_id.as_str())
                .await?
                .expect("legacy job should remain available")
                .status,
            AgentJobStatus::Running
        );
        Ok(())
    }

    #[tokio::test]
    async fn report_agent_job_item_result_completes_item_atomically() -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
        let (job_id, item_id, thread_id) = create_running_single_item_job(runtime.as_ref()).await?;
        let changes = runtime.subscribe_agent_job_changes();

        let before = runtime
            .get_agent_job_item(job_id.as_str(), item_id.as_str())
            .await?
            .expect("running item");
        assert_eq!(before.status, AgentJobItemStatus::Running);
        assert_eq!(
            before.assigned_thread_id.as_deref(),
            Some(thread_id.as_str())
        );
        assert_eq!(
            runtime
                .get_agent_job(&job_id)
                .await?
                .expect("running job")
                .status,
            AgentJobStatus::Running,
        );
        assert!(
            !runtime
                .report_agent_job_item_result(
                    &job_id,
                    &item_id,
                    "wrong-thread",
                    &json!({"wrong": true})
                )
                .await?
        );
        assert_eq!(
            runtime.get_agent_job_item(&job_id, &item_id).await?,
            Some(before),
        );
        assert!(!changes.has_changed()?);

        let accepted = runtime
            .report_agent_job_item_result(
                job_id.as_str(),
                item_id.as_str(),
                thread_id.as_str(),
                &json!({"ok": true}),
            )
            .await?;
        assert!(accepted);
        assert!(changes.has_changed()?);

        let item = runtime
            .get_agent_job_item(job_id.as_str(), item_id.as_str())
            .await?
            .expect("job item should exist");
        assert_eq!(item.status, AgentJobItemStatus::Completed);
        assert_eq!(item.result_json, Some(json!({"ok": true})));
        assert_eq!(item.assigned_thread_id, None);
        assert_eq!(item.last_error, None);
        assert!(item.reported_at.is_some());
        assert!(item.completed_at.is_some());
        let progress = runtime.get_agent_job_progress(job_id.as_str()).await?;
        assert_eq!(
            progress,
            AgentJobProgress {
                total_items: 1,
                pending_items: 0,
                running_items: 0,
                completed_items: 1,
                failed_items: 0,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn stop_report_cancels_job_and_rejects_other_running_worker() -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
        let job_id = "job-stop";
        let first_item_id = "item-1";
        let second_item_id = "item-2";
        let first_thread_id = "thread-1";
        let second_thread_id = "thread-2";
        runtime
            .create_agent_job(
                &AgentJobCreateParams {
                    id: job_id.to_string(),
                    name: "stop-job".to_string(),
                    instruction: "Return a result".to_string(),
                    auto_export: true,
                    max_runtime_seconds: None,
                    output_schema_json: None,
                    input_headers: vec!["path".to_string()],
                    input_csv_path: "/tmp/in.csv".to_string(),
                    output_csv_path: "/tmp/out.csv".to_string(),
                },
                &[
                    AgentJobItemCreateParams {
                        item_id: first_item_id.to_string(),
                        row_index: 0,
                        source_id: None,
                        row_json: json!({"path":"file-1"}),
                    },
                    AgentJobItemCreateParams {
                        item_id: second_item_id.to_string(),
                        row_index: 1,
                        source_id: None,
                        row_json: json!({"path":"file-2"}),
                    },
                ],
            )
            .await?;
        runtime.mark_agent_job_running(job_id).await?;
        assert!(
            runtime
                .mark_agent_job_item_running_with_thread(job_id, first_item_id, first_thread_id,)
                .await?
        );
        assert!(
            runtime
                .mark_agent_job_item_running_with_thread(job_id, second_item_id, second_thread_id,)
                .await?
        );

        let accepted = runtime
            .report_agent_job_item_result_and_maybe_cancel(
                job_id,
                first_item_id,
                first_thread_id,
                &json!({"stop": true}),
                /*cancel_job*/ true,
                "cancelled by worker request",
            )
            .await?;
        assert!(accepted);
        let job = runtime
            .get_agent_job(job_id)
            .await?
            .expect("job should exist");
        assert_eq!(job.status, AgentJobStatus::Cancelled);

        let late_report_accepted = runtime
            .report_agent_job_item_result(
                job_id,
                second_item_id,
                second_thread_id,
                &json!({"late": true}),
            )
            .await?;
        assert!(!late_report_accepted);
        assert!(
            runtime
                .mark_agent_job_item_failed(
                    job_id,
                    second_item_id,
                    "job cancelled before worker completion",
                )
                .await?
        );
        let second_item = runtime
            .get_agent_job_item(job_id, second_item_id)
            .await?
            .expect("second item should exist");
        assert_eq!(second_item.status, AgentJobItemStatus::Failed);
        assert_eq!(second_item.result_json, None);
        Ok(())
    }

    #[tokio::test]
    async fn report_agent_job_item_result_rejects_late_reports() -> anyhow::Result<()> {
        let codex_home = unique_temp_dir();
        let runtime = StateRuntime::init(codex_home, "test-provider".to_string()).await?;
        let (job_id, item_id, thread_id) = create_running_single_item_job(runtime.as_ref()).await?;

        let marked_failed = runtime
            .mark_agent_job_item_failed(job_id.as_str(), item_id.as_str(), "missing report")
            .await?;
        assert!(marked_failed);
        let accepted = runtime
            .report_agent_job_item_result(
                job_id.as_str(),
                item_id.as_str(),
                thread_id.as_str(),
                &json!({"late": true}),
            )
            .await?;
        assert!(!accepted);

        let item = runtime
            .get_agent_job_item(job_id.as_str(), item_id.as_str())
            .await?
            .expect("job item should exist");
        assert_eq!(item.status, AgentJobItemStatus::Failed);
        assert_eq!(item.result_json, None);
        assert_eq!(item.last_error, Some("missing report".to_string()));
        Ok(())
    }
}
