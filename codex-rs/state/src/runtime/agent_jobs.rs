use super::*;
use crate::model::AgentJobItemRow;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

const AGENT_JOB_RUNNER_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const AGENT_JOB_RUNNER_LEASE_TIMEOUT_SECONDS: i64 = 30;

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

        for item in items {
            let row_json = serde_json::to_string(&item.row_json)?;
            sqlx::query(
                r#"
INSERT INTO agent_job_items (
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
) VALUES (?, ?, ?, ?, ?, ?, NULL, 0, NULL, NULL, ?, ?, NULL, NULL)
                "#,
            )
            .bind(params.id.as_str())
            .bind(item.item_id.as_str())
            .bind(item.row_index)
            .bind(item.source_id.as_deref())
            .bind(row_json)
            .bind(AgentJobItemStatus::Pending.as_str())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

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
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            r#"
SELECT jobs.id, jobs.runner_instance_id
FROM agent_jobs AS jobs
LEFT JOIN agent_job_runner_instances AS runners
    ON runners.runner_instance_id = jobs.runner_instance_id
WHERE
    jobs.status = ?
    AND jobs.runner_instance_id IS NOT NULL
    AND jobs.runner_instance_id <> ?
    AND (runners.runner_instance_id IS NULL OR runners.heartbeat_at < ?)
ORDER BY jobs.created_at ASC, jobs.id ASC
            "#,
        )
        .bind(AgentJobStatus::Running.as_str())
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
SET status = ?, updated_at = ?, completed_at = ?, last_error = ?
WHERE
    id = ?
    AND status = ?
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
            .bind(owner_runner_instance_id.as_str())
            .bind(self.agent_job_runner_instance_id())
            .bind(owner_runner_instance_id.as_str())
            .bind(stale_before)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                continue;
            }
            sqlx::query(
                r#"
UPDATE agent_job_items
SET
    status = ?,
    assigned_thread_id = NULL,
    updated_at = ?,
    completed_at = ?,
    last_error = ?
WHERE job_id = ? AND status IN (?, ?)
                "#,
            )
            .bind(AgentJobItemStatus::Failed.as_str())
            .bind(now)
            .bind(now)
            .bind(Self::AGENT_JOB_RESTART_ERROR)
            .bind(job_id.as_str())
            .bind(AgentJobItemStatus::Pending.as_str())
            .bind(AgentJobItemStatus::Running.as_str())
            .execute(&mut *tx)
            .await?;
            job_ids.push(job_id);
        }
        tx.commit().await?;

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
        let now = Utc::now().timestamp();
        let serialized = serde_json::to_string(result_json)?;
        let mut tx = self.pool.begin().await?;
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
        .bind(serialized)
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
            return Ok(false);
        }

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
                return Ok(false);
            }
        }

        tx.commit().await?;
        Ok(true)
    }

    pub async fn mark_agent_job_item_completed(
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
    completed_at = ?,
    updated_at = ?,
    assigned_thread_id = NULL
WHERE
    job_id = ?
    AND item_id = ?
    AND status = ?
    AND result_json IS NOT NULL
            "#,
        )
        .bind(AgentJobItemStatus::Completed.as_str())
        .bind(now)
        .bind(now)
        .bind(job_id)
        .bind(item_id)
        .bind(AgentJobItemStatus::Running.as_str())
        .execute(self.pool.as_ref())
        .await?;
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
        Ok(result.rows_affected() > 0)
    }

    pub async fn get_agent_job_progress(&self, job_id: &str) -> anyhow::Result<AgentJobProgress> {
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

    async fn create_running_single_item_job(
        runtime: &StateRuntime,
    ) -> anyhow::Result<(String, String, String)> {
        let job_id = "job-1".to_string();
        let item_id = "item-1".to_string();
        let thread_id = "thread-1".to_string();
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

        let accepted = runtime
            .report_agent_job_item_result(
                job_id.as_str(),
                item_id.as_str(),
                thread_id.as_str(),
                &json!({"ok": true}),
            )
            .await?;
        assert!(accepted);

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
