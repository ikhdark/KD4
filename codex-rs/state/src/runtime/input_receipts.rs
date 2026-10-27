use super::StateRuntime;
use codex_protocol::ThreadId;
use sqlx::Row;

/// An immutable RPC admission identity. A reserved row with no payload has an
/// uncertain/rejected core outcome and must never authorize another execution.
#[derive(Clone, Debug)]
pub struct InputReceipt {
    pub kind: String,
    pub scope: String,
    pub client_id: String,
    pub fingerprint: String,
    pub turn_id: String,
    pub payload: Option<String>,
}

impl StateRuntime {
    async fn input_receipts_pool(&self) -> anyhow::Result<&sqlx::SqlitePool> {
        self.input_receipts_pool.get_or_try_init(|| async {
            // The shared metadata pool deliberately uses NORMAL. Admissions
            // must survive a crash before model/tool work, so keep FULL on a
            // separately owned, single-connection pool for this same database.
            let options = self.pool.connect_options().as_ref().clone()
                .synchronous(sqlx::sqlite::SqliteSynchronous::Full);
            Ok(sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
                .connect_with(options).await?)
        }).await
    }

    pub async fn input_receipt(
        &self, thread_id: ThreadId, kind: &str, scope: &str, client_id: &str,
    ) -> anyhow::Result<Option<InputReceipt>> {
        let row = sqlx::query("SELECT kind, scope, client_id, fingerprint, turn_id, payload FROM input_receipts WHERE thread_id = ? AND kind = ? AND scope = ? AND client_id = ?")
            .bind(thread_id.to_string()).bind(kind).bind(scope).bind(client_id)
            .fetch_optional(self.pool.as_ref()).await?;
        Ok(row.map(|row| InputReceipt {
            kind: row.get("kind"), scope: row.get("scope"), client_id: row.get("client_id"),
            fingerprint: row.get("fingerprint"), turn_id: row.get("turn_id"), payload: row.get("payload"),
        }))
    }

    pub async fn reserve_input_receipt(
        &self, thread_id: ThreadId, receipt: &InputReceipt,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("INSERT INTO input_receipts (thread_id, kind, scope, client_id, fingerprint, turn_id, payload) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
            .bind(thread_id.to_string()).bind(&receipt.kind).bind(&receipt.scope)
            .bind(&receipt.client_id).bind(&receipt.fingerprint).bind(&receipt.turn_id).bind(&receipt.payload)
            .execute(self.input_receipts_pool().await?).await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn accept_input_receipt(
        &self, thread_id: ThreadId, kind: &str, scope: &str, client_id: &str,
        turn_id: &str, payload: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query("UPDATE input_receipts SET payload = ? WHERE thread_id = ? AND kind = ? AND scope = ? AND client_id = ? AND turn_id = ? AND payload IS NULL AND consumed = 0")
            .bind(payload).bind(thread_id.to_string()).bind(kind).bind(scope).bind(client_id).bind(turn_id)
            .execute(self.input_receipts_pool().await?).await?;
        anyhow::ensure!(result.rows_affected() == 1, "input receipt is not a fresh admission");
        Ok(())
    }

    pub async fn pending_input_receipts(&self, thread_id: ThreadId) -> anyhow::Result<Vec<InputReceipt>> {
        let rows = sqlx::query("SELECT kind, scope, client_id, fingerprint, turn_id, payload FROM input_receipts WHERE thread_id = ? AND (payload IS NOT NULL OR kind = 'start') AND consumed = 0 ORDER BY rowid")
            .bind(thread_id.to_string()).fetch_all(self.pool.as_ref()).await?;
        Ok(rows.into_iter().map(|row| InputReceipt {
            kind: row.get("kind"), scope: row.get("scope"), client_id: row.get("client_id"),
            fingerprint: row.get("fingerprint"), turn_id: row.get("turn_id"), payload: row.get("payload"),
        }).collect())
    }

    pub async fn consume_input_receipt(&self, thread_id: ThreadId, receipt: &InputReceipt) -> anyhow::Result<()> {
        sqlx::query("UPDATE input_receipts SET consumed = 1, payload = NULL WHERE thread_id = ? AND kind = ? AND scope = ? AND client_id = ?")
            .bind(thread_id.to_string()).bind(&receipt.kind).bind(&receipt.scope).bind(&receipt.client_id)
            .execute(self.input_receipts_pool().await?).await?;
        Ok(())
    }

    pub async fn cancel_turn_input_receipts(&self, thread_id: ThreadId, turn_id: &str) -> anyhow::Result<()> {
        // Accepted start payloads must reach the nonexecuting cold-recovery
        // history/artifact barrier before consumption. Revoke steering and
        // unaccepted reservations without erasing that accepted source.
        sqlx::query("UPDATE input_receipts SET consumed = 1, payload = NULL WHERE thread_id = ? AND turn_id = ? AND (kind != 'start' OR payload IS NULL)")
            .bind(thread_id.to_string()).bind(turn_id)
            .execute(self.input_receipts_pool().await?).await?;
        Ok(())
    }
}
