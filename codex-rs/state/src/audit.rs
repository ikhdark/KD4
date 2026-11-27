//! Read-only state database queries for diagnostics.

use anyhow::Result;
use log::LevelFilter;
use sqlx::ConnectOptions;
use sqlx::Row;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteRow;
use std::path::Path;
use std::path::PathBuf;

/// Minimal thread metadata used by read-only state database audits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadStateAuditRow {
    pub id: String,
    pub rollout_path: PathBuf,
    pub archived: bool,
    pub source: String,
    pub model_provider: String,
}

/// Read persisted thread rows from a state DB without creating, migrating, or repairing it.
pub async fn read_thread_state_audit_rows(path: &Path) -> Result<Vec<ThreadStateAuditRow>> {
    decode_thread_state_audit_rows(query_thread_state_audit_rows(path, /*limit*/ -1).await?)
}

impl ThreadStateAuditRow {
    /// Read the complete inventory up to `max_rows`, or return `None` if it exceeds that bound.
    pub async fn read_bounded(path: &Path, max_rows: u32) -> Result<Option<Vec<Self>>> {
        // Read one extra row from the same query to distinguish an exact fit from truncation.
        let rows = query_thread_state_audit_rows(path, i64::from(max_rows) + 1).await?;
        if rows.len() > max_rows as usize {
            return Ok(None);
        }
        decode_thread_state_audit_rows(rows).map(Some)
    }
}

async fn query_thread_state_audit_rows(path: &Path, limit: i64) -> Result<Vec<SqliteRow>> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .log_statements(LevelFilter::Off);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let rows = sqlx::query(
        r#"
SELECT id, rollout_path, archived, source, model_provider
FROM threads
LIMIT ?
        "#,
    )
    .bind(limit)
    .fetch_all(&pool)
    .await?;
    pool.close().await;
    Ok(rows)
}

fn decode_thread_state_audit_rows(rows: Vec<SqliteRow>) -> Result<Vec<ThreadStateAuditRow>> {
    rows.into_iter()
        .map(|row| {
            let archived: i64 = row.try_get("archived")?;
            Ok(ThreadStateAuditRow {
                id: row.try_get("id")?,
                rollout_path: PathBuf::from(row.try_get::<String, _>("rollout_path")?),
                archived: archived != 0,
                source: row.try_get("source")?,
                model_provider: row.try_get("model_provider")?,
            })
        })
        .collect()
}
