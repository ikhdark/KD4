use super::*;
use chrono::{DateTime, Utc};
use codex_protocol::{ThreadId, protocol::SessionSource};

#[tokio::test]
async fn audit20261023_expired_backfill_claim_cannot_mutate_its_successor() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let a = StateRuntime::init(home.path().into(), "test".into()).await?;
    let b = StateRuntime::init(home.path().into(), "test".into()).await?;
    let old = a.claim_backfill(60).await?.expect("first claim");
    sqlx::query("UPDATE backfill_state SET updated_at = 0").execute(a.pool.as_ref()).await?;
    let current = b.claim_backfill(60).await?.expect("replacement claim");
    assert_ne!(old.token, current.token);
    b.checkpoint_backfill_claim(&current, "successor").await?;
    assert!(a.checkpoint_backfill_claim(&old, "stale").await.is_err());
    assert!(a.release_backfill_claim(&old).await.is_err());
    assert!(a.complete_backfill_claim(&old, Some("stale")).await.is_err());
    assert!(a.mark_backfill_pending().await.is_err());
    assert!(a.mark_backfill_complete(None).await.is_err());
    assert_eq!(b.get_backfill_state().await?.last_watermark.as_deref(), Some("successor"));
    b.complete_backfill_claim(&current, None).await?;
    assert!(a.checkpoint_backfill_claim(&old, "resurrect").await.is_err());
    assert_eq!(b.get_backfill_state().await?.status, crate::BackfillStatus::Complete);
    b.request_backfill_refresh(0).await?;
    let next = b.claim_backfill(0).await?.expect("same-runtime new claim");
    assert_ne!(current.token, next.token);
    assert!(b.release_backfill_claim(&current).await.is_err());
    b.release_backfill_claim(&next).await?;
    Ok(())
}

#[tokio::test]
async fn audit20261023_live_timestamp_writes_use_the_committed_database_floor() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let a = StateRuntime::init(home.path().into(), "test".into()).await?;
    let b = StateRuntime::init(home.path().into(), "test".into()).await?;
    let id = ThreadId::new();
    let timestamp = DateTime::<Utc>::from_timestamp_millis(1_700_000_000_010).unwrap();
    let path = home.path().join("rollout.jsonl");
    std::fs::write(&path, "fixture")?;
    let mut builder = crate::ThreadMetadataBuilder::new(id, path.clone(), timestamp, SessionSource::Cli);
    builder.cwd = home.path().into();
    builder.updated_at = Some(timestamp);
    let metadata = builder.build("test");
    a.upsert_thread(&metadata).await?;
    b.touch_thread_updated_at(id, timestamp + chrono::Duration::milliseconds(11)).await?;
    let committed = b.get_thread(id).await?.unwrap().updated_at;
    a.touch_thread_updated_at(id, timestamp + chrono::Duration::milliseconds(10)).await?;
    let touched = a.get_thread(id).await?.unwrap().updated_at;
    assert!(touched > committed);
    b.upsert_thread(&metadata).await?;
    let upserted = b.get_thread(id).await?.unwrap().updated_at;
    assert!(upserted > touched);
    b.upsert_thread_preserving_timestamps(&metadata).await?;
    assert_eq!(b.get_thread(id).await?.unwrap().updated_at, timestamp);
    // Archive state is another live allocator consumer, not a reconciliation write.
    a.touch_thread_updated_at(id, Utc::now() + chrono::Duration::days(1)).await?;
    let before_archive = a.get_thread(id).await?.unwrap().updated_at;
    b.mark_archived(id, &path, Utc::now()).await?;
    assert!(b.get_thread(id).await?.unwrap().updated_at > before_archive);
    let row: (i64, i64) = sqlx::query_as("SELECT updated_at, updated_at_ms FROM threads WHERE id = ?").bind(id.to_string()).fetch_one(a.pool.as_ref()).await?;
    assert_eq!(row.0, row.1.div_euclid(1000));
    Ok(())
}

#[tokio::test]
async fn audit20261023_large_thread_membership_filters_preserve_query_semantics() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let runtime = StateRuntime::init(home.path().into(), "test".into()).await?;
    let entry = |thread: Option<&str>, message: &str| crate::LogEntry {
        ts: Utc::now().timestamp(), ts_nanos: 0, level: "INFO".into(), target: "audit".into(),
        message: Some(message.into()), feedback_log_body: Some(message.into()), thread_id: thread.map(str::to_string),
        process_uuid: None, module_path: None, file: None, line: None,
    };
    runtime.insert_logs(&[entry(Some("thread-1200"), "wanted"), entry(None, "threadless"), entry(Some("other"), "excluded")]).await?;
    let mut query = crate::LogQuery { thread_ids: (0..2000).map(|n| format!("thread-{n}")).collect(), ..Default::default() };
    let rows = runtime.query_logs(&query).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message.as_deref(), Some("wanted"));
    let max = runtime.max_log_id(&query).await?;
    assert_eq!(max, rows[0].id);
    query.include_threadless = true;
    assert_eq!(runtime.query_logs(&query).await?.len(), 2);
    query.after_id = Some(max);
    assert!(runtime.query_logs(&query).await?.iter().all(|row| row.id > max));
    Ok(())
}

#[tokio::test]
async fn audit20261023_retention_releases_its_connection_before_reclamation() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let runtime = StateRuntime::init(home.path().into(), "test".into()).await?;
    let entry = crate::LogEntry {
        ts: (Utc::now() - chrono::Duration::days(11)).timestamp(), ts_nanos: 0,
        level: "INFO".into(), target: "audit".into(), message: Some("x".repeat(4 * 1024 * 1024)),
        feedback_log_body: None, thread_id: Some("expired".into()), process_uuid: None,
        module_path: None, file: None, line: None,
    };
    let _ = runtime.insert_logs_deferred_retention(&[entry]).await?;
    let before: i64 = sqlx::query_scalar("PRAGMA page_count").fetch_one(runtime.logs_pool.as_ref()).await?;
    let mut occupied = Vec::new();
    for _ in 0..4 { occupied.push(runtime.logs_pool.acquire().await?); }
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), runtime.prune_log_retention(logs::LogRetentionScope::for_reconciliation())).await;
    drop(occupied);
    result.expect("retention must not wait for its own final pool slot")?;
    let after: i64 = sqlx::query_scalar("PRAGMA page_count").fetch_one(runtime.logs_pool.as_ref()).await?;
    assert!(after < before, "retention must reclaim pages, not silently skip maintenance");
    Ok(())
}

#[tokio::test]
async fn audit20261023_legacy_recency_history_opens_and_reopens_through_runtime() -> anyhow::Result<()> {
    use sqlx::migrate::Migration;
    for crlf in [false, true] {
        let home = tempfile::tempdir()?;
        let base = runtime_state_migrator();
        let mut migrations = base.migrations.iter().filter(|migration| migration.version <= 37).cloned().collect::<Vec<_>>();
        let recency = base.migrations.iter().find(|migration| migration.version == 39).unwrap();
        migrations.push(Migration::new(38, recency.description.clone(), recency.migration_type, recency.sql.clone(), recency.no_tx));
        let migrations = migrations.into_iter().map(|migration| {
            let sql = migration.sql.as_str().replace("\r\n", "\n");
            let sql = if crlf { sql.replace('\n', "\r\n") } else { sql };
            // Only line endings of embedded migrations change; no external SQL is accepted.
            let sql = sqlx::SqlSafeStr::into_sql_str(sqlx::AssertSqlSafe(sql));
            Migration::new(migration.version, migration.description, migration.migration_type, sql, migration.no_tx)
        }).collect();
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(state_db_path(home.path())).create_if_missing(true)).await?;
        let mut legacy = runtime_state_migrator();
        legacy.migrations = std::borrow::Cow::Owned(migrations);
        legacy.run(&pool).await?;
        let original: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 38").fetch_one(&pool).await?;
        pool.close().await;
        for _ in 0..2 {
            let runtime = StateRuntime::init(home.path().into(), "test".into()).await?;
            let repaired: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 39").fetch_one(runtime.pool.as_ref()).await?;
            assert_eq!(repaired, original, "{crlf}");
            let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'external_agent_config_imports'").fetch_one(runtime.pool.as_ref()).await?;
            assert_eq!(applied, 1);
            runtime.close().await;
        }
    }
    Ok(())
}
