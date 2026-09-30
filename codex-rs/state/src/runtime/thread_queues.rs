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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_support::test_thread_metadata;

    #[tokio::test]
    async fn thread_queue_survives_reopen_and_rejects_unknown_threads() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let id = ThreadId::default();
        let runtime = StateRuntime::init(home.path().to_path_buf(), "test".to_string()).await?;
        runtime
            .upsert_thread(&test_thread_metadata(
                home.path(),
                id,
                home.path().to_path_buf(),
            ))
            .await?;
        assert_eq!(runtime.read_thread_queue(id).await?, None);
        let payload = r#"{"version":1,"submissions":[{"id":"first"}]}"#;
        runtime.write_thread_queue(id, payload).await?;
        let reopened = StateRuntime::init(home.path().to_path_buf(), "test".to_string()).await?;
        assert_eq!(
            reopened.read_thread_queue(id).await?.as_deref(),
            Some(payload)
        );
        assert!(
            reopened
                .write_thread_queue(ThreadId::default(), "{}")
                .await
                .is_err()
        );
        runtime.delete_thread(id).await?;
        assert_eq!(reopened.read_thread_queue(id).await?, None);
        assert!(reopened.write_thread_queue(id, payload).await.is_err());
        Ok(())
    }
}
