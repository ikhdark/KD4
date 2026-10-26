use super::StateRuntime;
use codex_protocol::ThreadId;

impl StateRuntime {
    /// The caller holds the per-thread queue lock across read/modify/write.
    pub async fn read_thread_queue(&self, thread_id: ThreadId) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT payload FROM thread_queues WHERE thread_id = ?")
                .bind(thread_id.to_string())
                .fetch_optional(self.pool.as_ref())
                .await?,
        )
    }

    /// Persist only for an existing, unarchived thread. This also fences a queue
    /// write racing thread deletion or archival.
    pub async fn write_thread_queue(
        &self,
        thread_id: ThreadId,
        payload: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "INSERT INTO thread_queues (thread_id, payload)
             SELECT id, ? FROM threads WHERE id = ? AND archived = 0
             ON CONFLICT(thread_id) DO UPDATE SET payload = excluded.payload",
        )
        .bind(payload)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        anyhow::ensure!(result.rows_affected() == 1, "thread not found or archived");
        Ok(())
    }
}

