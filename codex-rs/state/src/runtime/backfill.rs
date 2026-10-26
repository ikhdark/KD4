use super::*;

impl StateRuntime {
    /// Reuse the leased backfill owner for continuing reconciliation, including
    /// files written by supported older writers and repaired damaged rollouts.
    pub async fn request_backfill_refresh(&self, minimum_age_seconds: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE backfill_state SET status = 'pending', last_watermark = NULL, updated_at = ? WHERE id = 1 AND status = 'complete' AND updated_at <= ?")
            .bind(Utc::now().timestamp())
            .bind(Utc::now().timestamp().saturating_sub(minimum_age_seconds))
            .execute(self.pool.as_ref()).await?;
        Ok(())
    }

    pub async fn get_backfill_state(&self) -> anyhow::Result<crate::BackfillState> {
        let query =
            "SELECT status, last_watermark, last_success_at FROM backfill_state WHERE id = 1";
        let row = match sqlx::query(query)
            .fetch_optional(self.pool.as_ref())
            .await?
        {
            Some(row) => row,
            None => {
                self.ensure_backfill_state_row().await?;
                sqlx::query(query).fetch_one(self.pool.as_ref()).await?
            }
        };
        crate::BackfillState::try_from_row(&row)
    }

    /// Attempt to claim ownership of rollout metadata backfill.
    ///
    /// Returns `true` when this runtime claimed the backfill worker slot.
    /// Returns `false` if backfill is already complete or currently owned by a
    /// non-expired worker.
    pub async fn try_claim_backfill(&self, lease_seconds: i64) -> anyhow::Result<bool> {
        Ok(self.claim_backfill(lease_seconds).await?.is_some())
    }

    pub async fn claim_backfill(&self, lease_seconds: i64) -> anyhow::Result<Option<crate::BackfillClaim>> {
        self.ensure_backfill_state_row().await?;
        let now = Utc::now().timestamp();
        let lease_cutoff = now.saturating_sub(lease_seconds.max(0));
        let token = uuid::Uuid::new_v4().to_string();
        let row = sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, updated_at = ?, claim_token = ?
WHERE id = 1
  AND status != ?
  AND (status != ? OR updated_at <= ?)
RETURNING last_watermark
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(now)
        .bind(&token)
        .bind(crate::BackfillStatus::Complete.as_str())
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(lease_cutoff)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| Ok(crate::BackfillClaim { token, last_watermark: sqlx::Row::try_get(&row, "last_watermark")? })).transpose()
    }

    pub async fn checkpoint_backfill_claim(&self, claim: &crate::BackfillClaim, watermark: &str) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE backfill_state SET last_watermark = ?, updated_at = ? WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(watermark).bind(Utc::now().timestamp()).bind(&claim.token).execute(self.pool.as_ref()).await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    pub async fn release_backfill_claim(&self, claim: &crate::BackfillClaim) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE backfill_state SET status = 'pending', updated_at = ?, claim_token = NULL WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(Utc::now().timestamp()).bind(&claim.token).execute(self.pool.as_ref()).await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    pub async fn complete_backfill_claim(&self, claim: &crate::BackfillClaim, watermark: Option<&str>) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        let result = sqlx::query("UPDATE backfill_state SET status = 'complete', last_watermark = COALESCE(?, last_watermark), last_success_at = ?, updated_at = ?, claim_token = NULL WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(watermark).bind(now).bind(now).bind(&claim.token).execute(self.pool.as_ref()).await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    /// Mark rollout metadata backfill as running.
    pub async fn mark_backfill_running(&self) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        let result = sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, updated_at = ?
WHERE id = 1 AND claim_token IS NULL
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill has an active claim; an explicit claim is required");
        Ok(())
    }

    /// Persist rollout metadata backfill progress.
    pub async fn checkpoint_backfill(&self, watermark: &str) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        let result = sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, last_watermark = ?, updated_at = ?
WHERE id = 1 AND claim_token IS NULL
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(watermark)
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill has an active claim; an explicit claim is required");
        Ok(())
    }

    /// Release rollout metadata backfill ownership while preserving its last
    /// contiguous successful watermark so a later worker can retry.
    pub async fn mark_backfill_pending(&self) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        let result = sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, updated_at = ?
WHERE id = 1 AND claim_token IS NULL
            "#,
        )
        .bind(crate::BackfillStatus::Pending.as_str())
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill has an active claim; an explicit claim is required");
        Ok(())
    }

    /// Mark rollout metadata backfill as complete.
    pub async fn mark_backfill_complete(&self, last_watermark: Option<&str>) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        let now = Utc::now().timestamp();
        let result = sqlx::query(
            r#"
UPDATE backfill_state
SET
    status = ?,
    last_watermark = COALESCE(?, last_watermark),
    last_success_at = ?,
    updated_at = ?
WHERE id = 1 AND claim_token IS NULL
            "#,
        )
        .bind(crate::BackfillStatus::Complete.as_str())
        .bind(last_watermark)
        .bind(now)
        .bind(now)
        .execute(self.pool.as_ref())
        .await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill has an active claim; an explicit claim is required");
        Ok(())
    }

    async fn ensure_backfill_state_row(&self) -> anyhow::Result<()> {
        ensure_backfill_state_row_in_pool(self.pool.as_ref()).await
    }
}

