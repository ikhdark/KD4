use super::*;

/// This runtime's side of the backfill lease.
#[derive(Default)]
pub(super) struct BackfillWorker {
    stop: std::sync::atomic::AtomicBool,
    claim: std::sync::Mutex<Option<crate::BackfillClaim>>,
}

impl BackfillWorker {
    fn held_claim(&self) -> std::sync::MutexGuard<'_, Option<crate::BackfillClaim>> {
        self.claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn forget_claim(&self, claim: &crate::BackfillClaim) {
        let mut held = self.held_claim();
        if held.as_ref().is_some_and(|held| held.token == claim.token) {
            *held = None;
        }
    }
}

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


    /// Whether rollout coverage is certified right now. See [`Self::await_thread_index`].
    pub async fn thread_index_certified(&self) -> bool {
        matches!(self.get_backfill_state().await, Ok(state) if state.coverage_certified())
    }

    /// Whether exit asked this runtime's worker to stop between rollouts.
    pub fn backfill_stop_requested(&self) -> bool {
        self.backfill_worker
            .stop
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Hands this runtime's claim back before exit, so the next process resumes from the
    /// checkpoint instead of waiting out the lease. A pass normally stops between rollouts
    /// and saves its progress; one inside a long read loses only what followed its last
    /// checkpoint. Holds up exit only while a pass is running.
    pub async fn stop_backfill_worker(&self) {
        self.backfill_worker
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(100);
        while self.backfill_worker.held_claim().is_some()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let held = self.backfill_worker.held_claim().clone();
        if let Some(claim) = held
            && let Err(error) = self.release_backfill_claim(&claim).await
        {
            tracing::warn!(%error, "failed to hand back the rollout index claim at exit");
        }
    }

    /// Waits, within a bound, for the background owner to certify rollout coverage. Startup
    /// serves the rows indexed so far; readers for which a missing row or edge means absence
    /// call this first. Returns false when coverage is still uncertified at the bound.
    pub async fn await_thread_index(&self) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match self.get_backfill_state().await {
                Ok(state) if state.coverage_certified() => return true,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "failed to read thread index coverage");
                    return false;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    pub async fn claim_backfill(&self, lease_seconds: i64) -> anyhow::Result<Option<crate::BackfillClaim>> {
        if self.backfill_stop_requested() {
            return Ok(None);
        }
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
        let claim = row
            .map(|row| {
                anyhow::Ok(crate::BackfillClaim {
                    token,
                    last_watermark: sqlx::Row::try_get(&row, "last_watermark")?,
                })
            })
            .transpose()?;
        if let Some(claim) = claim.as_ref() {
            *self.backfill_worker.held_claim() = Some(claim.clone());
            // Shutdown may have checked for a claim while the SQL acquisition
            // was still pending. Do not leave that late claim behind until expiry.
            if self.backfill_stop_requested() {
                self.release_backfill_claim(claim).await?;
                return Ok(None);
            }
        }
        Ok(claim)
    }

    pub async fn checkpoint_backfill_claim(&self, claim: &crate::BackfillClaim, watermark: &str) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE backfill_state SET last_watermark = ?, updated_at = ? WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(watermark).bind(Utc::now().timestamp()).bind(&claim.token).execute(self.pool.as_ref()).await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    /// A metadata gap can hide lineage even after an earlier complete pass.
    /// Only the current lease owner may withdraw that coverage certificate.
    pub async fn invalidate_backfill_coverage(&self, claim: &crate::BackfillClaim) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE backfill_state SET last_success_at = NULL WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(&claim.token).execute(self.pool.as_ref()).await?;
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    pub async fn release_backfill_claim(&self, claim: &crate::BackfillClaim) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE backfill_state SET status = 'pending', updated_at = ?, claim_token = NULL WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(Utc::now().timestamp()).bind(&claim.token).execute(self.pool.as_ref()).await?;
        self.backfill_worker.forget_claim(claim);
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }

    pub async fn complete_backfill_claim(&self, claim: &crate::BackfillClaim, watermark: Option<&str>) -> anyhow::Result<()> {
        let now = Utc::now().timestamp();
        let result = sqlx::query("UPDATE backfill_state SET status = 'complete', last_watermark = COALESCE(?, last_watermark), last_success_at = ?, updated_at = ?, claim_token = NULL WHERE id = 1 AND status = 'running' AND claim_token = ?")
            .bind(watermark).bind(now).bind(now).bind(&claim.token).execute(self.pool.as_ref()).await?;
        self.backfill_worker.forget_claim(claim);
        anyhow::ensure!(result.rows_affected() == 1, "backfill claim ownership was lost");
        Ok(())
    }





    async fn ensure_backfill_state_row(&self) -> anyhow::Result<()> {
        ensure_backfill_state_row_in_pool(self.pool.as_ref()).await
    }
}

