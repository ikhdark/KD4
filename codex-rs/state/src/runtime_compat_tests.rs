use super::*;

#[tokio::test]
async fn old_metadata_writer_survives_state_upgrade_with_a_held_wal_reader() {
    let home = tempfile::tempdir().unwrap();
    let path = crate::state_db_path(home.path());
    let old = SqlitePoolOptions::new()
        .max_connections(3)
        .connect_with(base_sqlite_options(&path))
        .await
        .unwrap();
    let mut prefix = runtime_state_migrator();
    prefix
        .migrations
        .to_mut()
        .retain(|migration| migration.version <= 46);
    prefix.run(&old).await.unwrap();
    let upsert = "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, model_provider, cwd, title, sandbox_policy, approval_mode, memory_mode) VALUES ('old-thread', 'rollout.jsonl', 1, 2, 'cli', 'fixture', '.', ?, 'read-only', 'never', 'disabled') ON CONFLICT(id) DO UPDATE SET title = excluded.title, memory_mode = excluded.memory_mode";
    sqlx::query(upsert)
        .bind("before")
        .execute(&old)
        .await
        .unwrap();
    let mut reader = old.begin().await.unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM threads WHERE id = 'old-thread'")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    assert_eq!(title, "before");

    // Keep the old pool/reader alive across actual runtime initialization.
    let runtime = StateRuntime::init(home.path().to_path_buf(), "fixture".into())
        .await
        .unwrap();
    sqlx::query(upsert)
        .bind("after")
        .execute(&old)
        .await
        .unwrap();
    let held: String = sqlx::query_scalar("SELECT title FROM threads WHERE id = 'old-thread'")
        .fetch_one(&mut *reader)
        .await
        .unwrap();
    assert_eq!(held, "before");
    reader.commit().await.unwrap();
    let row: (String, String) =
        sqlx::query_as("SELECT title, memory_mode FROM threads WHERE id = 'old-thread'")
            .fetch_one(&old)
            .await
            .unwrap();
    assert_eq!(row, ("after".into(), "disabled".into()));
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(&old)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");
    runtime.close().await;
    old.close().await;
}
