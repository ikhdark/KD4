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

    async fn ensure_agent_job_runner_registered(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.agent_job_runner_registered, "non-owning state runtime cannot own agent jobs");
        {
            let enrolled = self.agent_job_runner_enrollment.lock().await;
            anyhow::ensure!(!*self.agent_job_runner_heartbeat_shutdown.borrow(), "state runtime is closing");
            if *enrolled { return Ok(()); }
        }
        let runtime = self.clone();
        // Keep the registration mutation and heartbeat publication owned even
        // when the first job's caller stops waiting. Close waits on this guard.
        tokio::spawn(async move {
            let mut enrolled = runtime.agent_job_runner_enrollment.lock().await;
            anyhow::ensure!(!*runtime.agent_job_runner_heartbeat_shutdown.borrow(), "state runtime is closing");
            if !*enrolled {
                register_agent_job_runner_instance(runtime.pool.as_ref(), runtime.agent_job_runner_instance_id()).await?;
                spawn_agent_job_runner_heartbeat(
                    Arc::clone(&runtime.pool),
                    runtime.agent_job_runner_instance_id.clone(),
                    runtime.agent_job_runner_heartbeat_shutdown.subscribe(),
                );
                *enrolled = true;
            }
            Ok::<(), anyhow::Error>(())
        }).await?
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
        self.create_agent_job_with_cancellation(params, items, || false).await
    }

    /// Import atomically, observing cancellation before admitting another batch.
    /// A cancellation racing commit may leave a complete Pending job, never a
    /// runnable partial import; the caller retains its existing cleanup owner.
    pub async fn create_agent_job_with_cancellation(
        &self,
        params: &AgentJobCreateParams,
        items: &[AgentJobItemCreateParams],
        is_cancelled: impl Fn() -> bool + Send + Sync,
    ) -> anyhow::Result<AgentJob> {
        let observed = tracing::enabled!(target: "codex_csv_admission", tracing::Level::DEBUG)
            .then(std::time::Instant::now);
        tracing::debug!(target: "codex_csv_admission", phase = "import_begin",
            job_id = %params.id, rows = items.len());
        anyhow::ensure!(!is_cancelled(), "agent job cancelled during setup");
        anyhow::ensure!(self.agent_job_runner_registered, "non-owning state runtime cannot own agent jobs");
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
        self.ensure_agent_job_runner_registered().await?;
        anyhow::ensure!(!is_cancelled(), "agent job cancelled during setup");
        let mut tx = self.pool.begin().await?;
        if is_cancelled() {
            tx.rollback().await?;
            anyhow::bail!("agent job cancelled during setup");
        }
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
        let mut imported_rows = 0usize;
        for chunk in items.chunks(4_000) {
            let mut rows = Vec::with_capacity(chunk.len());
            for item in chunk {
                if is_cancelled() {
                    tx.rollback().await?;
                    tracing::debug!(target: "codex_csv_admission", phase = "import_cancelled",
                        job_id = %params.id, imported_rows, rolled_back = true,
                        elapsed_us = observed.as_ref().map_or(0, |started| started.elapsed().as_micros() as u64));
                    anyhow::bail!("agent job cancelled during setup");
                }
                rows.push(serde_json::to_string(&item.row_json)?);
            }
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
            imported_rows += chunk.len();
            tracing::debug!(target: "codex_csv_admission", phase = "import_batch",
                job_id = %params.id, imported_rows, total_rows = items.len());
        }

        if is_cancelled() {
            tx.rollback().await?;
            tracing::debug!(target: "codex_csv_admission", phase = "import_cancelled",
                job_id = %params.id, imported_rows, rolled_back = true,
                elapsed_us = observed.as_ref().map_or(0, |started| started.elapsed().as_micros() as u64));
            anyhow::bail!("agent job cancelled during setup");
        }
        tx.commit().await?;
        if let Some(started) = observed {
            tracing::debug!(target: "codex_csv_admission", phase = "import_commit",
                job_id = %params.id, imported_rows,
                elapsed_us = started.elapsed().as_micros() as u64);
        }
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

